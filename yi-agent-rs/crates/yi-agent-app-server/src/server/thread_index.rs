//! 会话索引、store 定位、线程摘要与客户端决策解析。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 3）。本组被 `rpc/thread.rs` 直接
//! 引用 11 个函数，`server.rs` 的 re-export 必须完整。

use crate::protocol::{Notification, RequestId, RpcError, ThreadStatus};
use crate::session::ThreadSession;
use crate::workspace_index::WorkspaceIndex;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::thread_root::ThreadRoot;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{
    BuiltAgent, ProjectRuntimes, ThreadRoots, attach_cwd_runtime_with, build_runtime_tooling,
    err_response, project_key, read_task_trace, wrap_for_delegation, write_notification,
    write_response,
};

/// 标签显示名的长度上限。必须与前端 `desktop/src/lib/threadTags.ts` 的
/// `TAG_NAME_MAX_LEN` 一致——超出会撑破菜单。
///
/// `pub(crate)`:校验 arm 在 `ui/settings/write` 搬进 `rpc/ui.rs` 后仍要读它
/// (测试模块经 `use super::*` 也取同一份)。
pub(crate) const TAG_NAME_MAX_LEN: usize = 24;
/// 一次 attach 的结果:可能被换过工具集的 agent,以及该 thread 首个 turn 要激活的 runtime。
pub(crate) struct Activation {
    pub(crate) built: BuiltAgent,
    pub(crate) runtime: Option<Arc<ThreadRoot>>,
    /// 降级装配的**真实原因**(`runtime: None` 时才有值;成功为 `None`)。
    ///
    /// 为什么需要它:降级会保留原 agent,而原 agent 的工具集里**没有**调用方要求
    /// 注入的那些环境变量。对 RPC 起的会话这无所谓(它从不要 env);对驱动侧的冷
    /// 装配则不行——插件注入的卡片句柄若是假的,合并 objective 里的状态命令会
    /// 打到错误的地方。所以冷装配必须能看见「我确实降级了、原因是这个」,再由它
    /// 决定是报 `RestoreFailed` 还是接受无委派(见 `server/driver_resume.rs`)。
    pub(crate) tooling_error: Option<String>,
}
/// The scope key a conversation in `cwd` belongs to.
///
/// The same resolution the guardian, the shell and the runtime status all use: one
/// rule, so a capability minted for this scope is matched here and nowhere else. It is
/// a *scope* key, not a root id -- deriving a root id from a directory is the mistake
/// this whole design removes.
fn scope_key_for(cwd: &str) -> String {
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(cwd));
    yi_agent_lifecycle::scope::resolve(
        &runtime_dir,
        Path::new(cwd),
        &crate::server::instance_home_dir(),
    )
    .map(|scope| scope.key)
    .unwrap_or_else(|_| runtime_dir.to_string_lossy().into_owned())
}

