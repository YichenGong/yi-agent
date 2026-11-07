//! 宿主侧的父来源解析：把「父 thread id」变成一个旁支可运行的
//! [`EffectiveSideSource`]。
//!
//! 两条路径，各自的硬约束完全不同：
//!
//! - **活跃父**（该 thread 正由本进程驱动）：只用内存里已经发布的那份来源。它的
//!   session 就是运行中 agent 的**同一个 `Arc`**，所以拷贝出来的是真正正在用的
//!   历史，而不是回读磁盘得到的近似值。
//! - **冷父**（没有 driver）：严格只读地解码持久文件，用与普通装配同一套
//!   **无副作用**配置解析算出「若此刻正常启动会用什么配置」。
//!
//! 冷读这条路径最容易犯错的地方是「顺手做点本可以做的事」：`resume` 一下、修一下
//! `partial`、抢一下 writer lock、生成个标题。任何一件都会让「打开旁支」这个纯读
//! 动作改变父会话的磁盘状态，甚至在父正被别的进程写时把它卡住。所以这里**一律不
//! 做**，且用测试钉住。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use yi_agent_core::{Agent, Session};
use yi_agent_runtime::bootstrap::{
    build_agent_config, build_provider, resolve_side_config, resolve_side_prompt,
};
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_runtime::side_conversation::{
    EffectiveSideSource, SideAttachment, SideAttachmentCatalog, SideError, SideErrorReason,
    SideResult, SideWebPolicy,
};

use crate::thread_store::ThreadStore;

/// 解析一个父 thread 的旁支来源。
///
/// 返回 `BoxFuture` 而不是 `async fn`：trait 对象里要能 await，且必须是 `Send`。
/// `'a` 只绑住 `parent`，实现方可以把 `self` 里的东西 `clone` 进 future——这正是
/// 活跃路径需要的（它要克隆 provider / session 的 `Arc`）。
pub trait SideParentResolver: Send + Sync {
    fn resolve<'a>(&'a self, parent: &'a str) -> BoxFuture<'a, SideResult<EffectiveSideSource>>;
}

/// 生产工厂：持有解析所需的全部依赖，**不**借用任何 `BuildAgent`。
///
/// 这一点是必须的：`BuildAgent<'a>` 借用了装配期的栈上配置，一旦被移进 spawn 的
/// task 就成了悬空引用。工厂只有拥有所有权的数据，才能被 `Arc` 起来跨 task 共享。
pub struct SideResolverFactory {
    /// 各项目按需装载的 runtime（模型目录、项目索引都从这里来）。
    pub runtime_dir: PathBuf,
    pub models_path: PathBuf,
}

impl SideResolverFactory {
    pub fn new(runtime_dir: impl Into<PathBuf>, models_path: impl Into<PathBuf>) -> Self {
        Self {
            runtime_dir: runtime_dir.into(),
            models_path: models_path.into(),
        }
    }
}

/// 生产端的父来源解析:按 threadId 在工作区索引里定位它的 store,再严格冷读。
///
/// 为什么只有冷读路径:活跃父的 source 需要 `ThreadSession` 持有
/// `SideSourcePublisher`(Task 10 留下、本任务文件清单外),而 `serve()` 拿到的
/// `threads` 是主循环独占的局部量。冷读是**原子的**(Task 10 的指纹重试),所以
/// 旁支不会读到撕裂的历史;代价是可能漏掉父正在流式输出的那一轮。这不是猜测:
/// `read_cold_parent` 先读内容、再核对读前读后的指纹,只有两次都一致才发布。
pub struct WorkspaceResolver {
    workspaces: Arc<crate::workspace_index::WorkspaceIndex>,
    base: RuntimeConfig,
}

impl WorkspaceResolver {
    pub fn new(
        workspaces: Arc<crate::workspace_index::WorkspaceIndex>,
        base: RuntimeConfig,
    ) -> Self {
        Self { workspaces, base }
    }
}

impl SideParentResolver for WorkspaceResolver {
    fn resolve<'a>(&'a self, parent: &'a str) -> BoxFuture<'a, SideResult<EffectiveSideSource>> {
        Box::pin(async move {
            let store = crate::server::store_for(&self.workspaces, &self.base, parent);
            ThreadStoreResolver::new(store, self.base.clone())
                .resolve(parent)
                .await
        })
    }
}

