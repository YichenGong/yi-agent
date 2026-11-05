//! 起轮入口：建立 thread 的运行时、装配 agent、然后委托给 `start_turn_core`。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。单独成片是因为 `turn/mod.rs`
//! 加上它后超过 700 行。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::protocol::SubagentTotals;
use crate::server::{
    BuiltAgent, ProcessWatch, ProjectRuntimes, ThreadRoots, TurnEvent, attach_delegation,
    run_thread_driver, watch_processes,
};
use crate::session::{InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt};
use crate::workspace_index::WorkspaceIndex;
use tokio::sync::{Mutex, mpsc, oneshot};
use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::thread_root::ThreadRoot;

/// 起一个 thread 的内核:attach_delegation → 建 driver 通道 → 登记进 `threads`
/// → spawn 进程守望者与 driver task。
///
/// `built` 由调用方先经 `build_agent` 造好(工厂在 `serve`/驱动器里同步调用,不在
/// 本内核内),这样内核不必把 `&F` 跨 await 持有——否则会强加 `F: Sync` 并波及
/// `serve` 的 Send 边界。只负责「得到一个已登记、driver 已 spawn 的 thread_id」
/// 这件事;RPC 层的事(`resolve_thread_cwd` 的错误响应、`thread/started` 通知与
/// 最终响应)留在调用方,以便驱动器复用同一内核而不发 RPC 帧。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_thread_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &mut HashMap<String, ProcessWatch>,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    models_path: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    mode: crate::thread_store::ThreadMode,
    thread_id: String,
    // 本会话的关联根（主根之外的附属根），由调用方在 `with_session_extra_roots`
    // 的同一作用域内捕获后传入：既写进 meta（快照），也交给 `attach_delegation`
    // 供委派路径构造多根工具集。**不在此处读槽位**，否则调用点一旦漏设就会静默
    // 拿到空集（或残留值）。
    extra_roots: Vec<PathBuf>,
    built: BuiltAgent,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    workspaces: &WorkspaceIndex,
    // 该会话的生效模型串与覆盖名(由调用方按清单解析后传入),落进 ThreadMeta
    // 并写进 ThreadSession,与 `thread/start`/`thread/resume` 同一来源。
    model: String,
    model_ref: Option<String>,
    // 被驱动会话的所属项目根与驱动方给的不透明句柄(通用名;旧名见
    // `ThreadMeta` 的 `serde(alias)`)。两者都只为 worktree 判定与侧栏归组落盘。
    driver_project: Option<&str>,
    driver_ref: Option<&str>,
    // 进程的组长信箱:转交 `attach_delegation`,使本会话的 `ask_lead` 能投递。
    lead_inbox: &crate::lead::LeadInbox,
    // 进程的回信登记表:与信箱同路转交,使本会话的 `ask_lead blocking: true`
    // 真的能等到回信(`attach_delegation` 把它接进 `AskLeadTool::with_replies`)。
    lead_replies: &crate::lead::LeadReplies,
    // 组长写通道与结果登记表:与 `lead_inbox`/`lead_replies` 同路转交(经
    // `attach_delegation` 进 `build_runtime_tooling`),使本会话若判定为组长便能
    // 注册那两个写工具。必须是主循环共享的同一实例——自造一份只会投进没人听的
    // 通道。
    lead_control: &crate::lead_control::LeadControlInbox,
    lead_control_replies: &crate::lead_control::LeadControlReplies,
    // 组长开关。由调用方从实例状态取好后传入（见 `lead_gate` 的模块文档：
    // 不在这里读全局量，那在并行测试里会互相污染）。
    lead_agent_enabled: Option<bool>,
    // 调用方额外注入给本会话 shell 的环境变量。被驱动的会话用它带插件给的卡片
    // 句柄；RPC 起的会话传空集（与改动前逐字一致）。
    extra_session_env: Vec<(String, String)>,
    // 本会话绑定的插件发布（零或多条）。驱动会话由 launcher 按已提交激活算好
    // 传进来；RPC 起的会话传空集（= 普通会话，不绑插件）。
    //
    // 为什么由调用方给而不是在这里算：算它需要插件名，而 `start_thread_core`
    // 对插件一无所知——这正是宿主的边界（不认识任何具体插件）。
    plugin_bindings: Vec<yi_agent_plugins::Binding>,
) -> anyhow::Result<()> {
    let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(cwd)));

    // 会话起点（默认 diff 基准）。**必须在 `attach_delegation` 之前算好并传进去**：
    // 该函数要把起点交给 `show_git_diff`，而 meta 要到本函数后半段才 `create`；
    // 若让 `attach_delegation` 自己读盘，新建路径永远读到 `None`，工具的默认基准
    // 就落不到「本次会话起点」，只剩未提交改动——与它自己的描述不符。
    // 取值口径不变：仍是「建 meta 时取 HEAD」（此后 HEAD 随会话推进）。
    let anchor_sha = crate::git_diff::session_anchor(std::path::Path::new(cwd));

    // 委派是可选能力:项目 runtime 起不来就保留原 agent,只记 trace。
    // 接线刻意放在这里(而非 `threads` 守卫之内),避免与其可变借用冲突。
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(cwd));
    let activation = attach_delegation(
        runtimes,
        thread_roots,
        &runtime_dir,
        models_path,
        cfg,
        cwd,
        &thread_id,
        theme,
        git_diff,
        // 调用方捕获的同一根集：落进 meta 的与交给委派路径的必须是同一个值，
        // 否则「meta 说有一根、实际工具集里没有」这类不一致无从察觉。
        extra_roots.clone(),
        lead_inbox,
        lead_replies,
        lead_control,
        lead_control_replies,
        // 新建路径：起点就是刚刚取的 HEAD。
        anchor_sha.clone(),
        lead_agent_enabled,
        extra_session_env,
        built,
    );
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        catalog,
        yolo,
        process_manager,
        mcp,
        ..
    } = activation.built;
    pending_activation.insert(thread_id.clone(), activation.runtime);
    // MCP 配置固定在 thread cwd 下(不是 daemon 回传的 toplevel):与
    // `build_runtime_tooling` 里 `ThreadStore::new(&cfg.workdir)` 同一口径。
    let mcp_workdir = std::path::PathBuf::from(cwd);

    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);

    let now = crate::thread_store::now_millis();
    // 会话起点已在上面算好并交给 `attach_delegation`（同一个值也落进 meta）：
    // 两处必须是同一个 sha，否则「meta 里的起点」与「工具用的起点」会分叉。
    let meta = crate::thread_store::ThreadMeta {
        plugin_bindings,
        thread_id: thread_id.clone(),
        cwd: cwd.to_string(),
        model: model.clone(),
        created_at: now,
        updated_at: now,
        title: None,
        permission_mode: mode,
        pin_seq: None,
        driver_project: driver_project.map(str::to_string),
        driver_ref: driver_ref.map(str::to_string),
        model_ref,
        reasoning_effort: None,
        tag: None,
        // 新会话默认开启跨轮保留(与 `ThreadMeta` 的缺省语义一致)。
        preserve_thinking: true,
        // 从索引快照下来的关联根：运行期不变（后续改索引不追改本会话）。
        extra_roots: extra_roots
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        role: None,
        content_epoch: 0,
        last_item_id: None,
        anchor_sha,
        // 新会话：从未被读过、也从未完成过回合 ⇒ 0/0，不亮蓝点。
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
    };
    // 组长 meta 可能已由 `ensure_lead_meta` 落盘:此时**不得覆盖**,否则会把它
    // 的 `role: "lead"` 抹掉(本函数的 meta 不带 role),懒创建出来的组长就
    // 成了一个普通会话。先探测再建,代价是一次 `meta.json` 读;`thread/start`
    // 每次都用新铸的 uuid,故该路径行为不变(仍然是建)。
    let already_persisted = thread_store.load(&thread_id).ok().flatten().is_some();
    if !already_persisted && let Err(e) = thread_store.create(&meta) {
        // 持久化是尽力而为:写失败不阻断 thread 创建。
        eprintln!("[app-server] failed to create thread meta for {thread_id}: {e}");
    }
    // 记录到全局「最近目录」索引,供 thread/listAll 与侧栏复用。
    // 索引键统一为 canonical,与 workspace/remove 的规范化对齐;
    // thread 的 cwd / meta 仍保持 `resolve_thread_cwd` 的原样。
    let index_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    if let Err(e) = workspaces.add(&index_path) {
        eprintln!(
            "[app-server] failed to record workspace {}: {e}",
            index_path.display()
        );
    }

    // 先建句柄:同一 `Arc` 既存进 session 供 `thread/list` 读,
    // 也交给 driver 供其推送 `thread/status/updated`。
    let store_status = ThreadSession::new_status();
    // driver 与 children watch 之间唯一的通道：watch 侧只送最新汇总，
    // driver 侧据此生成完整的 TokenUsage（它才拿得到 total_*）。
    let (subagent_tx, subagent_rx) = tokio::sync::watch::channel(SubagentTotals::default());
    // 新会话同样要成为 writer，同样要取锁。`thread/start` 刚生成的 id 理论上
    // 不会与别人撞，但锁的意义是「同一路径只有一个 writer」，不是「防撞 id」：
    // 落盘后用同一 id 另开一个进程 resume 时，这把锁就是唯一屏障。
    // 只有 `Busy` 才拒绝；`Io`（锁文件建不出来）退化为无锁，与加锁前行为一致。
    let writer_lock =
        match crate::thread_store::acquire_thread_lock(thread_store.root(), &thread_id) {
            Ok(lock) => Some(lock),
            Err(crate::thread_store::ThreadLockError::Busy(busy)) => {
                return Err(anyhow::anyhow!(busy.to_string()));
            }
            Err(e @ crate::thread_store::ThreadLockError::Io(_)) => {
                tracing::warn!(%e, thread_id = %thread_id, "会话写锁不可用，退化为无锁");
                None
            }
        };
    threads.insert(
        thread_id.clone(),
        ThreadSession {
            thread_id: thread_id.clone(),
            cwd: cwd.to_string(),
            model: model.clone(),
            active_turn_id: None,
            turn_permit: None,
            yolo,
            process_manager: Arc::clone(&process_manager),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store: Arc::clone(&thread_store),
            status: Arc::clone(&store_status),
            subagent_tx,
            writer_lock,
        },
    );

    // 先停旧守望者、再订阅本次生效的 manager。`thread/start` 通常没有
    // 旧守望者(remove 得到 None,no-op),但 `thread/resume` 恢复一个
    // 仍在内存的活 thread 时会换掉生效的 manager:旧守望者若被
    // `contains_key` 守卫留下,就仍订阅那份已被丢弃的 manager,与
    // `process/list` 用的新 manager 脱节,`process/updated` 静默失联。
    if let Some(previous) = process_watches.remove(&thread_id) {
        previous.stop().await;
    }
    let rx = process_manager.subscribe();
    let handle = tokio::spawn(watch_processes(
        Arc::clone(hub),
        thread_id.clone(),
        Arc::clone(&process_manager),
        rx,
    ));
    process_watches.insert(thread_id.clone(), ProcessWatch { task: handle });

    // 每个 thread 一个 driver task:独占 agent 与两个 receiver,
    // 串行驱动 turn。
    let driver_hub = Arc::clone(hub);
    let driver_client = client.clone();
    let driver_turn_tx = turn_tx.clone();
    let driver_thread_id = thread_id.clone();
    tokio::spawn(run_thread_driver(
        driver_thread_id,
        agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        driver_hub,
        driver_client,
        driver_turn_tx,
        decision_tx,
        Arc::clone(pending),
        permission_timeout,
        Arc::clone(perm_seq),
        catalog,
        Arc::clone(&thread_store),
        Arc::clone(&store_status),
        provider,
        config,
        subagent_rx,
        mcp,
        mcp_workdir,
        // Resolved from the conversation's own root on the first turn.
        None,
    ));

    Ok(())
}