/// 为该 cwd 的 agent 接上委派能力。失败即降级:保留原 agent,只记 trace。
///
/// 调用方必须已拿到 `build_agent` 的产物,且**不得**在 `threads` 的可变借用内调用:
/// 本函数只经 `runtimes` 的 `Mutex` 访问,不触碰 `threads`,所以先调用、再
/// `threads.insert(..)` 是安全的。
#[allow(clippy::too_many_arguments)]
pub(crate) fn attach_delegation(
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    runtime_dir: &Path,
    // The shell-brokered authority for this session, when this sidecar serves one.
    // `None` is the embedded shape and keeps the old behaviour byte for byte.
    managed: Option<Arc<crate::server::ManagedAuthority>>,
    models_path: &Path,
    // The snapshot the entry point resolved once: attach must not re-read the
    // process policy, or watch/install and attach can disagree.
    bringup: &yi_agent_subagent::binding::BringUpChoice,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    // 本次会话的关联根（主根之外），由调用方在 `with_session_extra_roots` 的同一
    // 作用域内捕获后传入——**不在此处再读槽位**，否则调用点一旦漏设就会静默拿到
    // 空集（或残留值）。
    extra_roots: Vec<PathBuf>,
    lead_inbox: &crate::lead::LeadInbox,
    // 阻塞式 `ask_lead` 的回信登记表。与 `lead_inbox` 必须成对传递:
    // 只传信箱而不传登记表,`await` 侧登记的名字将永远无人回填。
    lead_replies: &crate::lead::LeadReplies,
    // 组长写通道与结果登记表:转交 `build_runtime_tooling`,使本会话若判定为组长
    // 便能注册那两个写工具。与 `lead_inbox` 相对(那个是 thread→lead,这个是
    // lead→thread)、同一注入理由:必须与主循环共享同一实例,自造一份就投进
    // 没人听的通道。
    lead_control: &crate::lead_control::LeadControlInbox,
    lead_control_replies: &crate::lead_control::LeadControlReplies,
    // 本会话的起点 sha(默认 diff 基准)。**由调用方给出,不在本函数里读盘**:
    // `thread/start` 走到这里时 meta 尚未落盘(`start_thread_core` 先 attach 再
    // `create`),在这里读只会永远读到 `None`,工具就落不到会话起点上。调用方
    // 才知道自己在哪个分支——新建取 `session_anchor(cwd)`,resume 取已落盘 meta
    // 的 `anchor_sha`(旧会话没有该字段时传 `None`,表示「无起点」,不回填)。
    anchor_sha: Option<String>,
    // 组长开关。逐调用传入，理由同 `anchor_sha`：装配期定值，会话存续期间不再变
    // （开关变更靠 resume 生效）。
    lead_agent_enabled: Option<bool>,
    // 调用方额外注入给本会话 shell 的环境变量（见 `build_runtime_tooling`）。
    extra_session_env: Vec<(String, String)>,
    // 进程级运行期设置绑定。只有本进程自己起的 daemon 用得上（见
    // `attach_cwd_runtime`）；`None` 与历史行为逐字一致。
    live_settings: Option<&yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
    // The sandbox this conversation runs under. Passed in, never read from `cfg`
    // here: `cfg.sandbox` is today's default for *new* conversations, and using
    // it while re-opening an old one is the retroactive widening this whole
    // module is about. Callers resolve it from the conversation's own record.
    sandbox_baseline: Option<crate::session_sandbox::SandboxBaseline>,
    built: BuiltAgent,
) -> Activation {
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = PathBuf::from(cwd);
    // Overwrite the per-call copy only. `cfg` is the process-wide default shared
    // by every conversation, so writing this conversation's baseline into it
    // would leak one conversation's sandbox into the next one built.
    if let Some(baseline) = sandbox_baseline {
        thread_cfg.sandbox = baseline.mode;
        thread_cfg.sandbox_promotable = baseline.promotable;
    }
    // The choice was resolved once by the caller and the settings snapshot beside
    // it: both describe the same entry point, so passing only one would attach
    // with a policy the settings were not read under.
    let binding = match attach_cwd_runtime_with(
        runtimes,
        runtime_dir,
        &thread_cfg,
        models_path,
        bringup,
        live_settings,
    ) {
        Ok(binding) => binding,
        Err(cause) => {
            tracing::warn!(
                stage = "attach",
                %cause,
                cwd,
                "subagent delegation unavailable for this thread"
            );
            return Activation {
                built,
                runtime: None,
                tooling_error: Some(format!("subagent runtime unavailable: {cause}")),
            };
        }
    };
    // The daemon is shared per project, but the root is per conversation: the
    // root is what carries the child budget, so two conversations in one
    // directory must never share it.
    // The same captured set that goes into the thread meta: a conversation's
    // delegation root must carry the same linked roots the rest of the session
    // was built with, or the two disagree with no way to notice.
    // Which kind of root this conversation gets is a property of the deployment, not a
    // preference: a managed session must ask the shell for its own authority, and an
    // embedded one has nobody to ask. Taking the managed branch whenever the shell
    // authorized this session is what keeps a managed conversation from silently
    // falling back to a self-declared attach.
    let root = match managed.as_ref() {
        Some(authority) => ThreadRoot::managed(
            Arc::clone(&binding),
            thread_id,
            PathBuf::from(cwd),
            extra_roots.clone(),
            Arc::new(
                yi_agent_subagent::managed_binding::ManagedContext::new(
                    Arc::clone(&authority.store),
                    scope_key_for(cwd),
                    authority.client_run_id.clone(),
                )
                .with_broker(Arc::clone(&authority.broker)),
            ),
        ),
        None => ThreadRoot::new(
            Arc::clone(&binding),
            thread_id,
            PathBuf::from(cwd),
            extra_roots.clone(),
        ),
    };
    if let Err(cause) = root.attach() {
        tracing::warn!(
            stage = "attach_root",
            %cause,
            cwd,
            thread_id,
            "subagent delegation unavailable for this thread"
        );
        return Activation {
            built,
            runtime: None,
            tooling_error: Some(format!("subagent root could not attach: {cause}")),
        };
    }
    thread_roots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(thread_id.to_string(), Arc::clone(&root));
    match build_runtime_tooling(
        &thread_cfg,
        &root,
        thread_id,
        built.yolo.clone(),
        theme.clone(),
        git_diff.clone(),
        extra_roots,
        // 项目键从本次会话的 cwd 求得(与 `ask_lead` 的投递路由同一来源):
        // 会话给组长留言时,消息必须落到「本目录」的组长信箱。
        crate::thread_store::ThreadStore::new(Path::new(cwd)).project_key(),
        lead_inbox.clone(),
        lead_replies.clone(),
        lead_control.clone(),
        lead_control_replies.clone(),
        anchor_sha,
        lead_agent_enabled,
        extra_session_env,
    ) {
        Ok(tooling) => Activation {
            built: wrap_for_delegation(built, tooling),
            runtime: Some(root),
            tooling_error: None,
        },
        Err(cause) => {
            tracing::warn!(
                stage = "tooling",
                %cause,
                cwd,
                "subagent delegation unavailable for this thread"
            );
            Activation {
                built,
                runtime: None,
                tooling_error: Some(format!("runtime tooling could not be built: {cause}")),
            }
        }
    }
}
/// Reject an `agent/*` request for a conversation the server does not host.
///
/// Distinct from an unattached conversation, which exists and answers with an
/// empty result: a thread id that was never started is a caller error.
pub(crate) async fn require_known_thread(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    id: RequestId,
) -> anyhow::Result<bool> {
    if threads.contains_key(thread_id) {
        return Ok(true);
    }
    write_response(
        hub,
        client,
        err_response(id, RpcError::unknown_thread(thread_id)),
    )
    .await?;
    Ok(false)
}
/// The runtime socket a conversation's children live on.
///
/// The socket belongs to the attached project root, which is keyed by the
/// thread's own directory. A thread that never attached has no socket, and every
/// `agent/*` answer for it is empty rather than an error.
pub(crate) fn socket_for_thread(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
) -> Option<PathBuf> {
    let cwd = threads.get(thread_id)?.cwd.clone();
    let key = project_key(Path::new(&cwd));
    let entry = runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)?
        .clone();
    entry
        .ok()
        .and_then(|binding| binding.current().ok())
        .map(|handle| handle.socket_path)
}
/// The watch's opening frame carries the backlog the client would otherwise miss.
pub(crate) fn snapshot_for_watch(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    task_id: &str,
) -> Result<(Vec<crate::protocol::AgentTraceRow>, i64), RpcError> {
    read_task_trace(runtimes, threads, thread_id, task_id)
}
/// 该投递层是否属于「丢了就修不回来」的状态机帧。
///
/// `List`：回合结束与 thread 状态，是前端 `turnActive` / 未读蓝点 / 审批框的
/// 唯一驱动。`Global`：审批已处理、错误、主题等跨会话信号，丢一帧会留下关不掉
/// 的弹窗。两者都只有少量帧，适合承担背压。
pub(crate) fn is_load_bearing(delivery: crate::protocol::Delivery) -> bool {
    matches!(
        delivery,
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global
    )
}
/// 更新共享状态句柄并推送 `thread/status/updated`。
///
/// 加锁是同步的、不跨 `.await`；锁在写通知前即释放。锁中毒时沿用
/// `workspace_index.rs` 的恢复约定：取回内部值而非 panic(状态只是 UI 提示,
/// 不应因一次 panic 永久失效)。
pub(crate) async fn update_status(
    hub: &crate::broadcast::Broadcaster,
    handle: &std::sync::Mutex<ThreadStatus>,
    thread_id: &str,
    next: ThreadStatus,
) -> anyhow::Result<()> {
    *handle.lock().unwrap_or_else(|p| p.into_inner()) = next;
    write_notification(
        hub,
        &Notification::ThreadStatusUpdated {
            thread_id: thread_id.to_string(),
            status: next,
        },
    )
    .await
}
/// 读某 thread 的当前状态;不在内存(cold thread)一律 `Idle`。
pub(crate) fn thread_status(
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
) -> ThreadStatus {
    threads
        .get(thread_id)
        .map(|s| *s.status.lock().unwrap_or_else(|p| p.into_inner()))
        .unwrap_or(ThreadStatus::Idle)
}
/// 跨工作目录收集已置顶的 thread meta，按置顶分区契约排序：
/// `pin_seq` 降序（越大越靠前），`updated_at` 降序，`thread_id` 升序。
pub(crate) fn collect_pinned(workspaces: &WorkspaceIndex) -> Vec<crate::thread_store::ThreadMeta> {
    let mut out = Vec::new();
    for dir in workspaces.list() {
        let path = Path::new(&dir);
        if !path.is_dir() {
            continue;
        }
        match crate::thread_store::ThreadStore::new(path).list() {
            Ok(metas) => out.extend(metas.into_iter().filter(|m| m.pin_seq.is_some())),
            Err(e) => eprintln!("[app-server] collect_pinned failed to list {dir}: {e}"),
        }
    }
    out.sort_by(|a, b| {
        b.pin_seq
            .cmp(&a.pin_seq)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.thread_id.cmp(&b.thread_id))
    });
    out
}
/// 把 worktree 判定渲染成 `thread/delete` 的 `needs_confirmation.worktree` 字段。
///
/// `None` 表示"不适用/无需呈现"（普通会话、或不回收）——此时响应不带该字段，
/// 普通会话的既有响应因此零变化。
pub(crate) fn worktree_json(
    reclaim: &crate::worktree_reclaim::WorktreeReclaim,
) -> Option<serde_json::Value> {
    use crate::worktree_reclaim::WorktreeReclaim;
    match reclaim {
        WorktreeReclaim::None => None,
        WorktreeReclaim::Delete { path } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "remove",
            "reason": "干净且源分支已并入，删除不会丢失工作",
        })),
        WorktreeReclaim::DestructiveDelete { path, reason } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "remove",
            "reason": reason,
        })),
        WorktreeReclaim::Keep { path, reason } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "keep",
            "reason": reason,
        })),
    }
}
/// 客户端在握手第一帧声明的、某会话的缓存状态。
///
/// 生产调用点：`initialize` 分支读 `params.resumeHints` → `parse_resume_hints`
/// → `plan_sync`（Task 6 落地）。
#[derive(Debug, Clone)]
pub(crate) struct SyncHint {
    pub(crate) thread_id: String,
    pub(crate) content_epoch: u64,
    pub(crate) last_item_id: Option<String>,
}
/// 服务端对该 hint 的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncAction {
    /// 缓存内容必须整段作废，客户端须走全量 `thread/resume`。
    Full,
    /// 缓存内容**仍然有效**。注意这不是"客户端已是最新"——落后是正常的并存
    /// 形状，补齐由常规的 resume / readItems / 订阅流承担（spec §4.2）。
    None,
    /// 该会话已不存在，客户端须丢弃本地条目与视图。
    Drop,
}
/// 把 hints 与服务端现状裁成一份 `syncPlan`。纯函数：`server(id)` 由调用方
/// 提供该会话的 `(content_epoch, 锚点是否仍在日志中)`，`None` 表示不存在。
///
/// 判定按优先级短路（spec §4.2）：不存在 → drop；epoch 不符 → full；
/// 锚点缺失 → full；否则 none。返回的末位是服务端 epoch（`drop` 时 0），
/// 免得调用方为写 `syncPlan` 的 `epoch` 字段再查一次库。
pub(crate) fn plan_sync(
    hints: &[SyncHint],
    server: &dyn Fn(&str) -> Option<(u64, bool)>,
) -> Vec<(String, SyncAction, &'static str, u64)> {
    let mut out = Vec::with_capacity(hints.len());
    for h in hints {
        let (action, reason, epoch) = match server(&h.thread_id) {
            None => (SyncAction::Drop, "deleted", 0),
            Some((epoch, anchor_present)) => {
                if epoch != h.content_epoch {
                    (SyncAction::Full, "epoch_changed", epoch)
                } else if h.last_item_id.is_some() && !anchor_present {
                    (SyncAction::Full, "anchor_missing", epoch)
                } else {
                    (SyncAction::None, "match", epoch)
                }
            }
        };
        out.push((h.thread_id.clone(), action, reason, epoch));
    }
    out
}
/// 解析 `initialize` 的 `resumeHints`。上限 20 条（按 `cachedAt` 降序取，缺
/// `cachedAt` 视为 0），超出截断——声明必须是"非常小的数据量"（spec §4.1）。
/// 任何畸形条目静默跳过：声明是优化，不该让握手失败。
pub(crate) fn parse_resume_hints(raw: Option<&serde_json::Value>) -> Vec<SyncHint> {
    const MAX_HINTS: usize = 20;
    let Some(items) = raw.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut hints: Vec<(i64, SyncHint)> = items
        .iter()
        .filter_map(|it| {
            let thread_id = it.get("threadId")?.as_str()?.to_string();
            let content_epoch = it.get("contentEpoch").and_then(|v| v.as_u64()).unwrap_or(0);
            let last_item_id = it
                .get("lastItemId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let cached_at = it.get("cachedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            Some((
                cached_at,
                SyncHint {
                    thread_id,
                    content_epoch,
                    last_item_id,
                },
            ))
        })
        .collect();
    // 降序：cachedAt 越大越新，优先保留。
    hints.sort_by_key(|a| std::cmp::Reverse(a.0));
    hints.truncate(MAX_HINTS);
    hints.into_iter().map(|(_, h)| h).collect()
}
/// 一条落盘记录里的最后一条 item id（`items` 的末项）。
pub(crate) fn record_last_item_id(record: &crate::thread_store::TurnLine) -> Option<String> {
    match record {
        crate::thread_store::TurnLine::Turn { items, .. } => {
            items.last().and_then(item_id).map(str::to_string)
        }
    }
}
/// 把 thread meta 渲染成 wire 上的 ThreadSummary（含 `pinned`）。
pub(crate) fn thread_summary_json(
    m: &crate::thread_store::ThreadMeta,
    threads: &HashMap<String, ThreadSession>,
) -> serde_json::Value {
    thread_summary_json_with_status(m, thread_status(threads, &m.thread_id))
}
/// 与 [`thread_summary_json`] 同形，但状态由调用方给。
///
/// `thread/listAll` 把目录扫描搬出了主循环（否则一次长扫描会把
/// `turn/interrupt` 一起堵住）。扫描任务拿不到 `&mut threads`，所以主循环先
/// 把「每个 thread 此刻的状态」快照下来，任务按快照构造 JSON——除此之外两者
/// 逐字相同。
pub(crate) fn thread_summary_json_with_status(
    m: &crate::thread_store::ThreadMeta,
    status: ThreadStatus,
) -> serde_json::Value {
    let mut value = json!({
        "thread_id": m.thread_id,
        "cwd": m.cwd,
        "model": m.model,
        "model_ref": m.model_ref,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "title": m.title,
        "permission_mode": m.permission_mode,
        "pinned": m.pin_seq.is_some(),
        "driver_project": m.driver_project,
        "driver_ref": m.driver_ref,
        "tag": m.tag,
        "status": status,
        "content_epoch": m.content_epoch,
        "last_item_id": m.last_item_id,
        // 共享已读水位：客户端据此重算未读（`completed > read`）。断线重连后
        // 这是唯一能补回「离线期间跑完的回合」的来源——推送当时没人在听。
        "last_read_at": m.last_read_at,
        "last_completed_at": m.last_completed_at,
    });
    // 只在**合法**组长时发送该 key:普通会话多一个 `"role": null` 会让旧客户端
    // 的字节比对与快照测试失效,而我们只需要一个新值。卡片 worktree 里的
    // `role:"lead"` 是误落盘的假组长(那个目录不是项目),绝不能经这条 wire 暴露,
    // 否则侧栏会为它渲染出一条「组长」行。
    if crate::thread_store::is_lead_meta(m) {
        value["role"] = json!("lead");
    }
    value
}
/// wire 上的决定标签,与 `parse_client_decision` 接受的取值互逆。
///
/// `AlwaysAllowPrefix` 丢掉前缀:本标签只回答"是谁、做了什么类型的决定",
/// 不含策略细节;需要前缀的客户端可保留自己的弹窗上下文。
pub(crate) fn decision_label(decision: &Decision) -> String {
    match decision {
        Decision::AllowOnce => "allow_once",
        Decision::AlwaysAllowTool => "always_allow_tool",
        Decision::AlwaysAllowPrefix(_) => "always_allow_prefix",
        Decision::Deny => "deny",
    }
    .to_string()
}
/// 解析客户端的权限决定;任何未知/畸形取值一律按 Deny 处理(fail-safe)。
pub(crate) fn parse_client_decision(result: Option<&serde_json::Value>) -> Decision {
    let Some(result) = result else {
        return Decision::Deny;
    };
    match result.get("decision").and_then(|d| d.as_str()) {
        Some("allow_once") => Decision::AllowOnce,
        Some("always_allow_tool") => Decision::AlwaysAllowTool,
        Some("always_allow_prefix") => match result.get("prefix").and_then(|p| p.as_str()) {
            Some(p) => Decision::AlwaysAllowPrefix(p.to_string()),
            None => Decision::Deny,
        },
        _ => Decision::Deny,
    }
}
/// `u64` 累计值落到既有 `TurnUsage`（`u32`）时的饱和转换。
///
/// 单个 turn 的累加值几乎不可能超过 `u32::MAX`（4.29e9），但溢出的后果是数字
/// 突然变小，比封顶更难解释，所以封顶而不是截断。
pub fn saturate_u32(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}
/// 在全局索引的目录里定位 `thread_id` 所属目录。
pub(crate) fn find_thread_dir(workspaces: &WorkspaceIndex, thread_id: &str) -> Option<PathBuf> {
    workspaces
        .list()
        .into_iter()
        .map(PathBuf::from)
        .find(|d| crate::thread_store::ThreadStore::new(d).exists(thread_id))
}
/// 按 thread 定位其 store;索引找不到时回退 `cfg.workdir`。
pub(crate) fn store_for(
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    let dir = find_thread_dir(workspaces, thread_id).unwrap_or_else(|| cfg.workdir.clone());
    Arc::new(crate::thread_store::ThreadStore::new(&dir))
}
/// 取 thread 的 store:内存中的 thread 用其权威实例(与 driver 共享 lock);
/// 冷 thread 按全局索引定位,索引找不到再回退 `cfg.workdir`。
pub(crate) fn store_lookup(
    threads: &HashMap<String, ThreadSession>,
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    match threads.get(thread_id) {
        Some(s) => Arc::clone(&s.store),
        None => store_for(workspaces, cfg, thread_id),
    }
}
/// 解析某个 thread 的工作区 cwd(绝对路径)。
///
/// 内存中的会话最权威:直接取 `ThreadSession.cwd`(`thread/start` 已 canonicalize)。
/// 冷 thread(不在内存)回退 `store_lookup`——它按全局索引定位该 thread 的 store,
/// 或落回 `cfg.workdir`;store 的 root 恒为 `<cwd>/.yi-agent/threads`,上溯两级即得
/// 工作区(与 `thread/delete` 分支推导附件目录的写法同源)。
///
/// 找不到任何见证(索引里既没有该 thread 的目录,`cfg.workdir` 也不含它)时返回
/// `None`,由调用方按「未知 thread」拒绝。
pub(crate) fn thread_cwd(
    threads: &HashMap<String, ThreadSession>,
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Option<PathBuf> {
    if let Some(session) = threads.get(thread_id) {
        return Some(PathBuf::from(&session.cwd));
    }
    let store = store_lookup(threads, workspaces, cfg, thread_id);
    if store.exists(thread_id) {
        return store
            .root()
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
    }
    None
}
/// `image/read` 回传的 MIME:由 `guess_format` 猜出的格式决定。
///
/// 只认四种与 `image_prep`(`png/jpeg/gif/webp`)对齐的格式;`guess_format` 虽会
/// 认出 TIFF/BMP 等魔数,但那些解码器未启用,统一按 `application/octet-stream`
/// 报出(客户端据此可判「不是我能渲染的图」)。
pub(crate) fn image_media_type(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => "application/octet-stream",
    }
}
/// 一条 `Item` 的稳定 id（用于 `thread/readItems` 的 `afterItemId` 切片与前端去重）。
pub(crate) fn item_id(item: &crate::protocol::Item) -> Option<&str> {
    match item {
        crate::protocol::Item::UserMessage { id, .. }
        | crate::protocol::Item::AgentMessage { id, .. }
        | crate::protocol::Item::ToolCall { id, .. }
        | crate::protocol::Item::UserInterjection { id, .. }
        | crate::protocol::Item::Thinking { id, .. } => Some(id),
    }
}
/// 解析 `thread/start` 的目标目录:显式 `params.cwd` 优先,缺省用 `cfg.workdir`。
///
/// 显式 `cwd` canonicalize + 校验是目录;失败写 `-32602` 并返回 `Ok(None)`
/// (调用方 continue)。缺省时沿用 `cfg.workdir` 原样,不 canonicalize:保持旧的
/// 单目录行为,避免 macOS `/var` → `/private/var` 之类改写破坏既有路径语义。
pub(crate) async fn resolve_thread_cwd(
    params: &serde_json::Value,
    cfg: &RuntimeConfig,
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("cwd").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => match std::fs::canonicalize(s) {
            Ok(p) if p.is_dir() => Ok(Some(p.to_string_lossy().to_string())),
            _ => {
                write_response(
                    hub,
                    client,
                    err_response(id, RpcError::invalid_params("cwd is not a valid directory")),
                )
                .await?;
                Ok(None)
            }
        },
        _ => Ok(Some(cfg.workdir.display().to_string())),
    }
}
/// 从 params 提取 `threadId`;缺失时写 `-32602` 并返回 `Ok(None)`。
pub(crate) async fn require_thread_id(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("threadId").and_then(|v| v.as_str()) {
        Some(s) => Ok(Some(s.to_string())),
        None => {
            write_response(
                hub,
                client,
                err_response(id, RpcError::invalid_params("missing threadId")),
            )
            .await?;
            Ok(None)
        }
    }
}