/// 一份「父的持久状态 + 解析出的配置」，用来装配冷来源。
///
/// 单独成一个结构体，是为了让 `assemble_cold_source` 专注于「怎么把它变成
/// `EffectiveSideSource`」，而把读盘/解析留在 resolver 里——两者出错时要返回的
/// `SideError` 原因完全不同。
pub struct ColdParent {
    pub snapshot: crate::side_cold::StrictThreadSnapshot,
    pub config: RuntimeConfig,
}

/// 严格重试次数。
///
/// 取 3 与 `SOURCE_SNAPSHOT_ATTEMPTS` 一致：读到一半父被写是正常的（用户可能就在
/// 打字），但连续 3 次都撞上说明父在持续更新，此时有界失败比无限重试诚实——调用方
/// 会得到一个明确的「父上下文正在更新」，而不是一个永远转圈的 open。
pub const SIDE_COLD_ATTEMPTS: usize = 3;

/// 从冷读快照装配出来源。
///
/// 会做的：解析 system prompt（只读 skills 扫描）、构造独立的 session、按安全历史
/// 过滤附件 catalog。
/// 绝不会做的：调用 `resume`、登记 driver、`promote_partial`、写任何文件、连 MCP。
pub fn assemble_cold_source(
    snapshot: crate::side_cold::StrictThreadSnapshot,
    config: RuntimeConfig,
) -> SideResult<EffectiveSideSource> {
    if snapshot.messages.is_empty() {
        return Err(SideError::new(
            SideErrorReason::ParentNotStarted,
            "父会话还没有可继承的安全历史",
        ));
    }

    // 预算检查必须在克隆之前：一份 200 MiB 的历史本来就装不进旁支，先测量再决定
    // 要不要复制，比复制完再报错便宜得多。
    yi_agent_core::measure_messages(
        &snapshot.messages,
        &yi_agent_core::ManagedRunLimits::side_defaults(),
    )
    .map_err(|error| {
        SideError::new(
            SideErrorReason::SnapshotTooLarge,
            format!("父会话历史超出旁支预算：{error}"),
        )
    })?;

    let provider = build_provider(&config).map_err(|error| {
        SideError::new(
            SideErrorReason::ParentNotStarted,
            format!("无法按父会话配置构建 provider：{error}"),
        )
    })?;

    let system_prompt = resolve_side_prompt(&config).map_err(|error| {
        SideError::new(
            SideErrorReason::ParentNotStarted,
            format!("无法解析父会话系统提示：{error}"),
        )
    })?;

    // Same mapping the ordinary assembly uses, so a side inherits the parent's
    // compaction/turn budgets instead of core defaults.
    let side_config = build_agent_config(&config, system_prompt);

    let primary = config.workdir.clone();
    let mut roots = vec![primary.clone()];
    for extra in &config.sandbox_writable_roots {
        if !roots.contains(extra) {
            roots.push(extra.clone());
        }
    }

    // 附件 catalog 只由**持久化的 struct 字段**构成，且要求当前安全历史里仍然留着
    // 那条清单标记。这里不做任何路径推测。
    let session = Arc::new(std::sync::Mutex::new(Session::from_messages(
        snapshot.messages.clone(),
    )));
    let catalog = catalog_from_persisted(&snapshot.attachments, &session, &primary);

    Ok(EffectiveSideSource {
        session,
        provider,
        config: side_config,
        primary: primary.clone(),
        roots: roots.clone(),
        current_roots: Arc::new(yi_agent_tools::StaticSideRoots::new(roots)),
        attachments: catalog,
        revision: 1,
        generation: 1,
        provider_gate: None,
        web_policy: SideWebPolicy::default(),
        // Filled in by the caller that holds the parent agent's live source; a
        // side created from persisted state alone has none.
        live: None,
    })
}

/// 把持久化的附件记录转成 catalog，只保留清单标记仍在安全历史里的那些。
fn catalog_from_persisted(
    attachments: &[crate::protocol::Attachment],
    session: &Arc<std::sync::Mutex<Session>>,
    primary: &Path,
) -> SideAttachmentCatalog {
    let entries: Vec<SideAttachment> = attachments
        .iter()
        .map(|attachment| SideAttachment {
            original: resolve_manifest_path(primary, &attachment.path),
            manifest_marker: attachment.path.clone(),
        })
        .collect();
    let catalog = SideAttachmentCatalog(entries);
    // `resolve` 需要 `forkable_messages`：它会把「父的清单行已被裁掉」的附件剔除。
    // 失败（锁中毒）时保守地返回空 catalog：少给一个文件比多给一个未授权文件好。
    match catalog.resolve(session) {
        Ok(retained) => SideAttachmentCatalog(
            catalog
                .0
                .into_iter()
                .filter(|entry| retained.contains(&entry.original))
                .collect(),
        ),
        Err(_) => SideAttachmentCatalog::default(),
    }
}

/// 清单里记的是相对工作区的路径（`.yi-agent/attachments/...`）。
fn resolve_manifest_path(primary: &Path, recorded: &str) -> PathBuf {
    let path = Path::new(recorded);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        primary.join(path)
    }
}

/// 冷父的生成器：给定 store + 基础配置，严格读并解析。
///
/// 抽成自由函数而不是方法，是为了让「重试」这件事留在调用方，而这一层只负责
/// 「读一次、算一次」——重试逻辑一旦和读盘混在一起，就很难证明每次重试都重新取了
/// 指纹。
pub fn read_cold_parent(
    store: &ThreadStore,
    parent: &str,
    base: &RuntimeConfig,
) -> SideResult<ColdParent> {
    let snapshot = match store.read_side_snapshot_strict(parent) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            return Err(SideError::new(
                SideErrorReason::UnknownThread,
                format!("找不到父会话 {parent}"),
            ));
        }
        Err(error) => {
            // A corrupt or oversized parent is not "not started": the user can act on
            // a corrupt-file error (delete it, fix it), but telling them "父会话还没有
            // 开始" when the file is in fact broken sends them looking in the wrong
            // place.
            let reason = if error.to_string().contains("over the") {
                SideErrorReason::SnapshotTooLarge
            } else {
                SideErrorReason::ParentNotStarted
            };
            return Err(SideError::new(
                reason,
                format!("无法严格读取父会话 {parent}：{error}"),
            ));
        }
    };

    let cwd = PathBuf::from(&snapshot.meta.cwd);
    let config = resolve_side_config(
        base,
        &cwd,
        Some(&snapshot.meta.model),
        snapshot.meta.reasoning_effort.as_deref(),
        snapshot.meta.preserve_thinking,
    )
    .map_err(|error| {
        SideError::new(
            SideErrorReason::ParentNotStarted,
            format!("无法解析父会话配置：{error}"),
        )
    })?;
    // 关联根在 runtime 配置里就是 `sandbox_writable_roots`（工具层与权限层共用
    // 同一份）。不带上它，父能读的目录在旁支里会忽然不存在。
    let mut config = config;
    config.sandbox_writable_roots = snapshot
        .meta
        .extra_roots
        .iter()
        .map(PathBuf::from)
        .collect();

    Ok(ColdParent { snapshot, config })
}

/// 从运行中的 agent 抽一份来源（活跃父路径）。
///
/// 关键点：`session` 用的是 agent 的**同一份 `Arc`**，`provider` 也是同一个对象。
/// 这样旁支继承的正是父此刻使用的历史，而不是「上次落盘的版本」；凭据也只读一次。
pub fn source_from_live_agent(
    agent: &Agent,
    config: RuntimeConfig,
    primary: impl Into<PathBuf>,
    roots: Vec<PathBuf>,
    attachments: SideAttachmentCatalog,
) -> SideResult<EffectiveSideSource> {
    let session = agent.session_handle();
    // 在锁外测量：`measure_side_history` 会逐个消息序列化，持锁期间算它会让父的
    // 下一轮迟迟拿不到锁。
    {
        let guard = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        yi_agent_core::measure_side_history(
            &guard,
            &yi_agent_core::ManagedRunLimits::side_defaults(),
        )
        .map_err(|error| {
            SideError::new(
                SideErrorReason::SnapshotTooLarge,
                format!("父会话历史超出旁支预算：{error}"),
            )
        })?;
        if yi_agent_core::forkable_messages(&guard).is_empty() {
            return Err(SideError::new(
                SideErrorReason::ParentNotStarted,
                "父会话还没有可继承的安全历史",
            ));
        }
    }

    let system_prompt = resolve_side_prompt(&config).map_err(|error| {
        SideError::new(
            SideErrorReason::ParentNotStarted,
            format!("无法解析父会话系统提示：{error}"),
        )
    })?;

    let side_config = build_agent_config(&config, system_prompt);

    Ok(EffectiveSideSource {
        session,
        provider: yi_agent_runtime::bootstrap::build_provider(&config).map_err(|error| {
            SideError::new(
                SideErrorReason::ParentNotStarted,
                format!("无法按父会话配置构建 provider：{error}"),
            )
        })?,
        config: side_config,
        primary: primary.into(),
        roots: roots.clone(),
        current_roots: Arc::new(yi_agent_tools::StaticSideRoots::new(roots)),
        attachments,
        revision: 1,
        generation: 1,
        provider_gate: None,
        web_policy: SideWebPolicy::default(),
        // Filled in by the caller that holds the parent agent's live source; a
        // side created from persisted state alone has none.
        live: None,
    })
}

/// 一个共享的生成器：把本项目所有父 thread 的 store 与基础配置封装起来，供
/// resolver 的默认实现使用。
pub struct ThreadStoreResolver {
    store: Arc<ThreadStore>,
    base: RuntimeConfig,
    /// 在每次「读前 / 装配后」两次取指纹之间运行。生产恒为 `None`。
    ///
    /// 存在的理由：重试路径只在「父恰好在读的窗口里变了」时才走到，靠真的起一个
    /// 并发写者去撞这个窗口既慢又不稳定。注入一个钩子能把「变了」这件事变成确定的，
    /// 于是「连续三次变化必须失败而不是无限重试」这条才可测。
    churn: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ThreadStoreResolver {
    pub fn new(store: Arc<ThreadStore>, base: RuntimeConfig) -> Self {
        Self {
            store,
            base,
            churn: None,
        }
    }

    /// 注入一个「每轮读都会改变父」的钩子（测试用）。
    #[cfg(test)]
    pub fn with_churn(mut self, churn: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.churn = Some(churn);
        self
    }
}

impl SideParentResolver for ThreadStoreResolver {
    fn resolve<'a>(&'a self, parent: &'a str) -> BoxFuture<'a, SideResult<EffectiveSideSource>> {
        Box::pin(async move {
            for attempt in 0..SIDE_COLD_ATTEMPTS {
                let cold = read_cold_parent(&self.store, parent, &self.base)?;
                // 内容读完之后才让父变得「又不一样」：钩子正好落在
                // 「读到内容」与「取指纹确认」之间，也就是真实竞态发生的那个窗口。
                if let Some(churn) = &self.churn {
                    churn();
                }
                let before = self
                    .store
                    .side_cold_stamp(parent)
                    .map_err(|error| cold_io_error(parent, error))?;
                // 指纹必须与读到的内容对上：先读后取指纹跟先取指纹后读之间有一个
                // 窗口，父正好在那里被改写，我们就会把 A 的文件配上 B 的指纹当成
                // 一致。这里要求「读之前记录的 stamp」等于「读到内容里的 stamp」，
                // 再取一次确认没在装配期间被改。
                if cold.snapshot.stamp != before {
                    tokio::task::yield_now().await;
                    continue;
                }

                let source = assemble_cold_source(cold.snapshot, cold.config)?;
                let after = self
                    .store
                    .side_cold_stamp(parent)
                    .map_err(|error| cold_io_error(parent, error))?;
                if before != after {
                    // The parent moved while we assembled. Retry rather than publish a
                    // source whose config was derived from an older file set.
                    let _ = attempt;
                    tokio::task::yield_now().await;
                    continue;
                }
                return Ok(source);
            }
            Err(SideError::new(
                SideErrorReason::SourceChanged,
                "父上下文正在更新，请稍后重试",
            ))
        })
    }
}

fn cold_io_error(parent: &str, error: std::io::Error) -> SideError {
    SideError::new(
        SideErrorReason::ParentNotStarted,
        format!("无法读取父会话 {parent} 的文件状态：{error}"),
    )
}

#[cfg(test)]
mod tests;
