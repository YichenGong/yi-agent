//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::card_scheduler::{CardLauncher, LaunchRequest, ThreadFlags, TrackedThread};
use crate::pairing::PairingState;
use crate::protocol::{
    ClientResponse, JSONRPC_VERSION, Notification, NotificationEnvelope, PROTOCOL_VERSION,
    RequestEnvelope, RequestId, ResponseEnvelope, ReverseRequest, RpcError, Scope, ThreadStatus,
    TurnStatus,
};
use crate::session::{
    CompactOutcome, InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt,
};
use crate::translate::Translator;
use crate::transport::{MessageReader, MessageWriter};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_subagent::binding::RuntimeBinding;
use yi_agent_subagent::thread_root::ThreadRoot;

/// 权限审批等待客户端响应的默认超时;超时按 Deny 处理。
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// 看板调度器在主循环里跑一轮的间隔。
///
/// 采用「serve 主循环内周期性 tick」而非让调度器并发访问 `threads`(见 Task 6c
/// brief 的设计约束):整张 `threads` 表是主循环的局部量,重构成可共享结构会
/// 把改动面扩散到全部 RPC 分支。3s 足够跟上卡片状态流转,又远短于卡片的生命周期。
const CARD_SCHEDULER_TICK: Duration = Duration::from_secs(3);

/// driver task → 主循环的完成事件。
enum TurnEvent {
    Finished { thread_id: String, turn_id: String },
}

/// 工厂产出的 agent 及其重建所需的部件。
///
/// 项目 runtime attach 成功后,该 thread 的 agent 要用新的工具集与新的权限根**重建**
/// (见 `wrap_for_delegation`),所以这里只带出重建必须沿用的那些部件:重建 provider
/// 会重读凭据并多一个 client,重建决定通道会打断正在等待审批的 turn。被替换掉的
/// 工具集与权限检查器留在 `RuntimeTooling` 里,正是因为它俩不该出现在这张单子上。
pub(crate) struct BuiltAgent {
    agent: yi_agent_core::Agent,
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    /// 交互模式下的权限决定回传端;None 表示该 agent 不需要审批。
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    decision_rx: Option<yi_agent_runtime::bootstrap::DecisionReceiver>,
    /// 刷新 skills catalog 的句柄;无 skills 服务时为 `None`。
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    /// 该 agent 的运行时 yolo 开关;`ThreadSession` 存它以便 RPC 即时切换。
    yolo: yi_agent_core::autonomy::YoloSwitch,
    /// 支撑该 agent 工具集里的进程工具的 manager。**与生效的工具集同行**：
    /// `build_runtime_tooling` 换掉工具集时必须一并替换它。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
}

/// 一个 cwd 的工具集与其权限检查器。
struct RuntimeTooling {
    registry: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// 这一份注册表对应的 manager(与 registry 同源)。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
    /// The live caller transcript shared with this registry's delegation tools.
    /// `build_runtime_tooling` hands a clone to the tools and keeps this one, so
    /// binding it here (see [`wrap_for_delegation`]) is visible to those tools.
    caller: yi_agent_subagent::CallerContext,
}

/// 每 cwd 的已 attach runtime。app-server 是长驻多 cwd 进程,而 runtime 是按项目
/// 划分的,因此 attach 以 canonical cwd 为键、懒初始化。
///
/// 失败也缓存(存 `Err(原因)`):否则纯目录下每开一个 thread 都会重跑一遍注定失败的
/// bring-up(git 检查 + daemon 起停尝试)。
type ProjectRuntimes = Arc<StdMutex<HashMap<PathBuf, Result<Arc<RuntimeBinding>, String>>>>;

/// 每会话自己的 root,建在项目共享 binding 之上。
///
/// runtime(daemon)按项目共享,是 G2;root 按会话独立,是这次改动的核心——
/// 一个 root 才是一份 `MAX_DIRECT_CHILDREN` 预算,两个会话共用一个 root 就会共用
/// 一份预算,这正是桌面端「四个就满」的成因。
type ThreadRoots = Arc<StdMutex<HashMap<String, Arc<ThreadRoot>>>>;

/// 本进程的 runtime 接线:daemon 按项目共享,root 按会话持有。
///
/// 两者成对传递,调用点无法只更新一半——只换了 daemon 映射、忘了 root 映射,
/// 正是「每个会话应当有自己的 root」这条不变量最危险的破坏方式。
pub(crate) struct RuntimeAttachments {
    pub(crate) runtimes: ProjectRuntimes,
    pub(crate) thread_roots: ThreadRoots,
    /// Where the board registry lives. Injectable for the same reason
    /// `WorkspaceIndex` is: the `board/*` RPCs must be testable without
    /// touching the developer's real `~/.yi-agent`.
    pub(crate) board_dir: PathBuf,
    /// Where the generic resident-daemon registry lives (`$HOME/.yi-agent`).
    /// Injectable for the same reason `board_dir` is: `board/create` must be
    /// testable without writing the developer's real resident registry.
    pub(crate) resident_dir: PathBuf,
    /// How `board/create` starts a project's daemon. Injectable for the same
    /// reason `board_dir` is: a test must not spawn a real detached process,
    /// and the app's production launcher is what an integration test would
    /// otherwise have to reproduce.
    pub(crate) launcher: BoardLauncher,
    /// 桌面主题句柄：`ui/settings/read|write` 维护它，每个 thread 的
    /// `set_theme` 工具也持同一实例，故任一路径改主题都会落盘 + 广播。
    pub(crate) theme: crate::theme_tool::ThemeHandle,
    /// 安装常驻值守（macOS LaunchAgent）。可注入的理由同 `launcher`：测试里
    /// 绝不能真的调 `launchctl`，也绝不能碰用户真实的 `~/Library/LaunchAgents`。
    pub(crate) watchman_install: WatchmanInstall,
    /// 卸载常驻值守。可注入的理由同 `watchman_install`。
    pub(crate) watchman_uninstall: WatchmanUninstall,
    /// 值守安装/卸载所针对的 home（生产：真实 `$HOME`；测试：tempdir）。
    /// 它是「往哪个 `Library/LaunchAgents` 写」与「读哪个 plist 判已装」的
    /// 唯一来源，抽出来测试才不会误判真实 home 里的安装状态。
    pub(crate) watchman_home: PathBuf,
}

/// Starts a project's board daemon; see [`yi_agent_boards::lifecycle::create_with_project`].
type BoardLauncher = Arc<dyn Fn(&Path) -> Result<bool, String> + Send + Sync>;

/// Installs the resident watchman (macOS LaunchAgent) for `exe` under `home`;
/// see [`yi_agent_boards::watchman::install`].
type WatchmanInstall = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;

/// Removes the resident watchman under `home`; see
/// [`yi_agent_boards::watchman::uninstall`].
type WatchmanUninstall = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// 生产环境的 `$HOME`。未设置时回退空路径——与 `board_dir` 同一策略：宁可把
/// 值守装进一个无人读取的目录，也不要因为环境缺失让整个服务起不来。
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// 生产的安装闭包：设置 `YI_AGENT_DISABLE_WATCHMAN` 时退化成 no-op。
///
/// 这个开关是为那个**驱动真实二进制**的 e2e（`yi-agent/tests/board_e2e.rs`）而设：
/// 它以 `YI_AGENT_BOARD_E2E` 为闸、跑的是生产接线，于是 `board/create` 里的
/// 「首次创建时装值守」会走到这里。不拦的话它会在开发机真实的 launchd 域里执行
/// `launchctl bootstrap gui/<uid>`，并往真实的 `~/Library/LaunchAgents` 写 plist——
/// 测试绝不能碰宿主 launchd。为此 e2e 置该环境变量即可让安装/卸载变成空操作。
///
/// 只影响生产接线：测试用的 harness 各自注入记账闭包（见 `Harness`），不经过这里。
pub(crate) fn production_watchman_install() -> WatchmanInstall {
    if std::env::var_os("YI_AGENT_DISABLE_WATCHMAN").is_some() {
        return Arc::new(|_exe: &Path, _home: &Path| Ok(()));
    }
    Arc::new(yi_agent_boards::watchman::install)
}

/// 生产的卸载闭包；`YI_AGENT_DISABLE_WATCHMAN` 开关同
/// [`production_watchman_install`]。
pub(crate) fn production_watchman_uninstall() -> WatchmanUninstall {
    if std::env::var_os("YI_AGENT_DISABLE_WATCHMAN").is_some() {
        return Arc::new(|_home: &Path| Ok(()));
    }
    Arc::new(yi_agent_boards::watchman::uninstall)
}

/// 开关为开、且现有 plist 未指向当前可执行文件时安装值守。
///
/// best-effort：安装失败只记日志。调用方是 `board/create`，创建一个看板不该
/// 因为装不上开机自启而整体失败。`EXE` 换位置也要重装（升级后常见），故判据是
/// [`yi_agent_boards::watchman::is_installed_for`] 而非「plist 是否存在」。
fn ensure_watchman_installed(workdir: &Path, home: &Path, install: &WatchmanInstall) {
    if !crate::settings_store::load_watchman_enabled(workdir) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if yi_agent_boards::watchman::is_installed_for(home, &exe) {
        return;
    }
    if let Err(error) = install(&exe, home) {
        tracing::warn!(%error, "failed to install the board watchman");
    }
}

/// The key that identifies one project directory across this process.
///
/// Runtime attach and every later lookup must agree on this value, so it is
/// computed in exactly one place. A path that exists canonicalizes whole, which
/// resolves symlinks and, on macOS, `/tmp` -> `/private/tmp`. A path that does
/// not exist yet still has to key the *same* before and after it is created, so
/// the deepest existing ancestor is canonicalized and the remaining components
/// are appended unchanged. Without that second half an absent directory keys as
/// its literal spelling while the same directory keys as its canonical path once
/// it exists -- two keys for one project, which makes delegation silently
/// disappear (attach stores one key, the lookup misses it) whenever the two
/// spellings differ.
fn project_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        if let Some(name) = cursor.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(canonical) = std::fs::canonicalize(parent) {
            let mut key = canonical;
            for part in suffix.iter().rev() {
                key.push(part);
            }
            return key;
        }
        cursor = parent;
    }
}

/// 一次 attach 的结果:可能被换过工具集的 agent,以及该 thread 首个 turn 要激活的 runtime。
struct Activation {
    built: BuiltAgent,
    runtime: Option<Arc<ThreadRoot>>,
}

/// 为该 cwd 的 agent 接上委派能力。失败即降级:保留原 agent,只记 trace。
///
/// 调用方必须已拿到 `build_agent` 的产物,且**不得**在 `threads` 的可变借用内调用:
/// 本函数只经 `runtimes` 的 `Mutex` 访问,不触碰 `threads`,所以先调用、再
/// `threads.insert(..)` 是安全的。
#[allow(clippy::too_many_arguments)]
fn attach_delegation(
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
    theme: &crate::theme_tool::ThemeHandle,
    built: BuiltAgent,
) -> Activation {
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = PathBuf::from(cwd);
    let binding = match attach_cwd_runtime(runtimes, runtime_dir, &thread_cfg) {
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
            };
        }
    };
    // The daemon is shared per project, but the root is per conversation: the
    // root is what carries the child budget, so two conversations in one
    // directory must never share it.
    let root = ThreadRoot::new(Arc::clone(&binding), thread_id, PathBuf::from(cwd));
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
    ) {
        Ok(tooling) => Activation {
            built: wrap_for_delegation(built, tooling),
            runtime: Some(root),
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
            }
        }
    }
}

/// Reject an `agent/*` request for a conversation the server does not host.
///
/// Distinct from an unattached conversation, which exists and answers with an
/// empty result: a thread id that was never started is a caller error.
async fn require_known_thread(
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
fn socket_for_thread(
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

/// What a conversation-scoped cancellation did.
enum ThreadCancelOutcome {
    /// Nothing left to cancel, or the caller forced and everything was reaped.
    Cancelled(usize),
    /// Live children exist and the caller did not force: nothing was touched.
    NeedsConfirmation(usize),
}

/// Cancels every live child a conversation owns, before its files go away.
///
/// The children live in the shared project runtime, where the conversation's
/// files are not the authority, so deleting them alone would leave the agents
/// running where nobody can see them. The cancellation is scoped by the
/// conversation marker rather than the root: several conversations share one
/// attached root, and a root-scoped cancel would stop a sibling's work too.
///
/// Unless `force` is set, live children stop the deletion so the caller can ask
/// the user first: losing running work to one click on a sidebar × is not a
/// decision the backend should make silently.
fn cancel_thread_children(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    force: bool,
) -> Result<ThreadCancelOutcome, String> {
    let cwd = threads
        .get(thread_id)
        .map(|session| session.cwd.clone())
        .ok_or_else(|| "the thread is not held in memory".to_string())?;
    let key = project_key(Path::new(&cwd));
    let binding = {
        let guard = runtimes.lock().unwrap_or_else(|p| p.into_inner());
        guard.get(&key).cloned()
    }
    .ok_or_else(|| "no attached runtime for this project".to_string())??;
    let response = binding.send(
        |handle| yi_agent_store::ipc::IpcRequest::CancelThreadTasks {
            session_id: handle.session_id.clone(),
            thread_id: thread_id.to_owned(),
            force,
        },
    )?;
    match response {
        yi_agent_store::ipc::IpcResponse::ThreadTasksCancelled { task_ids } => {
            Ok(ThreadCancelOutcome::Cancelled(task_ids.len()))
        }
        yi_agent_store::ipc::IpcResponse::ThreadHasActiveChildren { count } => {
            Ok(ThreadCancelOutcome::NeedsConfirmation(count))
        }
        other => Err(format!(
            "daemon returned a non-cancellation response: {other:?}"
        )),
    }
}

/// The task summaries the daemon holds for a conversation's directory.
fn list_task_summaries(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
) -> Result<Vec<yi_agent_store::ipc::IpcTaskSummary>, String> {
    let socket = socket_for_thread(runtimes, threads, thread_id)
        .ok_or_else(|| "no attached runtime for this thread".to_string())?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } => Ok(tasks),
        other => Err(format!("daemon returned a non-summary response: {other:?}")),
    }
}

/// Read a task's persisted trace, plus the high-water mark to resume from.
fn read_task_trace(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    task_id: &str,
) -> Result<(Vec<crate::protocol::AgentTraceRow>, i64), RpcError> {
    let socket = socket_for_thread(runtimes, threads, thread_id)
        .ok_or_else(|| RpcError::unknown_thread(thread_id))?;
    let snapshot = yi_agent_store::ipc::read_task_trace(&socket, task_id)
        .map_err(|error| RpcError::internal(error.to_string()))?;
    Ok((trace_rows(snapshot.rows), snapshot.high_water_id))
}

/// The watch's opening frame carries the backlog the client would otherwise miss.
fn snapshot_for_watch(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    task_id: &str,
) -> Result<(Vec<crate::protocol::AgentTraceRow>, i64), RpcError> {
    read_task_trace(runtimes, threads, thread_id, task_id)
}

fn trace_rows(rows: Vec<yi_agent_store::ipc::IpcTraceRow>) -> Vec<crate::protocol::AgentTraceRow> {
    rows.into_iter().map(agent_trace_row).collect()
}

fn agent_trace_row(row: yi_agent_store::ipc::IpcTraceRow) -> crate::protocol::AgentTraceRow {
    crate::protocol::AgentTraceRow {
        event_id: row.event_id,
        task_id: row.task_id,
        kind: row.kind,
        payload_json: row.payload_json,
    }
}

/// Send one request to the daemon the conversation is attached to.
///
/// A request the daemon rejects is an error the caller must see, so the daemon's
/// own error frame is turned into the RPC error rather than swallowed. A
/// conversation with no attached runtime is an unknown-thread error, since there
/// is nothing to send to.
fn forward_to_daemon(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    request: yi_agent_store::ipc::IpcRequest,
) -> Result<yi_agent_store::ipc::IpcResponse, RpcError> {
    let socket = socket_for_thread(runtimes, threads, thread_id)
        .ok_or_else(|| RpcError::unknown_thread(thread_id))?;
    let response = yi_agent_store::ipc::send_request(&socket, request)
        .map_err(|error| RpcError::internal(format!("daemon is unavailable: {error}")))?;
    if let yi_agent_store::ipc::IpcResponse::Error { code, message } = &response {
        return Err(RpcError::internal(format!(
            "daemon rejected the request: {code:?} {}",
            message.clone().unwrap_or_default()
        )));
    }
    Ok(response)
}

/// Push the conversation's child list whenever it changes.
///
/// The first frame is skipped: the caller has just answered the same question
/// synchronously, so pushing it again would be a duplicate. Every later
/// difference is pushed whole, because the list is small and a client that
/// applies it wholesale cannot drift out of order.
async fn watch_children(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    socket: PathBuf,
    initial: Vec<crate::protocol::AgentChild>,
) {
    let mut last = initial;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tasks = match yi_agent_store::ipc::send_request(
            &socket,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        ) {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks }) => tasks,
            // A daemon that is briefly unavailable is not a change; the next
            // poll will pick the list back up.
            _ => continue,
        };
        let current = children_for_thread(&tasks, &thread_id);
        if current == last {
            continue;
        }
        last = current.clone();
        let notification = Notification::AgentChildrenUpdated {
            thread_id: thread_id.clone(),
            children: current,
        };
        if write_notification(&hub, &notification).await.is_err() {
            return;
        }
    }
}

/// 把 manager 的状态广播转成 `process/updated` 通知。
///
/// 只在状态变化时推送(`Output` 丢弃):高频输出走 `process/read` 按需拉取。
/// 关闭 thread 时由调用方 abort。
async fn watch_processes(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    manager: Arc<yi_agent_tools::ProcessManager>,
    mut rx: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
) {
    use yi_agent_tools::ProcessEvent;
    loop {
        match rx.recv().await {
            Ok(event) => {
                let (process_id, state) = match event {
                    ProcessEvent::Started { process_id } => (process_id, "starting"),
                    ProcessEvent::Ready { process_id } => (process_id, "ready"),
                    ProcessEvent::Exited { process_id, .. } => (process_id, "exited"),
                    ProcessEvent::Killed { process_id } => (process_id, "killed"),
                    // 输出是高频流:不转发,详情页自己按需增量拉。
                    ProcessEvent::Output { .. } => continue,
                };
                let _ = manager.list(); // 保证条目仍在(仅作存在性自检,结果丢弃)
                let _ = write_notification(
                    &hub,
                    &Notification::ProcessUpdated {
                        thread_id: thread_id.clone(),
                        process_id,
                        state: state.to_string(),
                    },
                )
                .await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Forward a watched task's rows until the stream ends or the task is aborted.
///
/// The subscription is non-blocking so the task yields between polls; it ends on
/// its own when the daemon closes the stream, and is aborted when the client
/// unwatches, watches another task, or closes the thread.
async fn stream_trace(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    task_id: String,
    socket: PathBuf,
    after_id: i64,
) {
    let subscription = match yi_agent_store::ipc::subscribe_trace(
        &socket,
        after_id,
        std::slice::from_ref(&task_id),
        &[],
    ) {
        Ok(subscription) => subscription,
        Err(error) => {
            tracing::warn!(%error, %thread_id, %task_id, "trace watch could not subscribe");
            return;
        }
    };
    if let Err(error) = subscription.set_nonblocking(true) {
        tracing::warn!(%error, %thread_id, %task_id, "trace watch could not poll");
        return;
    }
    let mut subscription = subscription;
    loop {
        match subscription.try_row() {
            Ok(Some(row)) => {
                let notification = Notification::AgentTraceEvent {
                    thread_id: thread_id.clone(),
                    task_id: task_id.clone(),
                    row: agent_trace_row(row),
                };
                if write_notification(&hub, &notification).await.is_err() {
                    return;
                }
            }
            // Nothing yet, or the stream ended: either way, yield and retry.
            // The stream ending is indistinguishable from a quiet moment by
            // design, so the loop is bounded by the abort that the caller does.
            Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => {
                tracing::warn!(%error, %thread_id, %task_id, "trace watch stream failed");
                return;
            }
        }
    }
}

/// The children of one conversation, derived from a task-summary snapshot.
///
/// A conversation is scoped by the `thread_id` the spawner bound into its tools,
/// not by the directory: several conversations can share one directory and one
/// attached root, so the marker is the only thing that separates their children.
/// Root tasks are never children, and the `thread_id` filter is exact, so an
/// untagged task belongs to no conversation rather than to all of them.
fn children_for_thread(
    tasks: &[yi_agent_store::ipc::IpcTaskSummary],
    thread_id: &str,
) -> Vec<crate::protocol::AgentChild> {
    tasks
        .iter()
        .filter(|task| !task.is_root)
        .filter(|task| task.thread_id.as_deref() == Some(thread_id))
        .map(|task| crate::protocol::AgentChild {
            task_id: task.task_id.clone(),
            // `ListTaskSummaries` carries no objective; a later source can fill
            // it without changing this shape.
            objective: None,
            state: task.state.clone(),
            last_step: None,
            parent_task_id: task.parent_task_id.clone(),
        })
        .collect()
}

/// Whether a task state can still make progress.
#[cfg(test)]
fn is_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "completed" | "completed_no_changes" | "failed" | "cancelled"
    )
}

/// One conversation's child-list watcher.
///
/// The plan called for a kind-filtered `SubscribeTrace`; polling the summary
/// list is used instead because it is strictly cheaper here (summaries are tiny
/// and carry the objective/state the rail needs, which trace rows do not) and it
/// reuses the same read the initial `agent/children/list` does, so the pushed
/// list can never disagree with a fresh request. The observable contract is
/// unchanged: whole-list `agent/children/updated` notifications on change.
struct ChildrenWatch {
    task: tokio::task::JoinHandle<()>,
}

impl ChildrenWatch {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// 每个 thread 一个进程状态守望者:订阅该 thread 生效 manager 的广播,把
/// 低频状态事件转成 `process/updated` 通知。`Output` 事件在此丢弃。
struct ProcessWatch {
    task: tokio::task::JoinHandle<()>,
}

impl ProcessWatch {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// One conversation's live trace watch.
///
/// At most one task per conversation is watched, because the client shows one
/// detail at a time; watching another replaces the watch rather than adding a
/// second stream, so rows never arrive for a task the user has left.
struct TraceWatch {
    task: tokio::task::JoinHandle<()>,
}

impl TraceWatch {
    /// Stop forwarding and wait for the task to observe the cancellation.
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// 取出(或建立)该 cwd 的项目 runtime。
fn attach_cwd_runtime(
    runtimes: &ProjectRuntimes,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
) -> Result<Arc<RuntimeBinding>, String> {
    let key = project_key(&cfg.workdir);
    if let Some(existing) = runtimes.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return existing.clone();
    }
    // Own a *usable* runtime: adopt a healthy daemon, replace a dead or wedged
    // one. The binding it returns heals itself on later calls, so a daemon that
    // dies after this point (e.g. its terminal closes) no longer strands the
    // thread's delegation tools.
    let outcome = yi_agent_subagent::attach::ensure_owned_runtime(cfg, runtime_dir.to_path_buf())
        .map(|attached| RuntimeBinding::managed(cfg, runtime_dir.to_path_buf(), Arc::new(attached)))
        .map_err(|failure| format!("{}: {}", failure.stage, failure.cause));
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, outcome.clone());
    outcome
}

/// 断开已 attach、但已无 thread 使用的项目 root。
///
/// 判据是「该项目目录下还有没有活着的 thread」,而不是「被删的 thread 属于哪个项目」:
/// `thread/delete` 靠 `store_lookup` 按 thread_id 定位,分支里拿不到 cwd。这样删掉某个
/// 项目里的**一个** thread 不会连带废掉同项目其它 thread 的委派(daemon 对重复 detach
/// 幂等,但被断开后那个 driver 不会再激活第二次,故不能多断)。
fn detach_unused_runtimes(runtimes: &ProjectRuntimes, live_cwds: &[String]) {
    for binding in attached_runtimes(runtimes) {
        if live_cwds
            .iter()
            .any(|cwd| project_key(Path::new(cwd)) == project_key(&binding.project_root()))
        {
            continue;
        }
        binding.detach();
    }
}

/// 本进程已 attach 的 runtime(只看成功项)。
fn attached_runtimes(runtimes: &ProjectRuntimes) -> Vec<Arc<RuntimeBinding>> {
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter_map(|entry| entry.clone().ok())
        .collect()
}

/// 委派就绪的工具集。
///
/// 只有内置工具与权限根搬到 checkout;skills 与 MCP 的项目根仍取 `cfg.workdir`
/// (用户选的项目)。`yolo` 必须是该 thread 自己的开关,否则运行期切 YOLO 时沙箱层
/// 与权限层会脱钩。
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    root: &Arc<ThreadRoot>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    theme: crate::theme_tool::ThemeHandle,
) -> Result<RuntimeTooling, String> {
    // The tools get the root handle itself, not its snapshot: they re-resolve
    // the live socket and this conversation's own root on every call.
    let workspace_root = root.handle()?.workspace_root;
    // One controller, one truth: it backs the root's builtin tools AND the
    // subagent spawn tools, and it reads the thread's live YOLO switch.
    let controller =
        yi_agent_tools::SandboxController::new(yolo.clone(), cfg.sandbox, cfg.sandbox_promotable);
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &workspace_root,
        controller.clone(),
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    let process_manager = Arc::clone(&setup.process_manager);
    // One caller handle, shared with the delegation tools: the host binds it to
    // the thread's live session right after building the Agent, and the tools
    // re-read that session on every call.
    let caller = yi_agent_subagent::CallerContext::unbound();
    // The conversation marker is bound here, not inferred later: the model that
    // calls `spawn_agent` never sees a thread id, so each thread's tools carry
    // their own so every child they spawn is tagged with this conversation.
    yi_agent_subagent::register_attached_root_tools_in_thread(
        &mut registry,
        Arc::clone(root),
        controller,
        Some(thread_id.to_string()),
        caller.clone(),
    );
    let permission =
        yi_agent_runtime::bootstrap::load_permission_checker_with_switch(&workspace_root, yolo)
            .map_err(|error| error.to_string())?;
    register_theme_tool(&mut registry, theme);
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
        process_manager,
        caller,
    })
}

/// 把主题工具注册进一个 thread 的工具集。
///
/// 委派可用时走 [`build_runtime_tooling`] 的 registry,不可用时走工厂里用
/// bootstrap `tools` 克隆出来的 registry——两条路径都必须注册,否则非 git
/// 目录下自然语言切主题会静默失效。
fn register_theme_tool(
    registry: &mut yi_agent_core::ToolRegistry,
    theme: crate::theme_tool::ThemeHandle,
) {
    registry.register(Arc::new(crate::theme_tool::SetThemeTool::new(theme)));
}

/// 克隆一份 bootstrap 工具集并补上主题工具。
///
/// 非委派路径(`Agent::new` + bootstrap 克隆)与委派路径共用同一段注册逻辑,
/// 避免两条路径漂移出「一边有 tool、一边没有」的缺口。
fn registry_with_theme_tool(
    tools: &Arc<yi_agent_core::ToolRegistry>,
    theme: crate::theme_tool::ThemeHandle,
) -> yi_agent_core::ToolRegistry {
    let mut registry = (**tools).clone();
    register_theme_tool(&mut registry, theme);
    registry
}

/// 用带主题工具的工具集重建 thread agent,并保留原 agent 的审批路径。
///
/// `Agent::new` 从零开始、不含权限检查器与决定接收端,所以重建后必须用
/// [`Agent::permission_checker`] / [`Agent::decision_rx`] 取回并重新装上——
/// 否则非 git 目录下的 thread 会退化成不弹审批。两个取回器都为 `None`
/// (AutoAllow)时保持不装,与原 agent 一致。
///
/// provider 沿用 bootstrap 的同一个实例:再建一个会重读凭据、多一个 client。
fn rebuild_thread_agent_with_theme(
    built: yi_agent_runtime::bootstrap::AgentBootstrap,
    session: Option<yi_agent_core::Session>,
    theme: crate::theme_tool::ThemeHandle,
) -> BuiltAgent {
    let yi_agent_runtime::bootstrap::AgentBootstrap {
        agent,
        provider,
        tools,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        process_manager,
        ..
    } = built;
    let config = agent.config().clone();
    let permission = agent.permission_checker();
    let decision = agent.decision_rx();
    // Reuse the original session `Arc`, not a deep clone: a `CallerContext`
    // bound to this thread's live conversation must keep seeing the same
    // session across the rebuild.
    let session_handle = agent.session_handle();
    let registry = registry_with_theme_tool(&tools, theme);
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
            .with_session_arc(session_handle);
    if let (Some(checker), Some(rx)) = (permission, decision) {
        rebuilt = rebuilt.with_permission(checker, rx);
    }
    apply_session(&mut rebuilt, session);
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        process_manager,
    }
}

/// 把 thread 的工具集与权限根换成 attached runtime 的,其余部件沿用。
fn wrap_for_delegation(built: BuiltAgent, tooling: RuntimeTooling) -> BuiltAgent {
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        ..
    } = built;
    // Reuse the live session `Arc` so the delegation tools' `CallerContext`,
    // bound just below, survives the rebuild and keeps reading the thread's
    // current transcript rather than a snapshot taken here.
    let session_handle = agent.session_handle();
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
            .with_session_arc(session_handle.clone());
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
    // Bind the caller to the rebuilt agent's *live* session handle. `tooling`'s
    // `caller` and the registry's tool instances share one inner slot, so this
    // bind is what makes `fork: true` read this conversation.
    tooling.caller.bind(session_handle);
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        // 注册表换了,manager 必须跟着换:否则面板查的是被丢弃的那一份。
        process_manager: tooling.process_manager,
    }
}

/// Why a board query produced no answer.
///
/// `code` is the stable string the UI branches on: "there is no board here" is
/// a different remedy from "the daemon is down" and from "the plugin is not
/// installed", and a bare message collapsed all three into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardQueryError {
    code: &'static str,
    message: String,
}

impl BoardQueryError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BoardQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Ask a supervised plugin a question and return its answer verbatim.
///
/// The host attaches no meaning to `method` or `params`, and unwraps no board
/// shape here: this is a generic channel, so the UI (not the server) owns what a
/// card or a switch field means.
///
/// The question goes to **`project`'s** daemon, resolved from `project`'s own
/// runtime socket. Using the app's cwd instead made every query for another
/// project hit the wrong socket (or none) — the "clicked and nothing happened"
/// bug. `global` is the board registry that says whether `project` has a board
/// at all; a project without one has no daemon worth dialing.
fn plugin_query(
    project: &Path,
    global: &Path,
    method: &str,
    plugin: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BoardQueryError> {
    if plugin.is_empty() {
        return Err(BoardQueryError::new(
            "invalid_params",
            "plugin/query needs a `plugin` name",
        ));
    }
    match yi_agent_boards::registry::contains(global, project) {
        Ok(true) => {}
        // An unreadable registry reads as "no board": `contains` already
        // degrades a corrupt file to empty, so this arm is the same absence.
        Ok(false) | Err(_) => {
            return Err(BoardQueryError::new(
                "board_not_created",
                "this project has no board",
            ));
        }
    }
    // The daemon that runs the plugin lives at the project root, which is also
    // what `yi_agent_boards::lifecycle` and `board_daemon` dial.
    // 复用共享 helper 而不是手写 `.yi-agent/runtime`：它先认 `YI_AGENT_RUNTIME_DIR`，
    // 与工作区其它地方对「runtime socket 在哪」保持同一个定义。
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(project);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        BoardQueryError::new(
            "daemon_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| {
        BoardQueryError::new(
            "daemon_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        // A daemon that is up but has no answer for this plugin is the plugin's
        // absence, not the daemon's: the wording keeps the shape the desktop's
        // `pluginIsUnavailable` matches on.
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!(
                "the plugin rejected the query: {code:?} {}",
                message.unwrap_or_default()
            ),
        )),
        other => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon returned an unexpected response: {other:?}"),
        )),
    }
}

/// The supervised plugin that owns the board.
const KANBAN_PLUGIN: &str = "superpowers-kanban";

/// Ask the kanban plugin a question.
///
/// `board_query` is the board-shaped name for `plugin_query`: it pins the
/// plugin, so a caller cannot accidentally ask a different plugin a board
/// question. `method` and `params` still pass through verbatim.
pub(crate) fn board_query(
    project: &Path,
    board_dir: &Path,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BoardQueryError> {
    plugin_query(project, board_dir, method, KANBAN_PLUGIN, params)
}

/// One card, in the shape the desktop needs to draw and act on it.
///
/// `thread_id` and `workdir` are optional because a card on a board the
/// runner has not launched yet has neither; `spec_path` is a plain string
/// (not a `PathBuf`) because it is copied to the desktop as-is.
#[derive(Debug, Clone)]
pub(crate) struct BoardCard {
    pub id: String,
    pub state: String,
    /// 卡片种类；旧/缺省卡片视为实现卡。
    pub kind: String,
    /// 合并卡的源分支。
    pub source: Option<String>,
    /// 合并卡的目标分支。
    pub base: Option<String>,
    pub thread_id: Option<String>,
    pub workdir: Option<PathBuf>,
    pub spec_path: String,
    /// 计划文件路径。看板 objective 要把它拼进首轮文案;老卡片可能没有,
    /// 故缺省为空串(与 `spec_path` 同款「复制到桌面/文案即用」的语义)。
    pub plan_path: String,
}

/// Parse a `list` payload into the cards the host works with.
///
/// Split out of `board_cards` so the mapping (including the `kind` default for
/// old cards that carry no `kind`) is testable without a live daemon.
pub(crate) fn parse_board_cards(value: &serde_json::Value) -> Vec<BoardCard> {
    let cards = value
        .get("cards")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    cards
        .into_iter()
        .filter_map(|card| {
            Some(BoardCard {
                id: card.get("id")?.as_str()?.to_string(),
                state: card.get("state")?.as_str()?.to_string(),
                kind: card
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("implementation")
                    .to_string(),
                source: card
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                base: card
                    .get("base")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                thread_id: card
                    .get("thread_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                workdir: card
                    .get("workdir")
                    .and_then(serde_json::Value::as_str)
                    .map(PathBuf::from),
                spec_path: card
                    .get("spec_path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                plan_path: card
                    .get("plan_path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

/// Every card on the board, dropped to the fields the desktop works with.
///
/// A malformed entry (one with no `id` or `state`) is skipped rather than
/// failing the whole call: a single bad card must not blank the board. A
/// missing `cards` key reads as an empty board.
pub(crate) fn board_cards(
    project: &Path,
    board_dir: &Path,
) -> Result<Vec<BoardCard>, BoardQueryError> {
    let value = board_query(project, board_dir, "list", json!({}))?;
    Ok(parse_board_cards(&value))
}

/// The project a `board/*` request names, canonicalized when the directory
/// exists.
///
/// The registry compares paths literally, so one project spelled two ways
/// (a symlink, a trailing slash, a relative path) would otherwise register
/// twice and draw two sidebar entries for one board. Returns `None` for a
/// missing or empty `project`, which every `board/*` handler answers with
/// `invalid_params`.
fn project_arg(params: &serde_json::Value) -> Option<PathBuf> {
    let raw = params.get("project").and_then(|value| value.as_str())?;
    if raw.is_empty() {
        return None;
    }
    Some(std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw)))
}

/// Serialize a board value for the wire. A value that cannot serialize is the
/// null case rather than a hang: these shapes are plain data, so this never
/// fires in practice, but an RPC must always write *some* response.
fn to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// 后台自愈循环:app 打开期间每 ~30s 一次,与常驻 watchman 重叠也无害(幂等)。
///
/// 单轮逻辑复用 `yi_agent_boards::watch::once`(读通用登记 → 交给 `ensure`),
/// **不再另写一份**「读登记 → 拉起」的胶水;`ensure_daemons` 同样是 CLI 与
/// watchman 共用的那一份。`resident_dir` 由调用方传入,循环内不重读 `HOME`。
fn spawn_board_watchman_loop(resident_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            yi_agent_boards::watch::once(&resident_dir, &mut |projects| {
                yi_agent_boards::watch::ensure_daemons(projects);
            });
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

/// app-server 入口:在 stdio(或任意读写流)上跑 JSON-RPC 主循环。
///
/// `cfg` 同时用于 `config/read` 响应与(每个 thread 的)`bootstrap_agent`。
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    // 配对状态(配对码 + 设备表)与 workspace 索引同位构造:主循环持有同一个
    // `Arc`,故 `pair/create` 铸出的码与 `device/list` / `device/revoke` 读写的
    // 是**同一张**设备表。`run` 的公开签名不因此改变。
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        crate::device_store::default_path(),
    )));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    // A fallback, not a policy: resolution failing (unset HOME) would otherwise
    // make the whole server fail to start. Board RPCs called in that state
    // register into a directory nobody lists from, which is the same as having
    // no board — better than losing every unrelated RPC.
    let board_dir = yi_agent_boards::global_dir().unwrap_or_default();
    // Same fallback policy as `board_dir`: an unset HOME must not stop the
    // server. Resident RPCs then register where nobody lists from, which is the
    // same as having no resident need.
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    // 会话期自愈:app 打开期间后台每 ~30s 重新确保每个登记项目都有活着的 daemon,
    // 与 launchd watchman 互补。只在生产入口启动;`serve_stdio`/`serve_scoped` 不装,
    // 故单元测试永远不会拉起真实进程。传入已算好的 `resident_dir`,循环内不重读 HOME。
    spawn_board_watchman_loop(resident_dir.clone());
    // 主题句柄:`ui/settings/read|write` 与每个 thread 的 `set_theme` 工具共用。
    // 工厂闭包 `'static`,拿不到主循环里的 `theme`;先克隆一份专供工厂。
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    serve_stdio(
        reader,
        writer,
        cfg.clone(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes,
            thread_roots,
            board_dir,
            resident_dir,
            launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            theme,
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
        },
        production_factory(cfg, theme_for_factory),
    )
    .await
}

/// 合并入口:`app-server --listen stdio:// --relay <url>` 用。
///
/// 与 [`run`] 同构(自建 workspaces/pairing/attachments/factory —— `RuntimeAttachments`
/// 与 `production_factory` 是 crate 私有的,故本函数**自包含**,供二进制 crate
/// 直接调用),差别只在多接一条环回 ws 前端 + 中继客户端:
///
/// - stdio 客户端仍是 `ClientId = local`、`Scope::Admin`(桌面 GUI,全权);
/// - 经环回 ws 接入的手机是 `ws-<uuid>`、`Control`(与直连 ws 时行为一致);
/// - 二者**共用同一个** `serve()` 与同一套 hub / `threads` / `client_scopes`,故
///   任一侧的 turn 事件、审批请求、状态通知都扇出到双方(spec §2.1)。
///
/// 环回 listener 绑 `127.0.0.1:0`,中继客户端是**出站**连接,故网络路径上没有
/// 新增暴露面;环回 ws 仍需 token(本函数用共享 `pairing` 取一枚
/// `seed_local_device("relay-bridge")` 凭据交中继客户端;该取法按名字**幂等**,
/// 重启不会重复落表)。stdio EOF(桌面退出)⇒
/// `serve()` 返回 ⇒ 整个合并会话优雅退出(spec §3.2 不变量 4)。
pub async fn serve_stdio_with_relay<R, W>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    relay_url: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        crate::device_store::default_path(),
    )));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    let board_dir = yi_agent_boards::global_dir().unwrap_or_default();
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    // 与 `run` 一致:本函数是合一模式的生产入口,故同样在会话期自愈地确保
    // 各登记项目有一枚活着的 daemon(桌面经 `--relay` 启动时不应丢掉该保障)。
    spawn_board_watchman_loop(resident_dir.clone());
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    serve_scoped_core(
        reader,
        writer,
        cfg.clone(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes,
            thread_roots,
            board_dir,
            resident_dir,
            launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            theme,
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
        },
        production_factory(cfg, theme_for_factory),
        // 桌面 = Admin(与 `run`/`serve_stdio` 的 stdio 注册完全一致)。
        Scope::Admin,
        Some(listener),
        Some(relay_url),
    )
    .await
}

/// 生产环境的 agent 工厂:按 thread 的 cwd 覆盖 workdir 与 yolo 后引导一个 agent。
pub(crate) fn production_factory(
    cfg: RuntimeConfig,
    theme: crate::theme_tool::ThemeHandle,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &thread_cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )?;
        // 主题工具必须进每个 thread 的工具集;委派随后可能用
        // `wrap_for_delegation` 换掉 registry,那条路径([`build_runtime_tooling`])
        // 同样注册。`Agent::new` 会顺带清掉权限检查器,故重建后必须重新装上
        // (见 [`rebuild_thread_agent_with_theme`],否则非 git 目录会退化成不弹审批)。
        Ok(rebuild_thread_agent_with_theme(
            built,
            session,
            theme.clone(),
        ))
    }
}

/// stdio 传输的接线:注册唯一 `local` 客户端,起读写泵,再进传输无关的 `serve`。
///
/// 主循环不再持有 reader/writer;`read_lines` 与 `pump_stdout` 各自独占一条流,
/// 只与主循环交换 channel 消息(均 cancel-safe)。退出前摘除 `local` 客户端并
/// 等待出口泵排空,保证最后一个响应总能写出。
///
/// stdio 就是桌面:注册为 `local` + `Scope::Admin`,保留全部能力。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_stdio<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    serve_scoped(
        reader,
        writer,
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        hub,
        attachments,
        build_agent,
        // 桌面 stdio 是 Admin:所有既有 RPC 行为不变。
        Scope::Admin,
    )
    .await
}

/// 把中继端点拆成「去掉 `session` 的 URL」+「session id」。
///
/// 与二进制 crate 的 `relay_parts` 同义(此处复制一份,app-server 不得反向依赖
/// 二进制):`yi_agent_relay::run_client` 会自己补 `?session=<id>`,故 base 里必须
/// 先摘掉,否则 URL 上会出现两个 `session`。只做本场景够用的查询串切分,不引
/// `url` crate 的解析——输入来自 CLI,格式由调用方保证。
fn parse_relay_url(relay_url: &str) -> anyhow::Result<(String, String)> {
    let (base, query) = match relay_url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (relay_url, None),
    };
    let session = query
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "session")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "relay endpoint `{relay_url}` is missing `?session=<id>`: \
                 the phone and the computer must agree on the same session id"
            )
        })?;

    let kept: Vec<&str> = query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("session="))
        .collect();
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    Ok((url, session))
}

/// 与 [`serve_stdio`] 逐字节相同,只多一个 `client_scope`。
///
/// 存在的唯一理由是让"低权客户端的门禁"可测:测试经 `Harness::with_scope`
/// 以 `Control` 注册那个 `local` 客户端。生产路径上 `serve_stdio` 传
/// `Scope::Admin`,`run` 与 `serve_stdio` 的公开签名都不因此改变。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_scoped<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scope: Scope,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    // `None` = 纯 stdio:不挂 ws 前端、不起中继,行为与改造前**逐字节一致**。
    serve_scoped_core(
        reader,
        writer,
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        hub,
        attachments,
        build_agent,
        client_scope,
        None,
        None,
    )
    .await
}

/// [`serve_scoped`] 的合并模式入口:额外把一条环回 ws 前端(以及可选的中继
/// 客户端)接到**同一个** `serve()` 上。
///
/// `client_scope` 恒为 stdio 侧的 `Admin`;ws 侧每连接注册为 `Control`。单测直接
/// 传一个已绑定的 `listener` 并令 `relay_url = None`——不引入对真中继的依赖,手机
/// 用共享 `pairing` 铸出的 token 直接连环回监听。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn serve_scoped_with_loopback<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scope: Scope,
    listener: Option<tokio::net::TcpListener>,
    relay_url: Option<String>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    serve_scoped_core(
        reader,
        writer,
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        hub,
        attachments,
        build_agent,
        client_scope,
        listener,
        relay_url,
    )
    .await
}

/// stdio(+ 可选环回 ws / 中继)的共享实现。
///
/// `listener` 为 `None` 时就是纯 stdio([`serve_scoped`] / [`serve_stdio`]),此时
/// 不建任何 ws/中继接线,行为与改造前逐字节一致;为 `Some` 时,stdio 的 `local`
/// 与每条环回 ws 连接**共用同一个** `serve()`、同一套 hub/scopes/threads。
///
/// 本节点的唯一不变量:**恰好一个 `serve(...)` 调用**。两条前端都只是往同一个
/// `inbound_tx` 灌帧、把出站登记进同一个 hub。
#[allow(clippy::too_many_arguments)]
async fn serve_scoped_core<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scope: Scope,
    listener: Option<tokio::net::TcpListener>,
    relay_url: Option<String>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    let local = crate::broadcast::ClientId::local();
    // 可靠登记:stdio 只有这一条出站流,广播的背压**不得**摘除它。桌面 host
    // 一旦来不及读 stdout,旧的 lossy `broadcast` 会把它当慢消费者摘掉,主循环
    // 随即写响应失败、sidecar 以错误退出——用户看到的就是「打开很多页面后
    // broken pipe (os error 32)」。真正断连由 `pump_stdout` 写失败时显式摘除。
    let outbound = hub.register_reliable(local.clone());
    // channel 里携带 `Result`,区分「读到一行」「EOF(channel 关闭)」与
    // 「读/传输错误」。若不区分,超大帧或 broken pipe 会被误当成干净 EOF。
    let (inbound_tx, inbound_rx) =
        mpsc::channel::<(crate::broadcast::ClientId, anyhow::Result<String>)>(64);
    let pump = tokio::spawn(pump_stdout(
        outbound,
        writer,
        Arc::clone(&hub),
        local.clone(),
    ));
    // stdio EOF 的观察者(仅合并模式需要,理由见下方 oneshot 的注释)。
    let merged = listener.is_some();
    let (stdio_eof_tx, stdio_eof_rx) = tokio::sync::oneshot::channel::<()>();
    // `read_lines` 独占 reader,并把 stdio 读端「结束」经 oneshot 转成主循环可
    // `select!` 的事件(纯 stdio 下传 `None`,行为与改造前一致)。
    tokio::spawn(read_lines(
        reader,
        local.clone(),
        inbound_tx.clone(),
        if merged { Some(stdio_eof_tx) } else { None },
    ));
    // scope 随连接登记一次;主循环按 `client` id 查它。stdio 会话只此一条,故
    // 传入的表就是一张只装 `local` 的注册表(与 ws 的动态注册同型,便于共享)。
    let client_scopes: ClientScopes =
        Arc::new(Mutex::new(HashMap::from([(local.clone(), client_scope)])));
    let client_initialized: ClientInitialized = Arc::new(Mutex::new(HashMap::new()));

    // ---------- 合并模式:把一条环回 ws 前端(与可选的中继客户端)接到同一个
    // `serve()` 上。`listener` 为 `None` 时以下整块都跳过,纯 stdio 因此与改造前
    // 一致。 ----------
    //
    // stdio EOF 的观察者:合并后入站 channel 至少还有 ws 前端一只 sender,单靠
    // `recv() == None` 已无法感知 stdio 读端关闭。桌面关闭(EOF)必须整进程退出
    // ——spec §3.2 不变量 4「桌面关了,手机没东西可控」。故 `read_lines` 结束时
    // 经 oneshot 通知主循环,后者据此 `break`;`serve` 返回后 `inbound_rx` 被丢弃,
    // 仍在灌帧的前端(ws / 中继)随之收尾。纯 stdio 下 `stdio_eof_rx` 为 `None`,
    // `select!` 该分支 `pending`,EOF 仍由 channel 关闭感知(与改造前一致)。
    let mut stdio_eof_rx = if merged {
        Some(Box::pin(stdio_eof_rx))
    } else {
        None
    };
    let mut frontend: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    let mut relay_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    if let Some(listener) = listener {
        // 环回监听地址:中继客户端要用它拼本地 ws URL。listener 随后被 move 进
        // 前端,故先取地址。
        let addr = listener.local_addr()?;
        // 每进程一张「设备 id → ws ClientId」表。本进程只会有一条 ws 前端(stdio
        // 或纯 ws 二选一),故与 `serve_ws_inner` 一样经进程级句柄安装即可。
        let device_clients = install_device_registry(Arc::new(StdMutex::new(HashMap::new())));
        // axum 前端:握手鉴权、升级后把每条连接登记为 `ws-<uuid>`/Control,并把
        // 帧灌进**同一个** `inbound_tx`、把出站登记进**同一个** hub。
        frontend = Some(crate::ws::attach_ws_frontend(
            listener,
            Arc::clone(&hub),
            inbound_tx.clone(),
            Arc::clone(&client_scopes),
            Arc::clone(&client_initialized),
            device_clients,
            Arc::clone(&pairing),
        ));
        // 中继客户端是 **outbound** 连接,不新增入站端口。仅在给了 `relay_url` 时
        // 才起(生产路径);单测传 `None`,自己用共享 `pairing` 铸出的 token 连前端。
        if let Some(relay_url) = relay_url.as_deref() {
            let (relay_base, session) = parse_relay_url(relay_url)?;
            let relay = url::Url::parse(&relay_base)
                .map_err(|e| anyhow::anyhow!("invalid relay url `{relay_base}`: {e}"))?;
            let app_server_ws =
                url::Url::parse(&format!("ws://{addr}/ws")).expect("loopback ws url");
            // 本机中继桥的凭据:与桌面共用同一个 `pairing`,取一枚 Control 设备
            // (等价于本机走一次正常配对),交中继客户端带进 `?token=`。**不**把环回
            // ws 改成免认证——那会削弱「准入即认证」。该取法按名字幂等:同名已铸过
            // 即复用同一台设备,故每次 `--relay` 启动不会新增永久凭据。
            let local_token = pairing.seed_local_device("relay-bridge");
            relay_task = Some(tokio::spawn(async move {
                yi_agent_relay::run_client(relay, app_server_ws, session, local_token).await
            }));
        }
    }

    // 原 `inbound_tx` 在这里就必须丢弃:此后入站 channel 的 sender 只剩
    // `read_lines`(及合并模式下的 ws 前端)。否则它一直被本函数持有,stdio EOF
    // 后 channel 永不关闭,主循环的 `recv()` 拿不到 `None`,优雅退出被拖住。
    drop(inbound_tx);

    let result = serve(
        inbound_rx,
        Arc::clone(&hub),
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        attachments,
        build_agent,
        client_scopes,
        client_initialized,
        stdio_eof_rx.take(),
    )
    .await;
    // `serve` 返回(EOF 或传输错误)后主循环不再产出帧。摘除 `local` 客户端会
    // 关闭它的出站 channel;出口泵先把队列里已入队的帧全部写出、再因 channel
    // 关闭退出,故必须 `await` 它——否则 EOF 前刚入队的最后一个响应会随进程
    // 退出而丢失(与改造前「写完才结束」的 stdio 语义不符)。
    hub.unregister(&local);
    let _ = pump.await;
    // 合并模式收尾:stdio EOF(桌面退出)或前端出错后,前端监听任务与中继客户端
    // 都必须停——二者都是 spawn 出的**独立**任务(tokio 在 JoinHandle drop 时不
    // 取消),不显式 abort 就会继续 accept/重连。
    if let Some(frontend) = frontend {
        frontend.abort();
    }
    if let Some(relay_task) = relay_task {
        relay_task.abort();
    }
    result
}

/// 从一条 `AsyncRead` 逐行读取并送入主循环。
///
/// `MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,因此由本任务
/// 独占 reader,主循环只 select cancel-safe 的 channel `recv`。
async fn read_lines<R>(
    reader: R,
    client: crate::broadcast::ClientId,
    tx: mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    // 合并模式下,stdio 读端「结束」要在主循环里被 `select!` 看到。纯 stdio 传
    // `None`:此时 EOF 直接体现为入站 channel 关闭(`tx` 随本任务结束而 drop),
    // 与改造前一致。
    eof_tx: Option<tokio::sync::oneshot::Sender<()>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = MessageReader::new(reader);
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                if tx.send((client.clone(), Ok(line))).await.is_err() {
                    break;
                }
            }
            Ok(None) => break, // EOF → 丢弃 tx → 主循环优雅退出
            Err(e) => {
                tracing::error!("app-server read error: {e}");
                let _ = tx.send((client.clone(), Err(e))).await;
                break;
            }
        }
    }
    // 通知主循环「stdio 读端结束了」。纯 stdio 下 `eof_tx` 为 `None`,no-op。
    if let Some(eof_tx) = eof_tx {
        let _ = eof_tx.send(());
    }
}

/// 把该客户端的出站帧写到一条 `AsyncWrite`(stdio 场景即 stdout)。
///
/// 写失败(对端已消失)时把该客户端从 hub 摘除:主循环之后经 `hub.reply` 定向写
/// 响应会拿到 `Closed`,按**改造前**的语义以错误退出——`write_response(..)?` 曾把
/// 写失败直接变成会话错误,这条路径必须保留。
///
/// **不持有入站 channel 的 sender**:否则读端 EOF 后 channel 仍有一只存活 sender,
/// 主循环的 `recv` 永不返回 `None`,优雅退出被拖住。入站的存活只由 `read_lines`
/// 决定(它 EOF 即丢弃自己的 sender)。
async fn pump_stdout<W>(
    mut outbound: mpsc::Receiver<serde_json::Value>,
    writer: W,
    hub: Arc<crate::broadcast::Broadcaster>,
    client: crate::broadcast::ClientId,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let writer = MessageWriter::new(writer);
    while let Some(frame) = outbound.recv().await {
        if let Err(e) = writer.write_value(&frame).await {
            tracing::error!("app-server stdout write failed: {e}");
            // 出口泵写失败 = 对端真的没了(stdout 管道断裂)。这是 stdio 会话的
            // **唯一**断连判据:可靠登记不会被背压摘除,故此处必须显式摘除,
            // 让主循环随后的 `reply`/`write_response` 拿到 `Closed` 并按既有
            // 致命语义结束会话(`driver_reports_finished_when_writer_fails`)。
            hub.unregister(&client);
            break;
        }
    }
}

/// 每连接的 scope 注册表(共享、可增长)。
///
/// ws 在连接建立时登记 `ClientId → device.scope`、断连即摘除;主循环按 id 查。
/// stdio 路径只装一个 `local` 条目。用 `Arc<Mutex<..>>` 而非启动快照,是因为 ws
/// 的 `ClientId` 在 upgrade 闭包里才铸造。
pub(crate) type ClientScopes = Arc<Mutex<HashMap<crate::broadcast::ClientId, Scope>>>;

/// 每客户端的 `initialize` 状态(共享、可增长)。
///
/// 与 [`ClientScopes`] 同型、同生命周期:传输层在连接建立时**无须**登记——缺省
/// 即「未握手」(fail-closed),主循环收到 `initialize` 时置位,连接断连时由传输
/// 层摘除。用共享表而非主循环里的局部 `HashMap`,是为了能在断连时**回收**条目:
/// 否则每个 ws 连接都会永久留下一个 `ClientId` 键(连接可来去,表只增不减)。
pub(crate) type ClientInitialized = Arc<Mutex<HashMap<crate::broadcast::ClientId, bool>>>;

/// 主题变化守望者:把 [`crate::theme_tool::ThemeHandle`] 的广播翻译成
/// `ui/settings/updated`,经 hub 扇出——stdio 的 `local` 与每个 ws 客户端都要收到。
///
/// 单独成形是为了可测:测试直接驱动这段生产循环,而不是照抄一份。
///
/// `Lagged` 必须当作**继续**:它只是订阅者一时落后(客户端读得慢),`recv()` 仍可
/// 继续调用;若像旧写法那样把它与 `Closed` 一并 `return`,一次背压就会杀掉守望者,
/// 此后本进程再也推不出主题通知。只有 `Closed`(所有发送端都没了)才结束。
async fn pump_theme_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::settings_store::Theme>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    loop {
        let theme = match rx.recv().await {
            Ok(theme) => theme,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "theme watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let n = Notification::UiSettingsUpdated {
            theme: theme.as_str().to_string(),
        };
        if write_notification(&hub, &n).await.is_err() {
            return;
        }
    }
}

/// 主循环的传输无关核心:agent 工厂由调用方注入(测试用 mock provider)。
///
/// **取消安全**:入站读取由传输层任务负责(`read_lines` / WS 读循环),主循环只在
/// 一个 channel 上 `recv`。
///
/// `client_scopes` 是**每连接一次**的 scope 登记(传输层注册客户端时确定):
/// stdio 的 `local` 是 `Admin`,WS 连接是它 token 的 scope。scope 不进 channel
/// 载荷——它不随单条消息变化,而客户端 id 已经是载荷的一部分,用一张表按 id
/// 查即可。
///
/// 这张表是**共享可增长**的注册表(`Arc<Mutex<..>>`)而非启动快照:ws 的
/// `ClientId` 在 upgrade 闭包里才铸造、且随连接动态来去,启动时的一份 `HashMap`
/// 根本无法按 id 命中它们。stdio 路径只装一个 `local` 条目,与快照等价。
///
/// `pairing` 与 `workspaces` 一样由调用方注入,因为 `pair/create` 的兑现必须
/// 落在**同一个**设备表上:注入使 `run`(生产)与测试 `Harness` 各自决定表在
/// 哪里,同时 `pair/create`、`device/list`、`device/revoke` 共享同一个 `Arc`。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve<F>(
    mut inbound: mpsc::Receiver<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    hub: Arc<crate::broadcast::Broadcaster>,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scopes: ClientScopes,
    // 每客户端 `initialize` 状态的共享表(见 [`ClientInitialized`]):放在主循环
    // 之外,传输层断连时才摘得掉键。主循环在此处插入/读取,不自己拥有它。
    client_initialized: ClientInitialized,
    // 合并模式下 stdio 读端关闭的信号(见 `serve_scoped_core` 里 `read_lines` 的
    // oneshot);纯 stdio / 纯 ws 传 `None`,该 `select!` 分支 `pending`。
    mut stdio_eof: Option<std::pin::Pin<Box<tokio::sync::oneshot::Receiver<()>>>>,
) -> anyhow::Result<()>
where
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    let RuntimeAttachments {
        runtimes,
        thread_roots,
        board_dir,
        resident_dir,
        launcher: board_launcher,
        theme,
        watchman_install,
        watchman_uninstall,
        watchman_home,
    } = attachments;
    // driver task 会 clone 该 sender 上报 turn 完成事件;主循环持有它,
    // 保证 `turn_rx` 不会提前关闭。
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);

    // 反向权限请求的等待登记表:key 为 `perm-<seq>`,value 为向 driver
    // 回传决定的 oneshot 端。主循环在读到客户端响应时据此路由。
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // 审批 id 必须跨 thread 全局唯一:每个 thread 的 PermissionChecker 都从
    // request_id=1 开始,若用 request_id 直接做 key 会跨 thread 碰撞。
    let perm_seq = Arc::new(AtomicU64::new(1));

    // 每客户端 `initialized` 与 scope 登记。Tier 0 只有一个客户端时
    // `initialized` 等价于一个 bool;Tier 1 起 stdio 与多台 ws 设备共享本主
    // 循环,「谁 initialize 过了」「谁是什么 scope」都必须按 ClientId 记账,
    // 否则一台设备握手会替另一台解锁、或低权设备的请求被当成高权。
    // scope 由传输层在注册时给定(`client_scopes` 参数);这里补一条兜底,
    // 未登记的客户端按最低权 `Observe` 处理(fail-closed)。
    // `initialized` 由调用方传入(共享表),理由同 `client_scopes`:断连时传输层
    // 要能摘键,主循环持有私有 `HashMap` 就够不着它。
    let initialized = client_initialized;
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    // 该 thread 首个 turn 要激活的 runtime。驱动里做激活(不在请求循环里)以免一个
    // thread 的 socket 调用卡住所有 thread;这里只暂存 attach 的产物。
    let mut pending_activation: HashMap<String, Option<Arc<ThreadRoot>>> = HashMap::new();
    // 每个 thread 至多一条被关注的轨迹流。换任务即替换(不并存),thread 删除即收尾。
    let mut trace_watches: HashMap<String, TraceWatch> = HashMap::new();
    // 每个 thread 至多一个子任务列表守望者,首次 agent/children/list 时建立。
    let mut children_watches: HashMap<String, ChildrenWatch> = HashMap::new();
    // 每个 thread 一个进程状态守望者,建立 thread 时拉起。
    let mut process_watches: HashMap<String, ProcessWatch> = HashMap::new();

    // 看板调度器状态:本进程为哪些卡片起了会话(`card_id → TrackedThread`)。
    // 只被主循环的 tick 分支读写,与 `threads` 同属主循环局部量。
    let mut tracked: HashMap<String, TrackedThread> = HashMap::new();
    // 启动恢复:插件里 `running` 但本进程无法接手的卡片回写 `needs_you`,否则
    // 重启后它们会永久占着槽位(见 `recover_orphan_cards`)。在进主循环前跑一次:
    // 此刻进程内确定没有任何会话,判定不会误伤。
    recover_orphan_cards(&board_dir);
    // 调度器 tick:3s 一次。`interval` 的第一个 tick **立即就绪**——若就这么进
    // 循环,任何在 3s 门限前到达的请求都会先被一整轮阻塞式看板 I/O 排队。这里把
    // 首个 tick 推到 3s 之后:启动瞬间「看板立刻动起来」的收益,不值得让第一个
    // 请求等一轮 tick。
    let mut card_ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + CARD_SCHEDULER_TICK,
        CARD_SCHEDULER_TICK,
    );

    // 主题变化 → ui/settings/updated(全局一条流,与 thread 无关)。经 hub 广播:
    // stdio 的 `local` 与任意 ws 设备都能收到并切 `data-theme`。
    tokio::spawn(pump_theme_notifications(
        theme.subscribe(),
        Arc::clone(&hub),
    ));
    // 写路径仍需 theme 句柄;`set` 会落盘 + 广播,由上面的 watcher 推通知。
    let theme_handle = theme.clone();

    loop {
        tokio::select! {
            // 合并模式:stdio 读端 EOF → 结束整个循环(并丢弃 `inbound`),故
            // 桌面关闭即整进程优雅退出(spec §3.2 不变量 4)。纯 stdio / 纯 ws
            // 下该 future 为 `pending`(或 `None` 分支永不就绪)。
            _ = async {
                match stdio_eof.as_mut() {
                    Some(rx) => { let _ = rx.await; }
                    None => std::future::pending::<()>().await,
                }
            } => break,
            line = inbound.recv() => {
                let Some((client, item)) = line else { break }; // EOF → graceful exit
                // 早退守卫:某客户端(如被 `device/revoke` 踢掉、或被广播背压
                // 摘除)在断连后仍可能有一帧已在 channel 里排队。它已经不在 hub
                // 上,任何按它寻址的响应都写不出去;在主循环里处理它只会走到
                // `write_response` 的 `Closed` 分支。这里直接丢弃,别再费事。
                //
                // 只管**非本地**客户端:stdio 的 `local` 若已注销(出口泵写失败),
                // 仍要走 `write_response` 的致命分支退出会话——那是改造前的语义
                // (`oversized_frame_returns_err` / `driver_reports_finished_when_writer_fails`
                // 钉死的),不能因为这个守卫被悄悄改成优雅退出。
                if client != crate::broadcast::ClientId::local() && !hub.is_connected(&client) {
                    continue;
                }
                let line = match item {
                    Ok(l) => l,
                    Err(e) => {
                        // 尽力告知客户端我们为何退出。
                        let _ = write_response(
                            &hub, &client,
                            err_response(
                                RequestId::Num(0),
                                RpcError::invalid_request(format!("transport error: {e}")),
                            ),
                        )
                        .await;
                        // stdio 只有 `local` 一个客户端,其读端出错即整个会话结束:
                        // 把失败向上抛出,避免被伪装成干净退出(`oversized_frame_returns_err`)。
                        // WS 传输是**多客户端共享**一条主循环,单个连接的传输错误只
                        // 能摘除该 ClientId(spec §6),连接与服务器都必须存活。
                        if client == crate::broadcast::ClientId::local() {
                            return Err(e);
                        }
                        hub.unregister(&client);
                        continue;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("app-server parse error: {e}");
                        // 畸形帧里没有可用的 id。JSON-RPC 2.0 要求此时响应
                        // 的 id 为 `null`,但 `RequestId` 目前没有 Null 变体,
                        // 故暂以 id 0 代替(见 docs/bug-list.md)。
                        write_response(
                            &hub, &client,
                            err_response(RequestId::Num(0), RpcError::parse_error(e.to_string())),
                        )
                        .await?;
                        continue;
                    }
                };
                let req: RequestEnvelope = match serde_json::from_value(value.clone()) {
                    Ok(r) => r,
                    Err(_) => {
                        // 没有 `method`:可能是客户端对反向请求的响应。必须带
                        // `result` 或 `error` 才算响应,否则视为畸形帧报错。
                        match serde_json::from_value::<ClientResponse>(value) {
                            Ok(resp) if resp.result.is_some() || resp.error.is_some() => {
                                // 首次命中才广播:第二次应答(另一台设备也点了
                                // "允许")是 no-op 成功,不再产生广播,也就不会
                                // 来回弹提示。
                                if let Some((perm_id, decision)) =
                                    route_client_response(resp, &pending).await
                                {
                                    write_notification(
                                        &hub,
                                        &Notification::ToolCallApprovalResolved {
                                            perm_id,
                                            by: client.as_str().to_string(),
                                            decision: decision_label(&decision),
                                        },
                                    )
                                    .await?;
                                }
                            }
                            _ => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        RequestId::Num(0),
                                        RpcError::parse_error("malformed frame"),
                                    ),
                                )
                                .await?;
                            }
                        }
                        continue;
                    }
                };
                let id = req.id.clone();
                let method = req.method.clone();

                // 未 initialize 前,除 `initialize` 外的请求一律拒绝。按客户端
                // 判断,而非全局一个 bool。
                let client_initialized = initialized
                    .lock()
                    .await
                    .get(&client)
                    .copied()
                    .unwrap_or(false);
                if !client_initialized && method != "initialize" {
                    write_response(&hub, &client, err_response(id, RpcError::not_initialized())).await?;
                    continue;
                }

                // admin 类方法:control/observe 客户端一律拒绝。放在
                // `!initialized` 检查之后,故未握手的客户端仍先得到
                // `not_initialized`;scope 缺失按 fail-closed 的 `Observe` 处理。
                //
                // `pair/create`(铸出可换设备 token 的配对码)与 `device/revoke`
                // (踢设备、废 token)是特权桌面操作,一并入闸:低权客户端若能铸
                // 凭据或踢设备,scope 体系形同虚设。`device/list` 只暴露设备名/
                // scope/时间戳(无秘密),任何已握手客户端可读,故**不**入闸。
                const ADMIN_METHODS: [&str; 5] = [
                    "thread/delete",
                    "process/kill",
                    "thread/setPermissionMode",
                    "pair/create",
                    "device/revoke",
                ];
                let client_scope = client_scopes
                    .lock()
                    .await
                    .get(&client)
                    .copied()
                    .unwrap_or(Scope::Observe);
                if ADMIN_METHODS.contains(&method.as_str()) && client_scope < Scope::Admin {
                    write_response(
                        &hub,
                        &client,
                        err_response(id, RpcError::insufficient_scope(Scope::Admin)),
                    )
                    .await?;
                    continue;
                }

                match method.as_str() {
                    "initialize" => {
                        initialized.lock().await.insert(client.clone(), true);
                        write_response(
                            &hub, &client,
                            ok_response(
                                id,
                                json!({
                                    "serverInfo": {
                                        "name": "yi-agent-app-server",
                                        "version": env!("CARGO_PKG_VERSION"),
                                    },
                                    "protocolVersion": PROTOCOL_VERSION,
                                    "capabilities": {},
                                }),
                            ),
                        )
                        .await?;
                    }
                    "config/read" => {
                        write_response(&hub, &client, ok_response(id, cfg.redacted_view())).await?;
                    }
                    "ui/settings/read" => {
                        // 以共享句柄的当前值为权威（与 `set_theme` 工具同一实例）;
                        // 句柄在构造时已从磁盘载入，故首次读即反映持久化值。
                        // 值守开关是宿主级偏好，与 theme 同处一个文件，目录取
                        // 句柄的 workdir 以保证两者读写的是同一个 preferences.json。
                        let theme = theme_handle.current();
                        let workdir = theme_handle.workdir();
                        let watchman_enabled =
                            crate::settings_store::load_watchman_enabled(workdir);
                        // 侧车中继地址（桌面设置页写入）：与 theme/值守同处一个
                        // preferences.json，故一并从同一个 workdir 读。未配置即
                        // `null`，桌面首屏据此回填空串。
                        let relay_url = crate::settings_store::load_relay_url(workdir);
                        write_response(
                            &hub,
                            &client,
                            ok_response(
                                id,
                                json!({
                                    "theme": theme.as_str(),
                                    "board_watchman_enabled": watchman_enabled,
                                    "relay_url": relay_url,
                                }),
                            ),
                        )
                        .await?;
                    }
                    "ui/settings/write" => {
                        if let Some(enabled) =
                            req.params.get("board_watchman_enabled").and_then(|v| v.as_bool())
                        {
                            let workdir = theme_handle.workdir();
                            // 落盘失败就整体失败：偏好没写成功却去动系统状态，
                            // 会让开关与磁盘记录不一致。
                            if let Err(error) =
                                crate::settings_store::save_watchman_enabled(workdir, enabled)
                            {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::internal(error.to_string())),
                                )
                                .await?;
                                continue;
                            }
                            // 落盘成功但装/卸失败不能静默：开关状态与系统实际
                            // 状态已经不一致，前端需要拿到原因做内联提示。
                            let outcome = if enabled {
                                match std::env::current_exe() {
                                    Ok(exe) => watchman_install(&exe, &watchman_home),
                                    Err(error) => Err(error.to_string()),
                                }
                            } else {
                                watchman_uninstall(&watchman_home)
                            };
                            let warning = outcome.err();
                            if let Some(warning) = &warning {
                                tracing::warn!(%warning, enabled, "board watchman switch did not take effect");
                            }
                            write_response(
                                &hub,
                                &client,
                                ok_response(id, json!({ "ok": true, "warning": warning })),
                            )
                            .await?;
                            continue;
                        }
                        // theme 只在请求里显式带上时才校验/应用：只写 relay_url
                        // 的请求不该因为「没有 theme」而被拒绝。先校验再落盘，
                        // 免得非法 theme 把一个有效的 relay_url 写了一半。
                        let requested_theme = if req.params.get("theme").is_some() {
                            let requested =
                                req.params.get("theme").and_then(|v| v.as_str()).unwrap_or("");
                            match requested.trim().to_ascii_lowercase().as_str() {
                                "dark" => Some(crate::settings_store::Theme::Dark),
                                "light" => Some(crate::settings_store::Theme::Light),
                                other => {
                                    write_response(
                                        &hub,
                                        &client,
                                        err_response(
                                            id,
                                            RpcError::invalid_params(format!(
                                                "unsupported theme '{other}': expected 'dark' or 'light'"
                                            )),
                                        ),
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                        } else {
                            None
                        };

                        // `relay_url`（桌面设置页）：与 theme 各写各的键，共处一个
                        // 文件，故两条路径都走 settings_store 的读-改-写。
                        // `null` / 空 / 空白 → 清除该键。非串非 null（客户端写错
                        // 类型）不改动既有配置——静默清除是破坏性的。
                        if let Some(value) = req.params.get("relay_url") {
                            if value.is_null() || value.is_string() {
                                let workdir = theme_handle.workdir();
                                if let Err(error) =
                                    crate::settings_store::save_relay_url(workdir, value.as_str())
                                {
                                    write_response(
                                        &hub,
                                        &client,
                                        err_response(id, RpcError::internal(error.to_string())),
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                        }

                        if let Some(theme) = requested_theme {
                            theme_handle.set(theme);
                        }
                        write_response(&hub, &client, ok_response(id, json!({ "ok": true })))
                            .await?;
                    }
                    "plugin/query" => {
                        // `project` names the project being queried, not the
                        // server's cwd: the whole point of the RPC is to reach
                        // a board that lives under another project's daemon.
                        let Some(project) = project_arg(&req.params) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing or empty project")),
                            )
                            .await?;
                            continue;
                        };
                        let plugin = req
                            .params
                            .get("plugin")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let method = req
                            .params
                            .get("method")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let params = req
                            .params
                            .get("params")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        match plugin_query(&project, &board_dir, method, plugin, params) {
                            Ok(value) => {
                                write_response(&hub, &client, ok_response(id, value)).await?
                            }
                            Err(error) => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::board_query(error.code, error.message),
                                    ),
                                )
                                .await?
                            }
                        }
                    }
                    "board/create" => {
                        let Some(project) = project_arg(&req.params) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing or empty project")),
                            )
                            .await?;
                            continue;
                        };
                        let mut launcher = |project: &Path| board_launcher(project);
                        match yi_agent_boards::lifecycle::create_with_project(
                            &project,
                            &board_dir,
                            &resident_dir,
                            &mut launcher,
                        ) {
                            Ok(status) => {
                                // 看板建好后确保值守在装：开关为开且 plist 未指向
                                // 当前 exe 时装上。best-effort——装不上值守不该让
                                // 「创建成功」变成失败（前端已拿到看板）。
                                ensure_watchman_installed(
                                    theme_handle.workdir(),
                                    &watchman_home,
                                    &watchman_install,
                                );
                                write_response(&hub, &client, ok_response(id, to_json(&status))).await?
                            }
                            Err(message) => {
                                write_response(&hub, &client, err_response(id, RpcError::internal(message)))
                                    .await?
                            }
                        }
                    }
                    "board/remove" => {
                        let Some(project) = project_arg(&req.params) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing or empty project")),
                            )
                            .await?;
                            continue;
                        };
                        match yi_agent_boards::lifecycle::remove_in(&project, &board_dir, &resident_dir) {
                            Ok(()) => write_response(&hub, &client, ok_response(id, json!({}))).await?,
                            Err(message) => {
                                write_response(&hub, &client, err_response(id, RpcError::internal(message)))
                                    .await?
                            }
                        }
                    }
                    "board/list" => {
                        // The registry is the whole answer: the sidebar decides
                        // which projects get a board entry from exactly this.
                        match yi_agent_boards::registry::list(&board_dir) {
                            Ok(boards) => {
                                let boards: Vec<serde_json::Value> = boards
                                    .into_iter()
                                    .map(|board| {
                                        json!({
                                            "project": board.project.to_string_lossy(),
                                            "created_at": board.created_at,
                                            "status": to_json(
                                                &yi_agent_boards::lifecycle::status_with(
                                                    &board.project,
                                                    &board_dir,
                                                ),
                                            ),
                                        })
                                    })
                                    .collect();
                                write_response(&hub, &client, ok_response(id, json!({ "boards": boards })))
                                    .await?
                            }
                            Err(message) => {
                                write_response(&hub, &client, err_response(id, RpcError::internal(message.to_string())))
                                    .await?
                            }
                        }
                    }
                    "workspace/list" => {
                        let list: Vec<serde_json::Value> = workspaces
                            .list()
                            .into_iter()
                            .map(|p| json!({ "path": p, "exists": Path::new(&p).is_dir() }))
                            .collect();
                        write_response(&hub, &client, ok_response(id, json!({ "workspaces": list })))
                            .await?;
                    }
                    "workspace/add" => {
                        let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        match std::fs::canonicalize(raw) {
                            Ok(p) if p.is_dir() => {
                                let value = p.to_string_lossy().to_string();
                                match workspaces.add(&p) {
                                    Ok(()) => {
                                        write_response(&hub, &client, ok_response(id, json!({ "path": value })))
                                            .await?
                                    }
                                    Err(e) => {
                                        write_response(
                                            &hub, &client,
                                            err_response(id, RpcError::internal(e.to_string())),
                                        )
                                        .await?
                                    }
                                }
                            }
                            _ => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("path is not a directory"),
                                    ),
                                )
                                .await?;
                            }
                        }
                    }
                    "workspace/remove" => {
                        let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        // add 存的是 canonical 路径;remove 也必须规范化,否则
                        // 符号链接 / 相对路径 / 尾斜杠会静默 no-op 却仍回成功。
                        // canonicalize 失败(路径已不存在)时回退原始串,保持幂等。
                        let key = std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw));
                        match workspaces.remove(&key) {
                            Ok(()) => write_response(&hub, &client, ok_response(id, json!({}))).await?,
                            Err(e) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?
                            }
                        }
                    }
                    "thread/list" => {
                        // 单目录列表:仍是 `cfg.workdir` 的 store。跨目录分组见下方
                        // `thread/listAll`。
                        let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
                        match store.list() {
                        Ok(metas) => {
                            // 显式映射而非直接序列化 ThreadMeta:wire 契约与存储结构解耦,
                            // 存储字段重命名不会悄悄改变 RPC 输出。
                            let threads: Vec<serde_json::Value> = metas
                                .into_iter()
                                .map(|m| thread_summary_json(&m, &threads))
                                .collect();
                            write_response(&hub, &client, ok_response(id, json!({ "threads": threads })))
                                .await?;
                        }
                        Err(e) => {
                            write_response(&hub, &client, err_response(id, RpcError::internal(e.to_string())))
                                .await?;
                        }
                        }
                    }
                    "thread/listAll" => {
                        // 分组两段式,与抽取前逐字等价,只多一层「归属优先」的折拢:
                        // ① 按索引顺序(最近在前)为**每个**目录建组——含失效目录
                        //   (exists:false、threads 空,不深扫),普通会话留在本目录组;
                        // ② 带 `board_project` 的卡片会话折进它所属的项目组(项目可不在
                        //   索引里,则合成一组);只含卡片会话的目录(卡片 worktree)
                        //   第一段被抑制,不再顶层成组。
                        // **不动索引**:目录仍留在 workspaces 里,冷会话定位靠它。
                        let dirs = workspaces.list();
                        // 组顺序:先索引目录(保持最近在前),再追加索引外的新项目组。
                        let mut order: Vec<String> = Vec::new();
                        let mut seen: HashSet<String> = HashSet::new();
                        let mut exists_of: HashMap<String, bool> = HashMap::new();
                        let mut plain: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
                        let mut cards: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
                        for dir in &dirs {
                            if seen.insert(dir.clone()) {
                                order.push(dir.clone());
                                exists_of.insert(dir.clone(), Path::new(dir).is_dir());
                            }
                            // 失效目录:不深扫,组照留(threads 空),与抽取前一致。
                            let path = Path::new(dir);
                            if !path.is_dir() {
                                continue;
                            }
                            // 读取错误(如权限拒绝)不能静默等同于「无 thread」:
                            // 记 stderr 后再降级为空组,与 thread/list 的错误可见性一致。
                            let metas = match crate::thread_store::ThreadStore::new(path).list() {
                                Ok(metas) => metas,
                                Err(e) => {
                                    eprintln!(
                                        "[app-server] thread/listAll failed to list {dir}: {e}"
                                    );
                                    Vec::new()
                                }
                            };
                            let own: Vec<_> = metas
                                .iter()
                                .filter(|m| m.board_project.is_none())
                                .collect();
                            // 该目录有「自己的」普通会话(或本就为空)才作为顶层组保留;
                            // 否则它只是卡片 worktree,归拢后被抑制。
                            if !own.is_empty() || metas.is_empty() {
                                for m in own {
                                    plain.entry(dir.clone())
                                        .or_default()
                                        .push(thread_summary_json(m, &threads));
                                }
                            } else {
                                // 只含卡片会话:抑制顶层空组(稍后不产出)。
                                seen.remove(dir);
                                order.retain(|d| d != dir);
                            }
                            // 卡片会话:归入 board_project 指定的组(可为索引外的项目)。
                            for m in metas.iter().filter(|m| m.board_project.is_some()) {
                                let target = m.board_project.clone().unwrap();
                                if seen.insert(target.clone()) {
                                    order.push(target.clone());
                                    exists_of.insert(
                                        target.clone(),
                                        Path::new(&target).is_dir(),
                                    );
                                }
                                exists_of
                                    .entry(target.clone())
                                    .or_insert_with(|| Path::new(&target).is_dir());
                                cards
                                    .entry(target)
                                    .or_default()
                                    .push(thread_summary_json(m, &threads));
                            }
                        }
                        let groups: Vec<serde_json::Value> = order
                            .iter()
                            .map(|ws| {
                                let mut merged =
                                    plain.remove(ws).unwrap_or_default();
                                if let Some(mut folded) = cards.remove(ws) {
                                    merged.append(&mut folded);
                                }
                                json!({
                                    "workspace": ws,
                                    "exists": exists_of.get(ws).copied().unwrap_or(false),
                                    "threads": merged,
                                })
                            })
                            .collect();
                        let pinned: Vec<serde_json::Value> = collect_pinned(&workspaces)
                            .iter()
                            .map(|m| thread_summary_json(m, &threads))
                            .collect();
                        write_response(
                            &hub, &client,
                            ok_response(id, json!({ "groups": groups, "pinned": pinned })),
                        )
                        .await?;
                    }
                    "thread/start" => {
                        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

                        // 目录决定 agent / store / 权限 / 沙箱 / skills 的根。
                        let cwd = match resolve_thread_cwd(&req.params, &cfg, &hub, &client, id.clone()).await? {
                            Some(c) => c,
                            None => continue,
                        };

                        // 新线程一律以 Normal 起步;同一值既用于建 agent,也落盘 meta,
                        // 抽成局部量避免两处字面量漂移。
                        let mode = crate::thread_store::ThreadMode::Normal;

                        let built = match build_agent(None, Path::new(&cwd), mode) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(&hub, &client, err_response(id, RpcError::internal(e.to_string()))).await?;
                                continue;
                            }
                        };

                        // 内核负责「attach_delegation → 建 driver 通道 → insert 到
                        // `threads` → spawn 守望者与 driver」;RPC 层的通知与响应仍在
                        // 本分支完成,与抽取前逐字一致。
                        start_thread_core(
                            &mut threads,
                            &mut pending_activation,
                            &mut process_watches,
                            &runtimes,
                            &thread_roots,
                            &cfg,
                            &cwd,
                            mode,
                            thread_id.clone(),
                            built,
                            &hub,
                            &client,
                            &turn_tx,
                            &pending,
                            permission_timeout,
                            &perm_seq,
                            &theme,
                            &workspaces,
                            None,
                            None,
                        )
                        .await?;

                        let model = cfg.model.clone();
                        write_notification(&hub, &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        write_response(
                            &hub, &client,
                            ok_response(
                                id,
                                json!({
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                }),
                            ),
                        )
                        .await?;
                    }
                    "thread/resume" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // Design §8:内存中的 thread 知道其权威 cwd,若目标目录已被删除,
                        // 必须明确报错,而不是经 store_lookup 回退到 cfg.workdir(那会让
                        // 后续 load 落空,退化成含混的 -32011,甚至悄悄换到别的目录)。
                        // 冷 thread 的 meta 存在 workspace 目录内,删目录后本就不可读,
                        // 仍走下方的 -32011,不在此覆盖。
                        if let Some(session) = threads.get(&thread_id) {
                            if !Path::new(&session.cwd).is_dir() {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id.clone(),
                                        RpcError::invalid_params(format!(
                                            "working directory no longer exists: {}",
                                            session.cwd
                                        )),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        }
                        // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
                        // 冷 thread 才按全局索引 / cfg.workdir 定位,再从中载入历史。
                        let thread_store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        // 若该 thread 仍在内存且有活跃 turn,先请求中断,再等待 driver
                        // 落盘完成,否则紧随 turn/completed 的 resume 会读到尚未写入
                        // 的历史。详见 `interrupt_and_wait_for_persist`。
                        interrupt_and_wait_for_persist(&mut threads, &mut turn_rx, &thread_id).await;

                        let loaded = match thread_store.load(&thread_id) {
                            Ok(Some(l)) => l,
                            Ok(None) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            Err(e) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };

                        // 该 thread 持久化的自主权模式:决定重建 agent 时的 yolo 初值。
                        let mode = loaded.meta.permission_mode;

                        // meta 缺 cwd/model 时(损坏重建)用当前配置兜底。
                        let cwd = if loaded.meta.cwd.is_empty() {
                            cfg.workdir.display().to_string()
                        } else {
                            loaded.meta.cwd.clone()
                        };
                        let model = if loaded.meta.model.is_empty() {
                            cfg.model.clone()
                        } else {
                            loaded.meta.model.clone()
                        };

                        let mut session = yi_agent_core::Session::new();
                        // 恢复上次用量,使 auto-compact 在 resume 后的首轮即生效。
                        if let Some(u) = &loaded.usage {
                            session.set_last_input_tokens(Some(u.input_tokens));
                        }
                        session.replace_messages(loaded.messages);

                        // 按 cwd(`meta.cwd`,损坏时用 cfg.workdir 兜底)重建本 thread
                        // 的 store,让 agent 与 driver 都跑在对话真实目录,而非全局
                        // cfg.workdir——这正是此前 resume 的隐患所在。
                        let thread_store =
                            Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

                        let built = match build_agent(Some(session), Path::new(&cwd), mode) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };
                        let runtime_dir =
                            yi_agent_subagent::attach::project_runtime_directory(Path::new(&cwd));
                        let activation = attach_delegation(
                            &runtimes,
                            &thread_roots,
                            &runtime_dir,
                            &cfg,
                            &cwd,
                            &thread_id,
                            &theme,
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
                            ..
                        } = activation.built;
                        pending_activation.insert(thread_id.clone(), activation.runtime);

                        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
                        let (interject_tx, interject_rx) =
                            mpsc::channel::<InterjectionRequest>(16);
                        let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
                        // 同一 `Arc` 句柄:session 存一份供 `thread/list` 读,
                        // driver 拿一份用于推送 `thread/status/updated`。
                        let store_status = ThreadSession::new_status();
                        threads.insert(
                            thread_id.clone(),
                            ThreadSession {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                active_turn_id: None,
                                yolo,
                                process_manager: Arc::clone(&process_manager),
                                prompt_tx,
                                interrupt_tx,
                                interject_tx,
                                session_tx,
                                store: Arc::clone(&thread_store),
                                status: Arc::clone(&store_status),
                            },
                        );

                        // 与 `thread/start` 同款:先停旧守望者、再对本次生效的 manager 重订。
                        // 这里正是缺陷所在——resume 一个活 thread 会新建 manager,若沿用
                        // 「有守望者就不重建」的守卫,旧守望者会继续订阅被丢弃的 manager。
                        if let Some(previous) = process_watches.remove(&thread_id) {
                            previous.stop().await;
                        }
                        let rx = process_manager.subscribe();
                        let handle = tokio::spawn(watch_processes(
                            Arc::clone(&hub),
                            thread_id.clone(),
                            Arc::clone(&process_manager),
                            rx,
                        ));
                        process_watches.insert(thread_id.clone(), ProcessWatch { task: handle });

                        let driver_hub = Arc::clone(&hub);
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
                            Arc::clone(&pending),
                            permission_timeout,
                            Arc::clone(&perm_seq),
                            catalog,
                            Arc::clone(&thread_store),
                            Arc::clone(&store_status),
                            provider,
                            config,
                        ));

                        // 回放:thread/started → 每条历史 item/completed → 最近用量 → 响应。
                        write_notification(&hub, &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        for item in loaded.items {
                            write_notification(&hub, &Notification::ItemCompleted {
                                    thread_id: thread_id.clone(),
                                    item,
                                },
                            )
                            .await?;
                        }
                        if let Some(u) = loaded.usage {
                            write_notification(&hub, &Notification::TokenUsage {
                                    thread_id: thread_id.clone(),
                                    model: u.model,
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                    cache_creation_input_tokens: u.cache_creation_input_tokens,
                                    cache_read_input_tokens: u.cache_read_input_tokens,
                                },
                            )
                            .await?;
                        }
                        // 末尾若是崩溃残留的未收尾 turn，补一条 interrupted 标记，
                        // 让客户端把它显示为"这一轮被中断"，而不是当成正常轮次。
                        if loaded.pending_turn {
                            // 采纳的 partial 必须在这一刻**升格**进主 jsonl：它是这一轮
                            // 唯一的持久副本，若不 append 就返回，用户下一条消息的 turn-start
                            // checkpoint 会原子覆盖同一文件，该轮 items 就此永久丢失（而
                            // session 上下文仍"记得"它们，item 与上下文就此不一致）。
                            // 失败只记 stderr：恢复出不完整好过让整个 resume 报错；
                            // 判据与 load 共用，重复调用不会重复 append。
                            if let Err(e) = thread_store.promote_partial(&thread_id) {
                                eprintln!(
                                    "[app-server] failed to promote recovered turn ({thread_id}): {e}"
                                );
                            }
                            write_notification(
                                &hub,
                                &Notification::TurnCompleted {
                                    thread_id: thread_id.clone(),
                                    turn_id: "crashed".to_string(),
                                    status: TurnStatus::Interrupted,
                                    error: None,
                                },
                            )
                            .await?;
                        }
                        write_response(
                            &hub, &client,
                            ok_response(
                                id,
                                json!({
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                }),
                            ),
                        )
                        .await?;
                    }
                    "thread/rename" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let title = req
                            .params
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if title.is_empty() {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("title must not be empty")),
                            )
                            .await?;
                            continue;
                        }
                        match store_lookup(&threads, &workspaces, &cfg, &thread_id)
                            .rename(&thread_id, &title)
                        {
                            Ok(true) => {
                                write_response(&hub, &client, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
                    "thread/setPermissionMode" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // mode 校验先于 thread 存在性:非法 mode 一律 `-32602`,
                        // 不因 threadId 未知而改变错误码。
                        let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
                            Some("normal") => crate::thread_store::ThreadMode::Normal,
                            Some("yolo") => crate::thread_store::ThreadMode::Yolo,
                            _ => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(
                                            "mode must be \"normal\" or \"yolo\"",
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 仅内存中的活跃线程可切换:UI 的 mode chip 只对当前打开的线程可见,而该
                        // 线程必先经 thread/start 或 thread/resume 进入内存。冷线程没有运行期
                        // switch 可翻转(其持久化模式在 resume 时被读取),故此处不为其落盘。
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 运行期开关立即生效:与权限层、沙箱共享同一 `Arc`,故
                        // 无需重建 agent 本轮即可放行。
                        session.yolo.set(mode == crate::thread_store::ThreadMode::Yolo);
                        // 最佳努力落盘:运行期开关已生效,落盘失败不阻断本轮切换。
                        match session.store.set_permission_mode(&thread_id, mode) {
                            Ok(true) => {}
                            Ok(false) => eprintln!(
                                "[app-server] permission_mode not persisted for {thread_id}: thread meta missing"
                            ),
                            Err(e) => eprintln!(
                                "[app-server] failed to persist permission_mode for {thread_id}: {e}"
                            ),
                        }
                        write_response(&hub, &client, ok_response(id, json!({}))).await?;
                    }
                    "thread/setPinned" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // 参数校验先于 thread 存在性:非布尔一律 `-32602`。
                        let Some(pinned) = req.params.get("pinned").and_then(|v| v.as_bool())
                        else {
                            write_response(
                                &hub, &client,
                                err_response(
                                    id,
                                    RpcError::invalid_params("pinned must be a boolean"),
                                ),
                            )
                            .await?;
                            continue;
                        };
                        // 降序契约:now_millis 是当前最大值 → 新置顶项天然在最顶。
                        let seq = if pinned {
                            Some(crate::thread_store::now_millis())
                        } else {
                            None
                        };
                        match store_lookup(&threads, &workspaces, &cfg, &thread_id)
                            .set_pin_seq(&thread_id, seq)
                        {
                            Ok(true) => {
                                write_response(&hub, &client, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
                    "thread/reorderPinned" => {
                        let arr = match req.params.get("threadIds").and_then(|v| v.as_array()) {
                            Some(a) => a,
                            None => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(
                                            "threadIds must be an array of strings",
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 全部必须是字符串,否则拒绝(不允许静默丢弃非字符串项)。
                        let ids: Vec<String> = match arr
                            .iter()
                            .map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Option<Vec<_>>>()
                        {
                            Some(v) => v,
                            None => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(
                                            "threadIds must be an array of strings",
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 校验:与「当前全部置顶集合」完全一致,且无重复。
                        let pinned_now = collect_pinned(&workspaces);
                        let current: HashSet<&str> =
                            pinned_now.iter().map(|m| m.thread_id.as_str()).collect();
                        let unique: HashSet<&str> =
                            ids.iter().map(|s| s.as_str()).collect();
                        let valid = unique.len() == ids.len()
                            && ids.len() == current.len()
                            && ids.iter().all(|i| current.contains(i.as_str()));
                        if !valid {
                            write_response(
                                &hub, &client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(
                                        "threadIds must list exactly the pinned threads, no duplicates",
                                    ),
                                ),
                            )
                            .await?;
                            continue;
                        }
                        let current_seq: HashMap<String, Option<i64>> = pinned_now
                            .iter()
                            .map(|m| (m.thread_id.clone(), m.pin_seq))
                            .collect();
                        let assignments =
                            crate::thread_store::assign_pin_seqs(&ids, &current_seq);
                        let mut err: Option<RpcError> = None;
                        for (tid, seq) in assignments {
                            let store = store_lookup(&threads, &workspaces, &cfg, &tid);
                            match store.set_pin_seq(&tid, Some(seq)) {
                                Ok(true) => {}
                                Ok(false) => {
                                    err = Some(RpcError::unknown_thread(&tid));
                                    break;
                                }
                                Err(e) => {
                                    err = Some(RpcError::internal(e.to_string()));
                                    break;
                                }
                            }
                        }
                        match err {
                            None => {
                                write_response(&hub, &client, ok_response(id, json!({}))).await?;
                            }
                            Some(e) => {
                                write_response(&hub, &client, err_response(id, e)).await?;
                            }
                        }
                    }
                    "thread/delete" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
                        // 冷 thread 才按全局索引 / cfg.workdir 定位;存在性与删除都
                        // 作用在该 thread 的真实目录上。
                        let thread_store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        let in_memory = threads.contains_key(&thread_id);
                        let on_disk = thread_store.exists(&thread_id);
                        if !in_memory && !on_disk {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        // 删除前先问一句:这个会话还有子代理在跑吗?要问在中断 turn
                        // 之前——被拒的前提是不该动它分毫。子代理跑在项目 daemon 里,
                        // 对话文件不是它的权威,只删文件会把它留在无人可见的地方继续跑。
                        // force=true 才真的回收,由客户端在用户确认后携带。
                        let force = req
                            .params
                            .get("force")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        match cancel_thread_children(&runtimes, &threads, &thread_id, force) {
                            Ok(ThreadCancelOutcome::NeedsConfirmation(count)) => {
                                write_response(
                                    &hub,
                                    &client,
                                    ok_response(
                                        id,
                                        json!({
                                            "status": "needs_confirmation",
                                            "active_children": count,
                                        }),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                            Ok(ThreadCancelOutcome::Cancelled(cancelled)) => {
                                if cancelled > 0 {
                                    eprintln!(
                                        "[app-server] cancelled {cancelled} subagent task(s) for {thread_id}"
                                    );
                                }
                            }
                            Err(cause) => eprintln!(
                                "[app-server] could not cancel subagents for {thread_id}: {cause}"
                            ),
                        }
                        // 活跃 thread:先中断,再等 driver 落盘完成才删文件。若像旧
                        // 实现那样在 driver 落盘前就删文件,driver 的 `append_turn`
                        // (`create(true)`)会把 `<id>.jsonl` 复活,导致删除后
                        // `store.exists` 仍为真、`thread/resume` 能把已删 thread 拉回。
                        // 故复用 resume 的等待模式,等落盘后再删。
                        interrupt_and_wait_for_persist(&mut threads, &mut turn_rx, &thread_id).await;
                        // 落盘已结束:现在从内存移除(drop prompt_tx 让 driver 收尾)并删文件。
                        threads.remove(&thread_id);
                        pending_activation.remove(&thread_id);
                        // End this conversation's own root before the shared binding is
                        // considered for detach: the root is per conversation, the
                        // binding is per project and outlives any single conversation.
                        if let Some(root) = thread_roots
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&thread_id)
                        {
                            root.detach();
                        }
                        if let Some(watch) = trace_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        if let Some(watch) = children_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        if let Some(watch) = process_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        let live_cwds = threads
                            .values()
                            .map(|session| session.cwd.clone())
                            .collect::<Vec<_>>();
                        detach_unused_runtimes(&runtimes, &live_cwds);
                        if let Err(e) = thread_store.delete(&thread_id) {
                            eprintln!("[app-server] failed to delete thread files for {thread_id}: {e}");
                        }
                        write_response(&hub, &client, ok_response(id, json!({}))).await?;
                    }
                    "thread/clear" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 清空跑在半个 turn 上会产出不自洽的历史，直接拒绝。
                        if session.active_turn_id.is_some() {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::turn_in_progress(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if session
                            .session_tx
                            .send(SessionCommand::Clear { reply: reply_tx })
                            .await
                            .is_err()
                        {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::internal("thread driver is gone")),
                            )
                            .await?;
                            continue;
                        }
                        match reply_rx.await {
                            Ok(Ok(())) => {
                                write_response(&hub, &client, ok_response(id, json!({}))).await?
                            }
                            Ok(Err(message)) => write_response(
                                &hub, &client,
                                err_response(id, RpcError::internal(message)),
                            )
                            .await?,
                            Err(_) => write_response(
                                &hub, &client,
                                err_response(id, RpcError::internal("thread driver dropped")),
                            )
                            .await?,
                        }
                    }
                    "thread/compact" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 压缩会重写整段历史,turn 进行中不接受。
                        if session.active_turn_id.is_some() {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::turn_in_progress(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        let (reply_tx, reply_rx) = oneshot::channel();
                        if session
                            .session_tx
                            .send(SessionCommand::Compact { reply: reply_tx })
                            .await
                            .is_err()
                        {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::internal("thread driver is gone")),
                            )
                            .await?;
                            continue;
                        }
                        // compact 要调一次 provider 生成摘要,属于长请求;不设短超时。
                        let result = match reply_rx.await {
                            Ok(CompactOutcome::Compacted) => json!({"status": "compacted"}),
                            Ok(CompactOutcome::NotReduced) => json!({"status": "not_reduced"}),
                            Ok(CompactOutcome::Failed(message)) => {
                                json!({"status": "failed", "error": message})
                            }
                            Err(_) => json!({"status": "failed", "error": "thread driver dropped"}),
                        };
                        write_response(&hub, &client, ok_response(id, result)).await?;
                    }
                    "turn/start" => {
                        // 校验 / 占 `active_turn_id` / turn/started / Running 状态 /
                        // 响应 / 投递 prompt 全在 start_turn_core 内,顺序与抽取前一致;
                        // `Ok(None)` 表示已写过错误响应,这里 continue。
                        if start_turn_core(
                            &mut threads,
                            &pending_activation,
                            &hub,
                            &client,
                            &req.params,
                            id,
                        )
                        .await?
                        .is_none()
                        {
                            continue;
                        }
                    }
                    "turn/interrupt" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        if let Some(turn_id) = session.active_turn_id.clone() {
                            // 幂等 level 信号:用 try_send 避免主循环在满队列上阻塞。
                            let _ = session.interrupt_tx.try_send(turn_id);
                        }
                        write_response(&hub, &client, ok_response(id, json!({}))).await?;
                    }
                    "turn/interject" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let text = match extract_prompt(&req.params) {
                            Some(p) => p,
                            None => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("missing or empty input text"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 单值 `active_turn_id` 决定了"往哪一轮追加":没有活跃 turn
                        // 就没有可并入的上下文,返回 -32013 让客户端改走 turn/start。
                        let (tx, turn_id) = {
                            let Some(session) = threads.get(&thread_id) else {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            match session.active_turn_id.clone() {
                                Some(turn_id) => (session.interject_tx.clone(), turn_id),
                                None => {
                                    write_response(
                                        &hub, &client,
                                        err_response(id, RpcError::not_running()),
                                    )
                                    .await?;
                                    continue;
                                }
                            }
                        };
                        let interjection_id =
                            format!("interject-{}-{}", turn_id, uuid::Uuid::new_v4());
                        if tx
                            .send(InterjectionRequest {
                                turn_id: turn_id.clone(),
                                interjection_id: interjection_id.clone(),
                                text,
                            })
                            .await
                            .is_err()
                        {
                            // driver 已退出:没有接收方,按"没有活跃 turn"报。
                            write_response(&hub, &client, err_response(id, RpcError::not_running()))
                                .await?;
                            continue;
                        }
                        write_response(
                            &hub, &client,
                            ok_response(
                                id,
                                json!({
                                    "turn_id": turn_id,
                                    "interjection_id": interjection_id,
                                }),
                            ),
                        )
                        .await?;
                    }
                    "agent/children/list" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        // 列表是尽力而为的只读视图:attach 未就绪或 daemon 不可达时返回空表,
                        // 而不是报错。桌面端的暂留区因此永远能渲染,turn 也照常跑。
                        let snapshot = list_task_summaries(&runtimes, &threads, &thread_id);
                        let children =
                            children_for_thread(&snapshot.unwrap_or_default(), &thread_id);
                        // 首次拉取时建立守望者:此后列表变化由 agent/children/updated 推出,
                        // 客户端不必轮询。daemon 不可达时不建立(没有可观察的变化)。
                        if !children_watches.contains_key(&thread_id) {
                            if let Some(socket) = socket_for_thread(&runtimes, &threads, &thread_id) {
                                let task = tokio::spawn(watch_children(
                                    Arc::clone(&hub),
                                    thread_id.clone(),
                                    socket,
                                    children.clone(),
                                ));
                                children_watches.insert(thread_id.clone(), ChildrenWatch { task });
                            }
                        }
                        write_response(&hub, &client, ok_response(id, json!({ "children": children })))
                            .await?;
                    }
                    "agent/trace/read" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let Some(task_id) =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing taskId")),
                            )
                            .await?;
                            continue;
                        };
                        match read_task_trace(&runtimes, &threads, &thread_id, &task_id) {
                            Ok((rows, high_water_id)) => {
                                write_response(
                                    &hub, &client,
                                    ok_response(
                                        id,
                                        json!({ "rows": rows, "highWaterId": high_water_id }),
                                    ),
                                )
                                .await?;
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/trace/watch" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let Some(task_id) =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing taskId")),
                            )
                            .await?;
                            continue;
                        };
                        // 替换而不是叠加:一个 thread 只关注一个任务,旧流必须先停,
                        // 否则换任务后两个任务的行使会同时到达客户端。
                        if let Some(previous) = trace_watches.remove(&thread_id) {
                            previous.stop().await;
                        }
                        match snapshot_for_watch(&runtimes, &threads, &thread_id, &task_id) {
                            Ok((rows, high_water_id)) => {
                                let handle = tokio::spawn(stream_trace(
                                    Arc::clone(&hub),
                                    thread_id.clone(),
                                    task_id.clone(),
                                    socket_for_thread(&runtimes, &threads, &thread_id)
                                        .unwrap_or_default(),
                                    high_water_id,
                                ));
                                trace_watches.insert(thread_id.clone(), TraceWatch { task: handle });
                                write_response(
                                    &hub, &client,
                                    ok_response(id, json!({ "rows": rows, "highWaterId": high_water_id })),
                                )
                                .await?;
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/message" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let task_id =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string);
                        let message =
                            req.params.get("message").and_then(|v| v.as_str()).map(str::to_string);
                        let (Some(task_id), Some(message)) = (task_id, message) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing taskId or message")),
                            )
                            .await?;
                            continue;
                        };
                        match forward_to_daemon(
                            &runtimes,
                            &threads,
                            &thread_id,
                            yi_agent_store::ipc::IpcRequest::SendUserMessage {
                                task_id: task_id.clone(),
                                message,
                            },
                        ) {
                            Ok(_) => {
                                write_response(&hub, &client, ok_response(id, json!({ "queued": true })))
                                    .await?;
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/cancel/preview" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let Some(task_id) =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing taskId")),
                            )
                            .await?;
                            continue;
                        };
                        // 取消必须两步:预览拿 token,确认才真取消。这里只做第一步,
                        // 绝不代客户端跳过确认。
                        match forward_to_daemon(
                            &runtimes,
                            &threads,
                            &thread_id,
                            yi_agent_store::ipc::IpcRequest::PreviewCancel {
                                task_id: task_id.clone(),
                                recursive: false,
                            },
                        ) {
                            Ok(yi_agent_store::ipc::IpcResponse::CancelPreview {
                                confirmation_token,
                                task_ids,
                                expires_in_secs,
                                ..
                            }) => {
                                write_response(
                                    &hub, &client,
                                    ok_response(
                                        id,
                                        json!({
                                            "confirmationToken": confirmation_token,
                                            "taskIds": task_ids,
                                            "expiresInSecs": expires_in_secs,
                                        }),
                                    ),
                                )
                                .await?;
                            }
                            Ok(other) => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::internal(format!(
                                            "daemon returned a non-preview response: {other:?}"
                                        )),
                                    ),
                                )
                                .await?;
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/cancel" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let task_id =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string);
                        let confirmation_token = req
                            .params
                            .get("confirmationToken")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let (Some(task_id), Some(confirmation_token)) =
                            (task_id, confirmation_token)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(
                                        "missing taskId or confirmationToken; preview first",
                                    ),
                                ),
                            )
                            .await?;
                            continue;
                        };
                        match forward_to_daemon(
                            &runtimes,
                            &threads,
                            &thread_id,
                            yi_agent_store::ipc::IpcRequest::ConfirmCancel {
                                task_id: task_id.clone(),
                                recursive: false,
                                confirmation_token,
                            },
                        ) {
                            Ok(_) => {
                                write_response(&hub, &client, ok_response(id, json!({ "cancelled": true })))
                                    .await?;
                            }
                            Err(error) => {
                                write_response(&hub, &client, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/trace/unwatch" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        if let Some(watch) = trace_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        if let Some(watch) = children_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        if let Some(watch) = process_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        write_response(&hub, &client, ok_response(id, json!({ "stopped": true }))).await?;
                    }
                    "process/list" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing thread_id")),
                            )
                            .await?;
                            continue;
                        };
                        // 未知 thread 返回空表而非错误:切走再切回、thread 已删除
                        // 都是正常路径,报错只会弹一条无意义的红条。
                        let processes = threads
                            .get(&thread_id)
                            .map(|s| s.process_manager.list())
                            .unwrap_or_default();
                        write_response(&hub, &client, ok_response(id, json!({ "processes": processes })))
                            .await?;
                    }
                    "process/read" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing thread_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(process_id) = req
                            .params
                            .get("process_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing process_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        let cursor = req.params.get("cursor").and_then(|v| v.as_u64());
                        let max_bytes = req
                            .params
                            .get("max_bytes")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(64 * 1024) as usize;
                        match session
                            .process_manager
                            .read(yi_agent_tools::ProcessSelector::Id(process_id), cursor, max_bytes)
                            .await
                        {
                            Ok(result) => {
                                write_response(
                                    &hub, &client,
                                    ok_response(
                                        id,
                                        serde_json::to_value(result)
                                            .unwrap_or(serde_json::Value::Null),
                                    ),
                                )
                                .await?
                            }
                            Err(message) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::invalid_params(message)),
                                )
                                .await?
                            }
                        }
                    }
                    "process/kill" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing thread_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(process_id) = req
                            .params
                            .get("process_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::invalid_params("missing process_id")),
                            )
                            .await?;
                            continue;
                        };
        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        match session
                            .process_manager
                            .kill(yi_agent_tools::ProcessSelector::Id(process_id))
                            .await
                        {
                            Ok(()) => {
                                write_response(&hub, &client, ok_response(id, json!({ "ok": true })))
                                    .await?
                            }
                            Err(message) => {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::invalid_params(message)),
                                )
                                .await?
                            }
                        }
                    }
                    "pair/create" => {
                        // 铸一枚一次性配对码,交桌面端渲染二维码。设备注册属于
                        // `pair/redeem`(见下),这里只铸码。
                        let code = pairing.create_code();
                        write_response(
                            &hub,
                            &client,
                            ok_response(
                                id,
                                json!({ "code": code.code, "expires_in": code.expires_in }),
                            ),
                        )
                        .await?;
                    }
                    "pair/redeem" => {
                        // 帧级兑换:一次性配对码换设备 token。与 ws 升级时的
                        // `?pair=<code>` 等价,但走**帧**——这是**经中继**配对的唯一
                        // 通路:中继只转发 ws 帧、不改写升级查询串,所以手机的
                        // `?pair=` 到不了本机 app-server;而手机连上中继后发的这条
                        // RPC 会被中继桥原样转发过来,本机据此铸设备。
                        //
                        // **不在 `ADMIN_METHODS` 内**:调用者(含中继桥)无需 admin——
                        // 凭证是那枚一次性码本身(只有桌面能铸),无码即无效。
                        let Some(code) = req
                            .params
                            .get("code")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::invalid_params("missing code")),
                            )
                            .await?;
                            continue;
                        };
                        let name = req
                            .params
                            .get("device_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("remote device")
                            .to_string();
                        match pairing.redeem(&code, &name) {
                            Ok((device, token)) => {
                                write_response(
                                    &hub,
                                    &client,
                                    ok_response(
                                        id,
                                        json!({
                                            "device_id": device.id,
                                            "token": token,
                                            "scope": device.scope,
                                        }),
                                    ),
                                )
                                .await?;
                            }
                            Err(_) => {
                                // 码不存在/已用/已过期:与"无 token"一致的说法,
                                // 不泄露码是否存在。
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::invalid_pairing_code()),
                                )
                                .await?;
                            }
                        }
                    }
                    "device/list" => {
                        let devices: Vec<serde_json::Value> = pairing
                            .store()
                            .list()
                            .into_iter()
                            .map(|d| {
                                json!({
                                    "id": d.id,
                                    "name": d.name,
                                    "scope": d.scope,
                                    "created_at": d.created_at,
                                    "last_seen_at": d.last_seen_at,
                                })
                            })
                            .collect();
                        write_response(&hub, &client, ok_response(id, json!({ "devices": devices })))
                            .await?;
                    }
                    "device/revoke" => {
                        let Some(device_id) = req
                            .params
                            .get("device_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                        else {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::invalid_params("missing device_id")),
                            )
                            .await?;
                            continue;
                        };
                        let revoked = pairing.revoke(&device_id).unwrap_or(false);
                        if revoked {
                            // 先取当前连接:撤销即掉线由「`hub.unregister` + 读循环
                            // 的 disconnected tick」共同完成。用 `get` + `forget`
                            // 两步是为了兼顾两种语义——`ws_client_for_device` 仍是
                            // 「此刻该设备连在哪」的权威读法(测试与将来 RPC 复用),
                            // `forget_ws_device` 顺带把该映射从全局表摘掉,不留陈旧项。
                            let cid = ws_client_for_device(&device_id);
                            forget_ws_device(&device_id);
                            if let Some(cid) = cid {
                                hub.unregister(&cid);
                            }
                        }
                        write_response(&hub, &client, ok_response(id, json!({ "revoked": revoked })))
                            .await?;
                    }
                    "thread/subscribe" => {
                        // 整体替换该客户端的订阅集合。**不在 ADMIN_METHODS 内**:
                        // 任何已初始化客户端都能收窄自己的 feed。未调用过的客户端
                        // 保持 `Feed::All`(桌面行为不变)。
                        let ids: Option<Vec<String>> = req
                            .params
                            .get("threadIds")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(str::to_string))
                                    .collect()
                            });
                        let Some(ids) = ids else {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::invalid_params("missing threadIds")),
                            )
                            .await?;
                            continue;
                        };
                        if ids.len() > crate::broadcast::MAX_SUBSCRIPTIONS {
                            write_response(
                                &hub,
                                &client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(format!(
                                        "at most {} threadIds",
                                        crate::broadcast::MAX_SUBSCRIPTIONS
                                    )),
                                ),
                            )
                            .await?;
                            continue;
                        }
                        hub.subscribe(&client, ids.clone());
                        write_response(
                            &hub,
                            &client,
                            ok_response(id, json!({ "subscribed": ids })),
                        )
                        .await?;
                    }
                    "thread/readItems" => {
                        // 只读补齐：读已落盘的 items，**不 resume、不重建 agent、
                        // 不中断正在跑的回合**（这正是它相对 thread/resume 的意义）。
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let after = req
                            .params
                            .get("afterItemId")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        let loaded = match store.load(&thread_id) {
                            Ok(Some(l)) => l,
                            Ok(None) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            Err(e) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };
                        let items: Vec<crate::protocol::Item> = match after {
                            Some(aid) => match loaded
                                .items
                                .iter()
                                .position(|it| item_id(it) == Some(aid.as_str()))
                            {
                                // 找到锚点：只给它之后的部分。
                                Some(i) => loaded.items.into_iter().skip(i + 1).collect(),
                                // 锚点缺失（可能被 compact 丢弃）：返回全部，由客户端按 id 去重。
                                None => loaded.items,
                            },
                            None => loaded.items,
                        };
                        write_response(&hub, &client, ok_response(id, json!({ "items": items })))
                            .await?;
                    }
                    _ => {
                        write_response(&hub, &client, err_response(id, RpcError::method_not_found(&method)))
                            .await?;
                    }
                }
            }
            ev = turn_rx.recv() => {
                if let Some(TurnEvent::Finished { thread_id, turn_id }) = ev {
                    if let Some(s) = threads.get_mut(&thread_id) {
                        // 仅当完成的正是当前活跃 turn 时才清除,避免迟到的
                        // 完成事件误清掉已开始的下一个 turn。
                        if s.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                            s.active_turn_id = None;
                        }
                    }
                }
            }
            _ = card_ticker.tick() => {
                // 看板调度器内联跑一轮:启动(queued → 起可见会话)与对账
                // (会话结束 → 卡片终态)。所有局部量按顺序借用,不与 RPC 分支
                // 并发,故 `threads` 无需改成共享状态(见 Task 6c brief)。
                card_scheduler_tick(
                    &board_dir,
                    &mut tracked,
                    &mut threads,
                    &mut pending_activation,
                    &mut process_watches,
                    &runtimes,
                    &thread_roots,
                    &cfg,
                    &hub,
                    &turn_tx,
                    &pending,
                    permission_timeout,
                    &perm_seq,
                    &theme,
                    &workspaces,
                    &build_agent,
                )
                .await;
            }
        }
    }

    // 进程退出前断开所有项目 root。同步调用:已经不在热路径上,而进程即将结束。
    for binding in attached_runtimes(&runtimes) {
        binding.detach();
    }

    Ok(())
}

/// 恢复会话:`Some` 时用载入的历史覆盖 agent 的 session,`None` 时保持新建的空 session。
///
/// 用 `set_session_messages` 就地替换内容而不是 `with_session` 换成新 `Arc`:会话
/// 句柄可能已被委派工具的 `CallerContext` 绑定(见 [`wrap_for_delegation`]),
/// 换 `Arc` 会让那枚句柄指向被丢弃的旧会话,`fork` 便再也看不到这位调用者的记录。
fn apply_session(agent: &mut yi_agent_core::Agent, session: Option<yi_agent_core::Session>) {
    if let Some(s) = session {
        agent.set_session_messages(s.messages().to_vec());
        // `set_session_messages` 只搬运消息,若不在此补写,resume 载入的
        // `last_input_tokens` 会在live 句柄上归零,使上面的注释承诺的
        // 「resume 后首轮 auto-compact 即生效」落空(`maybe_auto_compact`
        // 读到 `None` 直接短路)。句柄仍是同一个 Arc,只是补一个字段。
        agent
            .session_handle()
            .lock()
            .unwrap()
            .set_last_input_tokens(s.last_input_tokens());
    }
}

/// 把一个设备 id 映射到它当前的 ws `ClientId`——设备被撤销时据此摘除连接。
///
/// 这张表在 **ws 传输内部**维护(`ws.rs`):每个已认证的连接把自己
/// `device_id → ClientId` 登记进去,断连即摘除。主循环(本文件)不能直接持有
/// 它——`ws.rs` 依赖 `server.rs`,反向依赖会成环——故 ws 层在启动时把
/// `Arc<Mutex<HashMap<String, ClientId>>>` 交给本函数要读的进程级句柄
/// ([`install_device_registry`])。表为空(纯 stdio、无 ws,或尚未起 ws)时
/// 安全返回 `None`。
fn ws_client_for_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
    let registry = device_registry().get()?;
    let guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard.get(device_id).cloned()
}

/// 取出并移除某设备 id 的连接映射(撤销时用)。
///
/// 与 [`ws_client_for_device`] 同一张表,但顺带摘掉条目:设备已撤销,它的
/// `device_id → ClientId` 映射不该留在全局表里等下一个同名设备(或迟到的清理)
/// 撞上。真正的连接关闭由调用方 `hub.unregister` + ws 读循环的 disconnected
/// tick 完成;这里只负责让主循环侧不再记得它。
fn forget_ws_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
    let registry = device_registry().get()?;
    let mut guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard.remove(device_id)
}

/// 进程级「设备 id → ws `ClientId`」句柄。
///
/// 用 `OnceLock` 而非 `RuntimeAttachments` 字段:它天然是**每进程一张表**
/// (一个 app-server 进程只有一套 ws 连接),且只在 ws 传输启动时被设置一次。
/// ws 起不来时它保持未设置,`device/revoke` 于是退化为「只改设备表、不摘连接」,
/// 在纯 stdio 场景下本就没有 ws 连接可摘。
static DEVICE_REGISTRY: OnceLock<WsDeviceRegistry> = OnceLock::new();

pub(crate) type WsDeviceRegistry = Arc<StdMutex<HashMap<String, crate::broadcast::ClientId>>>;

fn device_registry() -> &'static OnceLock<WsDeviceRegistry> {
    &DEVICE_REGISTRY
}

/// 由 ws 传输在启动时登记它的「设备 id → `ClientId`」表(幂等;重复调用保留
/// 第一张)。返回该表,便于 ws 侧直接持有同一份 `Arc`。
pub(crate) fn install_device_registry(registry: WsDeviceRegistry) -> WsDeviceRegistry {
    device_registry().set(Arc::clone(&registry)).ok();
    device_registry().get().cloned().unwrap_or(registry)
}

fn ok_response(id: RequestId, result: serde_json::Value) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: Some(result),
        error: None,
    }
}

fn err_response(id: RequestId, error: RpcError) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: None,
        error: Some(error),
    }
}

async fn write_response(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(&resp)
        .map_err(|e| anyhow::anyhow!("failed to serialize response: {e}"))?;
    // 定向回复:`reply` 会 await 到入队成功,保留 stdio 改造前
    // 「写阻塞直到对端读」的语义。
    match hub.reply(client, frame).await {
        Ok(()) => Ok(()),
        // 写失败到**非本地**客户端(ws 手机)不得掀翻共享主循环:那是**多客户端
        // 共享**的一台服务器,一个被 `device/revoke` 踢掉、或被广播背压摘除
        // (慢消费者)的连接,不能让所有其它客户端(以及后续连接)一起陪葬。
        // 摘掉它、让本轮请求就此了结即可(它的读循环靠 `is_connected` 票 tick
        // 自行收尾)。
        Err(_) if *client != crate::broadcast::ClientId::local() => {
            tracing::warn!(
                client = client.as_str(),
                "app-server response write failed; dropping the ws client"
            );
            hub.unregister(client);
            Ok(())
        }
        // stdio 的 `local` 仍保留改造前的语义:出口泵已死,再写就是错,把错误
        // 抛出以结束会话(`driver_reports_finished_when_writer_fails` 钉死这条)。
        Err(_) => Err(anyhow::anyhow!("client {} disconnected", client.as_str())),
    }
}

async fn write_notification(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    // S2：列表层/全局帧恒推（无键）；内容层按 thread 过滤。无订阅者时等价于全广播。
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_for(key, frame);
    Ok(())
}

/// 回放分块的字节软预算（256 KiB）。单帧序列化后须**远小于**
/// `MAX_FRAME_BYTES = 1 MiB`,故留足余量。
const REPLAY_CHUNK_BYTES: usize = 256 * 1024;
/// 回放分块的条数上限，兜住「海量极小 item」把帧数压不下来的极端。
const REPLAY_CHUNK_MAX_ITEMS: usize = 200;

/// 把历史 items 切成回放帧的块：顺序保持；累计序列化字节超过
/// [`REPLAY_CHUNK_BYTES`] 或条数达到 [`REPLAY_CHUNK_MAX_ITEMS`] 即切块。
/// 单条自身超预算时它单独成块（容积为 1），不在此函数内再切分。
fn chunk_items_for_replay(items: Vec<crate::protocol::Item>) -> Vec<Vec<crate::protocol::Item>> {
    let mut chunks: Vec<Vec<crate::protocol::Item>> = Vec::new();
    let mut cur: Vec<crate::protocol::Item> = Vec::new();
    let mut cur_bytes = 0usize;
    for item in items {
        let approx = serde_json::to_vec(&item).map(|v| v.len()).unwrap_or(64);
        // 字节预算：当前块非空且再加一条会超预算 → 先结块。
        if !cur.is_empty() && cur_bytes + approx > REPLAY_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(item);
        cur_bytes += approx;
        // 条数上限：达到上限即结块（与字节预算互为兜底）。
        if cur.len() >= REPLAY_CHUNK_MAX_ITEMS {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// 逐字流合并器：按 thread 攒 `(item_id, text)`。跨 item_id 不合并（顺序优先）。
#[derive(Default)]
struct DeltaCoalescer {
    /// thread → 当前正在累加的 (item_id, text)。
    pending: std::collections::HashMap<String, (String, String)>,
}

impl DeltaCoalescer {
    const FLUSH_BYTES: usize = 4096;

    /// 追加一段 delta，返回**此刻应当立即发出去的**若干条（跨 item 或超限时）。
    fn push(&mut self, thread: &str, item_id: &str, delta: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        match self.pending.get_mut(thread) {
            Some((pid, text)) if pid == item_id => text.push_str(delta),
            _ => {
                if let Some(prev) = self.pending.remove(thread) {
                    out.push(prev);
                }
                self.pending
                    .insert(thread.to_string(), (item_id.to_string(), delta.to_string()));
            }
        }
        if self.pending.get(thread).map(|(_, t)| t.len()).unwrap_or(0) >= Self::FLUSH_BYTES {
            if let Some(p) = self.pending.remove(thread) {
                out.push(p);
            }
        }
        out
    }

    /// 取出并清空该 thread 的待发（屏障/tick 调用）。
    fn take(&mut self, thread: &str) -> Option<(String, String)> {
        self.pending.remove(thread)
    }

    /// 取出全部待发（tick 用）。
    fn take_all(&mut self) -> Vec<(String, (String, String))> {
        self.pending.drain().collect()
    }
}

/// 更新共享状态句柄并推送 `thread/status/updated`。
///
/// 加锁是同步的、不跨 `.await`；锁在写通知前即释放。锁中毒时沿用
/// `workspace_index.rs` 的恢复约定：取回内部值而非 panic(状态只是 UI 提示,
/// 不应因一次 panic 永久失效)。
async fn update_status(
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
fn thread_status(threads: &HashMap<String, ThreadSession>, thread_id: &str) -> ThreadStatus {
    threads
        .get(thread_id)
        .map(|s| *s.status.lock().unwrap_or_else(|p| p.into_inner()))
        .unwrap_or(ThreadStatus::Idle)
}

/// 跨工作目录收集已置顶的 thread meta，按置顶分区契约排序：
/// `pin_seq` 降序（越大越靠前），`updated_at` 降序，`thread_id` 升序。
fn collect_pinned(workspaces: &WorkspaceIndex) -> Vec<crate::thread_store::ThreadMeta> {
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

/// 把 thread meta 渲染成 wire 上的 ThreadSummary（含 `pinned`）。
fn thread_summary_json(
    m: &crate::thread_store::ThreadMeta,
    threads: &HashMap<String, ThreadSession>,
) -> serde_json::Value {
    json!({
        "thread_id": m.thread_id,
        "cwd": m.cwd,
        "model": m.model,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "title": m.title,
        "permission_mode": m.permission_mode,
        "pinned": m.pin_seq.is_some(),
        "board_project": m.board_project,
        "card_id": m.card_id,
        "status": thread_status(threads, &m.thread_id),
    })
}

/// 把客户端对反向请求的响应路由到等待中的 driver。
///
/// 返回 `Some(decision)` 表示本次响应**首次**命中;`None` 表示该审批已被别的
/// 客户端处理(或根本不存在)。后者不是错误:双端同时点"允许"时,后到者是
/// no-op 成功,而不是报错——报错会让另一端弹出一个无意义的失败提示。
async fn route_client_response(
    resp: ClientResponse,
    pending: &Mutex<HashMap<String, oneshot::Sender<Decision>>>,
) -> Option<(String, Decision)> {
    let RequestId::Str(key) = resp.id else {
        tracing::warn!("ignoring client response with non-string id");
        return None;
    };
    let Some(tx) = pending.lock().await.remove(&key) else {
        // 已被先到的应答取走:安静地当作 no-op,不 warn(双答是预期场景)。
        tracing::debug!("approval {key} was already resolved");
        return None;
    };
    let decision = parse_client_decision(resp.result.as_ref());
    let _ = tx.send(decision.clone());
    Some((key, decision))
}

/// wire 上的决定标签,与 `parse_client_decision` 接受的取值互逆。
///
/// `AlwaysAllowPrefix` 丢掉前缀:本标签只回答"是谁、做了什么类型的决定",
/// 不含策略细节;需要前缀的客户端可保留自己的弹窗上下文。
fn decision_label(decision: &Decision) -> String {
    match decision {
        Decision::AllowOnce => "allow_once",
        Decision::AlwaysAllowTool => "always_allow_tool",
        Decision::AlwaysAllowPrefix(_) => "always_allow_prefix",
        Decision::Deny => "deny",
    }
    .to_string()
}

/// 解析客户端的权限决定;任何未知/畸形取值一律按 Deny 处理(fail-safe)。
fn parse_client_decision(result: Option<&serde_json::Value>) -> Decision {
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

/// 构造一个 `TurnEvent::Finished`,避免在 driver 里重复拼字段。
fn finished_event(thread_id: &str, turn_id: &str) -> TurnEvent {
    TurnEvent::Finished {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
    }
}

/// 若 `thread_id` 有活跃 turn,中断它并等待 driver 落盘完成。
///
/// driver 是「先发 turn/completed,后 append/touch」,而 `TurnEvent::Finished`
/// 在落盘之后才发出;因此本函数返回后,调用方可以依赖「该 turn 已落盘」。
/// 有界等待:driver 异常卡死时不至于拖垮整个请求循环。
async fn interrupt_and_wait_for_persist(
    threads: &mut HashMap<String, ThreadSession>,
    turn_rx: &mut mpsc::Receiver<TurnEvent>,
    thread_id: &str,
) {
    let Some(tid) = threads
        .get(thread_id)
        .and_then(|s| s.active_turn_id.clone())
    else {
        return;
    };
    if let Some(s) = threads.get(thread_id) {
        let _ = s.interrupt_tx.try_send(tid.clone());
    }
    let wait_for_persist = async {
        while let Some(TurnEvent::Finished {
            thread_id: done_id,
            turn_id: done_turn,
        }) = turn_rx.recv().await
        {
            if let Some(s) = threads.get_mut(&done_id) {
                if s.active_turn_id.as_deref() == Some(done_turn.as_str()) {
                    s.active_turn_id = None;
                }
            }
            if done_id == thread_id && done_turn == tid {
                break;
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_persist)
        .await
        .is_err()
    {
        eprintln!(
            "[app-server] timed out waiting for turn of {thread_id} to persist; \
             a late driver append may resurrect its files"
        );
    }
}

/// 组装当前 turn 的 checkpoint：已 finalize 的 item（含开头的用户提问）+ 当前
/// 上下文快照。进行中的流式文本不在 `completed_items` 里，故天然不入内。
fn build_partial(
    turn_id: &str,
    user_prompt: &str,
    completed_items: &[crate::protocol::Item],
    messages: Vec<yi_agent_core::Message>,
    usage: Option<crate::thread_store::TurnUsage>,
) -> crate::thread_store::PartialTurn {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    items.push(opening_user_item(turn_id, user_prompt));
    items.extend(completed_items.iter().cloned());
    crate::thread_store::PartialTurn {
        turn_id: turn_id.to_string(),
        items,
        messages,
        usage,
    }
}

/// 尽力写一次 checkpoint：失败只记 stderr，绝不打断 turn。
fn checkpoint(
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    partial: crate::thread_store::PartialTurn,
) {
    if let Err(e) = store.write_partial(thread_id, &partial) {
        eprintln!("[app-server] failed to write turn checkpoint ({thread_id}): {e}");
    }
}

/// 一个 turn 结束后（或被 clear / compact 改动后）的收尾：把当前 session 快照
/// 落盘、置 Idle，并在 `turn_id` 为 `Some` 时上报 Finished。
///
/// clear / compact 之后必须调用它：`/clear` 要落一条空快照（否则 resume 回放的是
/// 旧日志），`/compact` 要把压缩结果写回 `.jsonl`。此时**没有 turn 在收尾**，
/// 传 `turn_id = None`：不发 `TurnEvent::Finished`，也不 touch meta。
#[allow(clippy::too_many_arguments)]
async fn persist_and_finish_turn(
    thread_id: &str,
    turn_id: Option<&str>,
    user_prompt: Option<&str>,
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    if let Some(prompt) = user_prompt {
        // 基线 server 不 emit userMessage，必须在落盘时补齐，否则 resume 会丢用户提问。
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{}", turn_id.unwrap_or("session-command")),
            text: prompt.to_string(),
        });
    }
    items.extend(completed_items);

    let record = crate::thread_store::TurnLine::Turn {
        items,
        usage: last_usage,
        messages: agent.session().messages().to_vec(),
    };
    // append 失败则跳过 touch：避免出现"幽灵" thread。
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {thread_id}: {e}");
    } else if let Some(prompt) = user_prompt {
        if let Err(e) = store.touch(thread_id, Some(prompt)) {
            eprintln!("[app-server] failed to update meta for {thread_id}: {e}");
        }
    }

    let _ = update_status(hub, status, thread_id, ThreadStatus::Idle).await;
    if let Some(turn_id) = turn_id {
        let _ = turn_tx.send(finished_event(thread_id, turn_id)).await;
    }
}

/// 在**没有 turn 在跑**的时刻执行一条会话命令，返回（可能被重建过的）agent。
///
/// 清空与压缩都**就地**替换会话内容(`set_session_messages`),不换 `Arc`:同一
/// 会话句柄可能已被委派工具的 `CallerContext` 绑定,换句柄会让 `fork` 读到被丢弃
/// 的旧记录。按值传入/返回保留原样——将来若某个命令确实需要换掉 `Agent` 本体,
/// 这条签名就不必再改。
#[allow(clippy::too_many_arguments)]
async fn apply_session_command(
    mut agent: yi_agent_core::Agent,
    command: SessionCommand,
    provider: &Arc<dyn yi_agent_core::Provider>,
    config: &yi_agent_core::AgentConfig,
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) -> yi_agent_core::Agent {
    match command {
        SessionCommand::Clear { reply } => {
            agent.set_session_messages(Vec::new());
            // `set_session_messages` 只换消息,不碰 `last_input_tokens`;若不在此清零,
            // 清空后仍留着一个陈旧的(可能很大的)计数,`maybe_auto_compact` 会在下一轮
            // 立刻误触发一次压缩。旧实现换的是全新 `Session`(计数为 `None`),这里补回
            // 那个语义——与 CLI 的 `/clear` 路径一致。
            agent
                .session_handle()
                .lock()
                .unwrap()
                .set_last_input_tokens(None);
            let truncate = store.truncate(thread_id);
            if let Err(e) = &truncate {
                eprintln!("[app-server] failed to truncate thread log {thread_id}: {e}");
            }
            // 不是 turn 收尾：turn_id 传 None（不发 Finished），也不 touch meta。
            persist_and_finish_turn(
                thread_id,
                None,
                None,
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                status,
            )
            .await;
            // `truncate` 返回 io::Result<bool>，而 reply 通道是 Result<(), String>。
            let _ = reply.send(truncate.map(|_| ()).map_err(|e| e.to_string()));
        }
        SessionCommand::Compact { reply } => {
            let session = agent.session();
            let outcome = match yi_agent_core::compact_session(provider, config, &session).await {
                Ok(Some(compacted)) => {
                    // Replace the contents in place so the session `Arc` — and
                    // any `CallerContext` bound to it — stays valid.
                    agent.set_session_messages(compacted.messages().to_vec());
                    // The replaced session used to be brand new, so its token
                    // count meant "no measurement yet". Keep that: a stale
                    // pre-compaction count would re-trigger auto-compaction on
                    // the very next turn.
                    agent
                        .session_handle()
                        .lock()
                        .unwrap()
                        .set_last_input_tokens(None);
                    CompactOutcome::Compacted
                }
                Ok(None) => CompactOutcome::NotReduced,
                Err(e) => CompactOutcome::Failed(e.to_string()),
            };
            persist_and_finish_turn(
                thread_id,
                None,
                None,
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                status,
            )
            .await;
            let _ = reply.send(outcome);
        }
    }
    agent
}

/// 单个 thread 的 driver task:串行消费 turn,驱动 `agent.run()` 的 stream,
/// 经 `Translator` 写成协议通知。
///
/// **取消安全**:`Agent::run()` 每次都会重置 cancel token,因此必须在
/// `run().await` 返回**之后**再取 `cancel_token()`,否则 `turn/interrupt` 无效。
///
/// **权限审批闭环**:遇到 `AgentEvent::PermissionRequest` 时,driver 发出反向
/// 请求 `item/toolCall/requestApproval`(id 取自进程级 `perm_seq`)并等待客户端
/// 经 `pending` 登记表回传的决定;超时或中断按 `Deny` 处理。
#[allow(clippy::too_many_arguments)]
async fn run_thread_driver(
    thread_id: String,
    mut agent: yi_agent_core::Agent,
    mut prompt_rx: mpsc::Receiver<TurnPrompt>,
    mut interrupt_rx: mpsc::Receiver<String>,
    mut interject_rx: mpsc::Receiver<InterjectionRequest>,
    mut session_rx: mpsc::Receiver<SessionCommand>,
    hub: Arc<crate::broadcast::Broadcaster>,
    // 发起本 thread 的客户端(取自某次 thread/start)。反向审批请求**广播**给
    // 所有客户端(决策 5:谁先答谁生效),故这里不再需要它来寻址;保留它是为了
    // 复刻「发起方断开即收尾本轮」的原判据(见下方 `is_connected` 守卫)。
    client: crate::broadcast::ClientId,
    turn_tx: mpsc::Sender<TurnEvent>,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: Arc<AtomicU64>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    store: Arc<crate::thread_store::ThreadStore>,
    // 该 thread 的共享状态句柄;driver 在各转换点更新并推送
    // `thread/status/updated`(与写进 `ThreadSession.status` 的是同一 `Arc`)。
    status: Arc<std::sync::Mutex<ThreadStatus>>,
    // clear / compact 需要它们：compact 要调 provider 生成摘要，两者都要重建 agent。
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
) {
    // 每个 thread 一个 translator:item id 带 turn_id 前缀(`item-<turn_id>-<n>`),
    // 故即便 resume 后计数器归 1,新 item 也不会与回放的历史 id 冲突。
    let mut translator = Translator::new(thread_id.clone());
    // 只在首个 turn 尝试激活:objective 会被写进 root 任务,不能用占位串,也不能
    // 每轮重写。即使 `activate` 为 None 也置位,避免每轮重试。
    let mut activation_attempted = false;
    // 内层 select 期间收到的会话命令。此刻 turn 正在跑，不能改 agent，
    // 暂存到这里，等本轮收尾时执行（见 persist_and_finish_turn 之后的处理）。
    let mut pending_session_command: Option<SessionCommand> = None;
    loop {
        // 空闲路径:没有 turn 在跑时收到的会话命令,立即执行——此刻改 agent 是安全的。
        // 没有这条路径,空闲 thread 永远读不到 clear / compact(命令会一直躺在 channel 里)。
        //
        // 只在 `select!` 里做 `recv`,把执行放到 `select!` 之外:`apply_session_command`
        // 按值消费并返回 agent,若写进分支体就会与另一分支的 `prompt_rx` 借用冲突。
        let turn_prompt = tokio::select! {
            Some(command) = session_rx.recv() => {
                agent = apply_session_command(
                    agent, command, &provider, &config, &store, &thread_id, &hub, &turn_tx,
                    &status,
                )
                .await;
                continue;
            }
            maybe_prompt = prompt_rx.recv() => maybe_prompt,
        };
        let Some(TurnPrompt {
            turn_id,
            prompt,
            activate,
        }) = turn_prompt
        else {
            break; // prompt_rx 关闭:driver 收尾退出
        };
        if !activation_attempted {
            activation_attempted = true;
            if let Some(binding) = activate {
                let objective = prompt.clone();
                // 同步调用放到阻塞线程池:driver 是 async 任务,直接调用会占住 executor。
                // `activate` 先探活、必要时重建 runtime,所以即便 daemon 在这轮之前
                // 已经死掉(例如承载它的终端被关掉),这里也能自愈后再激活。
                let outcome =
                    tokio::task::spawn_blocking(move || binding.activate(&objective)).await;
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(cause)) => tracing::warn!(
                        stage = "activation",
                        %cause,
                        %thread_id,
                        "subagent delegation unavailable for this thread"
                    ),
                    Err(error) => tracing::warn!(
                        stage = "activation",
                        %error,
                        %thread_id,
                        "subagent delegation unavailable for this thread"
                    ),
                }
            }
        }
        // 本轮累加器:最终 item 与最近一次用量(用于落盘)。
        // `last_usage` 记录的是本轮**最后一次** provider 调用的完整快照
        // (input 来自 `message_start`、output 来自 `message_delta`,由 Translator 合并)。
        let mut completed_items: Vec<crate::protocol::Item> = Vec::new();
        let mut last_usage: Option<crate::thread_store::TurnUsage> = None;
        let user_prompt = prompt.clone();
        translator.set_turn(turn_id.clone());

        // turn 开始就把提问落进 checkpoint：即使这一轮随后立刻崩溃，
        // 至少提问不会丢。
        checkpoint(
            &store,
            &thread_id,
            build_partial(
                &turn_id,
                &user_prompt,
                &[],
                agent.session().messages().to_vec(),
                None,
            ),
        );

        // 每轮开跑前刷新 skills catalog:skills 热重载,让本轮看到最新的
        // system prompt(catalog 未变时输出逐字节相同,不破坏 prompt cache)。
        if let Some(handle) = &catalog {
            if let Some(new_prompt) = handle.current_system_prompt() {
                agent.set_system_prompt(Some(new_prompt));
            }
        }

        let mut stream = match agent.run(prompt).await {
            Ok(s) => s,
            Err(e) => {
                // run() 本身失败:翻译成 Error → turn/completed(failed)。
                for n in translator.on_event(yi_agent_core::AgentEvent::Error(e)) {
                    let _ = write_notification(&hub, &n).await;
                }
                let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                // run() 失败即本轮结束:thread 立刻回 Idle(失败是事件不是状态)。
                let _ = update_status(&hub, &status, &thread_id, ThreadStatus::Idle).await;
                // 本轮已结束（失败）：turn 开始写的 checkpoint 必须清掉，否则盘上会
                // 留一条已结束 turn 的残留，重启后被 resume 当成待恢复 turn。
                // 与其它 clear 站点同款：尽力删，失败只记 stderr，绝不影响 turn 收尾。
                if let Err(e) = store.clear_partial(&thread_id) {
                    eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                }
                continue;
            }
        };

        // 必须在 run() 之后捕获:run() 内部会重建 cancel token。
        let cancel_token = agent.cancel_token();
        // 同一次 run 的投递句柄;run() 结束即失效,下一轮重新取。
        let inbox = agent.inbox_handle();
        let mut cancel_sent = false;
        // 逐字流合并器（仅在本轮驱动内有效）。当存在订阅者时，`item/delta`
        // 先按 thread 攒批，再由 100ms tick 或非 delta 屏障刷出；无订阅者时
        // 走原来的直发路径，字节不变。
        let mut coalescer = DeltaCoalescer::default();
        let mut delta_tick = tokio::time::interval(Duration::from_millis(100));
        delta_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // checkpoint 去抖：item finalize 只标脏，由 500ms tick 合并成一次写盘，
        // 避免长 turn 里每个 item 都同步写一次。
        let mut checkpoint_tick = tokio::time::interval(Duration::from_millis(500));
        checkpoint_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut checkpoint_dirty = false;

        loop {
            tokio::select! {
                ev = stream.next() => {
                    match ev {
                        Some(yi_agent_core::AgentEvent::PermissionRequest {
                            request_id, tool_name, tool_input, prefix_suggestion, kind,
                        }) => {
                            let perm_id = format!("perm-{}", perm_seq.fetch_add(1, Ordering::SeqCst));
                            let (dtx, mut drx) = oneshot::channel::<Decision>();
                            pending.lock().await.insert(perm_id.clone(), dtx);

                            let reverse = ReverseRequest {
                                jsonrpc: JSONRPC_VERSION,
                                id: perm_id.clone(),
                                method: "item/toolCall/requestApproval",
                                params: json!({
                                    "thread_id": thread_id,
                                    "turn_id": turn_id,
                                    "request_id": request_id,
                                    "tool_name": tool_name,
                                    "tool_input": tool_input,
                                    "prefix_suggestion": prefix_suggestion,
                                    "kind": kind,
                                }),
                            };
                            let frame = match serde_json::to_value(&reverse) {
                                Ok(frame) => frame,
                                Err(e) => {
                                    tracing::error!("failed to serialize reverse request: {e}");
                                    pending.lock().await.remove(&perm_id);
                                    let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                    // 本轮就此结束：turn-start 写的 checkpoint 必须清掉，
                                    // 否则盘上留一条已结束 turn 的残留，重启后 resume 会
                                    // 把最后这一轮误标为 interrupted。与其它 clear 站点同款：
                                    // 尽力删，失败只记 stderr，绝不影响收尾。
                                    if let Err(e) = store.clear_partial(&thread_id) {
                                        eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                                    }
                                    return;
                                }
                            };
                            // 反向审批请求**广播**给所有客户端(spec §4.4;Tier 1
                            // 决策 5:审批先到先得)。发起 turn 的是共享的
                            // app-server,等待决定的设备可能不止一台——每台都要
                            // 看到弹窗,谁先答谁生效,其余设备靠随后的
                            // `item/toolCall/approvalResolved` 广播关掉弹窗。
                            //
                            // 断连语义:`broadcast` 是 `try_send`,不会像旧的
                            // `reply` 那样在被寻址客户端已注销时返回 `Closed`。
                            // 若发起客户端已断开,本轮不该继续等一个永远不会到的
                            // 决定,故这里用 `is_connected` 复刻原判据——但仍要
                            // 先把帧广播出去,让其它在线客户端(如第二台手机)能
                            // 应答:发起方断线不等于该审批无人可答。
                            // 只推给"订阅了该 thread"的客户端 + 全收的客户端（桌面）：
                            // 没打开这个会话的手机不该被它的审批打断。
                            hub.broadcast_for(Some(&thread_id), frame);
                            if !hub.is_connected(&client) {
                                // 发起客户端已断开:与旧语义一致(写失败即收尾)。
                                pending.lock().await.remove(&perm_id);
                                let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                // 本轮就此结束（客户端断连，不是崩溃）：清掉 checkpoint，
                                // 否则重启后会把这一轮误报为 interrupted。与其它站点同款。
                                if let Err(e) = store.clear_partial(&thread_id) {
                                    eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                                }
                                return;
                            }
                            // 反向请求已写出:进入等待审批状态。
                            let _ = update_status(
                                &hub,
                                &status,
                                &thread_id,
                                ThreadStatus::AwaitingApproval,
                            )
                            .await;

                            // 等客户端决定;超时或中断按 Deny 处理。只接受针对当前 turn 的中断,
                            // 其它 turn 的残留信号忽略后继续等待(deadline 不重置)。
                            let timeout = tokio::time::sleep(permission_timeout);
                            tokio::pin!(timeout);
                            let decision = loop {
                                tokio::select! {
                                    d = &mut drx => break d.unwrap_or(Decision::Deny),
                                    _ = &mut timeout => {
                                        pending.lock().await.remove(&perm_id);
                                        break Decision::Deny;
                                    }
                                    Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                                        if target == turn_id {
                                            cancel_sent = true;
                                            cancel_token.cancel();
                                            pending.lock().await.remove(&perm_id);
                                            break Decision::Deny;
                                        }
                                        // 其它 turn 的残留信号:忽略,继续等待。
                                    }
                                }
                            };

                            // 决定已到(或超时/中断按 Deny):恢复为 Running,
                            // 继续消费本轮 stream。
                            let _ = update_status(&hub, &status, &thread_id, ThreadStatus::Running)
                                .await;

                            if let Some(tx) = &decision_tx {
                                let _ = tx.send((request_id, decision)).await;
                            }
                        }
                        Some(e) => {
                            for n in translator.on_event(e) {
                                if let crate::protocol::Notification::ItemCompleted { item, .. } = &n {
                                    completed_items.push(item.clone());
                                    checkpoint_dirty = true;
                                }
                                // 用量通知携带本轮累积快照(见 Translator::on_event),最后一条即落盘用的完整用量。
                                if let crate::protocol::Notification::TokenUsage {
                                    model,
                                    input_tokens,
                                    output_tokens,
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                    ..
                                } = &n
                                {
                                    last_usage = Some(crate::thread_store::TurnUsage {
                                        model: model.clone(),
                                        input_tokens: *input_tokens,
                                        output_tokens: *output_tokens,
                                        cache_creation_input_tokens: *cache_creation_input_tokens,
                                        cache_read_input_tokens: *cache_read_input_tokens,
                                    });
                                }
                                // 逐字流：仅当存在订阅者时合并；否则走原来的直发。
                                // 非 delta 通知前先刷该 thread 的待发，保证顺序。
                                let mut to_send: Vec<Notification> = Vec::new();
                                match &n {
                                    Notification::ItemDelta { thread_id: t, item_id, delta }
                                        if hub.has_subscribed_clients() =>
                                    {
                                        for (pid, text) in coalescer.push(t, item_id, delta) {
                                            to_send.push(Notification::ItemDelta {
                                                thread_id: t.clone(),
                                                item_id: pid,
                                                delta: text,
                                            });
                                        }
                                    }
                                    _ => {
                                        if let Notification::ItemDelta { thread_id: t, .. } = &n {
                                            if let Some((pid, text)) = coalescer.take(t) {
                                                to_send.push(Notification::ItemDelta {
                                                    thread_id: t.clone(),
                                                    item_id: pid,
                                                    delta: text,
                                                });
                                            }
                                        }
                                        to_send.push(n.clone());
                                    }
                                }
                                for out in to_send {
                                    if write_notification(&hub, &out).await.is_err() {
                                        // 客户端可能已断开;先上报 Finished,
                                        // 避免 active_turn_id 永久卡住。
                                        let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                        // 本轮就此结束（写失败，不是崩溃）：清掉 checkpoint，
                                        // 否则重启后会把这一轮误报为 interrupted。与其它站点同款。
                                        if let Err(e) = store.clear_partial(&thread_id) {
                                            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                        None => {
                            // 流结束：把该 thread 攒下的逐字流刷出，避免丢尾
                            // （最后一个 tick 之后、流结束之前可能仍有待发 delta）。
                            if let Some((pid, text)) = coalescer.take(&thread_id) {
                                let _ = write_notification(
                                    &hub,
                                    &Notification::ItemDelta {
                                        thread_id: thread_id.clone(),
                                        item_id: pid,
                                        delta: text,
                                    },
                                )
                                .await;
                            }
                            break;
                        }
                    }
                }
                _ = delta_tick.tick() => {
                    // 100ms 兜底刷出：即便没有后续非 delta 通知，攒下的逐字流
                    // 也必须按时送达。
                    for (t, (pid, text)) in coalescer.take_all() {
                        let _ = write_notification(
                            &hub,
                            &Notification::ItemDelta {
                                thread_id: t,
                                item_id: pid,
                                delta: text,
                            },
                        )
                        .await;
                    }
                }
                _ = checkpoint_tick.tick(), if checkpoint_dirty => {
                    checkpoint(
                        &store,
                        &thread_id,
                        build_partial(
                            &turn_id,
                            &user_prompt,
                            &completed_items,
                            agent.session().messages().to_vec(),
                            last_usage.clone(),
                        ),
                    );
                    checkpoint_dirty = false;
                }
                Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                    // 只接受针对当前 turn 的中断;忽略上一轮残留的信号。
                    if target == turn_id {
                        cancel_sent = true;
                        cancel_token.cancel();
                    }
                    // 继续消费 stream,直到 run loop 发 Cancelled 并结束。
                }
                Some(request) = interject_rx.recv() => {
                    // 只并入这条请求所指的 turn;上一轮的残留请求直接还回客户端,
                    // 否则它会落进错误的上下文。
                    let matches_turn = request.turn_id == turn_id;
                    let accepted = matches_turn
                        && inbox
                            .as_ref()
                            .is_some_and(|handle| {
                                handle
                                    .interject(request.text.clone(), Some(request.interjection_id.clone()))
                                    .is_ok()
                            });
                    if !accepted {
                        // 未能并入(非当前 turn / 无句柄 / inbox 满):把文本还回去,
                        // 让客户端能恢复输入而不是静默丢弃。
                        for n in translator.on_event(
                            yi_agent_core::AgentEvent::InterjectionsReturned {
                                items: vec![yi_agent_core::Interjection {
                                    seq: 0,
                                    text: request.text,
                                    tag: Some(request.interjection_id),
                                }],
                            },
                        ) {
                            let _ = write_notification(&hub, &n).await;
                        }
                    }
                }
                Some(command) = session_rx.recv() => {
                    // 本轮已有一个待执行命令时保留先到的那个,后到的直接拒绝,
                    // 避免两端各自 await 一个永远不会有回应的 reply。
                    if pending_session_command.is_some() {
                        match command {
                            SessionCommand::Clear { reply } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::Compact { reply } => {
                                let _ = reply.send(CompactOutcome::Failed(
                                    "另有一个会话命令待执行".into(),
                                ));
                            }
                        }
                    } else {
                        pending_session_command = Some(command);
                    }
                }
            }
        }

        // 先执行本轮暂存的会话命令（此刻 turn 已结束，改 agent 是安全的），
        // 再为刚结束的 turn 收尾。
        if let Some(command) = pending_session_command.take() {
            agent = apply_session_command(
                agent, command, &provider, &config, &store, &thread_id, &hub, &turn_tx, &status,
            )
            .await;
            // 命令路径已 append 过最终状态;这里只需为这个 turn 发 Finished 让主循环清
            // active_turn_id,且**不要**再 append 一次(否则会用 turn 前的 session 覆盖)。
            let _ = update_status(&hub, &status, &thread_id, ThreadStatus::Idle).await;
            let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
            // 这条路径不走常规收尾；本轮 checkpoint 同样必须清掉，否则会在盘上
            // 留一条已结束 turn 的残留，重启后被 resume 当成待恢复 turn。
            if let Err(e) = store.clear_partial(&thread_id) {
                eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
            }
            continue;
        }

        // 无暂存命令：常规收尾（把本轮结果落盘）。
        persist_and_finish_turn(
            &thread_id,
            Some(&turn_id),
            Some(&user_prompt),
            &agent,
            std::mem::take(&mut completed_items),
            last_usage.take(),
            &store,
            &hub,
            &turn_tx,
            &status,
        )
        .await;

        // 收尾已把整轮 append 进主 jsonl；checkpoint 的使命结束。
        if let Err(e) = store.clear_partial(&thread_id) {
            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
        }
    }
}

/// 在全局索引的目录里定位 `thread_id` 所属目录。
fn find_thread_dir(workspaces: &WorkspaceIndex, thread_id: &str) -> Option<PathBuf> {
    workspaces
        .list()
        .into_iter()
        .map(PathBuf::from)
        .find(|d| crate::thread_store::ThreadStore::new(d).exists(thread_id))
}

/// 按 thread 定位其 store;索引找不到时回退 `cfg.workdir`。
fn store_for(
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    let dir = find_thread_dir(workspaces, thread_id).unwrap_or_else(|| cfg.workdir.clone());
    Arc::new(crate::thread_store::ThreadStore::new(&dir))
}

/// 取 thread 的 store:内存中的 thread 用其权威实例(与 driver 共享 lock);
/// 冷 thread 按全局索引定位,索引找不到再回退 `cfg.workdir`。
fn store_lookup(
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

/// 一条 `Item` 的稳定 id（用于 `thread/readItems` 的 `afterItemId` 切片与前端去重）。
pub(crate) fn item_id(item: &crate::protocol::Item) -> Option<&str> {
    match item {
        crate::protocol::Item::UserMessage { id, .. }
        | crate::protocol::Item::AgentMessage { id, .. }
        | crate::protocol::Item::ToolCall { id, .. }
        | crate::protocol::Item::UserInterjection { id, .. } => Some(id),
    }
}

/// 看板调度器的一轮:对每个已登记项目跑一次 `run_once`(启动 + 对账)。
///
/// **内联在主循环里**(3s interval tick),不与任何 RPC 分支并发——`threads`
/// 等仍是主循环的局部量,`&mut` 借用按顺序发生(见 Task 6c brief 的设计约束:
/// 不让调度器另起任务并发访问 `threads`)。
///
/// 每个项目的 thread 状态快照(`flags`)在**循环内**采集:一轮 tick 里若刚起了
/// 一个会话,下一张卡的快照才看得到它,且闭包持有的是 owned `HashMap`,不与
/// launcher 需要的 `&mut threads` 冲突。
///
/// 单个项目的失败(daemon 掉线、坏看板)只记日志:一台项目的 daemon 不可用
/// 不得影响其它项目,更不得影响 RPC。
#[allow(clippy::too_many_arguments)]
async fn card_scheduler_tick<F>(
    board_dir: &Path,
    tracked: &mut HashMap<String, TrackedThread>,
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &mut HashMap<String, ProcessWatch>,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    cfg: &RuntimeConfig,
    hub: &Arc<crate::broadcast::Broadcaster>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    workspaces: &WorkspaceIndex,
    build_agent: &F,
) where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    let projects = match yi_agent_boards::registry::list(board_dir) {
        Ok(projects) => projects,
        Err(error) => {
            eprintln!("[app-server] could not list boards for the card scheduler: {error}");
            return;
        }
    };
    for board in projects {
        // 先快照 flags(owned `HashMap`):闭包因此不再借用 `threads`,与下面
        // launcher 的 `&mut threads` 不冲突(brief Step 2 的借用手法)。
        let snapshot: HashMap<String, ThreadFlags> = threads
            .iter()
            .map(|(thread_id, session)| {
                let idle = matches!(
                    *session.status.lock().unwrap_or_else(|p| p.into_inner()),
                    ThreadStatus::Idle
                ) && session.active_turn_id.is_none();
                (
                    thread_id.clone(),
                    ThreadFlags {
                        idle,
                        // failed / needs_you 目前无生产来源(见报告「已知限制」):
                        // `TurnEvent::Finished` 不带终态,故不在此处伪造判定。
                        failed: false,
                        needs_you: false,
                    },
                )
            })
            .collect();
        let flags = move |thread_id: &str| snapshot.get(thread_id).copied();
        let mut launcher = ServeLauncher {
            threads,
            pending_activation,
            process_watches,
            runtimes,
            thread_roots,
            cfg,
            hub,
            turn_tx,
            pending,
            permission_timeout,
            perm_seq,
            theme,
            workspaces,
            build_agent,
        };
        crate::card_scheduler::run_once(&board.project, board_dir, tracked, &mut launcher, &flags)
            .await;
    }
}

/// 调度器起会话的真实实现:复用 6a/6b 的无帧内核(`start_thread_core` +
/// `prepare_turn_core`),**不**调用会向客户端发帧的 `start_turn_core`。
///
/// 它借用主循环的局部量(`threads` / `pending_activation` / `process_watches`)
/// 直接登记会话,故调度器无需另起一个并发访问它们的任务(见 Task 6c brief)。
///
/// `build_agent` 是 `serve` 的工厂参数(按引用借入,不要求 `'static`,也不要求
/// launcher 自己拥有它)——与 `thread/start` 分支用同一个工厂,因此看板会话与
/// 手工会话的 agent 构造完全同源。
struct ServeLauncher<'a, F> {
    threads: &'a mut HashMap<String, ThreadSession>,
    pending_activation: &'a mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &'a mut HashMap<String, ProcessWatch>,
    runtimes: &'a ProjectRuntimes,
    thread_roots: &'a ThreadRoots,
    cfg: &'a RuntimeConfig,
    hub: &'a Arc<crate::broadcast::Broadcaster>,
    turn_tx: &'a mpsc::Sender<TurnEvent>,
    pending: &'a Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &'a Arc<AtomicU64>,
    theme: &'a crate::theme_tool::ThemeHandle,
    workspaces: &'a WorkspaceIndex,
    build_agent: &'a F,
}

impl<F> CardLauncher for ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    async fn launch(&mut self, request: &LaunchRequest) -> anyhow::Result<String> {
        self.launch_inner(request).await.inspect_err(|error| {
            // 调度器把失败翻成 `board.release`,而插件只记终态、不留 detail;
            // 不在这里说清原因,一张卡为什么没起来就没有任何线索。
            eprintln!(
                "[app-server] could not launch a session for board card {}: {error}",
                request.card_id
            );
        })
    }
}

impl<F> ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    async fn launch_inner(&mut self, request: &LaunchRequest) -> anyhow::Result<String> {
        // 会话与 RPC 起的 thread 完全同型:同一内核、同一 driver、同一侧栏视角。
        // 唯一的不同是 mode(看板会话跑 Yolo)与「不发任何 RPC 帧」。
        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());
        let built = (self.build_agent)(
            None,
            Path::new(&request.workdir),
            crate::thread_store::ThreadMode::Yolo,
        )?;

        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.cfg,
            &request.workdir,
            crate::thread_store::ThreadMode::Yolo,
            thread_id.clone(),
            built,
            &hub,
            &client,
            self.turn_tx,
            self.pending,
            self.permission_timeout,
            self.perm_seq,
            self.theme,
            self.workspaces,
            Some(&request.board_project),
            Some(&request.card_id),
        )
        .await?;

        // 侧栏显示的名片:插件给的 `看板 · <spec stem>`。不设的话,这个会话在
        // 首轮落盘前标题为空、用户只看到一串 thread id——「可见会话」就不成立了。
        // 失败只记日志:标题是展示层信息,不该让一张看板卡起不来。
        if !request.title.is_empty() {
            if let Some(session) = self.threads.get(&thread_id) {
                if let Err(error) = session.store.rename(&thread_id, &request.title) {
                    eprintln!("[app-server] could not title board thread {thread_id}: {error}");
                }
            }
        }

        // 无帧地占好首个 turn,再自行投递 prompt——`start_turn_core` 会发
        // `turn/started`/状态/响应,调度器一个都不该发。
        let params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": request.objective }],
        });
        let prepared = match prepare_turn_core(self.threads, self.pending_activation, &params).await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                // 准备失败:把占位清掉,否则该 thread 的 `active_turn_id` 永久卡住。
                if let Some(session) = self.threads.get_mut(&thread_id) {
                    session.active_turn_id = None;
                }
                // 把具体原因带上:调度器只把它写进 `board.release` 的 detail,
                // 「准备失败」四个字不足以定位(是 thread 不在?还是 turn 撞车?)。
                return Err(anyhow::anyhow!(
                    "could not prepare the first turn for {}: {}",
                    request.card_id,
                    turn_prepare_rpc_error(error).message
                ));
            }
        };
        // 状态走 `thread/status/updated`(通知),不是向某个 client 的响应,故可发:
        // 侧栏据此看到该会话正在跑。用 prepared 的句柄,保证与 `active_turn_id`
        // 的占位来自同一个 session。
        let _ = update_status(
            &hub,
            &prepared.status_handle,
            &prepared.thread_id,
            ThreadStatus::Running,
        )
        .await;
        // 照 $18 发本轮的开启项(用户提问=卡片 objective),再投递 prompt。
        // 这不是「向客户端发 RPC 帧」——它是 thread-keyed 的 content 通知,与
        // RPC 客户端收到的同一条流;少了它,卡片会话点开后 objective 会像修复前
        // 那样被迟到追到最底部(见 `opening_user_item`)。
        emit_item(
            &hub,
            &prepared.thread_id,
            opening_user_item(&prepared.turn_id, &prepared.prompt),
        )
        .await?;
        if prepared
            .prompt_tx
            .send(TurnPrompt {
                turn_id: prepared.turn_id,
                prompt: prepared.prompt,
                activate: prepared.activate,
            })
            .await
            .is_err()
        {
            return Err(anyhow::anyhow!(
                "thread driver for {} is gone before its first prompt",
                prepared.thread_id
            ));
        }
        Ok(thread_id)
    }
}

/// 启动恢复:插件此刻报 `running`、但本进程并未跟踪的卡片,一律回写
/// `needs_you`。
///
/// 重启后内存里的 `threads`/`tracked` 都空了,而 board.json 里仍有 `running`
/// 卡片:它们既不会被 `next_launch`(只认 `queued`)再次启动,也不会被 `plan`
/// 对账(没有 thread 可查),会永久占着槽位。本任务**不做真实 resume**
/// (那要走完整的 thread/resume 机制,复杂度单独评估),按 plan 的兜底约定把它
/// 置为 `needs_you`,让用户决定下一步。
///
/// 只有卡片**没有 thread_id** 才在这里兜底:有 thread_id 的卡片是「本进程稍后
/// 会接手对账的活会话」或「需要用户决定的会话」,启动瞬间无法区分二者,故留给
/// 主循环的第一步 tick(对账路径)处理,免得把活会话误判成孤儿。
fn recover_orphan_cards(board_dir: &Path) {
    let projects = match yi_agent_boards::registry::list(board_dir) {
        Ok(projects) => projects,
        Err(error) => {
            eprintln!("[app-server] could not list boards for orphan recovery: {error}");
            return;
        }
    };
    for board in projects {
        // 项目 daemon 没起来就跳过:此刻没有 `running` 卡片可言,而本函数的目的
        // 只是清理「重启后残留的 running」。绝不在这里顺手拉起 daemon——启动
        // 项目 daemon 是 `board/create` 的职责。
        if !yi_agent_boards::board_daemon::is_running(&board.project) {
            continue;
        }
        let cards = match board_cards(&board.project, board_dir) {
            Ok(cards) => cards,
            Err(_) => continue,
        };
        for card in cards {
            if card.state != "running" || card.thread_id.is_some() {
                continue;
            }
            eprintln!(
                "[app-server] board card {} is running without a thread_id and this process \
                 cannot resume it; marking it needs_you",
                card.id
            );
            let _ = board_query(
                &board.project,
                board_dir,
                "board.mark_terminal",
                json!({ "card_id": card.id, "outcome": "needs_you" }),
            );
        }
    }
}

/// 起一个 thread 的内核:attach_delegation → 建 driver 通道 → 登记进 `threads`
/// → spawn 进程守望者与 driver task。
///
/// `built` 由调用方先经 `build_agent` 造好(工厂在 `serve`/调度器里同步调用,不在
/// 本内核内),这样内核不必把 `&F` 跨 await 持有——否则会强加 `F: Sync` 并波及
/// `serve` 的 Send 边界。只负责「得到一个已登记、driver 已 spawn 的 thread_id」
/// 这件事;RPC 层的事(`resolve_thread_cwd` 的错误响应、`thread/started` 通知与
/// 最终响应)留在调用方,以便调度器(Task 6b)复用同一内核而不发 RPC 帧。
#[allow(clippy::too_many_arguments)]
async fn start_thread_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &mut HashMap<String, ProcessWatch>,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    cfg: &RuntimeConfig,
    cwd: &str,
    mode: crate::thread_store::ThreadMode,
    thread_id: String,
    built: BuiltAgent,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    workspaces: &WorkspaceIndex,
    board_project: Option<&str>,
    card_id: Option<&str>,
) -> anyhow::Result<()> {
    let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(cwd)));

    // 委派是可选能力:项目 runtime 起不来就保留原 agent,只记 trace。
    // 接线刻意放在这里(而非 `threads` 守卫之内),避免与其可变借用冲突。
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(cwd));
    let activation = attach_delegation(
        runtimes,
        thread_roots,
        &runtime_dir,
        cfg,
        cwd,
        &thread_id,
        theme,
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
        ..
    } = activation.built;
    pending_activation.insert(thread_id.clone(), activation.runtime);

    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);

    let model = cfg.model.clone();

    let now = crate::thread_store::now_millis();
    let meta = crate::thread_store::ThreadMeta {
        thread_id: thread_id.clone(),
        cwd: cwd.to_string(),
        model: model.clone(),
        created_at: now,
        updated_at: now,
        title: None,
        permission_mode: mode,
        pin_seq: None,
        board_project: board_project.map(str::to_string),
        card_id: card_id.map(str::to_string),
    };
    if let Err(e) = thread_store.create(&meta) {
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
    threads.insert(
        thread_id.clone(),
        ThreadSession {
            thread_id: thread_id.clone(),
            cwd: cwd.to_string(),
            model: model.clone(),
            active_turn_id: None,
            yolo,
            process_manager: Arc::clone(&process_manager),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store: Arc::clone(&thread_store),
            status: Arc::clone(&store_status),
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
    ));

    Ok(())
}

/// `prepare_turn_core` 的失败。每一种都对应 `turn/start` 的一条固定错误响应,
/// 由 `start_turn_core` 负责把它写出去(这样「校验」与「写响应」解耦,调度器可以
/// 只复用前者而不向任何客户端广播)。
enum TurnPrepareError {
    MissingThreadId,
    EmptyInput,
    UnknownThread(String),
    TurnInProgress(String),
}

/// 一个已经「占位」好的 turn,但**尚未向任何客户端发帧**。
///
/// 校验通过后 `active_turn_id` 已被占成 `turn_id`;`prompt_tx` 是投递通道,
/// `status_handle` 供调用方推 Running 状态。字段全部 `pub(crate)`,以便
/// `card_scheduler` 复用这条无帧路径。
pub(crate) struct PreparedTurn {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) prompt_tx: mpsc::Sender<TurnPrompt>,
    pub(crate) prompt: String,
    pub(crate) activate: Option<Arc<ThreadRoot>>,
    pub(crate) status_handle: Arc<std::sync::Mutex<ThreadStatus>>,
}

/// 起一个 turn 的**无帧准备段**:校验 thread 存在、`active_turn_id` 空闲、
/// 提取 prompt、占 `active_turn_id = Some(turn_id)`、构造 `TurnPrompt`。
///
/// **不写任何通知 / 响应**——正因如此调度器才能「起会话而不向客户端发帧」
/// (见 Task 6a 评审:调度器不得复用带帧的 `start_turn_core`)。失败返回
/// `Err(TurnPrepareError)`,由调用方决定怎么写错误响应(或对调度器而言记日志)。
async fn prepare_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    params: &serde_json::Value,
) -> Result<PreparedTurn, TurnPrepareError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or(TurnPrepareError::MissingThreadId)?
        .to_string();
    let prompt = extract_prompt(params).ok_or(TurnPrepareError::EmptyInput)?;

    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    // 内层作用域:让 `&mut threads` 的借用先结束。
    let (prompt_tx, status_handle) = {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        session.active_turn_id = Some(turn_id.clone());
        (session.prompt_tx.clone(), Arc::clone(&session.status))
    };

    let activate = pending_activation.get(&thread_id).cloned().flatten();
    Ok(PreparedTurn {
        thread_id,
        turn_id,
        prompt_tx,
        prompt,
        activate,
        status_handle,
    })
}

/// 把 `prepare_turn_core` 的失败翻成 6a 建立的错误响应(逐字不变)。
fn turn_prepare_rpc_error(error: TurnPrepareError) -> RpcError {
    match error {
        TurnPrepareError::MissingThreadId => RpcError::invalid_params("missing threadId"),
        TurnPrepareError::EmptyInput => RpcError::invalid_params("missing or empty input text"),
        TurnPrepareError::UnknownThread(thread_id) => RpcError::unknown_thread(&thread_id),
        TurnPrepareError::TurnInProgress(thread_id) => RpcError::turn_in_progress(&thread_id),
    }
}

/// The item that opens a turn: the user's own message.
///
/// The baseline server emitted this **only at persist time**
/// (`persist_and_finish_turn`), so a client watching a turn live saw the agent's
/// items stream in first and the prompt appended last — a viewer opening the
/// thread later found the opening prompt at the very bottom of the transcript.
/// Emitting it as the turn's *first* item, from the one helper both the RPC path
/// (`start_turn_core`) and the board path (`ServeLauncher`) call, fixes the order
/// at the source and keeps the id identical to the persisted one (`user-<turn_id>`),
/// so the desktop merges the live bubble with the replayed one by id.
fn opening_user_item(turn_id: &str, text: &str) -> crate::protocol::Item {
    crate::protocol::Item::UserMessage {
        id: format!("user-{turn_id}"),
        text: text.to_string(),
    }
}

/// Emit one item as both `item/started` and `item/completed`.
///
/// Content-layer notifications carry no response frame, so both the RPC path and
/// the board launcher may (and must, to stay identical) call this. Failures are
/// propagated: a broken hub is a real fault, not something to swallow.
async fn emit_item(
    hub: &Arc<crate::broadcast::Broadcaster>,
    thread_id: &str,
    item: crate::protocol::Item,
) -> anyhow::Result<()> {
    write_notification(
        hub,
        &Notification::ItemStarted {
            thread_id: thread_id.to_string(),
            item: item.clone(),
        },
    )
    .await?;
    write_notification(
        hub,
        &Notification::ItemCompleted {
            thread_id: thread_id.to_string(),
            item,
        },
    )
    .await
}

/// 起一个 turn 的 RPC 包装:调 `prepare_turn_core` → 发 `turn/started` →
/// 推 Running 状态 → 写响应 → 投递 prompt。
///
/// 校验失败时在此就地写错误响应并返回 `Ok(None)`,调用方据此 continue——帧的
/// 顺序与错误响应语义与 6a 抽取前逐字一致(响应急在 prompt 投递之前)。
/// 成功返回 `Ok(Some(thread_id))`。
async fn start_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    let prepared = match prepare_turn_core(threads, pending_activation, params).await {
        Ok(prepared) => prepared,
        Err(error) => {
            write_response(hub, client, err_response(id, turn_prepare_rpc_error(error))).await?;
            return Ok(None);
        }
    };

    // 顺序确定:先 turn/started 通知,再发本轮开启项(用户提问),再推 Running
    // 状态,再响应,最后投递 prompt。开启项排在 agent 任何 item 之前,实时观看与
    // 事后回放的顺序才一致(见 `opening_user_item`)。
    write_notification(
        hub,
        &Notification::TurnStarted {
            thread_id: prepared.thread_id.clone(),
            turn_id: prepared.turn_id.clone(),
        },
    )
    .await?;
    emit_item(
        hub,
        &prepared.thread_id,
        opening_user_item(&prepared.turn_id, &prepared.prompt),
    )
    .await?;
    update_status(
        hub,
        &prepared.status_handle,
        &prepared.thread_id,
        ThreadStatus::Running,
    )
    .await?;
    write_response(
        hub,
        client,
        ok_response(id, json!({ "turn_id": prepared.turn_id.clone() })),
    )
    .await?;

    if prepared
        .prompt_tx
        .send(TurnPrompt {
            turn_id: prepared.turn_id,
            prompt: prepared.prompt,
            activate: prepared.activate,
        })
        .await
        .is_err()
    {
        // driver 已退出(理论上不会):清掉活跃标记,
        // 避免后续 turn 永远报 turn_in_progress。
        if let Some(s) = threads.get_mut(&prepared.thread_id) {
            s.active_turn_id = None;
        }
    }
    Ok(Some(prepared.thread_id))
}

/// 解析 `thread/start` 的目标目录:显式 `params.cwd` 优先,缺省用 `cfg.workdir`。
///
/// 显式 `cwd` canonicalize + 校验是目录;失败写 `-32602` 并返回 `Ok(None)`
/// (调用方 continue)。缺省时沿用 `cfg.workdir` 原样,不 canonicalize:保持旧的
/// 单目录行为,避免 macOS `/var` → `/private/var` 之类改写破坏既有路径语义。
async fn resolve_thread_cwd(
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
async fn require_thread_id(
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

/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let input = params.get("input")?.as_array()?;
    let mut text = String::new();
    for block in input {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
            text.push_str(t);
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The `board/*` RPCs, driven through the real stdio loop.
///
/// They are side-effecting commands (a temp dir on disk) rather than pure
/// functions, so the seam under test is the dispatch itself: request in, frame
/// out. The harness points the registry at a temp dir, so these never touch the
/// developer's real `~/.yi-agent`.
#[cfg(test)]
mod board_rpc_tests {
    use super::*;

    async fn rpc(
        h: &mut tests::Harness,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        h.send(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        )
        .await;
        loop {
            let value = h.read_value().await;
            if value.get("id") == Some(&json!(id)) {
                return value;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_create_then_remove_lists_the_project_between_the_two() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = tempfile::TempDir::new().unwrap();
        let project_path = project
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let created = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project_path }),
        )
        .await;
        assert!(created.get("error").is_none(), "{created}");
        assert_eq!(created["result"]["registered"], true, "{created}");

        let listed = rpc(&mut h, 3, "board/list", json!({})).await;
        let boards = listed["result"]["boards"].as_array().unwrap();
        assert_eq!(boards.len(), 1, "登记表必须恰好一条:{listed}");
        assert_eq!(boards[0]["project"], project_path);
        assert_eq!(boards[0]["status"]["registered"], true, "{listed}");

        let removed = rpc(
            &mut h,
            4,
            "board/remove",
            json!({ "project": project_path }),
        )
        .await;
        assert!(removed.get("error").is_none(), "{removed}");

        let listed = rpc(&mut h, 5, "board/list", json!({})).await;
        assert_eq!(
            listed["result"]["boards"].as_array().unwrap().len(),
            0,
            "移除后不得再出现在登记表:{listed}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_board_created_through_the_rpc_is_written_to_the_injected_registry() {
        // 侧栏按登记表决定给哪些项目画看板条目,所以「写进注入的那个目录」
        // 本身就是契约:写错地方等于点创建没反应。
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = tempfile::TempDir::new().unwrap();
        let project_path = project.path().canonicalize().unwrap();

        rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project_path.to_string_lossy() }),
        )
        .await;

        let boards = yi_agent_boards::registry::list(h.board_dir.path()).unwrap();
        assert_eq!(boards.len(), 1, "登记表必须落在注入的目录里");
        assert_eq!(boards[0].project, project_path);
        h.shutdown().await;
    }

    /// 开关为开（缺省）时，首次 `board/create` 确保值守已装：装的是当前 exe、
    /// 针对注入的 home。
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_a_board_ensures_the_watchman_is_installed() {
        // 自带 workdir，避免与其它测试共享的默认目录里的偏好互相干扰。
        let workdir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests::default_config();
        cfg.workdir = workdir.path().to_path_buf();
        let mut h = tests::Harness::with_cfg(cfg).await;
        tests::initialize(&mut h).await;
        let calls = Arc::clone(&h.watchman_calls);
        let home = h._watchman_home.path().to_path_buf();
        let project = tempfile::TempDir::new().unwrap();

        let created = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project.path().canonicalize().unwrap().to_string_lossy() }),
        )
        .await;
        assert!(created.get("error").is_none(), "{created}");

        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "开关为开时创建看板要装一次值守: {recorded:?}"
        );
        assert!(recorded[0].0, "是安装而不是卸载");
        assert_eq!(recorded[0].1, home, "必须针对注入的 home");
        h.shutdown().await;
    }

    /// 开关为关时 `board/create` 不得装值守：用户明确关掉了开机自启。
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_a_board_leaves_the_watchman_alone_when_the_switch_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests::default_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = tests::Harness::with_cfg(cfg).await;
        tests::initialize(&mut h).await;
        let calls = Arc::clone(&h.watchman_calls);

        let off = rpc(
            &mut h,
            2,
            "ui/settings/write",
            json!({ "board_watchman_enabled": false }),
        )
        .await;
        assert_eq!(off["result"]["ok"], true, "{off}");

        let project = tempfile::TempDir::new().unwrap();
        let created = rpc(
            &mut h,
            3,
            "board/create",
            json!({ "project": project.path().canonicalize().unwrap().to_string_lossy() }),
        )
        .await;
        assert!(created.get("error").is_none(), "{created}");

        let recorded = calls.lock().unwrap().clone();
        assert!(
            recorded.iter().all(|(installed, _)| !installed),
            "关掉后只应有卸载，不应有安装: {recorded:?}"
        );
        h.shutdown().await;
    }

    /// 值守安装失败不得让 `board/create` 失败：看板本身已经建好。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_watchman_install_does_not_fail_board_creation() {
        let workdir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests::default_config();
        cfg.workdir = workdir.path().to_path_buf();
        let mut h = tests::Harness::with_watchman(
            cfg,
            Arc::new(|_exe: &Path, _home: &Path| Err("launchctl is unavailable".to_string())),
            Arc::new(|_home: &Path| Ok(())),
        )
        .await;
        tests::initialize(&mut h).await;
        let project = tempfile::TempDir::new().unwrap();

        let created = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project.path().canonicalize().unwrap().to_string_lossy() }),
        )
        .await;
        assert!(
            created.get("error").is_none(),
            "装值守失败不该拖垮创建:{created}"
        );
        assert_eq!(created["result"]["registered"], true, "{created}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_list_of_an_empty_registry_answers_with_no_boards() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let listed = rpc(&mut h, 2, "board/list", json!({})).await;
        assert_eq!(listed["result"]["boards"], json!([]));
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_create_without_a_project_is_invalid_params() {
        // `project` 是必填参数。空串会被当成当前目录，凭空给调用方开一块
        // 谁都认不出的看板；缺参数必须是 invalid_params，与其它 RPC 一致。
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let response = rpc(&mut h, 2, "board/create", json!({})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(
            yi_agent_boards::registry::list(h.board_dir.path())
                .unwrap()
                .is_empty(),
            "缺参数的请求不得登记任何东西"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_remove_without_a_project_is_invalid_params() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let response = rpc(&mut h, 2, "board/remove", json!({})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_create_for_a_missing_directory_reports_an_error_and_registers_nothing() {
        // 侧栏按登记表画条目，所以「拒绝」必须同时意味着「没登记」：否则
        // 点一次创建会多出一条点开什么都没有的看板。
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        let response = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": missing.to_string_lossy() }),
        )
        .await;
        assert!(response.get("error").is_some(), "{response}");
        assert!(
            yi_agent_boards::registry::list(h.board_dir.path())
                .unwrap()
                .is_empty(),
            "被拒绝的创建不得登记"
        );
        h.shutdown().await;
    }
}

#[cfg(test)]
mod plugin_query_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    /// A fake daemon bound to `dir`'s runtime socket that records the request it
    /// received and replies with one frame carrying `result`, so the test can
    /// prove the host forwards the plugin name, method and params verbatim
    /// without interpreting any of them.
    fn fake_daemon(
        dir: &Path,
        result: serde_json::Value,
    ) -> (
        PathBuf,
        Arc<StdMutex<serde_json::Value>>,
        std::thread::JoinHandle<()>,
    ) {
        // The exact path `plugin_query` dials, via the same helper the
        // implementation uses: a fake bound anywhere else would let a
        // wrong-path implementation pass.
        let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(dir);
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
        let seen = Arc::new(StdMutex::new(serde_json::Value::Null));
        let captured = Arc::clone(&seen);
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let request: serde_json::Value =
                    serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                *captured.lock().unwrap() = request.clone();
                // Echo the request id: the client rejects a response for another
                // request, so a fake daemon that hardcodes one would look broken.
                let reply = json!({
                    "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                    "request_id": request["request_id"],
                    "result": result,
                });
                let mut stream = stream;
                let _ = stream.write_all(reply.to_string().as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        (socket, seen, handle)
    }

    /// A project directory plus the registry it is (or is not) registered in.
    fn project_and_registry(dir: &Path, register: bool) -> (PathBuf, PathBuf) {
        let project = dir.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.join("global");
        if register {
            yi_agent_boards::registry::register(&global, &project).unwrap();
        }
        (project, global)
    }

    #[test]
    fn a_query_reaches_the_daemon_with_the_plugin_and_method_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), true);
        let (_socket, seen, handle) = fake_daemon(
            &project,
            json!({ "type": "PluginResult", "value": { "cards": [] } }),
        );

        plugin_query(
            &project,
            &global,
            "list",
            "superpowers-kanban",
            json!({ "verbose": true }),
        )
        .unwrap();
        handle.join().unwrap();

        let request = seen.lock().unwrap().clone();
        assert_eq!(request["command"]["type"], "PluginQuery");
        assert_eq!(request["command"]["plugin"], "superpowers-kanban");
        assert_eq!(request["command"]["method"], "list");
        assert_eq!(request["command"]["params"]["verbose"], true);
    }

    /// `board_query` is the board-shaped alias for `plugin_query`: it has to
    /// name the kanban plugin while passing `method`/`params` through
    /// untouched, or every board call would ask the wrong plugin a question it
    /// never asked for.
    #[test]
    fn board_query_targets_the_kanban_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), true);
        let (_socket, seen, handle) = fake_daemon(
            &project,
            json!({ "type": "PluginResult", "value": { "cards": [] } }),
        );

        board_query(&project, &global, "list", json!({})).unwrap();
        handle.join().unwrap();

        let request = seen.lock().unwrap().clone();
        assert_eq!(request["command"]["type"], "PluginQuery");
        assert_eq!(request["command"]["plugin"], "superpowers-kanban");
        assert_eq!(request["command"]["method"], "list");
    }

    #[test]
    fn a_missing_plugin_name_is_refused_before_touching_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), true);
        let error = plugin_query(&project, &global, "list", "", json!({})).unwrap_err();
        assert!(error.message.contains("plugin"), "{error}");
        assert_eq!(
            error.code, "invalid_params",
            "空 plugin 名与「没有看板」是两回事:{error}"
        );
    }

    #[test]
    fn an_unreachable_daemon_reports_that_the_plugin_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), true);
        let error =
            plugin_query(&project, &global, "list", "superpowers-kanban", json!({})).unwrap_err();
        assert!(error.message.contains("daemon is unavailable"), "{error}");
        assert_eq!(error.code, "daemon_unavailable", "{error}");
    }

    #[test]
    fn a_query_without_a_registered_board_is_refused_with_its_own_code() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), false);

        let error =
            plugin_query(&project, &global, "list", "superpowers-kanban", json!({})).unwrap_err();
        assert_eq!(error.code, "board_not_created", "{error}");
    }

    #[test]
    fn a_registered_board_without_a_daemon_reports_daemon_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), true);

        let error =
            plugin_query(&project, &global, "list", "superpowers-kanban", json!({})).unwrap_err();
        assert_eq!(
            error.code, "daemon_unavailable",
            "不能让 UI 误以为是「插件没装」:{error}"
        );
    }

    /// The daemon owns the plugin table, so a plugin it does not supervise is
    /// its answer to give. The wording has to stay matchable by the desktop's
    /// `pluginIsUnavailable`, which looks for `is not available`.
    #[test]
    fn a_daemon_that_does_not_supervise_the_plugin_reports_plugin_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (project, global) = project_and_registry(dir.path(), true);
        let (_socket, _seen, handle) = fake_daemon(
            &project,
            json!({
                "type": "Error",
                "code": "not_found",
                "message": "plugin superpowers-kanban is not available",
            }),
        );

        let error =
            plugin_query(&project, &global, "list", "superpowers-kanban", json!({})).unwrap_err();
        handle.join().unwrap();

        assert_eq!(error.code, "plugin_unavailable", "{error}");
        assert!(error.message.contains("is not available"), "{error}");
        assert!(
            error.message.contains("plugin"),
            "既有措辞形状必须保留:{error}"
        );
    }

    /// The dispatch mapping the UI actually reads. The numeric code is the
    /// coarse fallback; `data.code` is the stable string, so both have to reach
    /// the wire together.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_rpc_maps_a_missing_board_to_its_own_code_in_data() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = h.board_dir.path().join("unregistered");
        std::fs::create_dir_all(&project).unwrap();

        h.send(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "plugin/query",
                "params": {
                    "project": project.to_string_lossy(),
                    "plugin": "superpowers-kanban",
                    "method": "list",
                    "params": {},
                },
            })
            .to_string(),
        )
        .await;

        let value = loop {
            let value = h.read_value().await;
            if value.get("id") == Some(&json!(2)) {
                break value;
            }
        };
        assert_eq!(
            value["error"]["data"]["code"], "board_not_created",
            "{value}"
        );
        assert_eq!(value["error"]["code"], -32020, "{value}");
        h.shutdown().await;
    }
}

/// 看板调度器接进 `serve` 之后的集成测试:真 `ServeLauncher` 起**可见**会话、
/// 调度器对账终态、启动孤儿恢复。
///
/// 夹具尽量走**真实路径**:真 socket(按请求转发到绑定的假看板 daemon)、真
/// `ThreadStore`、真 `run_once`、真 launcher 起的会话。假的只有模型与「插件怎么
/// 回答看板问题」。
#[cfg(test)]
mod card_scheduling_tests {
    use super::*;
    use crate::server::tests::{build_test_agent, test_config};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::card_scheduler::{CardLauncher, LaunchRequest};

    /// 一台听在项目 runtime socket 上的假 daemon。
    ///
    /// 它做生产 daemon 对 `PluginQuery` 做的那一件事——把查询转给绑定
    /// (`on_query`)的看板回答——并把每次插件调用记录在案,供断言。其余请求
    /// (`Status` 探活、daemon attach 等)统一回 `internal`:这既是诚实的「我不是
    /// 真 daemon」,也让 `attach_cwd_runtime` 按生产里的降级路径失败,而不必起一
    /// 个真 daemon。
    struct FakeDaemon {
        socket: PathBuf,
        calls: Arc<StdMutex<Vec<serde_json::Value>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
        _dir: tempfile::TempDir,
        project: PathBuf,
        #[allow(dead_code)]
        runtime_dir: PathBuf,
    }

    impl FakeDaemon {
        fn bind(
            on_query: impl Fn(&Path, &str, &serde_json::Value) -> Result<serde_json::Value, String>
            + Send
            + 'static,
        ) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&project);
            std::fs::create_dir_all(&runtime_dir).unwrap();
            let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            let query_project = project.clone();

            let calls: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let request: serde_json::Value =
                        serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                    let command = &request["command"];
                    let method = command["method"].as_str().unwrap_or_default().to_string();
                    let value = if command["type"] == "PluginQuery" {
                        let plugin = command["plugin"].as_str().unwrap_or_default();
                        let params = command["params"].clone();
                        recorded.lock().unwrap().push(json!({
                            "method": method,
                            "params": params.clone(),
                        }));
                        if plugin == "superpowers-kanban" {
                            match on_query(&query_project, &method, &params) {
                                Ok(result) => json!({ "type": "PluginResult", "value": result }),
                                // 被拒的查询在宿主侧呈现为传输错误(与「daemon 没有
                                // 该插件的路由」同型)——正是 `run_once` 退出本轮
                                // 启动循环的条件。
                                Err(_) => {
                                    json!({ "type": "Error", "code": "internal", "message": "refused" })
                                }
                            }
                        } else {
                            json!({ "type": "Error", "code": "internal", "message": "unrouted plugin" })
                        }
                    } else if command["type"] == "Status" {
                        // `is_running` 靠这个探活。诚实回答它,否则恢复路径会把
                        // 「我没有真 daemon」误读成「项目没有看板」。
                        json!({ "type": "Status", "high_water_event_id": 0 })
                    } else {
                        json!({ "type": "Error", "code": "internal", "message": "not a real daemon" })
                    };
                    let reply = json!({
                        "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                        "request_id": request["request_id"],
                        "result": value,
                    });
                    let mut stream = stream;
                    let _ = stream.write_all(reply.to_string().as_bytes());
                    let _ = stream.write_all(b"\n");
                }
            });
            Self {
                socket,
                calls,
                stop,
                handle: Some(handle),
                _dir: dir,
                project,
                runtime_dir,
            }
        }

        fn calls(&self) -> Vec<serde_json::Value> {
            self.calls.lock().unwrap().clone()
        }

        fn calls_to(&self, method: &str) -> Vec<serde_json::Value> {
            self.calls()
                .into_iter()
                .filter(|call| call["method"] == method)
                .collect()
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = std::os::unix::net::UnixStream::connect(&self.socket);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// `ServeLauncher` 需要的全部主循环局部量,一次性备齐。
    struct Host {
        threads: HashMap<String, ThreadSession>,
        pending_activation: HashMap<String, Option<Arc<ThreadRoot>>>,
        process_watches: HashMap<String, ProcessWatch>,
        runtimes: ProjectRuntimes,
        thread_roots: ThreadRoots,
        cfg: RuntimeConfig,
        hub: Arc<crate::broadcast::Broadcaster>,
        turn_tx: mpsc::Sender<TurnEvent>,
        turn_rx: mpsc::Receiver<TurnEvent>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
        perm_seq: Arc<AtomicU64>,
        theme: crate::theme_tool::ThemeHandle,
        workspaces: WorkspaceIndex,
        workdir: PathBuf,
        _workdir_dir: tempfile::TempDir,
        _theme_dir: tempfile::TempDir,
        _workspace_dir: tempfile::TempDir,
    }

    impl Host {
        fn new() -> Self {
            let workdir_dir = tempfile::TempDir::new().unwrap();
            let workdir = workdir_dir.path().to_path_buf();
            let theme_dir = tempfile::TempDir::new().unwrap();
            let workspace_dir = tempfile::TempDir::new().unwrap();
            let (turn_tx, turn_rx) = mpsc::channel::<TurnEvent>(8);
            Self {
                threads: HashMap::new(),
                pending_activation: HashMap::new(),
                process_watches: HashMap::new(),
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                cfg: test_config(),
                hub: Arc::new(crate::broadcast::Broadcaster::new()),
                turn_tx,
                turn_rx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                perm_seq: Arc::new(AtomicU64::new(1)),
                theme: crate::theme_tool::ThemeHandle::new(theme_dir.path().to_path_buf()),
                workspaces: WorkspaceIndex::new(workspace_dir.path().join("workspaces.json")),
                workdir,
                _workdir_dir: workdir_dir,
                _theme_dir: theme_dir,
                _workspace_dir: workspace_dir,
            }
        }

        fn launcher<'a, F>(&'a mut self, build_agent: &'a F) -> ServeLauncher<'a, F>
        where
            F: Fn(
                Option<yi_agent_core::Session>,
                &Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>,
        {
            ServeLauncher {
                threads: &mut self.threads,
                pending_activation: &mut self.pending_activation,
                process_watches: &mut self.process_watches,
                runtimes: &self.runtimes,
                thread_roots: &self.thread_roots,
                cfg: &self.cfg,
                hub: &self.hub,
                turn_tx: &self.turn_tx,
                pending: &self.pending,
                permission_timeout: PERMISSION_TIMEOUT,
                perm_seq: &self.perm_seq,
                theme: &self.theme,
                workspaces: &self.workspaces,
                build_agent,
            }
        }

        /// 像真主循环那样收 `TurnEvent::Finished`:清除该 thread 的
        /// `active_turn_id`,直到看到目标 thread 的完成事件。
        ///
        /// 不这样做,`active_turn_id` 永远不会被清——在 `serve` 里那是主循环的
        /// 活儿,这条测试没有主循环,必须自己复刻。
        async fn settle_turn(&mut self, thread_id: &str, timeout: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return self.is_idle(thread_id);
                }
                match tokio::time::timeout(remaining, self.turn_rx.recv()).await {
                    Ok(Some(TurnEvent::Finished {
                        thread_id: id,
                        turn_id,
                    })) => {
                        if let Some(session) = self.threads.get_mut(&id) {
                            if session.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                                session.active_turn_id = None;
                            }
                        }
                        if id == thread_id {
                            return true;
                        }
                    }
                    Ok(None) => return false,
                    Err(_) => return self.is_idle(thread_id),
                }
            }
        }

        /// 该 thread 此刻是否**空闲**(`Idle` 且无活跃 turn)——与调度器快照
        /// 用的是同一条判据。
        fn is_idle(&self, thread_id: &str) -> bool {
            self.threads.get(thread_id).is_some_and(|session| {
                matches!(
                    *session.status.lock().unwrap_or_else(|p| p.into_inner()),
                    ThreadStatus::Idle
                ) && session.active_turn_id.is_none()
            })
        }
    }

    fn request(card_id: &str, workdir: &str) -> LaunchRequest {
        LaunchRequest {
            card_id: card_id.to_string(),
            board_project: "/test/project".to_string(),
            workdir: workdir.to_string(),
            title: format!("看板 · {card_id}"),
            objective: format!("Implement the plan for {card_id}"),
        }
    }

    /// 并发等一个条件,最多 `timeout`;返回是否等到。
    async fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if check() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 核心行为:launcher 为一张卡起一个**登记在册、driver 活着、首轮已投递、
    /// 标题已落盘**的会话。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_launcher_starts_a_registered_session_with_a_title_and_a_first_turn() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let thread_id = {
            let mut launcher = host.launcher(&build_test_agent);
            launcher
                .launch(&request("card-1", &workdir.to_string_lossy()))
                .await
                .expect("the launch must succeed for a healthy factory")
        };

        assert_eq!(host.threads.len(), 1, "the board thread is registered");
        let session = host
            .threads
            .get(&thread_id)
            .expect("the session is visible");
        assert_eq!(session.cwd, workdir.to_string_lossy());
        assert!(
            session.active_turn_id.is_some(),
            "the first turn must be占位, otherwise the card looks idle instantly"
        );
        assert_eq!(
            session
                .store
                .load(&thread_id)
                .unwrap()
                .and_then(|loaded| loaded.meta.title),
            Some("看板 · card-1".to_string()),
            "the sidebar needs a name, not a raw thread id"
        );

        // driver 真的收了首轮并收尾:主循环会收到 `Finished`,清除占位。
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the driver must consume the prompt and finish"
        );
        assert!(host.pending_activation.contains_key(&thread_id));
    }

    /// build 失败时绝不谎报:没有 thread 被登记。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_build_leaves_no_session_behind() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let build_failure = |_s: Option<yi_agent_core::Session>,
                             _cwd: &Path,
                             _m: crate::thread_store::ThreadMode| {
            Err::<BuiltAgent, _>(anyhow::anyhow!("no model for you"))
        };
        {
            let mut launcher = host.launcher(&build_failure);
            let error = launcher
                .launch(&request("card-1", &workdir.to_string_lossy()))
                .await
                .expect_err("a factory failure must surface, not be swallowed");
            assert!(error.to_string().contains("no model"), "{error}");
        }
        assert!(host.threads.is_empty(), "a failed launch registers nothing");
    }

    /// 看板调度器端到端:一张 `next_launch` 交出的卡被起成可见会话 → 插件收到
    /// `board.mark_running(thread_id)`;会话收尾后,宿主把它对账成
    /// `board.mark_terminal(awaiting_merge)` 并从跟踪表移除。
    ///
    /// 走**真 `run_once` + 真 `ServeLauncher` + 真 socket 转发**,是 6c
    /// 「接线」的最小完整闭环。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_claimed_card_becomes_a_visible_session_then_awaits_merge() {
        let handed_out = Arc::new(AtomicBool::new(false));
        let thread_id = Arc::new(StdMutex::new(None::<String>));
        let seen = Arc::clone(&thread_id);
        let handed = Arc::clone(&handed_out);
        let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
            "board.next_launch" => {
                if handed.swap(true, Ordering::SeqCst) {
                    Ok(serde_json::Value::Null)
                } else {
                    Ok(json!({ "card_id": "card-1", "title": "看板 · card-1" }))
                }
            }
            "list" => Ok(json!({ "cards": [{
                "id": "card-1",
                "state": "running",
                "thread_id": seen.lock().unwrap().clone(),
                "spec_path": "card-1.spec.md",
                "plan_path": "card-1.plan.md",
            }] })),
            _ => Ok(json!({ "ok": true })),
        });

        let mut host = Host::new();
        // 监听宿主 hub:卡片会话是 thread-keyed 的 content 通知,注册一个客户端
        // 就能看到调度器起会话时到底发了什么、按什么顺序。
        let mut frames = host
            .hub
            .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));
        let board_dir = tempfile::TempDir::new().unwrap();
        yi_agent_boards::registry::register(board_dir.path(), &daemon.project).unwrap();

        let flags = Arc::new(StdMutex::new(HashMap::<String, ThreadFlags>::new()));
        let mut tracked: HashMap<String, TrackedThread> = HashMap::new();

        // 第一轮:启动。
        {
            let snapshot = Arc::clone(&flags);
            let flags_fn = move |id: &str| snapshot.lock().unwrap().get(id).copied();
            let mut launcher = host.launcher(&build_test_agent);
            crate::card_scheduler::run_once(
                &daemon.project,
                board_dir.path(),
                &mut tracked,
                &mut launcher,
                &flags_fn,
            )
            .await;
        }

        let running = daemon.calls_to("board.mark_running");
        assert_eq!(
            running.len(),
            1,
            "the plugin must learn the card is running: {:?}",
            daemon.calls()
        );
        let launched = running[0]["params"]["thread_id"]
            .as_str()
            .unwrap()
            .to_string();
        *thread_id.lock().unwrap() = Some(launched.clone());
        assert_eq!(host.threads.len(), 1, "exactly one visible session");
        assert!(host.threads.contains_key(&launched));
        assert!(tracked.contains_key("card-1"), "the card is tracked");

        // 卡片会话对客户端「可见」的前提是 hub 里有它的 item;其中最关键的
        // 是**本轮开启项**(用户提问=卡片 objective)必须排在 agent 任何 item
        // 之前——修复前它只在落盘时补发,点开会话会在最底部才看到开场白。
        let mut seen: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        while let Ok(v) = frames.try_recv() {
            let method = v["method"].as_str().unwrap_or_default().to_string();
            let item = &v["params"]["item"];
            let itype = item["type"].as_str().map(str::to_string);
            let itext = item["text"].as_str().map(str::to_string);
            seen.push((method, itype, itext));
        }
        let first_content = seen
            .iter()
            .find(|(_, t, _)| t.is_some())
            .expect("the launch must emit at least one item frame");
        assert_eq!(
            first_content.1.as_deref(),
            Some("userMessage"),
            "the card session's first content item must be the opening objective, \
             not an agent item: {seen:?}"
        );
        assert_eq!(
            first_content.0, "item/started",
            "the opening item starts the transcript: {seen:?}"
        );

        // 等 driver 跑完首轮(主循环语义:收 Finished → 清占位)→ 下一轮对账
        // 应回写终态。
        assert!(
            host.settle_turn(&launched, Duration::from_secs(10)).await,
            "the board thread must finish its first turn"
        );
        {
            // 把真实快照灌进共享表(与主循环 tick 里 owned 快照同源)。
            let snapshot = Arc::clone(&flags);
            {
                let mut table = snapshot.lock().unwrap();
                for (id, session) in &host.threads {
                    let idle = matches!(
                        *session.status.lock().unwrap_or_else(|p| p.into_inner()),
                        ThreadStatus::Idle
                    ) && session.active_turn_id.is_none();
                    table.insert(
                        id.clone(),
                        ThreadFlags {
                            idle,
                            failed: false,
                            needs_you: false,
                        },
                    );
                }
            }
            let flags_fn = move |id: &str| snapshot.lock().unwrap().get(id).copied();
            let mut launcher = host.launcher(&build_test_agent);
            crate::card_scheduler::run_once(
                &daemon.project,
                board_dir.path(),
                &mut tracked,
                &mut launcher,
                &flags_fn,
            )
            .await;
        }

        let terminal = daemon.calls_to("board.mark_terminal");
        assert_eq!(
            terminal.len(),
            1,
            "an idle session must reconcile its card: {:?}",
            daemon.calls()
        );
        assert_eq!(terminal[0]["params"]["card_id"], "card-1");
        assert_eq!(
            terminal[0]["params"]["outcome"], "awaiting_merge",
            "failed/needs_you have no production source yet, so awaiting_merge is the only outcome"
        );
        assert!(
            tracked.is_empty(),
            "the reconciled card stops being tracked"
        );
    }

    /// 孤儿恢复:插件报 `running` 但**没有 thread_id**(本进程无法接手)的卡被
    /// 回写 `needs_you`;有 thread_id 的卡留给对账路径,不被误伤。
    #[tokio::test(flavor = "multi_thread")]
    async fn startup_recovery_marks_only_threadless_running_cards_needs_you() {
        let daemon = FakeDaemon::bind(|_project, method, _params| match method {
            "list" => Ok(json!({ "cards": [
                { "id": "orphan", "state": "running", "spec_path": "s", "plan_path": "p" },
                { "id": "live", "state": "running", "thread_id": "thread-x",
                  "spec_path": "s", "plan_path": "p" },
                { "id": "queued", "state": "queued", "spec_path": "s", "plan_path": "p" },
            ] })),
            _ => Ok(json!({ "ok": true })),
        });
        let board_dir = tempfile::TempDir::new().unwrap();
        yi_agent_boards::registry::register(board_dir.path(), &daemon.project).unwrap();

        recover_orphan_cards(board_dir.path());

        let terminal = daemon.calls_to("board.mark_terminal");
        assert_eq!(
            terminal.len(),
            1,
            "only the orphan is recovered: {:?}",
            daemon.calls()
        );
        assert_eq!(terminal[0]["params"]["card_id"], "orphan");
        assert_eq!(terminal[0]["params"]["outcome"], "needs_you");
    }

    #[test]
    fn board_cards_parses_a_merge_card_and_defaults_the_kind() {
        // 直接喂一个假 `list` 结果给解析逻辑（与现有 board_cards 测试同款做法）。
        let cards = serde_json::json!({ "cards": [
            { "id": "m1", "state": "queued", "kind": "merge", "source": "kanban/a", "base": "main" },
            { "id": "impl", "state": "queued", "spec_path": "i.spec.md", "plan_path": "i.plan.md" }
        ]});
        let parsed = parse_board_cards(&cards);
        assert_eq!(parsed[0].kind, "merge");
        assert_eq!(parsed[0].source.as_deref(), Some("kanban/a"));
        assert_eq!(parsed[1].kind, "implementation");
        assert_eq!(parsed[1].source, None);
    }

    /// 真主循环接线:启动 `serve` 后,3s tick(首个立即触发)把看板卡起成会话
    /// 并让插件看到 `board.mark_running`——证明调度器确实挂在主循环上。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_serve_loop_tick_launches_a_claimed_card() {
        let handed_out = Arc::new(AtomicBool::new(false));
        let handed = Arc::clone(&handed_out);
        let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
            "board.next_launch" => {
                if handed.swap(true, Ordering::SeqCst) {
                    Ok(serde_json::Value::Null)
                } else {
                    Ok(json!({ "card_id": "card-1", "title": "看板 · card-1" }))
                }
            }
            "list" => Ok(json!({ "cards": [{
                "id": "card-1", "state": "queued",
                "spec_path": "card-1.spec.md", "plan_path": "card-1.plan.md",
            }] })),
            _ => Ok(json!({ "ok": true })),
        });

        let mut h = crate::server::tests::Harness::new();
        yi_agent_boards::registry::register(h.board_dir.path(), &daemon.project).unwrap();
        crate::server::tests::initialize(&mut h).await;

        assert!(
            eventually(Duration::from_secs(15), || {
                !daemon.calls_to("board.mark_running").is_empty()
            })
            .await,
            "the serve tick must launch the claimed card: {:?}",
            daemon.calls()
        );
        let running = daemon.calls_to("board.mark_running");
        assert_eq!(running[0]["params"]["card_id"], "card-1");

        h.shutdown().await;
    }

    /// 一张卡被起成会话后：它落盘的 meta 与 `thread/listAll` 的条目都必须带
    /// `board_project`(项目根) 与 `card_id`——这是侧栏归组的唯一依据。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_launched_card_records_its_board_origin() {
        let workdir_dir = tempfile::TempDir::new().unwrap();
        let workdir = workdir_dir.path().canonicalize().unwrap();
        let handed = Arc::new(AtomicBool::new(false));
        let work_path = workdir.to_string_lossy().to_string();
        let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
            "board.next_launch" => {
                if handed.swap(true, Ordering::SeqCst) {
                    Ok(serde_json::Value::Null)
                } else {
                    Ok(json!({
                        "card_id": "card-1",
                        "workdir": work_path,
                        "title": "看板 · card-1"
                    }))
                }
            }
            "list" => Ok(json!({ "cards": [{
                "id": "card-1", "state": "running",
                "spec_path": "card-1.spec.md", "plan_path": "card-1.plan.md"
            }] })),
            _ => Ok(json!({ "ok": true })),
        });

        let mut host = Host::new();
        let board_dir = tempfile::TempDir::new().unwrap();
        yi_agent_boards::registry::register(board_dir.path(), &daemon.project).unwrap();
        let flags = Arc::new(StdMutex::new(HashMap::<String, ThreadFlags>::new()));
        let mut tracked: HashMap<String, TrackedThread> = HashMap::new();
        {
            let snapshot = Arc::clone(&flags);
            let flags_fn = move |id: &str| snapshot.lock().unwrap().get(id).copied();
            let mut launcher = host.launcher(&build_test_agent);
            crate::card_scheduler::run_once(
                &daemon.project,
                board_dir.path(),
                &mut tracked,
                &mut launcher,
                &flags_fn,
            )
            .await;
        }

        // meta 落盘：归属写进了卡片会话自己的 meta.json。
        let metas = crate::thread_store::ThreadStore::new(&workdir)
            .list()
            .unwrap();
        assert_eq!(metas.len(), 1, "the card session persists exactly one meta");
        let expected_project = std::fs::canonicalize(&daemon.project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            metas[0].board_project.as_deref(),
            Some(expected_project.as_str()),
            "meta must carry the project root"
        );
        assert_eq!(metas[0].card_id.as_deref(), Some("card-1"));
    }
}

/// 跨传输复用的测试夹具（`ws.rs` 的 E2E 与 `mod tests` 共用）。
///
/// 放在 `mod tests` 之外，因为 `#[cfg(test)]` 的 `mod tests` 对其它文件不可见；
/// 这里同样只在 test 构建下存在，避免生产构建出现 dead_code。
#[cfg(test)]
pub(crate) mod tests_support {
    use futures::{SinkExt, StreamExt};
    use serde_json::Value;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use yi_agent_runtime::config::RuntimeConfig;

    /// 对一条已连接的 ws 发 `initialize` 并读回响应。
    ///
    /// 跨传输复用(`ws.rs` 的多客户端 E2E 与若干准入测试都要先握手),故放在
    /// tests_support 里;`#[cfg(test)]` 的 `mod tests` 对其它文件不可见。
    pub(crate) async fn initialize<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        ws.send(ClientMessage::Text(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into(),
        ))
        .await
        .unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        assert_eq!(v["id"], 1, "initialize must respond with the request id");
    }

    /// 一份无凭据、无网络的测试配置；provider 只是占位字符串。
    pub(crate) fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".to_string(),
            api_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "test-model".to_string(),
            max_turns: 20,
            max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
            workdir: std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    /// 单轮逻辑是共享的 `watch::once`(Task 3 已在其自身 crate 内测过);这里只
    /// 断言 app-server 用的是同名同款:读登记 → 把登记项目交给 ensurer。
    #[test]
    fn the_app_side_loop_ensures_every_registered_project() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        yi_agent_store::resident::require(dir.path(), &a, "superpowers-kanban").unwrap();
        let mut seen: Vec<std::path::PathBuf> = Vec::new();
        let count = yi_agent_boards::watch::once(dir.path(), &mut |projects| {
            seen.extend_from_slice(projects)
        });
        assert_eq!(count, 1);
        assert_eq!(seen, vec![a]);
    }

    /// A throwaway Git repository. Coding children need one; attaching does not.
    fn init_git_repo(dir: &std::path::Path) {
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        }
    }

    /// The six tools a root agent needs to delegate.
    const DELEGATION_TOOLS: [&str; 6] = [
        "spawn_agent",
        "send_message",
        "wait_agent",
        "inspect_agent",
        "cancel_agent",
        "review_agent",
    ];

    /// A git project thread reaches the delegation tools.
    ///
    /// This is the desktop half of the TUI's
    /// `build_tui_root_tools_registers_subagent_tools_for_attached_runtime`: the
    /// app-server never had the wiring, so the model had no `spawn_agent`.
    #[test]
    fn a_git_project_gets_the_delegation_tools() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();

        let attached = Arc::new(
            yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime.path().to_path_buf())
                .expect("a clean git repo must attach"),
        );
        let binding =
            RuntimeBinding::managed(&cfg, runtime.path().to_path_buf(), Arc::clone(&attached));
        let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

        let tooling = build_runtime_tooling(
            &cfg,
            &root,
            "thread-test",
            yi_agent_core::autonomy::YoloSwitch::new(false),
            test_theme(),
        )
        .expect("tooling");
        let names = tooling.registry.names();

        for expected in DELEGATION_TOOLS {
            assert!(
                names.contains(&expected.to_string()),
                "an attached root must expose {expected}, got {names:?}"
            );
        }

        // Existing is not enough: the six tools have to work here. Drive the
        // same request `spawn_agent` sends, so the test fails the day a git
        // project stops admitting a delegated child.
        yi_agent_subagent::attach::activate_root(
            &attached.socket_path,
            &attached.attached_root,
            "investigate the build",
        )
        .expect("activation must succeed");
        let spawned = yi_agent_store::ipc::send_request(
            &attached.socket_path,
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: attached.attached_root.session_id.clone(),
                parent_task_id: attached.attached_root.task_id.clone(),
                capability: attached.attached_root.capability.clone(),
                objective: "a delegated read-only investigation".into(),
                mode: Some("read_only".into()),
                model: None,
                workdir: None,
                thread_id: None,
                sandbox: None,
                fork_token: None,
            },
        )
        .expect("the runtime socket must answer");
        let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id } = spawned else {
            panic!("a git project must admit a delegated child, got {spawned:?}");
        };
        assert!(!task_id.is_empty(), "the child must get an id to wait on");
    }

    /// One live controller backs both the root's builtin tools and the
    /// subagent spawn tools: flipping the thread's YOLO switch must move the
    /// root's `bash` sandbox, proving there is no second, static copy.
    #[test]
    fn delegation_tooling_uses_the_threads_live_yolo_switch() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();
        // The default test sandbox is workspace-write and promotable.
        assert!(cfg.sandbox_promotable);

        let attached =
            yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime.path().to_path_buf())
                .expect("a clean git repo must attach");

        let binding = yi_agent_subagent::binding::RuntimeBinding::fixed(
            yi_agent_subagent::binding::RuntimeHandle {
                socket_path: attached.socket_path.clone(),
                workspace_root: attached.workspace_root.clone(),
                session_id: attached.attached_root.session_id.clone(),
                task_id: attached.attached_root.task_id.clone(),
                capability: attached.attached_root.capability.clone(),
            },
        );
        let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
        let switch = yi_agent_core::autonomy::YoloSwitch::new(false);
        let tooling =
            build_runtime_tooling(&cfg, &root, "thread-test", switch.clone(), test_theme())
                .expect("tooling");
        let bash = tooling.registry.get("bash").expect("bash is registered");
        assert_eq!(
            bash.sandbox_mode(),
            Some("workspace-write"),
            "with YOLO off the root bash runs under workspace-write"
        );

        // Flipping the thread's live switch must escalate the already-built
        // registry: the controller is shared, not snapshotted.
        switch.set(true);
        assert_eq!(
            bash.sandbox_mode(),
            Some("danger-full-access"),
            "flipping the thread YOLO switch must escalate the root bash sandbox"
        );
    }

    /// A plain directory has no checkout to move into, so the root runs in
    /// place. That is the shared bring-up's decision (the TUI attaches a plain
    /// directory the same way), not an app-server rule, and the tools it
    /// registers are exactly as usable as the project is.
    #[test]
    fn a_non_git_cwd_attaches_in_place_with_the_delegation_tools() {
        let plain = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = plain.path().to_path_buf();

        let outcome =
            yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime.path().to_path_buf());

        let attached = Arc::new(outcome.expect("a plain directory attaches as an in-place root"));
        assert_eq!(
            attached.workspace_root, attached.project_root,
            "a non-git project has no checkout to move the root into"
        );
        let binding =
            RuntimeBinding::managed(&cfg, runtime.path().to_path_buf(), Arc::clone(&attached));
        let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
        let names = build_runtime_tooling(
            &cfg,
            &root,
            "thread-test",
            yi_agent_core::autonomy::YoloSwitch::new(false),
            test_theme(),
        )
        .expect("tooling")
        .registry
        .names();
        for expected in DELEGATION_TOOLS {
            assert!(
                names.contains(&expected.to_string()),
                "attach in place still yields a root, so {expected} must be present, got {names:?}"
            );
        }

        assert_eq!(
            attached.workspace_root, attached.project_root,
            "an in-place root is the project directory itself"
        );
        // Activation must still work: this is what the thread's first turn does,
        // and a plain directory is not itself a reason for that to fail.
        yi_agent_subagent::attach::activate_root(
            &attached.socket_path,
            &attached.attached_root,
            "investigate the build",
        )
        .expect("an in-place root must activate");

        // Delegating is where a plain directory stops: the daemon refuses the
        // worker because it cannot name a Git worktree lease to recover into.
        // Pin it here so the boundary is a documented fact rather than a
        // surprise in a chat window -- the desktop app's default cwd is $HOME.
        let spawned = yi_agent_store::ipc::send_request(
            &attached.socket_path,
            yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
                session_id: attached.attached_root.session_id.clone(),
                parent_task_id: attached.attached_root.task_id.clone(),
                capability: attached.attached_root.capability.clone(),
                objective: "a read-only investigation".into(),
                mode: Some("read_only".into()),
                model: None,
                workdir: None,
                thread_id: None,
                sandbox: None,
                fork_token: None,
            },
        )
        .expect("the runtime socket answers even when it refuses");
        assert!(
            !matches!(
                spawned,
                yi_agent_store::ipc::IpcResponse::TaskSpawned { .. }
            ),
            "delegation needs a checkpoint to recover into, so a plain directory \
             must be refused rather than half-admitted; got {spawned:?}"
        );
    }

    /// One directory must never key two ways depending on whether it exists yet.
    ///
    /// Attach runs while the project exists, but a lookup can be asked to key a
    /// path that is momentarily absent; if the two disagree the map misses and
    /// delegation goes silently dark. `/tmp` vs `/private/tmp` on macOS is the
    /// everyday case: canonicalizing resolved it only once the directory was
    /// there, so an absent path used to key as its literal spelling.
    #[test]
    fn a_project_directory_keys_the_same_before_and_after_it_exists() {
        // Reach the directory through a symlinked parent so its literal spelling
        // and its canonical path genuinely differ -- this is the shape of the
        // macOS `/tmp` -> `/private/tmp` divergence.
        let real = tempfile::TempDir::new().unwrap();
        let link_holder = tempfile::TempDir::new().unwrap();
        let link = link_holder.path().join("linked");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let child = link.join("project");

        let absent_key = project_key(&child);
        std::fs::create_dir_all(&child).unwrap();
        let present_key = project_key(&child);

        assert_eq!(
            absent_key, present_key,
            "keying an absent directory must match keying it once it exists"
        );
        assert_eq!(
            present_key,
            child.canonicalize().unwrap(),
            "an existing directory must key as its canonical path"
        );
    }

    /// A path whose ancestor is a symlink keys identically whether reached
    /// through the link or the target, so two spellings of one project share a
    /// runtime instead of attaching two.
    #[test]
    fn a_symlinked_project_keys_by_its_target() {
        let target = tempfile::TempDir::new().unwrap();
        let link_parent = tempfile::TempDir::new().unwrap();
        let link = link_parent.path().join("link");
        std::os::unix::fs::symlink(target.path(), &link).unwrap();

        assert_eq!(project_key(&link), project_key(target.path()));
    }

    /// Two threads in one cwd share the daemon but never the root: sharing the
    /// root is what made every conversation in a directory split one
    /// `MAX_DIRECT_CHILDREN` budget.
    ///
    /// It drives the real wiring (`attach_delegation`), not two hand-built
    /// `ThreadRoot`s: a test that builds its own roots would stay green even
    /// while the wiring still shared one.
    #[test]
    fn two_threads_in_one_cwd_share_the_daemon_but_not_the_root() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();
        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
        let cwd = cfg.workdir.to_string_lossy().to_string();

        let first = attach_delegation(
            &runtimes,
            &thread_roots,
            runtime.path(),
            &cfg,
            &cwd,
            "thread-a",
            &test_theme(),
            build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
        );
        let second = attach_delegation(
            &runtimes,
            &thread_roots,
            runtime.path(),
            &cfg,
            &cwd,
            "thread-b",
            &test_theme(),
            build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
        );

        assert_eq!(runtimes.lock().unwrap().len(), 1, "one daemon per project");
        let a = first
            .runtime
            .expect("thread a attached")
            .root_task_id()
            .expect("thread a root is attached");
        let b = second
            .runtime
            .expect("thread b attached")
            .root_task_id()
            .expect("thread b root is attached");
        assert_ne!(a, b, "two conversations must not share one root");
    }

    /// A runtime whose daemon died (its owning terminal was closed, say) must
    /// come back on the next call instead of being cached as a failure forever.
    ///
    /// The first half is the exact desktop failure: a daemon started by another
    /// process exits, and the socket it left behind answers nothing.
    #[test]
    fn a_dead_runtime_is_repaired_on_the_next_tool_call() {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let runtime = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();

        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        let binding =
            attach_cwd_runtime(&runtimes, runtime.path(), &cfg).expect("a git project must attach");
        let socket = binding.current().unwrap().socket_path.clone();

        // Simulate the owning process exiting: drop every handle to the runtime
        // (this binding and the cached one) so its embedded daemon is dropped too.
        drop(binding);
        runtimes.lock().unwrap().clear();
        assert_eq!(
            yi_agent_subagent::attach::probe_runtime(&socket),
            yi_agent_subagent::attach::RuntimeProbe::Dead,
            "the daemon must be gone before we test recovery"
        );

        // Asking again must replace the dead runtime rather than cache the miss.
        let repaired = attach_cwd_runtime(&runtimes, runtime.path(), &cfg)
            .expect("a dead runtime must be replaced, not cached as a failure");
        assert_eq!(
            yi_agent_subagent::attach::probe_runtime(&repaired.current().unwrap().socket_path),
            yi_agent_subagent::attach::RuntimeProbe::Healthy,
            "the repaired runtime must be reachable"
        );
    }

    use async_trait::async_trait;
    use futures::StreamExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// 极简 provider:只回一段文本后结束。
    struct MockProvider;

    #[async_trait]
    impl yi_agent_core::Provider for MockProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("hi".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 记录每次调用收到的 message 数,并回显 `n=<count>` 作为 agent 文本。
    struct RecordingProvider {
        seen: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl yi_agent_core::Provider for RecordingProvider {
        async fn call_stream(
            &self,
            req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let n = req.messages.len();
            self.seen.lock().unwrap().push(n);
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta(format!("n={n}")),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 永不自行结束的 provider:每 5ms 吐一个 delta,turn 会一直活跃,
    /// 直到被 `turn/interrupt` 取消。
    struct SlowProvider;

    #[async_trait]
    impl yi_agent_core::Provider for SlowProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = futures::stream::unfold(0u64, |i| async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((
                    yi_agent_core::provider::ProviderEvent::TextDelta("x".into()),
                    i + 1,
                ))
            });
            Ok(events.boxed())
        }
    }

    /// 每次调用都返回 provider 错误:用于测试 `/compact` 的 `failed` 三态。
    struct ErroringProvider;

    #[async_trait]
    impl yi_agent_core::Provider for ErroringProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            Err(yi_agent_core::provider::ProviderError::Network(
                "boom".into(),
            ))
        }
    }

    /// 首帧延迟 20ms 的 provider:保证 driver 的 interrupt 分支在 stream
    /// 首次 yield 之前已被轮询到(用于确定性地测试中断标记)。
    struct DelayedProvider;

    #[async_trait]
    impl yi_agent_core::Provider for DelayedProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                yi_agent_core::provider::ProviderEvent::TextDelta("hi".into())
            })
            .chain(futures::stream::once(async {
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                }
            }));
            Ok(events.boxed())
        }
    }

    /// 第一次调用产出一段助手文本 + 一个工具调用后正常结束（core 会进入 ACT、
    /// 向 driver 发 `ToolCall`，translator 借此把累积文本 finalize 成一个 item）；
    /// 之后的调用**永不产出**，于是 turn 一直停在"已 finalize 一块文本、但还没
    /// persist"的窗口——正是 checkpoint 该已落盘的窗口。
    struct CheckpointProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl yi_agent_core::Provider for CheckpointProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            use yi_agent_core::provider::ProviderEvent as E;
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // 工具入参必须是合法 JSON（`{}`），否则 accumulate 会因解析失败报错。
                let head = futures::stream::iter(vec![
                    E::TextDelta("a".into()),
                    E::ToolUseStart {
                        id: "t1".into(),
                        name: "noop".into(),
                    },
                    E::ToolUseDelta {
                        id: "t1".into(),
                        partial_json: "{}".into(),
                    },
                    E::ToolUseEnd { id: "t1".into() },
                    E::Stop {
                        reason: yi_agent_core::provider::StopReason::EndTurn,
                    },
                ]);
                Ok(head.boxed())
            } else {
                // 永不产出的尾部：保证 turn 不收尾（否则会被正常 persist）。
                Ok(futures::stream::pending::<E>().boxed())
            }
        }
    }

    /// 测试用的单客户端 hub:注册 `local` 并起 `pump_stdout`,与生产的 stdio 接线同款。
    fn test_hub<W>(
        writer: W,
    ) -> (
        Arc<crate::broadcast::Broadcaster>,
        crate::broadcast::ClientId,
    )
    where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let client = crate::broadcast::ClientId::local();
        // 与生产的 stdio 接线同款:可靠登记,背压不摘除(见 `serve_scoped`)。
        let outbound = hub.register_reliable(client.clone());
        tokio::spawn(pump_stdout(
            outbound,
            writer,
            Arc::clone(&hub),
            client.clone(),
        ));
        (hub, client)
    }

    pub(crate) fn build_test_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    fn build_slow_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(SlowProvider);
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    fn build_delayed_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(DelayedProvider);
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    fn build_checkpoint_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CheckpointProvider {
            calls: AtomicUsize::new(0),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    /// 测试用的主题句柄：不落盘的临时 workdir，也不可被生产路径读到。
    fn test_theme() -> crate::theme_tool::ThemeHandle {
        crate::theme_tool::ThemeHandle::new(std::env::temp_dir())
    }

    pub(crate) fn test_config() -> RuntimeConfig {
        super::tests_support::test_config()
    }

    /// 与 `test_config` 同值，供 `board_rpc_tests` 构造自定义 config 的 harness。
    pub(crate) fn default_config() -> RuntimeConfig {
        super::tests_support::test_config()
    }

    /// 用两条 `duplex` 管道把 server 与测试客户端对接。
    pub(crate) struct Harness {
        client_w: tokio::io::DuplexStream,
        client_r: BufReader<tokio::io::DuplexStream>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
        /// 隔离的全局目录索引;持有它保证 tempdir 存活到 harness 结束。
        _index_dir: tempfile::TempDir,
        /// 隔离的看板登记表目录,理由同上:board/* RPC 不得写真的 `~/.yi-agent`。
        pub(crate) board_dir: tempfile::TempDir,
        /// 隔离的常驻登记目录,理由同 `board_dir`:board/create|remove 不得写
        /// 真的 `$HOME/.yi-agent`。字段仅用于持有 tempdir。
        _resident_dir: tempfile::TempDir,
        /// 与主循环**共享**的配对状态:测试经 [`Harness::pairing`] 直接 `redeem`,
        /// 所得设备必须能被同一主循环的 `device/list` 看见。
        pairing: Arc<PairingState>,
        /// 与主循环**共享**的扇出中心:测试经 [`Harness::hub`] 断言订阅状态。
        hub: Arc<crate::broadcast::Broadcaster>,
        /// 注入的 watchman 调用记录器:`(是否安装, 使用的 home)`，与
        /// `launcher` 注入同一个理由——测试绝不能真的调 `launchctl`，也不能
        /// 碰用户真实的 `~/Library/LaunchAgents`。
        pub(crate) watchman_calls: WatchmanCalls,
        /// 注入的 watchman home（tempdir），仅用于持有它。
        pub(crate) _watchman_home: tempfile::TempDir,
    }

    /// 记录到的 watchman 调用:安装为 `true`、卸载为 `false`，附所针对的 home。
    pub(crate) type WatchmanCalls = Arc<StdMutex<Vec<(bool, PathBuf)>>>;

    impl Harness {
        pub(crate) fn new() -> Self {
            Self::with_factory(build_test_agent, PERMISSION_TIMEOUT)
        }

        /// 用自定义 config 搭建 harness（持久化测试需要自定义 workdir）。
        pub(crate) async fn with_cfg(cfg: RuntimeConfig) -> Self {
            Self::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT)
        }

        /// 用自定义 agent 工厂搭建 harness(慢 provider / 中断 / 权限测试需要)。
        fn with_factory<F>(build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + Sync
                + 'static,
        {
            Self::with_config(test_config(), build, permission_timeout)
        }

        /// 用自定义 config + agent 工厂搭建 harness(持久化测试需要自定义 workdir)。
        ///
        /// 生产 stdio 恒为 `Scope::Admin`,故此处固定传 `Admin`;要驱动门禁分支
        /// 用 [`Harness::with_scope`]。
        fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + Sync
                + 'static,
        {
            Self::with_config_and_scope(cfg, build, permission_timeout, Scope::Admin)
        }

        /// 把 `local` 客户端注册为给定 `scope` 的 harness。
        ///
        /// 与 [`Harness::new`] 唯一的不同是这对 `serve_scoped` 的 `client_scope`
        /// 实参;生产路径(`serve_stdio`)恒为 `Admin`,故它只服务门禁测试。
        pub(crate) async fn with_scope(scope: Scope) -> Self {
            Self::with_config_and_scope(test_config(), build_test_agent, PERMISSION_TIMEOUT, scope)
        }

        /// 用注入的 watchman 安装/卸载行为搭建 harness。
        ///
        /// 默认 harness 的记录器只记账;要驱动「失败要回传 warning」这类分支,
        /// 需要一个真的返回 `Err` 的注入点。
        pub(crate) async fn with_watchman(
            cfg: RuntimeConfig,
            install: WatchmanInstall,
            uninstall: WatchmanUninstall,
        ) -> Self {
            Self::with_config_and_scope_and_watchman(
                cfg,
                build_test_agent,
                PERMISSION_TIMEOUT,
                Scope::Admin,
                install,
                uninstall,
                Arc::new(StdMutex::new(Vec::new())),
            )
        }

        fn with_config_and_scope<F>(
            cfg: RuntimeConfig,
            build: F,
            permission_timeout: Duration,
            scope: Scope,
        ) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + Sync
                + 'static,
        {
            // 默认注入:只记录调用,不碰真实 launchd / home。
            let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
            let install_calls = Arc::clone(&calls);
            let uninstall_calls = Arc::clone(&calls);
            Self::with_config_and_scope_and_watchman(
                cfg,
                build,
                permission_timeout,
                scope,
                Arc::new(move |_exe: &Path, home: &Path| {
                    install_calls
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push((true, home.to_path_buf()));
                    Ok(())
                }),
                Arc::new(move |home: &Path| {
                    uninstall_calls
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push((false, home.to_path_buf()));
                    Ok(())
                }),
                calls,
            )
        }

        fn with_config_and_scope_and_watchman<F>(
            cfg: RuntimeConfig,
            build: F,
            permission_timeout: Duration,
            scope: Scope,
            install: WatchmanInstall,
            uninstall: WatchmanUninstall,
            calls: WatchmanCalls,
        ) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + Sync
                + 'static,
        {
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let index_dir = tempfile::TempDir::new().unwrap();
            let board_dir = tempfile::TempDir::new().unwrap();
            // 常驻登记也落在隔离目录里:`board/create` 不得写真的 `~/.yi-agent`。
            let resident_dir = tempfile::TempDir::new().unwrap();
            let workspaces = Arc::new(WorkspaceIndex::new(
                index_dir.path().join("workspaces.json"),
            ));
            // 配对状态也落在隔离目录里:`device/list` 从空表起步,且绝不碰用户
            // 真实的 `~/.yi-agent/devices.json`。
            let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
                index_dir.path().join("devices.json"),
            )));
            // 与主循环共享同一个扇出中心,便于测试直接断言订阅状态
            // (`thread/subscribe` 的效果)。
            let hub = Arc::new(crate::broadcast::Broadcaster::new());
            // 主题句柄与 `cfg.workdir` 一致：`ui/settings/read` 从 `cfg.workdir`
            // 读、`write` 经句柄落盘，两者不同则会各看各的。
            let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
            // 值守的 home 也落在隔离目录里:安装/卸载经调用方注入的闭包,绝不
            // 触碰真实的 `~/Library/LaunchAgents`。
            let watchman_home = tempfile::TempDir::new().unwrap();
            let handle = tokio::spawn(serve_scoped(
                server_r,
                server_w,
                cfg,
                permission_timeout,
                workspaces,
                Arc::clone(&pairing),
                Arc::clone(&hub),
                RuntimeAttachments {
                    runtimes: Arc::new(StdMutex::new(HashMap::new())),
                    thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                    board_dir: board_dir.path().to_path_buf(),
                    // 测试里不起真进程:`board/create` 走注入的启动器,
                    // 与 board_dir 注入同一个理由。
                    resident_dir: resident_dir.path().to_path_buf(),
                    launcher: Arc::new(|_project: &Path| Ok(true)),
                    theme,
                    watchman_install: install,
                    watchman_uninstall: uninstall,
                    watchman_home: watchman_home.path().to_path_buf(),
                },
                build,
                scope,
            ));
            Self {
                client_w,
                client_r: BufReader::new(client_r),
                handle,
                _index_dir: index_dir,
                board_dir,
                _resident_dir: resident_dir,
                pairing,
                hub,
                watchman_calls: calls,
                _watchman_home: watchman_home,
            }
        }

        /// 与主循环共享的扇出中心句柄:测试据此断言 `thread/subscribe` 是否
        /// 真的把订阅写进了服务端的 `Broadcaster`。
        pub(crate) fn hub(&self) -> Arc<crate::broadcast::Broadcaster> {
            Arc::clone(&self.hub)
        }

        /// 测试直接驱动的配对状态句柄,与主循环共享同一个 `Arc`:这里 `redeem`
        /// 出的设备正是 `device/list` 会读到的那一条。
        pub(crate) fn pairing(&self) -> Arc<PairingState> {
            Arc::clone(&self.pairing)
        }

        pub(crate) async fn send(&mut self, line: &str) {
            self.client_w.write_all(line.as_bytes()).await.unwrap();
            self.client_w.write_all(b"\n").await.unwrap();
            self.client_w.flush().await.unwrap();
        }

        pub(crate) async fn read_value(&mut self) -> serde_json::Value {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), self.client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a message")
                .expect("read_line failed");
            assert!(n > 0, "unexpected EOF while waiting for a message");
            serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
        }

        /// 关掉客户端写端(触发 EOF)并等待 server 任务结束。
        pub(crate) async fn shutdown(self) {
            let Harness {
                client_w, handle, ..
            } = self;
            drop(client_w);
            let res = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("server must shut down within the timeout")
                .expect("server task must not panic");
            assert!(res.is_ok(), "serve_stdio should return Ok on EOF: {res:?}");
        }
    }

    pub(crate) async fn initialize(h: &mut Harness) {
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "initialize must respond with the request id");
    }

    /// `initialize` + `thread/start`,返回新建 thread 的 id。
    async fn start_thread(h: &mut Harness) -> String {
        initialize(h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        // 依次读到 thread/started 通知与 thread/start 响应,取响应里的 thread_id。
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                return v["result"]["thread_id"].as_str().unwrap().to_string();
            }
        }
        panic!("no thread/start response");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_in_flight_turn_leaves_a_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_checkpoint_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"do work"}}]}}}}"#
        ))
        .await;

        // 等到**助手 item** 的 item/completed 出现 —— 说明这一块文本已 finalize、
        // checkpoint 已被标脏。注意不能只等第一个 item/completed：turn 开启时发
        // 的用户 item 也是 item/completed，只等"第一个"会在助手文本 finalize 之前
        // 就 break，断言便退化成只校验 turn-start 那次写入（那时 flush 分支从未跑过）。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
                && v["params"]["item"]["type"] == serde_json::json!("agentMessage")
            {
                break;
            }
        }

        let partial = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.partial.json"));
        // checkpoint 是去抖写的，轮询等待。这里只接受"已 finalize 的助手 item 也
        // 在盘上"的版本：turn-start 那次写入只有用户 item（items 长度 1），
        // 助手 item 只能由 500ms tick 的 flush 分支落盘。
        let mut saved: Option<serde_json::Value> = None;
        for _ in 0..100 {
            if let Ok(t) = std::fs::read_to_string(&partial) {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["items"].as_array().map(|a| a.len() >= 2).unwrap_or(false) {
                    saved = Some(v);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let saved = saved.expect(
            "checkpoint must carry the finalized assistant item; only the debounced flush can write it",
        );
        let items = saved["items"].as_array().unwrap();
        // turn-start 那次写入只有用户 item（长度 1）；助手 item 只能由 flush 落盘。
        assert!(
            items.len() >= 2,
            "checkpoint must carry the finalized assistant item (turn-start write has only the user item): {items:?}"
        );
        assert_eq!(
            items[0]["type"],
            serde_json::json!("userMessage"),
            "checkpoint must carry the opening user item: {items:?}"
        );
        assert_eq!(
            items[0]["text"],
            serde_json::json!("do work"),
            "checkpoint must carry the prompt: {items:?}"
        );
        let assistant = items
            .iter()
            .find(|i| i["type"] == serde_json::json!("agentMessage"))
            .unwrap_or_else(|| {
                panic!("checkpoint must carry the finalized assistant item; only the debounced flush can write it: {items:?}")
            });
        assert_eq!(
            assistant["text"],
            serde_json::json!("a"),
            "checkpoint must carry the finalized assistant text: {items:?}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completed_turn_removes_its_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }
        let partial = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.partial.json"));
        for _ in 0..100 {
            if !partial.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !partial.exists(),
            "a finished turn must not leave a checkpoint"
        );
        h.shutdown().await;
    }

    /// 读到 id 匹配的那一帧响应,丢弃中间穿插的通知(如 `process/updated`)。
    ///
    /// Task 5 之后,进程状态守望者会随时把 `process/updated` 插进同一路流里,
    /// 请求响应不再保证是紧接着的下一帧,故按 id 收敛而非"读一帧就当响应"。
    async fn read_response(h: &mut Harness, want: u64) -> serde_json::Value {
        for _ in 0..16 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(want)) {
                return v;
            }
        }
        panic!("no response with id {want}");
    }

    /// 带总时限地读到「指定 method 的通知」,丢弃中间帧(其它通知、请求响应)。
    ///
    /// `process/updated` 由守望者异步推来,不由任何请求直接触发,故不能靠「读下一帧」;
    /// 只能按 method 收敛。逐帧读另设较短上限,以免没有帧时 `read_value` 内部 5s 超时
    /// 先 panic(那会让失败信息含混);总时限到则返回 `None`,由调用方给出明确断言。
    async fn await_notification(
        h: &mut Harness,
        method: &str,
        timeout: Duration,
    ) -> Option<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let slice = remaining.min(Duration::from_millis(1000));
            match tokio::time::timeout(slice, h.read_value()).await {
                Ok(v) => {
                    if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                        return Some(v);
                    }
                }
                Err(_) => {
                    if tokio::time::Instant::now() >= deadline {
                        return None;
                    }
                }
            }
        }
    }

    fn summary(
        task_id: &str,
        state: &str,
        thread_id: Option<&str>,
    ) -> yi_agent_store::ipc::IpcTaskSummary {
        yi_agent_store::ipc::IpcTaskSummary {
            task_id: task_id.into(),
            state: state.into(),
            is_root: false,
            parent_task_id: Some("root".into()),
            thread_id: thread_id.map(str::to_string),
        }
    }

    /// A conversation sees only the children it spawned.
    ///
    /// One directory can host several conversations sharing one root, so the
    /// marker bound at assembly time is the only thing that separates them. An
    /// untagged task belongs to no conversation rather than to all of them.
    #[test]
    fn children_are_scoped_by_the_conversation_marker() {
        let tasks = vec![
            summary("mine-1", "running", Some("thread-a")),
            summary("theirs", "running", Some("thread-b")),
            summary("untagged", "running", None),
            summary("mine-2", "completed", Some("thread-a")),
        ];
        let mine = children_for_thread(&tasks, "thread-a");
        assert_eq!(
            mine.iter().map(|c| c.task_id.as_str()).collect::<Vec<_>>(),
            vec!["mine-1", "mine-2"],
            "only this conversation's children, in order"
        );
        assert!(
            children_for_thread(&tasks, "thread-b")
                .iter()
                .all(|c| c.task_id == "theirs")
        );
        assert!(
            children_for_thread(&tasks, "thread-c").is_empty(),
            "a conversation with no children lists none"
        );
    }

    /// A root task is never a child, even when it carries the marker.
    #[test]
    fn the_root_is_never_listed_as_a_child() {
        let mut root = summary("root", "running", Some("thread-a"));
        root.is_root = true;
        let tasks = vec![root, summary("child", "running", Some("thread-a"))];
        let children = children_for_thread(&tasks, "thread-a");
        assert_eq!(
            children
                .iter()
                .map(|c| c.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["child"]
        );
    }

    /// A finished child stays in the list; the state is what marks it finished.
    #[test]
    fn a_finished_child_stays_in_the_list() {
        let tasks = vec![summary("done", "completed", Some("thread-a"))];
        let children = children_for_thread(&tasks, "thread-a");
        assert_eq!(children.len(), 1);
        assert!(
            is_terminal_state(&children[0].state),
            "the client marks it done"
        );
    }

    /// The notifications and rows carry the method names and camelCase fields
    /// the desktop client keys on.
    #[test]
    fn the_agent_notifications_use_their_documented_wire_shape() {
        let children = Notification::AgentChildrenUpdated {
            thread_id: "thread-1".into(),
            children: vec![crate::protocol::AgentChild {
                task_id: "task-1".into(),
                objective: Some("do the thing".into()),
                state: "running".into(),
                last_step: Some("running tests".into()),
                parent_task_id: Some("root".into()),
            }],
        };
        let json = serde_json::to_value(&children).unwrap();
        assert_eq!(json["method"], "agent/children/updated");
        assert_eq!(json["params"]["threadId"], "thread-1");
        assert_eq!(json["params"]["children"][0]["taskId"], "task-1");
        assert_eq!(json["params"]["children"][0]["lastStep"], "running tests");

        let event = Notification::AgentTraceEvent {
            thread_id: "thread-1".into(),
            task_id: "task-1".into(),
            row: crate::protocol::AgentTraceRow {
                event_id: 7,
                task_id: "task-1".into(),
                kind: "assistant_text".into(),
                payload_json: "{\"type\":\"assistant_text\",\"text\":\"hi\"}".into(),
            },
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["method"], "agent/trace/event");
        assert_eq!(json["params"]["taskId"], "task-1");
        assert_eq!(json["params"]["row"]["eventId"], 7);
        assert_eq!(
            json["params"]["row"]["payloadJson"],
            "{\"type\":\"assistant_text\",\"text\":\"hi\"}"
        );
    }

    /// The list degrades to empty rather than failing when the conversation has
    /// no runtime, so the desktop rail always renders and the turn keeps running.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_children_list_is_empty_without_an_attached_runtime() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"agent/children/list","params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 9);
        assert!(
            v.get("error").is_none(),
            "an unattached conversation answers with an empty list, not an error: {v}"
        );
        assert_eq!(v["result"]["children"], serde_json::json!([]));
        h.shutdown().await;
    }

    /// An unknown conversation is a caller error, not an empty list.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_methods_reject_an_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"agent/children/list","params":{"threadId":"thread-nope"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32011, "unknown thread: {v}");

        h.send(r#"{"jsonrpc":"2.0","id":10,"method":"agent/trace/read","params":{"threadId":"thread-nope","taskId":"t"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 10);
        assert_eq!(v["error"]["code"], -32011, "unknown thread: {v}");
        h.shutdown().await;
    }

    /// `agent/trace/read` without a task id is a parameter error.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_trace_read_requires_a_task_id() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"agent/trace/read","params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32602, "missing taskId: {v}");
        h.shutdown().await;
    }

    /// Cancelling without a preview token is refused, so the confirmation step
    /// cannot be skipped by a client that calls `agent/cancel` directly.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_requires_a_confirmation_token() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"agent/cancel","params":{{"threadId":"{thread_id}","taskId":"task-1"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32602, "no token, no cancel: {v}");
        h.shutdown().await;
    }

    /// A message to an unattached conversation fails loudly rather than
    /// pretending to queue: there is no daemon to queue it on.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_message_without_a_runtime_is_an_error() {
        // "Unattached" has to be arranged, not assumed: bring-up provisions the
        // runtime at `<workdir>/.yi-agent/runtime`, and a freshly made directory
        // is perfectly attachable. The shared `/tmp/yi-agent-app-server-test`
        // workdir made this test pass only by accident -- the attach key and the
        // lookup key used to disagree for a directory that did not exist yet
        // (`/tmp` versus `/private/tmp`), so the entry missed and the thread read
        // as unattached. `project_key` now keys both the same way, so that
        // accident is gone and any attachable workdir would reach a real daemon
        // and answer `-32603` instead of the `-32011` asserted here.
        //
        // Occupy `<workdir>/.yi-agent` with a regular file instead: the runtime
        // directory can never be created, bring-up fails deterministically, and
        // the conversation is unattached for the reason this test is about --
        // independent of `/tmp` state or of any daemon running elsewhere.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".yi-agent"), b"not a directory").unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let thread_id = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"agent/message","params":{{"threadId":"{thread_id}","taskId":"task-1","message":"hello"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32011, "unattached conversation: {v}");
        h.shutdown().await;
    }

    /// A malformed message request is a parameter error.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_message_requires_a_task_and_text() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"agent/message","params":{{"threadId":"{thread_id}","taskId":"task-1"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32602, "missing message: {v}");
        h.shutdown().await;
    }

    /// Unwatching a conversation that is not watching anything is a no-op, so a
    /// client closing a detail twice is not an error.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_trace_unwatch_is_idempotent() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;
        for id in [9, 10] {
            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"agent/trace/unwatch","params":{{"threadId":"{thread_id}"}}}}"#
            ))
            .await;
            let v = h.read_value().await;
            assert_eq!(v["id"], id);
            assert_eq!(v["result"]["stopped"], true, "unwatch always succeeds: {v}");
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_responds_with_server_info() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        assert_eq!(v["result"]["protocolVersion"], 1);
        assert!(
            v.get("error").is_none(),
            "success response must omit `error`: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn request_before_initialize_returns_not_initialized() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"config/read","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32010);
        assert!(
            v.get("result").is_none(),
            "error response must omit `result`: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_method_returns_method_not_found() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"bogus","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["error"]["code"], -32601);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_emits_thread_started_and_responds() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;

        let mut notif = None;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("thread/started") {
                notif = Some(v);
            } else if v.get("id") == Some(&serde_json::json!(2)) {
                resp = Some(v);
            }
            if notif.is_some() && resp.is_some() {
                break;
            }
        }

        let notif = notif.expect("expected a thread/started notification");
        let resp = resp.expect("expected a thread/start response");
        let notif_thread_id = notif["params"]["thread_id"]
            .as_str()
            .expect("notification thread_id must be a string");
        assert!(!notif_thread_id.is_empty(), "thread_id must be non-empty");
        assert_eq!(notif["params"]["model"], "test-model");
        assert_eq!(resp["result"]["thread_id"], notif_thread_id);
        assert_eq!(resp["result"]["model"], "test-model");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_read_returns_redacted_view() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["api_key"], "");
        assert_eq!(v["result"]["model"], "test-model");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_add_list_remove_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut h = Harness::new();
        initialize(&mut h).await;

        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"][0]["path"],
            cwd.to_string_lossy().to_string()
        );
        assert_eq!(v["result"]["workspaces"][0]["exists"], true);

        let rm = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"workspace/remove",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&rm.to_string()).await;
        let v = h.read_value().await;
        assert!(v.get("result").is_some());

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"].as_array().unwrap().len(),
            0,
            "removed workspace must be gone from the index: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_list_marks_missing_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut h = Harness::new();
        initialize(&mut h).await;

        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

        // 目录在磁盘上消失后,索引仍保留该条目,但 list 必须标记 exists=false。
        std::fs::remove_dir_all(&cwd).unwrap();

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"][0]["path"],
            cwd.to_string_lossy().to_string()
        );
        assert_eq!(
            v["result"]["workspaces"][0]["exists"], false,
            "missing directory must be flagged as not existing: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_remove_accepts_alternate_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut h = Harness::new();
        initialize(&mut h).await;

        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

        // 用与存储值不同、但 canonicalize 后等价的路径 remove,必须命中同一条目。
        // `dir.path()` 是非 canonical 前缀(如 macOS 的 /var → /private/var),
        // 末尾再缀 `/.`,因此字符串与 canonical 存储值不同、解析后却相同。
        let alternate = dir.path().join(".").to_string_lossy().to_string();
        assert_ne!(alternate, cwd.to_string_lossy().to_string());

        let rm = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"workspace/remove",
            "params":{"path": alternate}});
        h.send(&rm.to_string()).await;
        let v = h.read_value().await;
        assert!(v.get("result").is_some(), "remove must succeed: {v}");

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"].as_array().unwrap().len(),
            0,
            "alternate path must remove the canonicalized entry: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_add_rejects_non_directory() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"workspace/add","params":{"path":"/nope/nope"}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eof_exits_gracefully() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        let handle = tokio::spawn(serve_stdio(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            pairing,
            Arc::new(crate::broadcast::Broadcaster::new()),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: PathBuf::new(),
                resident_dir: PathBuf::new(),
                launcher: Arc::new(|_project: &Path| Ok(true)),
                theme: test_theme(),
                // 测试注入:只记录调用,不碰真实 launchd / home。
                watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
                watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
                watchman_home: PathBuf::new(),
            },
            build_test_agent,
        ));

        // 什么都不发,直接关掉写端 → server 应看到 EOF 并 Ok(()) 退出。
        drop(client_w);
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must exit within the timeout")
            .expect("server task must not panic");
        assert!(
            result.is_ok(),
            "serve_stdio should return Ok on EOF: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_json_returns_parse_error() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send("not json").await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32700, "expected parse error: {v}");
        assert_eq!(v["id"], 0, "malformed frame must be answered with id 0");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blank_lines_are_ignored() {
        let mut h = Harness::new();
        // 先发一个空行,再发合法的 initialize;空行不应产生任何帧。
        h.send("").await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "blank line must not produce a frame: {v}");
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_factory_failure_returns_internal_error() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        let handle = tokio::spawn(serve_stdio(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            pairing,
            Arc::new(crate::broadcast::Broadcaster::new()),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: PathBuf::new(),
                resident_dir: PathBuf::new(),
                launcher: Arc::new(|_project: &Path| Ok(true)),
                theme: test_theme(),
                // 测试注入:只记录调用,不碰真实 launchd / home。
                watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
                watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
                watchman_home: PathBuf::new(),
            },
            |_s: Option<yi_agent_core::Session>,
             _cwd: &std::path::Path,
             _mode: crate::thread_store::ThreadMode| {
                Err::<BuiltAgent, _>(anyhow::anyhow!("boom"))
            },
        ));

        let mut client_r = BufReader::new(client_r);

        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_w.flush().await.unwrap();
        let mut buf = String::new();
        client_r.read_line(&mut buf).await.unwrap();
        let init: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        assert_eq!(init["id"], 1);

        client_w
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"thread/start\",\"params\":{}}\n",
            )
            .await
            .unwrap();
        client_w.flush().await.unwrap();
        let mut buf = String::new();
        client_r.read_line(&mut buf).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(
            v["error"]["code"], -32603,
            "factory failure must map to internal error: {v}"
        );

        drop(client_w);
        let res = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(res.is_ok(), "serve_stdio should return Ok on EOF: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_returns_err() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        let handle = tokio::spawn(serve_stdio(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            pairing,
            Arc::new(crate::broadcast::Broadcaster::new()),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                board_dir: PathBuf::new(),
                resident_dir: PathBuf::new(),
                launcher: Arc::new(|_project: &Path| Ok(true)),
                theme: test_theme(),
                // 测试注入:只记录调用,不碰真实 launchd / home。
                watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
                watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
                watchman_home: PathBuf::new(),
            },
            build_test_agent,
        ));

        let mut frame = vec![b'x'; crate::protocol::MAX_FRAME_BYTES + 16];
        frame.push(b'\n');
        // duplex 缓冲小于帧,server 读到超限即报错并退出读端;写侧随后可能
        // 因 broken pipe 失败,这里忽略写错误——真正断言的是 server 的结果。
        let _ = client_w.write_all(&frame).await;
        let _ = client_w.flush().await;

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must exit within the timeout")
            .expect("server task must not panic");
        assert!(
            result.is_err(),
            "oversized frame must surface an error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_emits_full_notification_sequence() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut methods: Vec<String> = Vec::new();
        let mut resp_turn_id: Option<String> = None;
        let mut completed: Option<serde_json::Value> = None;
        let mut running_status: Option<String> = None;
        // 本轮开启项(用户提问)的 item/started 形状。
        let mut opener: Option<serde_json::Value> = None;
        for _ in 0..14 {
            let v = h.read_value().await;
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                methods.push(m.to_string());
                if m == "thread/status/updated" {
                    running_status = v["params"]["status"].as_str().map(|s| s.to_string());
                }
                if m == "item/started" && opener.is_none() {
                    opener = Some(v["params"]["item"].clone());
                }
                if m == "turn/completed" {
                    completed = Some(v);
                    break;
                }
            } else if v.get("id") == Some(&serde_json::json!(3)) {
                resp_turn_id = v["result"]["turn_id"].as_str().map(|s| s.to_string());
            }
        }

        assert_eq!(
            methods,
            vec![
                "turn/started",
                // 本轮开启项:用户提问(用户气泡)必须排在 agent 任何 item 之前,
                // 否则实时观看与事后回放的顺序不一致(点开时开场白跑到最底部)。
                "item/started",
                "item/completed",
                // turn/start 在 turn/started 之后立刻推 Running 状态,先于 turn 正文。
                "thread/status/updated",
                "item/started",
                "item/delta",
                // `Done` 会先 finalize 打开的 agentMessage,故 turn/completed
                // 之前必有一条 item/completed(见 Translator::finish_turn)。
                "item/completed",
                "turn/completed"
            ],
            "unexpected notification sequence"
        );
        let opener = opener.expect("the turn must open with an item");
        assert_eq!(
            opener["type"], "userMessage",
            "the first item of a turn must be the user's own message: {opener}"
        );
        assert_eq!(opener["text"], "hi");
        assert_eq!(
            opener["id"],
            format!("user-{}", resp_turn_id.as_deref().unwrap_or_default()),
            "the opener id must match the wire id, so the persisted turn and a \
             live client agree on identity: {opener}"
        );
        assert_eq!(
            running_status.as_deref(),
            Some("running"),
            "the status frame preceding the turn body must report `running`"
        );
        let resp_turn_id = resp_turn_id.expect("turn/start response must carry turn_id");
        assert!(!resp_turn_id.is_empty(), "turn_id must be non-empty");
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "completed");
        assert_eq!(completed["params"]["turn_id"], resp_turn_id);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_unknown_thread_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"threadId":"nope","input":[{"type":"text","text":"hi"}]}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_missing_thread_id_returns_invalid_params() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"input":[{"type":"text","text":"hi"}]}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_empty_input_returns_invalid_params() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[]}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interject_while_running_keeps_the_same_turn() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":11,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let started_turn = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(11)) {
                break v["result"]["turn_id"].as_str().unwrap().to_string();
            }
        };

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":12,"method":"turn/interject","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"also do X"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(12)) {
                assert_eq!(
                    v["result"]["turn_id"].as_str().unwrap(),
                    started_turn,
                    "an interjection must not open a new turn: {v}"
                );
                assert!(
                    v["result"]["interjection_id"].as_str().is_some(),
                    "expected an interjection_id: {v}"
                );
                break;
            }
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interject_without_an_active_turn_is_rejected() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":13,"method":"turn/interject","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"too early"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(13)) {
                assert_eq!(v["error"]["code"], -32013, "expected not running: {v}");
                break;
            }
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_while_turn_in_progress_returns_turn_in_progress() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 读到 id==3 的响应(跳过 turn/started 通知与 item/* 帧)。
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                assert!(v["result"]["turn_id"].is_string(), "expected turn_id: {v}");
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"again"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
                break;
            }
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interrupt_cancels_active_turn() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 等 turn/started,确认 turn 已活跃。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                completed = Some(v);
                break;
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "interrupted");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_ignores_interrupt_tagged_with_other_turn() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let (hub, client) = test_hub(server_w);

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        let built = build_delayed_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            hub,
            client,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            store,
            ThreadSession::new_status(),
            built.provider,
            built.config,
        ));

        // 上一轮残留的中断(属于 turn-0)必须被忽略。
        interrupt_tx.try_send("turn-0".into()).unwrap();
        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
                activate: None,
            })
            .await
            .unwrap();

        let mut client_r = BufReader::new(client_r);
        let mut status = None;
        for _ in 0..10 {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a notification")
                .unwrap();
            if n == 0 {
                break;
            }
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v["method"] == "turn/completed" {
                status = Some(v["params"]["status"].as_str().unwrap().to_string());
                break;
            }
        }
        assert_eq!(
            status.as_deref(),
            Some("completed"),
            "a stale interrupt must not cancel the new turn"
        );

        drop(prompt_tx);
        drop(interrupt_tx);
        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, TurnEvent::Finished { .. }));
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Critical 2:往一个已注销的 **ws** 客户端写响应**不得**是致命错误。
    ///
    /// 一台手机断连/被撤销、或被广播背压摘除后,主循环仍可能为它手里的一帧去写
    /// 响应;那是多客户端共享主循环上的常态,不该带走所有其它客户端。回归:
    /// `write_response(..)?` 曾把 `Closed` 冒泡成 `serve` 的致命错误。
    ///
    /// 反向钉死:stdio 的 `local` 仍保持改造前的**致命**语义(出口泵死了,再写
    /// 就是错),`driver_reports_finished_when_writer_fails` 依赖它。
    #[tokio::test]
    async fn write_response_to_a_gone_ws_client_is_not_fatal() {
        let hub = crate::broadcast::Broadcaster::new();
        let response = || ResponseEnvelope {
            jsonrpc: Some(JSONRPC_VERSION.to_string()),
            id: RequestId::Num(1),
            result: Some(serde_json::json!({})),
            error: None,
        };

        // ws 客户端已走:非致命,返回 Ok(它同时被确认已从 hub 摘除)。
        let ws_id = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
        let _rx = hub.register(ws_id.clone());
        hub.unregister(&ws_id);
        assert!(
            write_response(&hub, &ws_id, response()).await.is_ok(),
            "a response to a gone ws client must not be fatal"
        );
        assert!(!hub.is_connected(&ws_id), "the ws client must be gone");

        // 反面:stdio 的 `local` 保持致命。
        let local = crate::broadcast::ClientId::local();
        let _rx = hub.register(local.clone());
        hub.unregister(&local);
        assert!(
            write_response(&hub, &local, response()).await.is_err(),
            "the stdio local client must stay fatal on write failure"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_reports_finished_when_writer_fails() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        drop(client_r); // 断开读端 → 写通知失败
        let (hub, client) = test_hub(server_w);

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        let built = build_test_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            hub,
            client,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            store,
            ThreadSession::new_status(),
            built.provider,
            built.config,
        ));

        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
                activate: None,
            })
            .await
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .expect("driver must report Finished even when writes fail")
            .expect("turn channel must stay open");
        assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// 回归:driver 空闲(没有 turn 在跑)时,也必须在有限时间内响应会话命令。
    ///
    /// 修复前 `session_rx` 只在每轮的内层 select 里被轮询,空闲 thread 的命令
    /// 会一直躺在 channel 里,直到下一个 turn/start 才被读到——这个测试会超时。
    #[tokio::test(flavor = "multi_thread")]
    async fn idle_driver_serves_a_session_command_without_a_new_turn() {
        use crate::session::{CompactOutcome, SessionCommand};

        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        drop(client_r); // 本测试不看 writer 输出
        let (hub, client) = test_hub(server_w);

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(8);
        let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        // prompt_tx 保住不 drop:driver 应停在 idle,而不是因 prompt_rx 关闭退出。
        let _keep_prompt_tx = prompt_tx;

        let built = build_test_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();

        let handle = tokio::spawn(run_thread_driver(
            "thread-idle-1".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            hub,
            client,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(5),
            Arc::new(AtomicU64::new(0)),
            None,
            store,
            ThreadSession::new_status(),
            built.provider,
            built.config,
        ));

        // 没有任何 turn/start:直接投一条 compact,必须在有限时间内拿到回复。
        let (reply, answer) = oneshot::channel();
        session_tx
            .send(SessionCommand::Compact { reply })
            .await
            .unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("an idle driver must service a session command without a new turn")
            .expect("the reply channel must be fulfilled");
        assert_eq!(
            outcome,
            CompactOutcome::NotReduced,
            "an empty session has nothing to compact"
        );

        // driver 仍在运行(命令不该把它弄停)。
        assert!(!handle.is_finished());
        handle.abort();

        // turn_rx 里的 Finished 是可选的:本路径 turn_id 为 None,不该发 Finished。
        assert!(
            turn_rx.try_recv().is_err(),
            "a command with no turn must not report a turn as finished"
        );
    }

    /// 一个 thread 的 driver 复用同一个 `Translator`,因此 item id 跨 turn 单调
    /// 递增,不会出现两轮都用 `item-1` 的碰撞(回归 Fix #4)。
    #[tokio::test(flavor = "multi_thread")]
    async fn driver_uses_unique_item_ids_across_turns() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let (hub, client) = test_hub(server_w);

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        let built = build_test_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            hub,
            client,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            store,
            ThreadSession::new_status(),
            built.provider,
            built.config,
        ));

        let mut client_r = BufReader::new(client_r);
        // 每轮取第一个 `item/started` 的 item id(agentMessage 的起始项)。
        let mut item_ids: Vec<String> = Vec::new();
        for turn in ["turn-1", "turn-2"] {
            prompt_tx
                .send(TurnPrompt {
                    turn_id: turn.into(),
                    prompt: "hi".into(),
                    activate: None,
                })
                .await
                .unwrap();
            loop {
                let mut buf = String::new();
                let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                    .await
                    .expect("timed out waiting for a notification")
                    .unwrap();
                assert!(n > 0, "unexpected EOF while waiting for turn/completed");
                let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
                let method = v["method"].as_str().unwrap_or("");
                if method == "item/started" {
                    if let Some(id) = v["params"]["item"]["id"].as_str() {
                        item_ids.push(id.to_string());
                    }
                }
                if method == "turn/completed" {
                    break;
                }
            }
            let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(ev, TurnEvent::Finished { .. }));
        }

        assert!(
            item_ids.len() >= 2,
            "expected items from both turns: {item_ids:?}"
        );
        let mut unique = item_ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            item_ids.len(),
            "item ids must be unique across turns: {item_ids:?}"
        );

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// 第一次调用触发一个受权限管控的 bash 工具调用;第二次调用返回文本并结束。
    struct PermissionMockProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl yi_agent_core::Provider for PermissionMockProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            use yi_agent_core::provider::{ProviderEvent, StopReason};
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = if n == 0 {
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "c1".into(),
                        name: "bash".into(),
                    },
                    ProviderEvent::ToolUseDelta {
                        id: "c1".into(),
                        partial_json: r#"{"cmd":"ls"}"#.into(),
                    },
                    ProviderEvent::ToolUseEnd { id: "c1".into() },
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ]
            } else {
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ]
            };
            Ok(futures::stream::iter(events).boxed())
        }
    }

    struct FakeBash;

    #[async_trait]
    impl yi_agent_core::Tool for FakeBash {
        fn name(&self) -> &str {
            "bash"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn description(&self) -> &str {
            "fake bash"
        }
        async fn call(&self, _args: serde_json::Value) -> yi_agent_core::ToolResult {
            yi_agent_core::ToolResult::text("ran")
        }
    }

    /// 构造一个会触发 bash 审批的 agent,并把决定通道交给 driver。
    fn build_permission_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(PermissionMockProvider {
            calls: AtomicUsize::new(0),
        });
        let mut registry = yi_agent_core::ToolRegistry::new();
        registry.register(Arc::new(FakeBash));
        let config = yi_agent_core::AgentConfig::default();
        let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
            yi_agent_core::permission::PermissionsConfig::default(),
            yi_agent_core::autonomy::YoloSwitch::new(false),
            std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
            Arc::new(|_cmd: &str| None),
        ));
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
        let rx_arc = Arc::new(Mutex::new(decision_rx));
        let mut agent =
            yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
                .with_permission(checker.clone(), rx_arc.clone());
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: Some(decision_tx),
            decision_rx: Some(rx_arc),
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    /// Test-only ws entry: same multi-client server as production, but every
    /// thread is built by `build_permission_agent` so a `turn/start` triggers a
    /// real approval round trip. `ws.rs`'s two-client E2E needs this seam.
    pub(crate) async fn serve_ws_with_permission_agent(
        listener: tokio::net::TcpListener,
        cfg: RuntimeConfig,
        workspaces: Arc<WorkspaceIndex>,
        pairing: Arc<PairingState>,
    ) -> anyhow::Result<()> {
        let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
        crate::ws::serve_ws_inner(
            listener,
            cfg,
            workspaces,
            pairing,
            theme,
            build_permission_agent,
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permission_request_round_trip_allows_tool() {
        let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut saw_request = false;
        let mut saw_tool_started = false;
        let mut perm_id: Option<String> = None;
        for _ in 0..20 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/toolCall/requestApproval") => {
                    assert_eq!(v["id"], "perm-1", "reverse request id: {v}");
                    assert_eq!(v["params"]["tool_name"], "bash", "reverse params: {v}");
                    perm_id = v["id"].as_str().map(|s| s.to_string());
                    saw_request = true;
                    break;
                }
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_request,
            "expected an item/toolCall/requestApproval request"
        );
        let perm_id = perm_id.expect("reverse request must carry a string id");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"{perm_id}","result":{{"decision":"allow_once"}}}}"#
        ))
        .await;

        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..20 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                Some("turn/completed") => {
                    completed = Some(v);
                    break;
                }
                _ => {}
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "completed");
        assert!(
            saw_tool_started,
            "an allowed tool must emit an item/started toolCall item"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permission_timeout_defaults_to_deny() {
        let mut h = Harness::with_factory(build_permission_agent, Duration::from_millis(150));
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut saw_request = false;
        let mut saw_tool_started = false;
        for _ in 0..20 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/toolCall/requestApproval") => {
                    assert_eq!(v["id"], "perm-1", "reverse request id: {v}");
                    saw_request = true;
                    break;
                }
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_request,
            "expected an item/toolCall/requestApproval request"
        );

        // 不回复:driver 应在超时后按 Deny 处理,turn 仍能正常完成。
        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..40 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                Some("turn/completed") => {
                    completed = Some(v);
                    break;
                }
                _ => {}
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(
            completed["params"]["status"], "completed",
            "the agent must not hang after a permission timeout"
        );
        assert!(
            !saw_tool_started,
            "a denied tool must not emit an item/started toolCall item"
        );
        h.shutdown().await;
    }

    /// 读到下一个 `item/toolCall/requestApproval` 通知,返回其 id。
    async fn read_until_approval(h: &mut Harness) -> String {
        for _ in 0..20 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/toolCall/requestApproval") {
                return v["id"]
                    .as_str()
                    .expect("approval id must be a string")
                    .to_string();
            }
        }
        panic!("expected an item/toolCall/requestApproval notification");
    }

    /// 读帧直到出现 `id == want` 的响应,返回其 `result.thread_id`。
    async fn read_thread_start_response(h: &mut Harness, want: u64) -> String {
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(want)) {
                return v["result"]["thread_id"]
                    .as_str()
                    .expect("thread/start response must carry thread_id")
                    .to_string();
            }
        }
        panic!("no thread/start response for id {want}");
    }

    /// 两个并发 thread 的审批 id 必须不同:否则一个 thread 的 Allow 会被
    /// 应用到另一个 thread 的工具上(跨 thread 授权错配)。
    #[tokio::test(flavor = "multi_thread")]
    async fn permission_request_ids_are_unique_across_threads() {
        let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        // thread A。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_a = read_thread_start_response(&mut h, 2).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid_a}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let id_a = read_until_approval(&mut h).await;

        // thread B(其 PermissionChecker 的 request_id 也从 1 开始)。
        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{}}"#)
            .await;
        let tid_b = read_thread_start_response(&mut h, 4).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid_b}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let id_b = read_until_approval(&mut h).await;

        assert_ne!(id_a, id_b, "approval ids must be unique across threads");
        h.shutdown().await;
    }

    /// 上一轮残留的中断信号不得取消当前 turn 的审批等待(同 `e6068b0` 的
    /// bug 类:中断必须按目标 turn id 过滤)。
    #[tokio::test(flavor = "multi_thread")]
    async fn stale_interrupt_does_not_cancel_turn_during_approval() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let (hub, client) = test_hub(server_w);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let perm_seq = Arc::new(AtomicU64::new(1));

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let (_session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        let built = build_permission_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            hub,
            client,
            turn_tx,
            built.decision_tx,
            Arc::clone(&pending),
            Duration::from_secs(60),
            Arc::clone(&perm_seq),
            None,
            store,
            ThreadSession::new_status(),
            built.provider,
            built.config,
        ));

        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
                activate: None,
            })
            .await
            .unwrap();

        // 读到反向请求 id(此时 driver 已进入内层审批等待)。
        let mut client_r = BufReader::new(client_r);
        let mut approval_id = None;
        for _ in 0..20 {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                .await
                .expect("timed out")
                .unwrap();
            if n == 0 {
                break;
            }
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v["method"] == "item/toolCall/requestApproval" {
                approval_id = Some(v["id"].as_str().unwrap().to_string());
                break;
            }
        }
        let approval_id = approval_id.expect("expected a reverse request");

        // 现在投递一个属于上一轮(turn-0)的残留中断:它必须被忽略。
        interrupt_tx.try_send("turn-0".into()).unwrap();

        // 给 driver 足够时间处理该信号;若残留中断未被过滤,它会在此刻
        // 取走 pending 项并取消本轮。
        tokio::time::sleep(Duration::from_millis(80)).await;

        // 用正确的决定回应;若 pending 项已被残留中断移除,这里为 None → 断言失败。
        let sender = pending
            .lock()
            .await
            .remove(&approval_id)
            .expect("pending approval must survive a stale interrupt");
        sender.send(Decision::AllowOnce).unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .expect("turn must finish")
            .unwrap();
        assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_allocates_uuid_id_and_writes_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);

        let tid = start_thread(&mut h).await;
        let uuid_part = tid
            .strip_prefix("thread-")
            .unwrap_or_else(|| panic!("expected thread-<uuid>, got {tid}"));
        assert!(
            uuid_part.parse::<uuid::Uuid>().is_ok(),
            "suffix must be a uuid: {tid}"
        );

        // 通过 store API 读取,锁住落盘内容(不依赖文件布局细节)。
        let loaded = crate::thread_store::ThreadStore::new(dir.path())
            .load(&tid)
            .expect("load must not fail")
            .expect("thread/start must persist meta");
        assert_eq!(loaded.meta.thread_id, tid);
        assert_eq!(loaded.meta.model, "test-model");
        assert_eq!(loaded.meta.cwd, dir.path().display().to_string());
        assert!(loaded.meta.title.is_none(), "title starts empty");
        assert!(loaded.items.is_empty(), "no turns yet");

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_empty_returns_empty_array() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["threads"], serde_json::json!([]));
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_returns_created_threads() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_a = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
            .await;
        let tid_b = read_thread_start_response(&mut h, 3).await;

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/list must respond");
        let threads = v["result"]["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 2);
        let ids: std::collections::HashSet<&str> = threads
            .iter()
            .map(|t| t["thread_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(tid_a.as_str()) && ids.contains(tid_b.as_str()));
        // 两次 start 可能同毫秒,顺序不定(排序由 thread_store 单测锁定);
        // 此处只断言每个条目都带时间戳字段。
        for t in threads {
            assert!(t["created_at"].is_number());
            assert!(t["updated_at"].is_number());
        }
        h.shutdown().await;
    }

    /// Task 11:`thread/list` / `thread/listAll` 的条目必须显式带出
    /// `permission_mode`(wire 契约与存储解耦,但字段必须存在且为 lowercase)。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_exposes_permission_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_normal = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
            .await;
        let tid_yolo = read_thread_start_response(&mut h, 3).await;

        // 把第二个线程切成 yolo。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid_yolo}","mode":"yolo"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert!(
            v.get("error").is_none(),
            "setPermissionMode must succeed: {v}"
        );

        // thread/list:每个条目带 lowercase permission_mode。
        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/list must respond");
        let threads = v["result"]["threads"].as_array().unwrap();
        let mode_of = |tid: &str| -> serde_json::Value {
            threads
                .iter()
                .find(|t| t["thread_id"].as_str() == Some(tid))
                .unwrap_or_else(|| panic!("thread {tid} missing from thread/list: {v}"))["permission_mode"]
                .clone()
        };
        assert_eq!(mode_of(&tid_normal), serde_json::json!("normal"));
        assert_eq!(mode_of(&tid_yolo), serde_json::json!("yolo"));

        // thread/listAll:分组内的条目同样带该字段。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/listAll must respond");
        let all_threads: Vec<&serde_json::Value> = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap().iter())
            .collect();
        let mode_of_all = |tid: &str| -> serde_json::Value {
            all_threads
                .iter()
                .find(|t| t["thread_id"].as_str() == Some(tid))
                .unwrap_or_else(|| panic!("thread {tid} missing from thread/listAll: {v}"))["permission_mode"]
                .clone()
        };
        assert_eq!(mode_of_all(&tid_normal), serde_json::json!("normal"));
        assert_eq!(mode_of_all(&tid_yolo), serde_json::json!("yolo"));

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_with_cwd_writes_meta_cwd() {
        // 全局 workdir 与显式 cwd 指向不同目录:用于区分「按 thread 的 cwd 路由」
        // 与旧的「一律写全局 workdir」行为。
        let global = tempfile::TempDir::new().unwrap();
        let target = tempfile::TempDir::new().unwrap();
        let w = global.path().canonicalize().unwrap();
        let c = target.path().canonicalize().unwrap();

        let mut cfg = test_config();
        cfg.workdir = w.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": c.to_string_lossy() }});
        h.send(&req.to_string()).await;

        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                resp = Some(v);
                break;
            }
        }
        let resp = resp.expect("thread/start response");
        assert_eq!(
            resp["result"]["cwd"].as_str().unwrap(),
            c.to_string_lossy().to_string()
        );
        let thread_id = resp["result"]["thread_id"].as_str().unwrap().to_string();

        let store = crate::thread_store::ThreadStore::new(&c);
        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].cwd, c.to_string_lossy().to_string());
        assert!(
            !crate::thread_store::ThreadStore::new(&w).exists(&thread_id),
            "thread must not leak into the global workdir store"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_with_bad_cwd_returns_invalid_params() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": "/nonexistent/definitely/not/here" }});
        h.send(&req.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602);
        h.shutdown().await;
    }

    /// 索引键必须规范化为 canonical,与 `workspace/remove` 的规范化对齐:否则符号
    /// 链接形式的缺省 cwd 会以非 canonical 形式进索引,后续 remove 静默 no-op。
    /// 用符号链接制造 `canonical != raw`,确保本测试确实覆盖该回归面。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_default_cwd_indexes_canonical_path() {
        let real = tempfile::TempDir::new().unwrap();
        let link_parent = tempfile::TempDir::new().unwrap();
        let link = link_parent.path().join("linked");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let raw = link.to_string_lossy().to_string();
        let canonical = link.canonicalize().unwrap();
        assert_ne!(
            raw,
            canonical.to_string_lossy().to_string(),
            "symlink must be non-canonical for this test to be meaningful"
        );

        let mut cfg = test_config();
        cfg.workdir = link.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        // 不带 cwd → 走缺省 cfg.workdir(即符号链接路径)。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let _tid = read_thread_start_response(&mut h, 2).await;

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("workspace/list must respond");
        let path = v["result"]["workspaces"][0]["path"]
            .as_str()
            .expect("index must contain the default cwd");
        let recanon = Path::new(path)
            .canonicalize()
            .expect("indexed path must be canonicalizable");
        assert_eq!(
            recanon.to_string_lossy(),
            path,
            "index entry must already be canonical: {path}"
        );
        h.shutdown().await;
    }

    /// Design §11 的头号风险是「cwd 传递正确」:agent 工厂必须拿到该 thread 的 cwd,
    /// 而非全局 workdir。用记录型工厂把实参钉死。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_factory_receives_thread_cwd() {
        let target = tempfile::TempDir::new().unwrap();
        let c = target.path().canonicalize().unwrap();

        let seen: Arc<std::sync::Mutex<Vec<std::path::PathBuf>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_factory = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          cwd: &std::path::Path,
                          mode: crate::thread_store::ThreadMode| {
            seen_factory.lock().unwrap().push(cwd.to_path_buf());
            build_test_agent(session, cwd, mode)
        };
        let mut h = Harness::with_factory(build, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": c.to_string_lossy() }});
        h.send(&req.to_string()).await;
        let _tid = read_thread_start_response(&mut h, 2).await;

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![c.clone()],
            "agent factory must receive the thread's canonical cwd"
        );
        h.shutdown().await;
    }

    /// 两个目录各起一个 thread:`thread/listAll` 必须按 workspace 分成两组,每组
    /// 恰好含该目录的 thread(cwd 与 group.workspace 一致,thread_id 与 start 响应一致)。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_all_groups_by_workspace() {
        let dir_a = tempfile::TempDir::new().unwrap();
        let dir_b = tempfile::TempDir::new().unwrap();
        let a = dir_a
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let b = dir_b
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let mut h = Harness::new();
        initialize(&mut h).await;

        // workspace 路径 → 在该目录 start 出的 thread_id。
        let mut expected: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (id, path) in [(2u64, &a), (3u64, &b)] {
            let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start",
                "params":{"cwd": path}});
            h.send(&req.to_string()).await;
            let tid = read_thread_start_response(&mut h, id).await;
            expected.insert(path.clone(), tid);
        }

        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#)
            .await;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/listAll response");
        let groups = v["result"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "两个目录 → 两个分组: {v}");

        // 索引按「最近使用在前」排序:先 start a 再 start b → 组顺序为 [b, a]。
        // start 请求串行(读罢 a 的响应才发 b),故 workspace/add 的先后确定。
        assert_eq!(
            groups[0]["workspace"],
            b.as_str(),
            "most-recent workspace must come first"
        );
        assert_eq!(
            groups[1]["workspace"],
            a.as_str(),
            "the earlier workspace must follow"
        );

        for g in groups {
            let ws = g["workspace"].as_str().unwrap();
            assert!(
                ws == a.as_str() || ws == b.as_str(),
                "unexpected workspace {ws}"
            );
            assert_eq!(g["exists"], true, "canonical tempdir must exist: {g}");
            let threads = g["threads"].as_array().unwrap();
            assert_eq!(
                threads.len(),
                1,
                "each workspace holds exactly one thread: {g}"
            );
            assert_eq!(
                threads[0]["cwd"].as_str().unwrap(),
                ws,
                "thread cwd must match its group workspace"
            );
            assert_eq!(
                threads[0]["thread_id"].as_str().unwrap(),
                expected[ws].as_str(),
                "group must carry the thread started in that workspace"
            );
        }
        h.shutdown().await;
    }

    /// 失效目录(索引里有、磁盘上已删)仍必须出现为一个组:`exists:false` 且
    /// `threads` 为空(不深扫)。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_all_includes_missing_dir_with_empty_threads() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mut h = Harness::new();
        initialize(&mut h).await;

        // 先让目录存在时写入索引,再从磁盘删除。
        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": path_str}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert!(v.get("error").is_none(), "workspace/add must succeed: {v}");
        std::fs::remove_dir_all(&path).unwrap();

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/listAll","params":{}}"#)
            .await;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/listAll response");
        let groups = v["result"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "索引里恰有一个目录: {v}");
        let g = &groups[0];
        assert_eq!(g["workspace"].as_str().unwrap(), path_str);
        assert_eq!(g["exists"], false, "失效目录必须报 exists:false: {g}");
        assert_eq!(
            g["threads"].as_array().unwrap().len(),
            0,
            "失效目录不深扫,threads 必须为空: {g}"
        );
        h.shutdown().await;
    }

    async fn list_all(h: &mut Harness) -> serde_json::Value {
        h.send(r#"{"jsonrpc":"2.0","id":901,"method":"thread/listAll","params":{}}"#)
            .await;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(901)) {
                return v;
            }
        }
        panic!("thread/listAll must respond");
    }

    async fn add_workspace(h: &mut Harness, id: u64, path: &str) {
        h.send(
            &serde_json::json!({"jsonrpc":"2.0","id":id,"method":"workspace/add",
            "params":{"path": path}})
            .to_string(),
        )
        .await;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(id)) {
                return;
            }
        }
        panic!("workspace/add must respond");
    }

    fn write_meta(
        dir: &Path,
        id: &str,
        title: &str,
        board_project: Option<&str>,
        card_id: Option<&str>,
    ) {
        let meta = crate::thread_store::ThreadMeta {
            thread_id: id.into(),
            cwd: dir.to_string_lossy().into(),
            model: "m".into(),
            created_at: 0,
            updated_at: 0,
            title: Some(title.into()),
            permission_mode: crate::thread_store::ThreadMode::Normal,
            pin_seq: None,
            board_project: board_project.map(str::to_string),
            card_id: card_id.map(str::to_string),
        };
        crate::thread_store::ThreadStore::new(dir)
            .create(&meta)
            .unwrap();
    }

    /// 卡片会话(在 worktree、board_project=项目) 折进项目组；worktree 不再顶层成组。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_board_thread_folds_into_its_project_group() {
        let project_dir = tempfile::TempDir::new().unwrap();
        let worktree_dir = tempfile::TempDir::new().unwrap();
        let project = project_dir.path().canonicalize().unwrap();
        let worktree = worktree_dir.path().canonicalize().unwrap();
        let p = project.to_string_lossy().to_string();
        let w = worktree.to_string_lossy().to_string();

        write_meta(&project, "t-plain", "plain", None, None);
        write_meta(
            &worktree,
            "t-card",
            "看板 · card-1",
            Some(&p),
            Some("card-1"),
        );

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 11, &w).await; // worktree 进索引(=卡片会话落盘后的样子)
        add_workspace(&mut h, 12, &p).await;

        let v = list_all(&mut h).await;
        let groups = v["result"]["groups"].as_array().unwrap();
        let ws: Vec<&str> = groups
            .iter()
            .map(|g| g["workspace"].as_str().unwrap())
            .collect();
        assert!(
            ws.contains(&p.as_str()),
            "the project group must exist: {ws:?}"
        );
        assert!(
            !ws.contains(&w.as_str()),
            "the worktree must not be a top-level group: {ws:?}"
        );

        let project_group = groups
            .iter()
            .find(|g| g["workspace"] == p.as_str())
            .unwrap();
        let ids: Vec<&str> = project_group["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["thread_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"t-plain"), "own thread stays: {ids:?}");
        assert!(ids.contains(&"t-card"), "card thread folds in: {ids:?}");
        let card = project_group["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["thread_id"] == "t-card")
            .unwrap();
        assert_eq!(card["card_id"], "card-1", "wire must expose card_id");
        assert_eq!(card["board_project"], p.as_str());
        h.shutdown().await;
    }

    /// 零回归:一个普通目录的会话仍按目录成组,顺序/存在性与今天一致。
    #[tokio::test(flavor = "multi_thread")]
    async fn plain_workspaces_still_group_by_directory() {
        let dir_a = tempfile::TempDir::new().unwrap();
        let dir_b = tempfile::TempDir::new().unwrap();
        let a = dir_a
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let b = dir_b
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        write_meta(Path::new(&a), "t-a", "a", None, None);
        write_meta(Path::new(&b), "t-b", "b", None, None);

        let mut h = Harness::new();
        initialize(&mut h).await;
        add_workspace(&mut h, 21, &a).await;
        add_workspace(&mut h, 22, &b).await;

        let v = list_all(&mut h).await;
        let groups = v["result"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "two plain dirs → two groups: {v}");
        assert_eq!(groups[0]["workspace"], b.as_str(), "most-recent first");
        assert_eq!(groups[1]["workspace"], a.as_str());
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_completion_persists_items_and_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = {
            let mut c = test_config();
            c.workdir = dir.path().to_path_buf();
            c
        };
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        let log = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        // 落盘是尽力而为且发生在 driver 写完 turn/completed 之后,故轮询等待。
        let mut text = String::new();
        for _ in 0..100 {
            match std::fs::read_to_string(&log) {
                Ok(t) if !t.trim().is_empty() => {
                    text = t;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        assert!(!text.is_empty(), "turn must be persisted");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "one turn = one line");

        // 第一行必须含合成的 userMessage item 与 agent 回复。
        assert!(
            text.contains(r#""type":"userMessage""#),
            "missing user item: {text}"
        );
        assert!(text.contains("hello"), "missing prompt text: {text}");
        assert!(
            text.contains(r#""type":"agentMessage""#),
            "missing agent item: {text}"
        );
        assert!(
            text.contains(r#""role":"User""#),
            "missing core message: {text}"
        );

        h.shutdown().await;
    }

    /// 崩溃残留：主 jsonl 有一轮，另有未收尾的 partial。resume 必须回放两轮的
    /// items，并以一条 interrupted 的 turn/completed 收尾。
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_flags_a_crashed_partial_turn_as_interrupted() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        // `Harness::new()` 的 workdir 是 test_config() 的默认路径，与下面 store
        // 用的 tempdir 不一致会让 resume 找不到该 thread，故必须走 with_config。
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        // 直接用 store 造盘面：一轮已落盘 + 一段 partial 残留。
        let store = crate::thread_store::ThreadStore::new(dir.path());
        let tid = "thread-crash";
        store
            .create(&crate::thread_store::ThreadMeta {
                thread_id: tid.into(),
                cwd: dir.path().to_string_lossy().to_string(),
                model: "m".into(),
                created_at: 1,
                updated_at: 1,
                title: None,
                permission_mode: crate::thread_store::ThreadMode::Normal,
                pin_seq: None,
                board_project: None,
                card_id: None,
            })
            .unwrap();
        store
            .append_turn(
                tid,
                &crate::thread_store::TurnLine::Turn {
                    items: vec![crate::protocol::Item::UserMessage {
                        id: "user-t1".into(),
                        text: "first".into(),
                    }],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        store
            .write_partial(
                tid,
                &crate::thread_store::PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![
                        crate::protocol::Item::UserMessage {
                            id: "user-turn-t2".into(),
                            text: "second".into(),
                        },
                        crate::protocol::Item::AgentMessage {
                            id: "item-turn-t2-1".into(),
                            text: "half".into(),
                        },
                    ],
                    messages: vec![],
                    usage: None,
                },
            )
            .unwrap();

        initialize(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        let mut saw_half = false;
        let mut interrupted = false;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
                && v["params"]["item"]["text"] == "half"
            {
                saw_half = true;
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed")
                && v["params"]["status"] == "interrupted"
            {
                interrupted = true;
                break;
            }
            if v.get("id") == Some(&serde_json::json!(5)) && interrupted {
                break;
            }
        }
        assert!(
            saw_half,
            "the crashed turn's finished items must be replayed"
        );
        assert!(
            interrupted,
            "a crashed partial turn must be flagged interrupted"
        );
        h.shutdown().await;
    }

    /// C1 回归：resume 采纳的崩溃轮必须被**升格**（append）进主 jsonl。否则该 partial
    /// 是这一轮唯一的持久副本，用户下一条消息的 turn-start checkpoint 会原子覆盖它，
    /// 崩溃轮的 items 永久丢失——而 session 上下文仍"记得"它们，造成 item/上下文不一致。
    ///
    /// 走完整链路：崩溃（一轮 jsonl + 一段 partial）→ resume（回放 + interrupted，
    /// 且必须已升格）→ 第二个 turn 正常收尾 → store 冷 load：崩溃轮 items 仍在、
    /// 只一份、不重复。
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_promotes_the_crashed_turn_so_a_later_turn_cannot_drop_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let store = crate::thread_store::ThreadStore::new(dir.path());
        let tid = "thread-promote";
        store
            .create(&crate::thread_store::ThreadMeta {
                thread_id: tid.into(),
                cwd: dir.path().to_string_lossy().to_string(),
                model: "m".into(),
                created_at: 1,
                updated_at: 1,
                title: None,
                permission_mode: crate::thread_store::ThreadMode::Normal,
                pin_seq: None,
                board_project: None,
                card_id: None,
            })
            .unwrap();
        // 一轮正常收尾过的历史。
        store
            .append_turn(
                tid,
                &crate::thread_store::TurnLine::Turn {
                    items: vec![crate::protocol::Item::UserMessage {
                        id: "user-t1".into(),
                        text: "first".into(),
                    }],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        // 崩溃残留：这一轮只有 partial，jsonl 里没有它的首 item id。
        store
            .write_partial(
                tid,
                &crate::thread_store::PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![
                        crate::protocol::Item::UserMessage {
                            id: "user-turn-t2".into(),
                            text: "second".into(),
                        },
                        crate::protocol::Item::AgentMessage {
                            id: "item-turn-t2-1".into(),
                            text: "half".into(),
                        },
                    ],
                    messages: vec![],
                    usage: None,
                },
            )
            .unwrap();

        initialize(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        // resume 必须回放崩溃轮的 items 并标注 interrupted，随后响应。
        let mut saw_half = false;
        let mut interrupted = false;
        let mut resumed = false;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed")
                && v["params"]["item"]["text"] == "half"
            {
                saw_half = true;
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed")
                && v["params"]["status"] == "interrupted"
            {
                interrupted = true;
            }
            if v.get("id") == Some(&serde_json::json!(5)) {
                resumed = true;
                break;
            }
        }
        assert!(
            saw_half,
            "the crashed turn's finished items must be replayed"
        );
        assert!(
            interrupted,
            "a crashed partial turn must be flagged interrupted"
        );
        assert!(resumed, "resume must respond");

        // 第二个 turn：正常走完。它的 turn-start checkpoint 会覆盖盘上的 partial
        // 文件——若 resume 没有先升格，这一步就是崩溃轮数据的丢失点。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"third"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                assert_eq!(v["params"]["status"], "completed", "second turn: {v}");
                break;
            }
        }

        // 落盘是尽力而为且发生在 driver 收尾之后（见既有持久化测试），故轮询等待。
        let log = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        let mut text = String::new();
        for _ in 0..100 {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("item-turn-t2-1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            text.contains("item-turn-t2-1") && text.contains("half"),
            "the recovered crashed turn must survive a later turn on disk; \
             log now contains {} turn(s): {text}",
            text.lines().filter(|l| !l.trim().is_empty()).count()
        );

        // 冷 load：崩溃轮的内容仍在，且只一份。
        let loaded = store.load(tid).unwrap().unwrap();
        let ids: Vec<String> = loaded
            .items
            .iter()
            .map(|i| crate::server::item_id(i).unwrap().to_string())
            .collect();
        assert_eq!(
            &ids[..3],
            &["user-t1", "user-turn-t2", "item-turn-t2-1"],
            "the crashed turn must be first and intact: {ids:?}"
        );
        assert_eq!(
            ids.iter().filter(|i| *i == "item-turn-t2-1").count(),
            1,
            "the crashed turn must not be double-counted: {ids:?}"
        );
        assert!(
            !loaded.pending_turn,
            "promotion must clear the pending flag"
        );
        h.shutdown().await;
    }

    /// I1 回归：等待审批期间**发起客户端断连**（真实的重连场景，不是崩溃）时，
    /// driver 就此收尾并 `return`，必须清掉 checkpoint；否则盘上留一条已结束 turn
    /// 的残留，重启后 resume 会把最后这一轮误报为 interrupted。
    ///
    /// 直接以「发起方已在 hub 上注销」接线 `is_connected` 判据；生产里发起方是
    /// stdio 的 `local`。
    #[tokio::test(flavor = "multi_thread")]
    async fn driver_clears_the_checkpoint_when_the_initiator_disconnects_during_approval() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(dir.path()));
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        let _keep_session_tx = session_tx;
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);

        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        // 发起方：注册后立刻摘除 = 断连。
        let initiator = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
        let _init_rx = hub.register(initiator.clone());
        hub.unregister(&initiator);

        let built = build_permission_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let handle = tokio::spawn(run_thread_driver(
            "thread-disconnect".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            hub,
            initiator,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            Arc::clone(&store),
            ThreadSession::new_status(),
            built.provider,
            built.config,
        ));

        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
                activate: None,
            })
            .await
            .unwrap();

        // 审批请求命中"发起方已断连"分支 → 收尾并 return。
        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .expect("driver must report Finished when the initiator is gone")
            .expect("turn channel must stay open");
        assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

        // clear_partial 发生在 Finished 之后；轮询等待（文件可能已被删除）。
        let partial = dir
            .path()
            .join(".yi-agent/threads/thread-disconnect.partial.json");
        for _ in 0..100 {
            if !partial.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !partial.exists(),
            "a disconnect during approval must clear the turn checkpoint"
        );

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_replays_history_and_restores_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_factory = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          _cwd: &std::path::Path,
                          _mode: crate::thread_store::ThreadMode| {
            let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RecordingProvider {
                seen: Arc::clone(&seen_factory),
            });
            let config = yi_agent_core::AgentConfig::default();
            let mut agent = yi_agent_core::Agent::new(
                provider.clone(),
                Arc::new(yi_agent_core::ToolRegistry::new()),
                config.clone(),
            );
            apply_session(&mut agent, session);
            Ok(BuiltAgent {
                agent,
                provider,
                config,
                decision_tx: None,
                decision_rx: None,
                catalog: None,
                yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
                process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            })
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // turn 1
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        // resume:期望 thread/started → item/completed(含 user + agent) → 响应
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut replayed: Vec<String> = Vec::new();
        let mut item_ids: Vec<String> = Vec::new();
        let mut resumed = false;
        for _ in 0..12 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("thread/started") => {}
                Some("item/completed") => {
                    replayed.push(v["params"]["item"]["type"].as_str().unwrap().to_string());
                    item_ids.push(v["params"]["item"]["id"].as_str().unwrap().to_string());
                }
                _ => {}
            }
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["result"]["thread_id"], tid);
                resumed = true;
                break;
            }
        }
        assert!(resumed, "thread/resume must respond");
        assert!(
            replayed.contains(&"userMessage".to_string()),
            "replay must include the user item: {replayed:?}"
        );
        assert!(
            replayed.contains(&"agentMessage".to_string()),
            "replay must include the agent item: {replayed:?}"
        );

        // turn 2:provider 应看到恢复后的完整上下文(user + assistant + 新 user)
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"second"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
                item_ids.push(v["params"]["item"]["id"].as_str().unwrap().to_string());
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        let counts = seen.lock().unwrap().clone();
        assert_eq!(
            counts,
            vec![1, 3],
            "resumed turn must carry prior context (user + assistant + new user): {counts:?}"
        );

        // 回归:resume 后新 turn 的 item id 不得与回放的历史 id 冲突,
        // 否则前端按 id upsert 会覆盖历史。
        let mut unique = item_ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            item_ids.len(),
            "item ids must be unique across replay + resumed turn: {item_ids:?}"
        );

        h.shutdown().await;
    }

    /// 发 thread/clear，被 `-32012` 拒时重试。
    ///
    /// 主循环要等 `TurnEvent::Finished` 才清 `active_turn_id`，而 driver 是在
    /// `turn/completed` 之后（落盘之后）才发该事件；所以刚结束一轮就立刻 clear
    /// 可能撞上这个窗口。这是已接受的残留限制（方案 A），客户端重试即可。
    async fn clear_thread_retrying(h: &mut Harness, tid: &str) -> serde_json::Value {
        for attempt in 0..20 {
            let id = 100 + attempt;
            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
            ))
            .await;
            loop {
                let v = h.read_value().await;
                if v.get("id") == Some(&serde_json::json!(id)) {
                    if v["error"]["code"] != -32012 {
                        return v;
                    }
                    break; // 还在 turn 收尾窗口内：等一会儿再试
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
        panic!("thread/clear kept answering -32012");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_clear_empties_the_context_and_resume_does_not_revive_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // 先跑一轮，制造可被清空的上下文；必须等本轮真正收尾（日志已 append 且
        // turn/completed 已到）再发 clear——落盘发生在 turn/completed 之后,
        // 早发会被主循环以 -32012（turn 进行中）拒绝。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        let log = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        let mut persisted = false;
        for _ in 0..200 {
            if let Ok(t) = std::fs::read_to_string(&log) {
                if !t.trim().is_empty() {
                    persisted = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(persisted, "turn must be persisted before we clear");
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        // clear（可能撞上收尾窗口，重试即可）。
        let v = clear_thread_retrying(&mut h, &tid).await;
        assert!(
            v.get("error").is_none(),
            "clear must eventually succeed: {v}"
        );

        // 关键回归：resume 不得把旧消息回放回来。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut replayed = Vec::new();
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
                replayed.push(
                    v["params"]["item"]["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                );
            }
            if v.get("id") == Some(&serde_json::json!(5)) {
                assert!(v.get("error").is_none(), "resume must still work: {v}");
                break;
            }
        }
        assert!(
            !replayed.iter().any(|t| t == "hello"),
            "cleared context must not come back on resume: {replayed:?}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_clear_rejects_an_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/clear","params":{"threadId":"nope"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
        h.shutdown().await;
    }

    /// 构造一份「可压缩」的测试 agent 工厂:初始 session 有 3 条消息
    /// (user/assistant/user),压缩后会变成 2 条。
    ///
    /// 注意只放 1 条 user 消息是**不可压缩**的:`plan_compaction` 会把历史里
    /// 所有 user 消息合并成一条,消息数不减少即返回 `None`(→ `not_reduced`)。
    fn compactable_factory(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        let session = session.unwrap_or_else(|| {
            let mut s = yi_agent_core::Session::new();
            s.replace_messages(vec![
                yi_agent_core::Message::user("first"),
                yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                    "reply".into(),
                )]),
                yi_agent_core::Message::user("second"),
            ]);
            s
        });
        Ok(BuiltAgent {
            agent: yi_agent_core::Agent::new(
                provider.clone(),
                Arc::new(yi_agent_core::ToolRegistry::new()),
                config.clone(),
            )
            .with_session(session),
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    /// 起一个 thread,然后调 `thread/compact`,返回响应的信封。
    async fn compact_thread(h: &mut Harness, tid: &str) -> serde_json::Value {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        for _ in 0..8 {
            let v = h.read_value().await;
            // 跳过沿线可能出现的通知(thread/status/updated 等),只等响应。
            if v.get("id") == Some(&serde_json::json!(9)) {
                return v;
            }
        }
        panic!("thread/compact must respond");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_reports_compacted_when_history_shrinks() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, compactable_factory, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let v = compact_thread(&mut h, &tid).await;
        assert_eq!(
            v["result"]["status"], "compacted",
            "expected compaction: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_reports_not_reduced_for_a_short_history() {
        let mut h = Harness::new(); // build_test_agent 的 session 为空
        let tid = start_thread(&mut h).await;
        let v = compact_thread(&mut h, &tid).await;
        assert_eq!(v["result"]["status"], "not_reduced", "expected no-op: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_reports_failure_without_breaking_the_connection() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let build = |session: Option<yi_agent_core::Session>,
                     _cwd: &std::path::Path,
                     _mode: crate::thread_store::ThreadMode| {
            let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(ErroringProvider);
            let config = yi_agent_core::AgentConfig::default();
            let mut session = session.unwrap_or_default();
            session.replace_messages(vec![
                yi_agent_core::Message::user("first"),
                yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                    "reply".into(),
                )]),
                yi_agent_core::Message::user("second"),
            ]);
            Ok(BuiltAgent {
                agent: yi_agent_core::Agent::new(
                    provider.clone(),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    config.clone(),
                )
                .with_session(session),
                provider,
                config,
                decision_tx: None,
                decision_rx: None,
                catalog: None,
                yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
                process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            })
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        let v = compact_thread(&mut h, &tid).await;
        assert_eq!(v["result"]["status"], "failed", "expected failure: {v}");
        assert!(
            v["result"]["error"].is_string(),
            "must carry the reason: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_compact_is_rejected_while_a_turn_is_running() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        // slow provider 期间会持续推 item/delta 通知,必须按 id 找到响应本身。
        let mut rejected = None;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                rejected = Some(v);
                break;
            }
        }
        let v = rejected.expect("thread/compact must respond");
        assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_clear_is_rejected_while_a_turn_is_running() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 等到 turn 真正开始（turn/started）再发 clear。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        // slow provider 期间会持续推 item/delta 通知,必须按 id 找到响应本身。
        let mut rejected = None;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                rejected = Some(v);
                break;
            }
        }
        let v = rejected.expect("thread/clear must respond");
        assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
        h.shutdown().await;
    }

    /// Task 9:resume 必须读取该 thread 持久化的 `permission_mode` 并透传给 agent
    /// 工厂,使 yolo 线程重开后仍以 yolo 重建。工厂内部 `mode → cfg.yolo → 共享
    /// YoloSwitch` 的映射由 `bootstrap.rs::interactive_yolo_config_starts_switch_on`
    /// 钉死;此处只钉死「app-server 侧 resume 读了持久化模式并透传」这一契约
    /// (server 内存中的 switch 不可从外部观测,故用记录型工厂观察传参)。
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_passes_persisted_mode_to_factory() {
        use crate::thread_store::{ThreadMode, ThreadStore};

        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();

        let seen: Arc<std::sync::Mutex<Vec<ThreadMode>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          cwd: &std::path::Path,
                          mode: ThreadMode| {
            seen_c.lock().unwrap().push(mode);
            build_test_agent(session, cwd, mode)
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // thread/start 必须走 Normal。
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[ThreadMode::Normal],
            "thread/start must build with Normal"
        );

        // 绕过 RPC,直接把该线程持久化的 permission_mode 改成 Yolo。
        let store = ThreadStore::new(dir.path());
        assert!(
            store.set_permission_mode(&tid, ThreadMode::Yolo).unwrap(),
            "thread meta must exist after thread/start"
        );

        // resume 应读取持久化的 Yolo 并透传给工厂。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut resumed = false;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["result"]["thread_id"], tid);
                resumed = true;
                break;
            }
        }
        assert!(resumed, "thread/resume must respond");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.last(),
            Some(&ThreadMode::Yolo),
            "resume must pass the persisted mode to the factory: {seen:?}"
        );
        h.shutdown().await;
    }

    /// Task 10:`thread/setPermissionMode` 必须实时翻转该线程的 `YoloSwitch`
    /// (运行期立即生效)并把 `permission_mode` 落盘(resume 后仍读得到)。
    /// server 内存里的 switch 外部不可观测,故用记录型工厂捕获工厂实际交给
    /// 该线程的同一个 `YoloSwitch`,以它断言运行期开关被翻转。
    #[tokio::test(flavor = "multi_thread")]
    async fn set_permission_mode_toggles_and_persists() {
        use crate::thread_store::{ThreadMode, ThreadStore};

        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();

        let seen: Arc<std::sync::Mutex<Vec<yi_agent_core::autonomy::YoloSwitch>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          cwd: &std::path::Path,
                          mode: ThreadMode| {
            let sw = yi_agent_core::autonomy::YoloSwitch::new(mode == ThreadMode::Yolo);
            seen_c.lock().unwrap().push(sw.clone());
            let mut built = build_test_agent(session, cwd, mode)?;
            built.yolo = sw;
            Ok(built)
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        let sw = seen.lock().unwrap().last().cloned().unwrap();
        assert!(!sw.get(), "a freshly started thread must not be in yolo");

        // 切到 yolo:响应成功,运行期开关立即为 true,且已落盘。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert!(
            v.get("error").is_none(),
            "setPermissionMode must succeed: {v}"
        );
        assert_eq!(v["result"], serde_json::json!({}));
        assert!(sw.get(), "the thread's YoloSwitch must turn on immediately");
        let store = ThreadStore::new(dir.path());
        assert_eq!(
            store
                .load(&tid)
                .unwrap()
                .expect("thread meta must exist")
                .meta
                .permission_mode,
            ThreadMode::Yolo,
            "yolo must persist to disk"
        );

        // 切回 normal:开关关掉,落盘也回到 normal。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"normal"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert!(
            v.get("error").is_none(),
            "setPermissionMode must succeed: {v}"
        );
        assert!(
            !sw.get(),
            "the thread's YoloSwitch must turn off immediately"
        );
        assert_eq!(
            store
                .load(&tid)
                .unwrap()
                .expect("thread meta must exist")
                .meta
                .permission_mode,
            ThreadMode::Normal,
            "normal must persist to disk"
        );

        h.shutdown().await;
    }

    /// Task 10:非法 `mode` 报 `-32602`(且优先于 thread 存在性校验);未知
    /// `threadId` 报 `-32011`。两者分别用独立请求覆盖。
    #[tokio::test(flavor = "multi_thread")]
    async fn set_permission_mode_rejects_bad_mode_and_unknown_thread() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // 非法 mode:即便 thread 真实存在也必须 `-32602`。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"bogus"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "invalid mode must be invalid_params: {v}"
        );

        // 未知 thread + 合法 mode → `-32011`。
        h.send(
            r#"{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{"threadId":"nope","mode":"yolo"}}"#,
        )
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32011,
            "unknown thread must be unknown_thread: {v}"
        );

        // 非法 mode + 未知 thread → 仍是 `-32602`:证明 mode 校验先于 thread
        // 存在性,不会因 threadId 未知而退化成 `-32011`。
        h.send(
            r#"{"jsonrpc":"2.0","id":5,"method":"thread/setPermissionMode","params":{"threadId":"nope","mode":"bogus"}}"#,
        )
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "invalid mode must win over thread existence: {v}"
        );

        // 缺少 threadId → `-32602`(invalid params)。
        h.send(
            r#"{"jsonrpc":"2.0","id":6,"method":"thread/setPermissionMode","params":{"mode":"yolo"}}"#,
        )
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "missing threadId must be invalid_params: {v}"
        );

        // 非字符串 mode(数字)→ `-32602`:`as_str()` 落空,走 `_` 分支。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":5}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(7)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "non-string mode must be invalid_params: {v}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_unknown_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/resume","params":{"threadId":"nope"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    /// Design §8:resume 的目标目录已不存在时必须明确报错,不得静默回退 `$HOME`。
    /// 对内存中的 thread,其权威 cwd 已知(见 `ThreadSession`),故这是可达且必须
    /// 报错的路径。冷 thread 的元数据本就存在 workspace 目录内,目录删除后不可读,
    /// 那种情况仍走 `-32011`,不在此覆盖。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_missing_cwd_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();

        let mut cfg = test_config();
        cfg.workdir = cwd.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": cwd.to_string_lossy() }});
        h.send(&req.to_string()).await;
        let tid = read_thread_start_response(&mut h, 2).await;

        // 目录从磁盘消失:thread 仍在内存,但 cwd 已不可用。
        std::fs::remove_dir_all(&cwd).unwrap();

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/resume must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "resume into a vanished cwd must be invalid_params, not a silent fallback: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_ops_target_thread_cwd_not_global_workdir() {
        // 两个不同目录:thread 建在 `d`,全局 workdir 在 `w`。用于锁住
        // rename/delete 走 thread 自身 cwd,而不是全局 workdir / 索引回退。
        let w = tempfile::TempDir::new().unwrap();
        let d = tempfile::TempDir::new().unwrap();
        let w_dir = w.path().canonicalize().unwrap();
        let d_dir = d.path().canonicalize().unwrap();

        let mut cfg = test_config();
        cfg.workdir = w_dir.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let start = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": { "cwd": d_dir.to_string_lossy() },
        });
        h.send(&start.to_string()).await;
        let mut tid = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                assert!(v.get("error").is_none(), "thread/start must succeed: {v}");
                tid = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
                break;
            }
        }
        let tid = tid.expect("no thread/start response");

        // 把 `d` 从全局索引移除。此后只有「内存中该 thread 的权威 store」还能
        // 指向 `d`;若 rename/delete 退化成 store_for(索引 → cfg.workdir),
        // 就会落到全局 workdir `w` 上,从而暴露路由回归。
        let remove = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/remove",
            "params": { "path": d_dir.to_string_lossy() },
        });
        h.send(&remove.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert!(
            v.get("error").is_none(),
            "workspace/remove must succeed: {v}"
        );

        // rename 必须落在 `d`,而不是全局 workdir `w`。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/rename","params":{{"threadId":"{tid}","title":"scoped"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 4);
        assert!(v.get("error").is_none(), "rename must succeed: {v}");

        let in_d = crate::thread_store::ThreadStore::new(&d_dir)
            .load(&tid)
            .expect("load in d must not fail")
            .expect("thread must live in d");
        assert_eq!(in_d.meta.title.as_deref(), Some("scoped"));
        assert!(
            !crate::thread_store::ThreadStore::new(&w_dir).exists(&tid),
            "nothing must leak into the global workdir"
        );

        // delete 同样必须作用在 `d`。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 5);
        assert!(v.get("error").is_none(), "delete must succeed: {v}");
        assert!(
            !crate::thread_store::ThreadStore::new(&d_dir).exists(&tid),
            "thread must be actually deleted in d"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_updates_title() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"my chat"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert!(v.get("error").is_none(), "rename must succeed: {v}");

        let meta = std::fs::read_to_string(
            dir.path()
                .join(".yi-agent/threads")
                .join(format!("{tid}.meta.json")),
        )
        .unwrap();
        assert!(
            meta.contains("my chat"),
            "meta must carry the title: {meta}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_rejects_empty_title() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"   "}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(
            v["error"]["code"], -32602,
            "blank title must be rejected: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_unknown_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/rename","params":{"threadId":"nope","title":"x"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_delete_removes_files_and_unknown_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert!(v.get("error").is_none(), "delete must succeed: {v}");
        assert!(
            !dir.path()
                .join(".yi-agent/threads")
                .join(format!("{tid}.meta.json"))
                .exists(),
            "meta must be gone"
        );

        // 再次删除:thread 已完全未知 → -32011。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_delete_active_thread_removes_it_from_memory() {
        // 用隔离 workdir:删除活跃 thread 时必须能断言磁盘状态。
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 等 turn 活跃。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut deleted = false;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert!(v.get("error").is_none(), "delete must succeed: {v}");
                deleted = true;
                break;
            }
        }
        assert!(deleted, "thread/delete must respond");

        // 留出窗口让「未等待落盘」的实现迟到写入:driver 收到中断后仍会
        // append_turn,若 delete 抢在它之前删文件,日志会被复活。
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 删除后磁盘上两个文件都必须消失(活跃 turn 落盘不得复活 .jsonl)。
        let threads_dir = dir.path().join(".yi-agent/threads");
        assert!(
            !threads_dir.join(format!("{tid}.meta.json")).exists(),
            "meta must be gone after deleting an active thread"
        );
        assert!(
            !threads_dir.join(format!("{tid}.jsonl")).exists(),
            "log must be gone after deleting an active thread"
        );

        // 删除后该 thread 已不在内存:再发 turn 应得 -32011。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"x"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                assert_eq!(
                    v["error"]["code"], -32011,
                    "deleted thread must be unknown: {v}"
                );
                break;
            }
        }

        // 磁盘无日志 → resume 不能复活已删除的 thread。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                assert_eq!(
                    v["error"]["code"], -32011,
                    "resume must not resurrect a deleted thread: {v}"
                );
                break;
            }
        }
        h.shutdown().await;
    }

    /// 读到下一个指定 method 的通知(忽略其它帧)。
    async fn read_until_method(h: &mut Harness, method: &str) -> serde_json::Value {
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                return v;
            }
        }
        panic!("expected a {method} notification");
    }

    /// 正常一轮结束后,状态序列为 running → idle。
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_emits_running_then_idle_status() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let running = read_until_method(&mut h, "thread/status/updated").await;
        assert_eq!(running["params"]["thread_id"], tid);
        assert_eq!(running["params"]["status"], "running");

        let idle = loop {
            let v = read_until_method(&mut h, "thread/status/updated").await;
            if v["params"]["status"] == "idle" {
                break v;
            }
        };
        assert_eq!(idle["params"]["thread_id"], tid);
        h.shutdown().await;
    }

    /// 两个 thread 的 turn 可同时在跑:各自的 listAll 状态同为 running。
    #[tokio::test(flavor = "multi_thread")]
    async fn two_threads_run_turns_concurrently() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let a = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{}}"#)
            .await;
        let b = read_thread_start_response(&mut h, 4).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{a}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{b}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 两个 turn/started 都应到达(无跨 thread 的 -32012)。
        let mut started = std::collections::HashSet::new();
        for _ in 0..20 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                started.insert(v["params"]["thread_id"].as_str().unwrap().to_string());
            }
            if started.contains(&a) && started.contains(&b) {
                break;
            }
        }
        assert!(
            started.contains(&a) && started.contains(&b),
            "both turns must start: {started:?}"
        );

        // 此刻 listAll 里两个 thread 都是 running。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
            .await;
        let all = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                break v;
            }
        };
        let status_of = |id: &str| -> String {
            all["result"]["groups"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["threads"].as_array().unwrap())
                .find(|t| t["thread_id"] == id)
                .map(|t| t["status"].as_str().unwrap().to_string())
                .expect("thread must be listed")
        };
        assert_eq!(status_of(&a), "running");
        assert_eq!(status_of(&b), "running");

        // 收尾:中断两个 turn 以便优雅退出。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"turn/interrupt","params":{{"threadId":"{a}"}}}}"#
        ))
        .await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":8,"method":"turn/interrupt","params":{{"threadId":"{b}"}}}}"#
        ))
        .await;
        h.shutdown().await;
    }

    /// 审批超时后状态必须离开 awaiting_approval(前端据此清掉残留审批框)。
    #[tokio::test(flavor = "multi_thread")]
    async fn approval_timeout_leaves_awaiting_status() {
        let mut h = Harness::with_factory(build_permission_agent, Duration::from_millis(150));
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 应先看到 awaiting_approval。
        let mut saw_awaiting = false;
        // 之后必须离开 awaiting_approval(超时 → 回 running → idle)。
        let mut left_awaiting = false;
        for _ in 0..60 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("thread/status/updated") {
                match v["params"]["status"].as_str().unwrap() {
                    "awaiting_approval" => saw_awaiting = true,
                    _ if saw_awaiting => {
                        left_awaiting = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_awaiting, "must enter awaiting_approval");
        assert!(left_awaiting, "must leave awaiting_approval after timeout");
        h.shutdown().await;
    }

    /// cold thread（未 resume）在 listAll 里状态为 idle。
    #[tokio::test(flavor = "multi_thread")]
    async fn cold_thread_reports_idle() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await; // 已 start，但未 resume、未起 turn
        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#)
            .await;
        let all = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                break v;
            }
        };
        let listed = all["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap())
            .find(|t| t["thread_id"] == tid.as_str())
            .expect("thread must be listed");
        assert_eq!(listed["status"], "idle");
        h.shutdown().await;
    }

    #[tokio::test]
    async fn process_list_is_empty_for_a_fresh_thread_and_for_an_unknown_one() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;

        // 新 thread:没有进程,列表为空(不是错误)。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
        ))
        .await;
        let listed = h.read_value().await;
        assert_eq!(listed["id"], 3);
        assert!(listed["error"].is_null(), "{listed}");
        assert_eq!(listed["result"]["processes"].as_array().unwrap().len(), 0);

        // 未知 thread:仍返回空列表而非错误(切走再切回是正常路径)。
        h.send(
            r#"{"jsonrpc":"2.0","id":4,"method":"process/list","params":{"thread_id":"thread-nope"}}"#,
        )
        .await;
        let unknown = h.read_value().await;
        assert_eq!(unknown["id"], 4);
        assert!(unknown["error"].is_null(), "{unknown}");
        assert_eq!(unknown["result"]["processes"].as_array().unwrap().len(), 0);

        h.shutdown().await;
    }

    #[tokio::test]
    async fn process_read_reports_an_unknown_process_as_an_error() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"proc_999"}}}}"#
        ))
        .await;
        let bad = h.read_value().await;
        assert_eq!(bad["id"], 3);
        assert!(bad["result"].is_null(), "{bad}");
        assert!(
            bad["error"]["message"]
                .as_str()
                .unwrap()
                .contains("process not found")
        );

        h.shutdown().await;
    }

    /// 生效的那一份 manager 才被看见:thread 必须采用工厂给出的 manager,而不是
    /// 自建一份——否则进程面板显示空列表,而 agent 明明能起进程(设计 §4.2 的陷阱)。
    ///
    /// 顺带验证 `process/read` 的游标增量语义:两次读不重不漏。
    ///
    /// 隔离:`cfg.provider` 设成非 anthropic/openai,委派装配会在 `worker_factory` 处
    /// 确定性失败,`thread/start` 因而**不会**经 `wrap_for_delegation` 换掉工厂给出的
    /// manager——本测试的前提(工厂那一份生效)才成立,且与 `/tmp` 残留状态无关。
    #[tokio::test]
    async fn process_list_and_read_observe_the_managers_the_factory_handed_over() {
        use std::sync::Arc;

        // 修正:既有 API `ProcessManager::new` 直接返回 `Arc<Self>`,不能再套 `Arc::new`。
        let held: Arc<yi_agent_tools::ProcessManager> =
            yi_agent_tools::ProcessManager::new(std::env::temp_dir());
        let for_factory = Arc::clone(&held);

        let mut cfg = test_config();
        cfg.provider = "t5-isolated".to_string();
        let mut h = Harness::with_config(
            cfg,
            move |session, cwd, mode| {
                let mut built = build_test_agent(session, cwd, mode)?;
                // 这一份才是「生效」的:thread 必须采用它。
                built.process_manager = Arc::clone(&for_factory);
                Ok(built)
            },
            PERMISSION_TIMEOUT,
        );
        let thread_id = start_thread(&mut h).await;

        // ready_pattern 让 start() 等到输出出现才返回,断言因此是确定性的。
        let started = held
            .start(yi_agent_tools::ProcessStartOptions {
                command: "printf alpha".into(),
                name: Some("t4-probe".into()),
                cwd: None,
                env: Default::default(),
                on_exit: Default::default(),
                ready_pattern: Some("alpha".into()),
                ready_timeout_sec: Some(5),
            })
            .await
            .expect("start");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/list","params":{{"thread_id":"{thread_id}"}}}}"#
        ))
        .await;
        let listed = read_response(&mut h, 3).await;
        assert!(listed["error"].is_null(), "{listed}");
        let names: Vec<&str> = listed["result"]["processes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert!(
            names.contains(&"t4-probe"),
            "thread must see the held manager: {listed}"
        );

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
            started.process_id
        ))
        .await;
        let first = read_response(&mut h, 4).await;
        assert!(first["error"].is_null(), "{first}");
        assert!(
            first["result"]["stdout"]
                .as_str()
                .unwrap()
                .contains("alpha"),
            "{first}"
        );
        let cursor = first["result"]["next_cursor"].as_u64().unwrap();
        assert!(cursor > 0, "{first}");

        // 从上一轮游标继续读:没有新输出(不重不漏)。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}","cursor":{cursor}}}}}"#,
            started.process_id
        ))
        .await;
        let second = read_response(&mut h, 5).await;
        assert!(second["error"].is_null(), "{second}");
        assert_eq!(second["result"]["stdout"].as_str().unwrap(), "", "{second}");

        let _ = held.shutdown().await;
        h.shutdown().await;
    }

    #[tokio::test]
    async fn process_kill_reports_an_unknown_process_as_an_error() {
        let mut h = Harness::new();
        let thread_id = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"process/kill","params":{{"thread_id":"{thread_id}","process_id":"proc_999"}}}}"#
        ))
        .await;
        let bad = read_response(&mut h, 3).await;
        assert!(bad["result"].is_null(), "{bad}");
        assert!(!bad["error"].is_null());

        h.shutdown().await;
    }

    /// kill 真的能终止进程,并把状态推到 `exited`/`killed`。
    ///
    /// 隔离:`cfg.provider` 设成非 anthropic/openai 时,委派装配会在 `worker_factory`
    /// 处**确定性**失败(不需要起 daemon、也不看 `/tmp` 残留状态),于是 `thread/start`
    /// 不会经 `wrap_for_delegation` 把工厂给出的 manager 换成新建的那一份——工厂那一份
    /// 才是本 thread 生效的 manager,正是本测试要验证的对象。
    #[tokio::test]
    async fn process_kill_terminates_a_held_manager_process() {
        use std::sync::Arc;

        for run in 0..8 {
            // 修正:既有 API `ProcessManager::new` 已返回 `Arc<Self>`,不能再套 `Arc::new`。
            let held: Arc<yi_agent_tools::ProcessManager> =
                yi_agent_tools::ProcessManager::new(std::env::temp_dir());
            let for_factory = Arc::clone(&held);
            let mut cfg = test_config();
            cfg.provider = "t5-isolated".to_string();
            let mut h = Harness::with_config(
                cfg,
                move |session, cwd, mode| {
                    let mut built = build_test_agent(session, cwd, mode)?;
                    built.process_manager = Arc::clone(&for_factory);
                    Ok(built)
                },
                PERMISSION_TIMEOUT,
            );
            let thread_id = start_thread(&mut h).await;

            let started = held
                .start(yi_agent_tools::ProcessStartOptions {
                    command: "sleep 300".into(),
                    name: Some("t5-probe".into()),
                    cwd: None,
                    env: Default::default(),
                    on_exit: Default::default(),
                    ready_pattern: None,
                    ready_timeout_sec: None,
                })
                .await
                .expect("start");

            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"process/kill","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
                started.process_id
            ))
            .await;
            let killed = read_response(&mut h, 3).await;
            assert!(killed["error"].is_null(), "run {run}: {killed}");
            assert_eq!(killed["result"]["ok"], true, "run {run}");

            // 状态必须落到终态之一,而不是仍显示 running。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let snap = held
                    .list()
                    .into_iter()
                    .find(|p| p.process_id == started.process_id)
                    .expect("process must still be listed after kill");
                let state = serde_json::to_value(&snap.status).unwrap();
                let label = state["state"].as_str().unwrap_or("").to_string();
                if label == "killed" || label == "exited" {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "process never reached a terminal state: {state}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }

            let _ = held.shutdown().await;
            h.shutdown().await;
        }
    }

    /// 回归(Task 5 缺陷):resume 一个**仍在内存的活 thread** 时,该 thread 生效的
    /// manager 会被新工厂产出的一份替换;守望者必须跟着换到新 manager 上。
    ///
    /// 修复前:`thread/resume` 的守望者启动被 `if !process_watches.contains_key(..)`
    /// 拦住,仍旧订阅**旧的、已被丢弃**的 manager。于是 `process/list` 读得到新
    /// manager 的进程,`process/updated` 却永远不来——面板静默失联。本测试正是抓这个:
    /// 对新建后未起 turn 的活 thread 直接 resume(得到生效 manager 数组下标 1),在其上
    /// 起进程,断言能从客户端流里读到 process_id 匹配的 `process/updated`。
    ///
    /// 隔离:`cfg.provider` 设成非 anthropic/openai,委派装配确定性失败,`thread/start`
    /// 与 `thread/resume` 都不会经 `wrap_for_delegation` 换掉工厂给出的 manager,因此
    /// 「工厂第 N 次产出的 manager 即该路径生效的 manager」成立。
    ///
    /// 收敛读带超时(见 `await_notification`),修复前是「等不到」而非挂死。
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_of_a_live_thread_rebinds_process_watcher_to_the_effective_manager() {
        use std::sync::Arc;

        // 工厂每次被调用都把「本次生效的 manager」推进这里:start 产出下标 0,
        // resume 产出下标 1(即 resume 后生效的那一份)。
        let seen: Arc<std::sync::Mutex<Vec<Arc<yi_agent_tools::ProcessManager>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let for_factory = Arc::clone(&seen);

        let mut cfg = test_config();
        cfg.provider = "t5-isolated".to_string();
        let mut h = Harness::with_config(
            cfg,
            move |session, cwd, mode| {
                let built = build_test_agent(session, cwd, mode)?;
                let manager = Arc::clone(&built.process_manager);
                for_factory.lock().unwrap().push(manager);
                Ok(built)
            },
            PERMISSION_TIMEOUT,
        );
        let thread_id = start_thread(&mut h).await;

        // 活 thread:未起 turn 时直接 resume,走的是「仍在内存中的活 thread」那条分支。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{thread_id}"}}}}"#
        ))
        .await;
        let resumed = read_response(&mut h, 3).await;
        assert_eq!(
            resumed["result"]["thread_id"],
            thread_id.as_str(),
            "{resumed}"
        );

        //resume 之后该 thread 生效的 manager 就是工厂第二次产出的那一份。
        let managers = seen.lock().unwrap().clone();
        assert_eq!(
            managers.len(),
            2,
            "factory must run once for start and once for resume: {}",
            managers.len()
        );
        let effective = Arc::clone(&managers[1]);

        // 在生效 manager 上起一个进程;守望者若正确重订,应推送 process/updated。
        let started = effective
            .start(yi_agent_tools::ProcessStartOptions {
                command: "sleep 300".into(),
                name: Some("t5-resume-probe".into()),
                cwd: None,
                env: Default::default(),
                on_exit: Default::default(),
                ready_pattern: None,
                ready_timeout_sec: None,
            })
            .await
            .expect("start");

        let note = await_notification(&mut h, "process/updated", Duration::from_secs(10))
            .await
            .unwrap_or_else(|| {
                panic!(
                    "resume 后仍未重订守望者:等不到 process/updated(process_id={})",
                    started.process_id
                )
            });
        assert_eq!(
            note["params"]["process_id"],
            started.process_id.as_str(),
            "{note}"
        );
        assert_eq!(note["params"]["thread_id"], thread_id.as_str(), "{note}");
        // `start` 返回时进程已在运行,守望者至少应报出一个非终态标签。
        let state = note["params"]["state"].as_str().unwrap_or("");
        assert!(
            matches!(state, "starting" | "running" | "ready"),
            "unexpected state on a running process: {note}"
        );

        let _ = effective.shutdown().await;
        h.shutdown().await;
    }

    fn pin_test_config(dir: &std::path::Path) -> RuntimeConfig {
        let mut c = test_config();
        c.workdir = dir.to_path_buf();
        c
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_pinned_puts_thread_on_top_of_list_all_pinned() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(
            cfg,
            |s, p, m| build_test_agent(s, p, m),
            Duration::from_secs(5),
        );
        initialize(&mut h).await;

        let start = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}});
        h.send(&start.to_string()).await;
        let tid = read_thread_start_response(&mut h, 2).await;

        let pin = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setPinned",
            "params":{"threadId": tid, "pinned": true}});
        h.send(&pin.to_string()).await;
        let resp = read_response(&mut h, 3).await;
        assert!(
            resp.get("error").is_none(),
            "setPinned must succeed: {resp}"
        );

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/listAll","params":{}}"#)
            .await;
        let v = read_response(&mut h, 4).await;
        let pinned = v["result"]["pinned"].as_array().expect("顶层 pinned 数组");
        assert_eq!(pinned.len(), 1, "恰一个置顶: {v}");
        assert_eq!(pinned[0]["thread_id"].as_str().unwrap(), tid);
        assert_eq!(pinned[0]["pinned"], true);

        // 仍在原分组内,且带 pinned:true。
        let all: Vec<&serde_json::Value> = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap().iter())
            .collect();
        let me = all
            .iter()
            .find(|t| t["thread_id"].as_str() == Some(tid.as_str()))
            .expect("in group");
        assert_eq!(me["pinned"], true);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_pinned_false_removes_from_pinned_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(
            cfg,
            |s, p, m| build_test_agent(s, p, m),
            Duration::from_secs(5),
        );
        initialize(&mut h).await;
        let start = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}});
        h.send(&start.to_string()).await;
        let tid = read_thread_start_response(&mut h, 2).await;

        for (id, pinned) in [(3u64, true), (4u64, false)] {
            let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/setPinned",
                "params":{"threadId": tid, "pinned": pinned}});
            h.send(&req.to_string()).await;
            let r = read_response(&mut h, id).await;
            assert!(r.get("error").is_none(), "{r}");
        }
        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/listAll","params":{}}"#)
            .await;
        let v = read_response(&mut h, 5).await;
        assert_eq!(v["result"]["pinned"].as_array().unwrap().len(), 0);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_pinned_rejects_non_boolean_and_unknown_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(
            cfg,
            |s, p, m| build_test_agent(s, p, m),
            Duration::from_secs(5),
        );
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/setPinned","params":{"threadId":"thread-x","pinned":"yes"}}"#).await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["error"]["code"], -32602, "非布尔 → invalid_params: {v}");

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/setPinned","params":{"threadId":"thread-x","pinned":true}}"#).await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(v["error"]["code"], -32011, "未知 id → unknown_thread: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reorder_pinned_rewrites_order_and_rejects_bad_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(
            cfg,
            |s, p, m| build_test_agent(s, p, m),
            Duration::from_secs(5),
        );
        initialize(&mut h).await;

        let mut tids = Vec::new();
        for id in [2u64, 3u64] {
            let start =
                serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start","params":{}});
            h.send(&start.to_string()).await;
            tids.push(read_thread_start_response(&mut h, id).await);
        }
        // 两个都置顶(后置顶的 tids[1] 在顶)。
        for (i, tid) in tids.iter().enumerate() {
            let req = serde_json::json!({"jsonrpc":"2.0","id":10+i as u64,"method":"thread/setPinned",
                "params":{"threadId": tid, "pinned": true}});
            h.send(&req.to_string()).await;
            let r = read_response(&mut h, 10 + i as u64).await;
            assert!(r.get("error").is_none(), "{r}");
        }
        // 反转顺序：tids[0] 放到最顶。
        let rev = serde_json::json!({"jsonrpc":"2.0","id":20,"method":"thread/reorderPinned",
            "params":{"threadIds": tids}});
        h.send(&rev.to_string()).await;
        let r = read_response(&mut h, 20).await;
        assert!(r.get("error").is_none(), "reorder must succeed: {r}");

        h.send(r#"{"jsonrpc":"2.0","id":21,"method":"thread/listAll","params":{}}"#)
            .await;
        let v = read_response(&mut h, 21).await;
        let pinned = v["result"]["pinned"].as_array().unwrap();
        assert_eq!(pinned[0]["thread_id"].as_str().unwrap(), tids[0]);

        // 含未置顶 id → invalid_params。
        let bad = serde_json::json!({"jsonrpc":"2.0","id":22,"method":"thread/reorderPinned",
            "params":{"threadIds": ["thread-nope"]}});
        h.send(&bad.to_string()).await;
        let v = read_response(&mut h, 22).await;
        assert_eq!(
            v["error"]["code"], -32602,
            "未置顶 id → invalid_params: {v}"
        );

        // 重复 id → invalid_params。
        let dup = serde_json::json!({"jsonrpc":"2.0","id":23,"method":"thread/reorderPinned",
            "params":{"threadIds": [tids[0].clone(), tids[0].clone()]}});
        h.send(&dup.to_string()).await;
        let v = read_response(&mut h, 23).await;
        assert_eq!(v["error"]["code"], -32602, "重复 id → invalid_params: {v}");
        h.shutdown().await;
    }

    /// A `Control` client (a paired phone) must not be able to run admin-class
    /// RPCs; `thread/delete` is the canonical one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_control_client_cannot_delete_a_thread() {
        let mut h = Harness::with_scope(Scope::Control).await;
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 9).await;
        assert_eq!(
            v["error"]["code"], -32014,
            "admin op from a control client: {v}"
        );
        h.shutdown().await;
    }

    /// The desktop stdio client is `Admin`: the same call must go through.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_admin_client_may_delete_a_thread() {
        let mut h = Harness::with_scope(Scope::Admin).await;
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 9).await;
        assert!(
            v["result"].is_object(),
            "admin client must be allowed to delete: {v}"
        );
        h.shutdown().await;
    }

    /// The deliberately-extended admin gate: minting a pairing code
    /// (`pair/create`) or kicking a device (`device/revoke`) is desktop-privileged,
    /// so a `Control` phone must be rejected from both too.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_control_client_cannot_pair_or_revoke() {
        let mut h = Harness::with_scope(Scope::Control).await;
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"pair/create","params":{}}"#)
            .await;
        let v = read_response(&mut h, 9).await;
        assert_eq!(
            v["error"]["code"], -32014,
            "pair/create from a control client: {v}"
        );
        h.send(r#"{"jsonrpc":"2.0","id":10,"method":"device/revoke","params":{"device_id":"d1"}}"#)
            .await;
        let v = read_response(&mut h, 10).await;
        assert_eq!(
            v["error"]["code"], -32014,
            "device/revoke from a control client: {v}"
        );
        h.shutdown().await;
    }

    /// Frame-level pairing (`pair/redeem`) is the path that works *through the
    /// relay*: the relay forwards ws frames but never the upgrade query string,
    /// so a phone's `?pair=` cannot reach the local app-server. A code minted
    /// here must redeem over a normal (non-admin) initialized connection.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_control_client_can_redeem_a_code_with_pair_redeem() {
        let mut h = Harness::with_scope(Scope::Control).await;
        initialize(&mut h).await;
        // Desktop mints the code (same PairingState as the serving loop here).
        let code = h.pairing().create_code().code;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"pair/redeem","params":{{"code":"{code}","device_name":"iPhone"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 9).await;
        let token = v["result"]["token"]
            .as_str()
            .expect("redeem must return a token");
        assert!(token.starts_with("yia_"), "unexpected token: {token}");
        assert_eq!(v["result"]["scope"], "control");

        // A bad code is refused with the pairing-code error, not a crash.
        h.send(r#"{"jsonrpc":"2.0","id":10,"method":"pair/redeem","params":{"code":"NOPE-0000"}}"#)
            .await;
        let v = read_response(&mut h, 10).await;
        assert_eq!(v["error"]["code"], -32001, "bad code must be refused: {v}");
        h.shutdown().await;
    }

    /// `thread/subscribe` 整体替换订阅集合并回显;超上限报 invalid_params。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_can_subscribe_to_a_thread_set() {
        let mut h = Harness::with_scope(Scope::Control).await;
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/subscribe","params":{"threadIds":["t1","t2"]}}"#)
            .await;
        let v = read_response(&mut h, 9).await;
        assert_eq!(v["result"]["subscribed"].as_array().unwrap().len(), 2);
        assert!(h.hub().has_subscribed_clients(), "订阅后 hub 必须认得");

        // 超过 16 → invalid_params。
        let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        let req = serde_json::json!({"jsonrpc":"2.0","id":10,"method":"thread/subscribe","params":{"threadIds":many}});
        h.send(&req.to_string()).await;
        let v = read_response(&mut h, 10).await;
        assert_eq!(v["error"]["code"], -32602, "超上限必须报错: {v}");
        h.shutdown().await;
    }

    /// 通知按 thread 键路由：订阅 t1 的客户端只收 t1，未订阅的收全部。
    #[tokio::test(flavor = "multi_thread")]
    async fn notifications_are_routed_by_thread_key() {
        let hub = crate::broadcast::Broadcaster::new();
        let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
        let all = crate::broadcast::ClientId::local();
        let mut sub_rx = hub.register(sub.clone());
        let mut all_rx = hub.register(all.clone());
        hub.subscribe(&sub, vec!["t1".to_string()]);

        let content_t1 = serde_json::json!({"method":"turn/started","params":{"thread_id":"t1"}});
        let content_t2 = serde_json::json!({"method":"turn/started","params":{"thread_id":"t2"}});
        hub.broadcast_for(Some("t1"), content_t1);
        hub.broadcast_for(Some("t2"), content_t2);

        assert_eq!(sub_rx.recv().await.unwrap()["params"]["thread_id"], "t1");
        assert!(sub_rx.try_recv().is_err(), "订阅 t1 不该收到 t2");
        assert_eq!(all_rx.recv().await.unwrap()["params"]["thread_id"], "t1");
        assert_eq!(all_rx.recv().await.unwrap()["params"]["thread_id"], "t2");
    }

    /// `Notification::thread_key` 与协议字段一致（全局帧无键）。
    #[test]
    fn notification_thread_key_matches_the_wire() {
        use crate::protocol::Notification;
        assert_eq!(
            Notification::TurnStarted {
                thread_id: "t1".into(),
                turn_id: "x".into()
            }
            .thread_key(),
            Some("t1")
        );
        assert_eq!(
            Notification::UiSettingsUpdated {
                theme: "dark".into()
            }
            .thread_key(),
            None
        );
        assert_eq!(
            Notification::Error {
                message: "x".into()
            }
            .thread_key(),
            None
        );
        assert_eq!(
            Notification::ToolCallApprovalResolved {
                perm_id: "p".into(),
                by: "c".into(),
                decision: "allow_once".into()
            }
            .thread_key(),
            None
        );
    }

    /// 合并器：同一 item_id 的相邻 delta 拼接；跨 item_id 先刷旧的。
    #[test]
    fn delta_coalescer_concat_within_an_item_and_flush_across_items() {
        let mut c = DeltaCoalescer::default();
        // 同一 item 连续追加不立即发。
        assert!(c.push("t1", "i1", "Hel").is_empty());
        assert!(c.push("t1", "i1", "lo").is_empty());
        // 取出即 "Hello"。
        assert_eq!(c.take("t1"), Some(("i1".to_string(), "Hello".to_string())));
        assert_eq!(c.take("t1"), None, "取走后为空");

        // 跨 item_id：追新的之前先把旧的返回（顺序优先）。
        assert!(c.push("t1", "i1", "a").is_empty());
        assert_eq!(
            c.push("t1", "i2", "b"),
            vec![("i1".to_string(), "a".to_string())]
        );
        assert_eq!(c.take("t1"), Some(("i2".to_string(), "b".to_string())));

        // 超过 4KB 自动刷出。
        let big = "x".repeat(DeltaCoalescer::FLUSH_BYTES);
        let flushed = c.push("t1", "i3", &big);
        assert_eq!(flushed.len(), 1, "超上限必须立即返回: {flushed:?}");
        assert_eq!(c.take("t1"), None);
    }

    #[test]
    fn chunk_items_for_replay_preserves_order_and_splits_by_count() {
        use crate::protocol::Item;
        let items: Vec<Item> = (0..(REPLAY_CHUNK_MAX_ITEMS * 2 + 5))
            .map(|i| Item::AgentMessage {
                id: format!("i-{i}"),
                text: "x".to_string(),
            })
            .collect();
        let chunks = chunk_items_for_replay(items);
        assert_eq!(chunks.len(), 3, "200*2+5 must split into 3 chunks");
        assert_eq!(chunks[0].len(), REPLAY_CHUNK_MAX_ITEMS);
        assert_eq!(chunks[1].len(), REPLAY_CHUNK_MAX_ITEMS);
        assert_eq!(chunks[2].len(), 5);
        // 顺序保持：展平后 id 与原始一致。
        let flat: Vec<String> = chunks
            .iter()
            .flatten()
            .map(|it| match it {
                Item::AgentMessage { id, .. } => id.clone(),
                _ => unreachable!(),
            })
            .collect();
        for (i, id) in flat.iter().enumerate() {
            assert_eq!(id, &format!("i-{i}"));
        }
    }

    #[test]
    fn chunk_items_for_replay_splits_by_bytes() {
        use crate::protocol::Item;
        // 每条 ~64KiB 文本;预算 256KiB → 每块约 4 条。
        let big = "y".repeat(64 * 1024);
        let items: Vec<Item> = (0..10)
            .map(|i| Item::AgentMessage {
                id: format!("b-{i}"),
                text: big.clone(),
            })
            .collect();
        let chunks = chunk_items_for_replay(items);
        assert!(chunks.len() >= 3, "byte budget must force multiple chunks");
        for c in &chunks {
            let bytes: usize = c
                .iter()
                .map(|it| serde_json::to_vec(it).unwrap().len())
                .sum();
            // 允许「最后一条超预算」的余量,但每块仍须远小于 1MiB 硬上限。
            assert!(bytes < 900 * 1024, "chunk must stay under the frame limit");
        }
    }

    /// Approval is one-question/one-answer even with many clients: the first
    /// answer routes to the driver, a second answer for the same `perm_id` is a
    /// no-op success (not an error — the other device may simply have tapped
    /// "allow" a beat later).
    ///
    /// The full two-client ws round trip (including the `approvalResolved`
    /// broadcast) belongs to Task 5's E2E; here the routing core is pinned.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_second_approval_answer_is_a_noop_and_the_first_wins() {
        let pending: Mutex<HashMap<String, oneshot::Sender<Decision>>> = Mutex::new(HashMap::new());
        let (tx, mut rx) = oneshot::channel::<Decision>();
        pending.lock().await.insert("perm-1".to_string(), tx);

        let resp = ClientResponse {
            jsonrpc: Some("2.0".to_string()),
            id: RequestId::Str("perm-1".to_string()),
            result: Some(json!({ "decision": "allow_once" })),
            error: None,
        };

        let first = route_client_response(resp.clone(), &pending)
            .await
            .expect("the first answer must route to the waiting driver");
        assert_eq!(first.0, "perm-1");
        assert!(matches!(first.1, Decision::AllowOnce));

        // The same client (or a second one) answers again: no-op, not an error.
        let second = route_client_response(resp, &pending).await;
        assert!(
            second.is_none(),
            "a second answer for an already-resolved approval must be a no-op"
        );

        // The driver saw exactly one decision.
        assert!(matches!(rx.try_recv(), Ok(Decision::AllowOnce)));
        assert!(
            rx.try_recv().is_err(),
            "the driver must receive exactly one decision"
        );
    }

    /// `pair/create` mints a one-time code, `device/list` reflects the shared
    /// store, and `device/revoke` kicks the device back out of it. `Harness::new`
    /// is `Admin`, so the admin gate does not interfere here.
    #[tokio::test(flavor = "multi_thread")]
    async fn pair_create_returns_a_code_and_device_revoke_invalidates_it() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"pair/create","params":{}}"#)
            .await;
        let created = h.read_value().await;
        let code = created["result"]["code"].as_str().unwrap().to_string();
        assert!(created["result"]["expires_in"].as_u64().unwrap() > 0);

        // 设备表初始为空。
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"device/list","params":{}}"#)
            .await;
        let listed = h.read_value().await;
        assert_eq!(listed["result"]["devices"].as_array().unwrap().len(), 0);

        // 用码换 token,设备表出现一条。
        let (device, _token) = h.pairing().redeem(&code, "iPhone 15").unwrap();
        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"device/list","params":{}}"#)
            .await;
        let listed = h.read_value().await;
        assert_eq!(listed["result"]["devices"].as_array().unwrap().len(), 1);

        // 撤销。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"device/revoke","params":{{"device_id":"{}"}}}}"#,
            device.id
        ))
        .await;
        let revoked = h.read_value().await;
        assert_eq!(revoked["result"]["revoked"], true);
        h.shutdown().await;
    }

    /// `device/revoke` 对**在线** ws 设备的映射查找必须真的接上:撤销某设备时,
    /// 主循环经 `ws_client_for_device` 找到它的 ws 连接并摘除。这里直接验证这条
    /// 查找路径——ws 的 E2E 里 B 是"另一台设备",它靠这条映射被踢下线。
    #[tokio::test(flavor = "multi_thread")]
    async fn ws_client_for_device_resolves_through_the_installed_registry() {
        let registry: WsDeviceRegistry = Arc::new(StdMutex::new(HashMap::new()));
        // `install_device_registry` 幂等:进程里若已有 ws server 先设过全局句柄,
        // 它返回的是那**第一张**表,而不是本次传入的。故必须往返回值里插,才能
        // 保证 `ws_client_for_device` 读到;往局部 `registry` 里插会在并发测试下
        // 抖动失败。
        let global = install_device_registry(registry);
        let cid = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
        global
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert("dev-abc".to_string(), cid.clone());

        assert!(
            ws_client_for_device("dev-abc").is_some(),
            "an online ws device must resolve to its ClientId"
        );
        // 未知设备(或连接已摘除)返回 None,`device/revoke` 于是只改设备表。
        assert!(ws_client_for_device("dev-missing").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_read_and_write_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["theme"], "dark", "default before any write");
        assert_eq!(v["result"]["board_watchman_enabled"], true, "缺省为开:{v}");

        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"theme":"light"}}"#,
        )
        .await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(v["result"]["ok"], true);

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 4).await;
        assert_eq!(v["result"]["theme"], "light");

        // 落盘可被另一个进程读回
        assert_eq!(
            crate::settings_store::load(dir.path()),
            crate::settings_store::Theme::Light
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_read_reports_the_stored_relay_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        crate::settings_store::save_relay_url(
            dir.path(),
            Some("wss://relay.example.com/connect?session=x"),
        )
        .unwrap();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(
            v["result"]["relay_url"], "wss://relay.example.com/connect?session=x",
            "{v}"
        );
        h.shutdown().await;
    }

    /// 未配置时 `relay_url` 是显式的 `null`（不是缺键）：桌面设置页据此回填空串，
    /// 缺键与 `null` 在 JSON 里对前端是两码事。
    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_read_reports_null_when_no_relay_url_is_stored() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["relay_url"], json!(null), "{v}");
        assert!(
            v["result"].get("relay_url").is_some(),
            "key must be present: {v}"
        );
        h.shutdown().await;
    }

    /// 写 `relay_url` 后读回一致，且落盘可被另一个进程（如侧车）读回。
    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_then_read_round_trips_the_relay_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"relay_url":"wss://relay.example.com/connect?session=x"}}"#,
        )
        .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["ok"], true, "{v}");

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(
            v["result"]["relay_url"], "wss://relay.example.com/connect?session=x",
            "{v}"
        );
        assert_eq!(
            crate::settings_store::load_relay_url(dir.path()).as_deref(),
            Some("wss://relay.example.com/connect?session=x")
        );
        h.shutdown().await;
    }

    /// `null` / 空串 → 清除；清除后 read 报 `null`。
    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_clears_the_relay_url() {
        for clear in [json!(null), json!(""), json!("   ")] {
            let dir = tempfile::TempDir::new().unwrap();
            let mut cfg = test_config();
            cfg.workdir = dir.path().to_path_buf();
            crate::settings_store::save_relay_url(
                dir.path(),
                Some("wss://relay.example.com/connect?session=x"),
            )
            .unwrap();
            let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
            initialize(&mut h).await;

            h.send(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "ui/settings/write",
                    "params": { "relay_url": clear },
                })
                .to_string(),
            )
            .await;
            let v = read_response(&mut h, 2).await;
            assert_eq!(v["result"]["ok"], true, "clear={clear}: {v}");

            h.send(r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/read","params":{}}"#)
                .await;
            let v = read_response(&mut h, 3).await;
            assert_eq!(v["result"]["relay_url"], json!(null), "clear={clear}: {v}");
            assert_eq!(
                crate::settings_store::load_relay_url(dir.path()),
                None,
                "clear={clear}"
            );
            h.shutdown().await;
        }
    }

    /// 写错类型（非串非 null）不改动既有配置：静默清除是破坏性的。
    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_ignores_a_mistyped_relay_url() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        crate::settings_store::save_relay_url(dir.path(), Some("wss://r/connect?session=x"))
            .unwrap();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"relay_url":5}}"#)
            .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["ok"], true, "{v}");
        assert_eq!(
            crate::settings_store::load_relay_url(dir.path()).as_deref(),
            Some("wss://r/connect?session=x"),
            "a mistyped field must not wipe the stored url"
        );
        h.shutdown().await;
    }

    /// 只有 `relay_url` 的写不能影响 theme：两者共处一个文件。
    #[tokio::test(flavor = "multi_thread")]
    async fn writing_the_relay_url_keeps_the_theme() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"light"}}"#,
        )
        .await;
        let _ = read_response(&mut h, 2).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"relay_url":"wss://r/connect?session=x"}}"#,
        )
        .await;
        let _ = read_response(&mut h, 3).await;

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 4).await;
        assert_eq!(v["result"]["theme"], "light", "{v}");
        assert_eq!(v["result"]["relay_url"], "wss://r/connect?session=x", "{v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_rejects_an_unknown_theme() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"sepia"}}"#,
        )
        .await;
        let v = read_response(&mut h, 2).await;
        assert!(
            v.get("error").is_some(),
            "unsupported theme must be rejected"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ui_settings_write_notifies_subscribers() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"theme":"light"}}"#,
        )
        .await;
        // 响应(主循环写)与通知(watcher 任务写)共用一路流,到达顺序不定;
        // 逐帧收敛,两者都必须见到。
        let mut responded = false;
        let mut notified = None;
        for _ in 0..16 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                assert_eq!(v["result"]["ok"], true);
                responded = true;
            } else if v.get("method").and_then(|m| m.as_str()) == Some("ui/settings/updated") {
                notified = Some(v);
            }
            if responded && notified.is_some() {
                break;
            }
        }
        assert!(responded, "ui/settings/write must respond");
        let n = notified.expect("theme change must push ui/settings/updated");
        assert_eq!(n["params"]["theme"], "light");
        h.shutdown().await;
    }

    /// `board_watchman_enabled` 的写路径:`false` 卸载、`true` 安装,均经注入的
    /// 记录器;偏好落盘且读回一致。
    #[tokio::test(flavor = "multi_thread")]
    async fn writing_the_watchman_setting_installs_or_uninstalls_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_cfg(cfg).await;
        initialize(&mut h).await;
        let calls = Arc::clone(&h.watchman_calls);
        let home = h._watchman_home.path().to_path_buf();

        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"board_watchman_enabled":false}}"#,
        )
        .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["ok"], true, "{v}");
        assert_eq!(
            v["result"]["warning"],
            json!(null),
            "注入的卸载不会失败:{v}"
        );
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "恰好一次卸载:{recorded:?}");
        assert!(!recorded[0].0, "false triggers uninstall");
        assert_eq!(recorded[0].1, home, "必须针对注入的 home");
        assert!(!crate::settings_store::load_watchman_enabled(dir.path()));

        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"board_watchman_enabled":true}}"#,
        )
        .await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(v["result"]["ok"], true, "{v}");
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "第二次是安装:{recorded:?}");
        assert!(recorded[1].0, "true triggers install");

        // 开关与 theme 同处一个文件,互不覆盖。
        assert!(crate::settings_store::load_watchman_enabled(dir.path()));
        assert_eq!(
            crate::settings_store::load(dir.path()),
            crate::settings_store::Theme::Dark
        );

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
            .await;
        let v = read_response(&mut h, 4).await;
        assert_eq!(v["result"]["board_watchman_enabled"], true, "{v}");
        h.shutdown().await;
    }

    /// 安装失败不静默:落盘仍成功,回复带 `warning`,并由 `ok: true` 表明偏好已写入。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_watchman_install_reports_a_warning_without_failing_the_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_watchman(
            cfg,
            Arc::new(|_exe: &Path, _home: &Path| Err("launchctl is unavailable".to_string())),
            Arc::new(|_home: &Path| Ok(())),
        )
        .await;
        initialize(&mut h).await;

        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"board_watchman_enabled":true}}"#,
        )
        .await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["result"]["ok"], true, "{v}");
        assert_eq!(
            v["result"]["warning"], "launchctl is unavailable",
            "失败原因必须回传给前端:{v}"
        );
        // 偏好仍按用户的意图落盘:下次启动会再试。
        assert!(crate::settings_store::load_watchman_enabled(dir.path()));
        h.shutdown().await;
    }

    /// 每个 thread 的工具集都必须带主题工具——这是「对话切主题」的落点。
    #[test]
    fn the_theme_tool_is_registered_into_a_thread_registry() {
        let dir = tempfile::TempDir::new().unwrap();
        let handle = crate::theme_tool::ThemeHandle::new(dir.path().to_path_buf());
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_theme_tool(&mut registry, handle);
        assert!(
            registry.get("set_theme").is_some(),
            "a thread registry must carry the theme tool"
        );
    }

    /// 委派可用时,`build_runtime_tooling` 造出的 registry 也必须带主题工具
    /// (否则每个 git 项目里的 thread 都说不了「切主题」)。
    #[test]
    fn delegation_tooling_carries_the_theme_tool() {
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();

        let attached = Arc::new(
            yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime.path().to_path_buf())
                .expect("a clean git repo must attach"),
        );
        let binding =
            RuntimeBinding::managed(&cfg, runtime.path().to_path_buf(), Arc::clone(&attached));
        let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

        let tooling = build_runtime_tooling(
            &cfg,
            &root,
            "thread-test",
            yi_agent_core::autonomy::YoloSwitch::new(false),
            test_theme(),
        )
        .expect("tooling");
        assert!(
            tooling.registry.get("set_theme").is_some(),
            "the delegation registry must carry the theme tool, got {:?}",
            tooling.registry.names()
        );
    }

    /// 非委派路径(非 git 目录)用的是 bootstrap 工具集的克隆。那一步必须既
    /// 补上主题工具、又保留原有工具——用真实的 bootstrap 工具集断言,而不是
    /// 自搭一个空注册表。
    #[test]
    fn the_non_delegation_registry_carries_the_theme_tool() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )
        .expect("a default config bootstraps without network");
        assert!(
            built.tools.get("bash").is_some(),
            "precondition: the bootstrap carries real tools"
        );

        let registry = registry_with_theme_tool(&built.tools, test_theme());
        assert!(
            registry.get("set_theme").is_some(),
            "the non-delegation registry must carry the theme tool, got {:?}",
            registry.names()
        );
        assert!(
            registry.get("bash").is_some(),
            "cloning the bootstrap registry must keep its original tools"
        );
    }

    /// 重建 agent 会经过 `Agent::new`,它从零开始、**没有**权限检查器。
    /// 非委派路径既然要重建,就必须把审批路径装回去,否则非 git 目录静默失去审批。
    #[test]
    fn rebuilding_the_thread_agent_keeps_the_approval_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )
        .expect("a default config bootstraps without network");
        assert!(
            built.agent.permission_checker().is_some(),
            "precondition: the bootstrap attaches the checker"
        );

        let rebuilt = rebuild_thread_agent_with_theme(built, None, test_theme());
        assert!(
            rebuilt.agent.permission_checker().is_some(),
            "the rebuilt agent must keep the permission checker"
        );
        assert!(
            rebuilt.agent.decision_rx().is_some(),
            "the rebuilt agent must keep the decision receiver"
        );
    }

    /// `thread/readItems` 返回已落盘的 items；`afterItemId` 只返回其后的部分。
    #[tokio::test(flavor = "multi_thread")]
    async fn read_items_returns_persisted_items_and_slices_after_id() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        // 跑完一轮，产生至少一条已落盘的 item（user + agent 消息）。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 读到 turn/completed 为止，确认落盘完成。
        for _ in 0..16 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = read_response(&mut h, 4).await;
        let items = v["result"]["items"].as_array().expect("items array");
        assert!(!items.is_empty(), "readItems 必须返回已落盘的 items: {v}");
        let first_id = items[0]["id"].as_str().unwrap().to_string();

        // afterItemId = 第一条 → 不包含第一条。
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":5,"method":"thread/readItems",
            "params":{"threadId": tid, "afterItemId": first_id}
        });
        h.send(&req.to_string()).await;
        let v = read_response(&mut h, 5).await;
        let after = v["result"]["items"].as_array().unwrap();
        assert!(
            after
                .iter()
                .all(|i| i["id"].as_str() != Some(first_id.as_str())),
            "afterItemId 之后不该再含该 id: {v}"
        );

        // 未知 thread → unknown_thread(-32011)。
        h.send(
            r#"{"jsonrpc":"2.0","id":6,"method":"thread/readItems","params":{"threadId":"nope"}}"#,
        )
        .await;
        let v = read_response(&mut h, 6).await;
        assert_eq!(v["error"]["code"], -32011, "未知 thread 必须报错: {v}");
        h.shutdown().await;
    }

    /// `thread/readItems` 对**正在跑**的会话是只读的：调用它**不得**中断回合。
    ///
    /// 这正是它相对 `thread/resume` 的关键差异（resume 会先 interrupt 在跑的 turn）。
    #[tokio::test(flavor = "multi_thread")]
    async fn read_items_does_not_interrupt_a_running_turn() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 等回合真正开始。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        // 只读补齐：slow provider 期间会持续推 item/delta，按 id 找响应本身。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..12 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/readItems must respond");
        assert!(v.get("error").is_none(), "readItems 不应出错: {v}");
        // SlowProvider 每 5ms 推一段 item/delta、且永不结束。若 readItems 打断了
        // 回合，此后就再不会有任何 delta；反之则必有。故"响应之后还收得到 delta"
        // 是"回合仍在跑"的确定性判据。
        let mut deltas_after = 0usize;
        for _ in 0..20 {
            let n = h.read_value().await;
            match n.get("method").and_then(|m| m.as_str()) {
                Some("item/delta") => {
                    deltas_after += 1;
                    if deltas_after >= 2 {
                        break;
                    }
                }
                Some("turn/completed") => break,
                _ => {}
            }
        }
        assert!(
            deltas_after >= 2,
            "readItems 之后回合必须仍在跑（仍持续产生 item/delta），实际 {deltas_after} 段"
        );
        h.shutdown().await;
    }

    /// Rebuilding a thread agent must reuse the *same* session `Arc`.
    ///
    /// The delegation tools read the caller's live transcript through a
    /// `CallerContext` bound to that `Arc` (see [`wrap_for_delegation`]). If a
    /// rebuild swapped in a fresh `Arc` (as `with_session` does), the bound
    /// handle would point at the discarded session and `fork: true` would
    /// silently upload nothing. This drives every rebuild path.
    #[test]
    fn rebuilding_a_thread_agent_keeps_one_session_handle() {
        // 1. Delegation path: `wrap_for_delegation` swaps the tool registry and
        //    binds the caller in one step.
        let repo = tempfile::TempDir::new().unwrap();
        let runtime = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let mut cfg = test_config();
        cfg.workdir = repo.path().to_path_buf();

        let attached = Arc::new(
            yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime.path().to_path_buf())
                .expect("a clean git repo must attach"),
        );
        let binding =
            RuntimeBinding::managed(&cfg, runtime.path().to_path_buf(), Arc::clone(&attached));
        let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
        let tooling = build_runtime_tooling(
            &cfg,
            &root,
            "thread-handle",
            yi_agent_core::autonomy::YoloSwitch::new(false),
            test_theme(),
        )
        .expect("tooling");
        // Taken *before* the rebuild: a clone shares the caller slot the tools
        // hold, so it observes whatever `wrap_for_delegation` binds.
        let caller = tooling.caller.clone();

        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(
            &mut agent,
            Some(yi_agent_core::Session::from_messages(vec![
                yi_agent_core::Message::user("first"),
            ])),
        );
        let handle_a = agent.session_handle();
        handle_a
            .lock()
            .unwrap()
            .push(yi_agent_core::Message::assistant(vec![
                yi_agent_core::ContentBlock::Text("reply".into()),
            ]));

        let built = BuiltAgent {
            agent,
            provider: provider.clone(),
            config: config.clone(),
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        };
        let rebuilt = wrap_for_delegation(built, tooling);
        assert!(
            Arc::ptr_eq(&handle_a, &rebuilt.agent.session_handle()),
            "wrap_for_delegation must reuse the thread's session Arc"
        );
        let seen = caller
            .snapshot()
            .expect("wrap_for_delegation must bind the caller to the live session");
        assert_eq!(
            seen,
            handle_a.lock().unwrap().messages().to_vec(),
            "the bound caller must read the rebuilt agent's live transcript"
        );

        // 2. Theme rebuild path (no resume): `rebuild_thread_agent_with_theme`.
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let bootstrapped = yi_agent_runtime::bootstrap::bootstrap_agent(
            &cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )
        .expect("a default config bootstraps without network");
        let handle_b = bootstrapped.agent.session_handle();
        handle_b
            .lock()
            .unwrap()
            .push(yi_agent_core::Message::user("in-flight"));
        let rebuilt_theme = rebuild_thread_agent_with_theme(bootstrapped, None, test_theme());
        assert!(
            Arc::ptr_eq(&handle_b, &rebuilt_theme.agent.session_handle()),
            "rebuild_thread_agent_with_theme must reuse the session Arc"
        );
        assert_eq!(
            rebuilt_theme.agent.session().messages().len(),
            1,
            "the rebuilt agent must still see the in-flight message"
        );

        // 3. Resume path: `apply_session(Some(..))` applies contents in place,
        //    so the loaded history lands on the same Arc.
        let bootstrapped = yi_agent_runtime::bootstrap::bootstrap_agent(
            &cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )
        .expect("a default config bootstraps without network");
        let handle_c = bootstrapped.agent.session_handle();
        let mut loaded = yi_agent_core::Session::from_messages(vec![
            yi_agent_core::Message::user("loaded-one"),
            yi_agent_core::Message::user("loaded-two"),
        ]);
        // A resumed session carries the previous turn's usage; the rebuild must
        // land it on the live handle, or the first post-resume turn's
        // auto-compact check reads `None` and short-circuits.
        loaded.set_last_input_tokens(Some(1234));
        let rebuilt_loaded =
            rebuild_thread_agent_with_theme(bootstrapped, Some(loaded), test_theme());
        assert!(
            Arc::ptr_eq(&handle_c, &rebuilt_loaded.agent.session_handle()),
            "apply_session must keep the session Arc, not swap in a new one"
        );
        assert_eq!(
            rebuilt_loaded.agent.session().messages().len(),
            2,
            "apply_session must still apply the resumed history"
        );
        assert_eq!(
            rebuilt_loaded.agent.session().last_input_tokens(),
            Some(1234),
            "apply_session must carry the resumed input-token count onto the live handle"
        );
    }

    /// Compaction replaces the messages in place and keeps the session `Arc`.
    ///
    /// `with_session(compacted)` would drop the old handle, so a `CallerContext`
    /// taken before the compact would keep reading the pre-compaction
    /// transcript.
    #[tokio::test(flavor = "multi_thread")]
    async fn compaction_replaces_messages_without_changing_the_handle() {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        let session = yi_agent_core::Session::from_messages(vec![
            yi_agent_core::Message::user("first"),
            yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                "reply".into(),
            )]),
            yi_agent_core::Message::user("second"),
        ]);
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, Some(session));

        let handle = agent.session_handle();
        // The host binds this handle once; it must observe the compacted text.
        let caller = yi_agent_subagent::CallerContext::new(handle.clone());

        let snapshot = agent.session();
        let compacted = yi_agent_core::compact_session(&provider, agent.config(), &snapshot)
            .await
            .expect("compaction must run")
            .expect("the three-message history must reduce");
        let expected = compacted.messages().to_vec();
        agent.set_session_messages(compacted.messages().to_vec());

        assert!(
            Arc::ptr_eq(&handle, &agent.session_handle()),
            "compaction must not change the session Arc"
        );
        assert_eq!(
            agent.session().messages(),
            expected.as_slice(),
            "compaction must install the compacted messages"
        );
        assert_eq!(
            caller.snapshot().expect("the caller stays bound"),
            expected,
            "a caller bound before the compact must see the compacted transcript"
        );
    }

    /// `/clear` 与 `/compact` 都必须把陈旧的 `last_input_tokens` 清零。
    ///
    /// Task 8 把 `with_session(Session::new())` 换成 `set_session_messages(...)`
    /// 以保住 session `Arc`,但后者只换消息、不碰 token 计数:上一轮留下的(很大的)
    /// `last_input_tokens` 会存活到 `maybe_auto_compact`,让下一轮立刻误触发一次
    /// 无谓的自动压缩。本测试驱动**生产函数** `apply_session_command`,断言计数归零、
    /// 而 `Arc` 未变。
    #[tokio::test]
    async fn session_commands_clear_the_stale_input_tokens_without_changing_the_handle() {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        drop(client_r); // 本测试不看 writer 输出
        let (hub, _client) = test_hub(server_w);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let store_dir = tempfile::TempDir::new().unwrap();
        let store = crate::thread_store::ThreadStore::new(store_dir.path());

        // Clear 会 truncate 日志,故线程 id 必须合法。
        let cases = [
            ("/clear", "thread-tokens-clear"),
            ("/compact", "thread-tokens-compact"),
        ];
        for (label, thread_id) in cases {
            let mut agent = yi_agent_core::Agent::new(
                provider.clone(),
                Arc::new(yi_agent_core::ToolRegistry::new()),
                config.clone(),
            );
            let handle = agent.session_handle();
            let caller = yi_agent_subagent::CallerContext::new(handle.clone());
            // 可压缩的历史(两个 user 轮次)+ 上一轮留下的大计数,正是 finding 描述的
            // 陈旧状态。
            handle.lock().unwrap().replace_messages(vec![
                yi_agent_core::Message::user("first"),
                yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                    "reply".into(),
                )]),
                yi_agent_core::Message::user("second"),
            ]);
            handle.lock().unwrap().set_last_input_tokens(Some(150_000));

            let status = ThreadSession::new_status();
            if label == "/clear" {
                let (reply, answer) = oneshot::channel();
                agent = apply_session_command(
                    agent,
                    SessionCommand::Clear { reply },
                    &provider,
                    &config,
                    &store,
                    thread_id,
                    &hub,
                    &turn_tx,
                    &status,
                )
                .await;
                answer
                    .await
                    .expect("the clear reply channel must be fulfilled")
                    .expect("truncating a never-written log must succeed");
            } else {
                let (reply, answer) = oneshot::channel();
                agent = apply_session_command(
                    agent,
                    SessionCommand::Compact { reply },
                    &provider,
                    &config,
                    &store,
                    thread_id,
                    &hub,
                    &turn_tx,
                    &status,
                )
                .await;
                assert_eq!(
                    answer
                        .await
                        .expect("the compact reply channel must be fulfilled"),
                    CompactOutcome::Compacted,
                    "{label} must compact the two-user-turn history"
                );
            }

            assert_eq!(
                agent.session().last_input_tokens(),
                None,
                "{label} must clear the stale input-token count"
            );
            assert!(
                Arc::ptr_eq(&handle, &agent.session_handle()),
                "{label} must keep the session Arc"
            );
            assert_eq!(
                caller.snapshot().expect("the caller stays bound"),
                agent.session().messages().to_vec(),
                "{label} must land its outcome on the handle the caller holds"
            );
        }

        assert!(
            turn_rx.try_recv().is_err(),
            "a session command with no turn must not report a turn as finished"
        );
    }
    // ---------- merged stdio + loopback ws (serve_stdio_with_relay) ----------

    /// 合并模式接线:stdio 的 `reader`/`writer` + 一条已绑定的环回 ws listener,
    /// 共用**一个** `serve()` 与同一套 hub/表。
    ///
    /// 与 [`Harness`] 不同,这里同时驱动两条前端(stdio 的 `local` 与 ws 的
    /// `ws-<uuid>`),测试因此能证明两者共享同一张 `threads`。测试**不**起真中继:
    /// ws 客户端直接用注入 `pairing` 铸出的 token 连环回 listener。
    struct MergedHarness {
        client_w: tokio::io::DuplexStream,
        client_r: tokio::io::BufReader<tokio::io::DuplexStream>,
        addr: std::net::SocketAddr,
        token: String,
        #[allow(dead_code)]
        hub: Arc<crate::broadcast::Broadcaster>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
        /// 隔离的全局索引;持有它保证 tempdir 活到 harness 结束。
        _index_dir: tempfile::TempDir,
        /// 隔离的看板登记表目录,理由同 [`Harness::board_dir`]。
        _board_dir: tempfile::TempDir,
    }

    impl MergedHarness {
        async fn new() -> Self {
            // 单测不起真中继。
            Self::new_with_relay(None).await
        }

        /// [`MergedHarness::new`] 但可注入 `relay_url`:传 `Some` 时走生产路径
        /// ——起中继客户端(可指向一个不可达端点,它只会退避重连)。用来验证
        /// 中继接线不打断合并主循环、且 stdio EOF 时收尾仍返回。
        async fn new_with_relay(relay_url: Option<String>) -> Self {
            let cfg = test_config();
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let index_dir = tempfile::TempDir::new().unwrap();
            let board_dir = tempfile::TempDir::new().unwrap();
            let workspaces = Arc::new(WorkspaceIndex::new(
                index_dir.path().join("workspaces.json"),
            ));
            let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
                index_dir.path().join("devices.json"),
            )));
            let hub = Arc::new(crate::broadcast::Broadcaster::new());
            let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            // stdin 侧本地客户端仍是生产 stdio 的 Admin。
            let handle = tokio::spawn(serve_scoped_with_loopback(
                server_r,
                server_w,
                cfg,
                PERMISSION_TIMEOUT,
                workspaces,
                Arc::clone(&pairing),
                Arc::clone(&hub),
                RuntimeAttachments {
                    runtimes: Arc::new(StdMutex::new(HashMap::new())),
                    thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                    board_dir: board_dir.path().to_path_buf(),
                    resident_dir: PathBuf::new(),
                    launcher: Arc::new(|_project: &Path| Ok(true)),
                    theme,
                    // 测试注入:只记账,不碰真实 launchd / home。
                    watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
                    watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
                    watchman_home: PathBuf::new(),
                },
                build_test_agent,
                Scope::Admin,
                Some(listener),
                relay_url,
            ));
            // 手机侧凭据:与主循环**共享**的 `pairing` 取一枚(按名字幂等),故 ws 前端认得。
            let token = pairing.seed_local_device("relay-bridge");
            Self {
                client_w,
                client_r: tokio::io::BufReader::new(client_r),
                addr,
                token,
                hub,
                handle,
                _index_dir: index_dir,
                _board_dir: board_dir,
            }
        }

        async fn send(&mut self, line: &str) {
            use tokio::io::AsyncWriteExt;
            self.client_w.write_all(line.as_bytes()).await.unwrap();
            self.client_w.write_all(b"\n").await.unwrap();
            self.client_w.flush().await.unwrap();
        }

        async fn read_value(&mut self) -> serde_json::Value {
            use tokio::io::AsyncBufReadExt;
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), self.client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a message")
                .expect("read_line failed");
            assert!(n > 0, "unexpected EOF while waiting for a message");
            serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
        }

        async fn shutdown(self) {
            let MergedHarness {
                client_w, handle, ..
            } = self;
            drop(client_w);
            let res = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("server must shut down within the timeout")
                .expect("server task must not panic");
            assert!(
                res.is_ok(),
                "merged server should return Ok on stdio EOF: {res:?}"
            );
        }

        async fn connect_ws(
            &self,
        ) -> tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        > {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let uri: axum::http::Uri = format!("ws://{}/ws", self.addr).parse().unwrap();
            let request = tokio_tungstenite::tungstenite::ClientRequestBuilder::new(uri)
                .with_header("Authorization", format!("Bearer {}", self.token))
                .into_client_request()
                .unwrap();
            let (ws, _) = tokio_tungstenite::connect_async(request)
                .await
                .expect("ws client must authenticate with the seeded token");
            ws
        }
    }

    async fn ws_send_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, line: &str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        use futures::SinkExt as _;
        ws.send(tokio_tungstenite::tungstenite::Message::Text(line.into()))
            .await
            .unwrap();
    }

    async fn ws_recv_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> serde_json::Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a ws frame")
            .expect("ws stream ended before a frame arrived")
            .expect("ws transport error");
        serde_json::from_str(msg.to_text().expect("server sent a non-text frame"))
            .expect("server sent invalid JSON")
    }

    /// 读到 id 匹配的 ws 响应,丢弃中间穿插的通知/其它响应。
    async fn ws_read_response<S>(
        ws: &mut tokio_tungstenite::WebSocketStream<S>,
        want: u64,
    ) -> serde_json::Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        for _ in 0..16 {
            let v = ws_recv_json(ws).await;
            if v.get("id") == Some(&serde_json::json!(want)) {
                return v;
            }
        }
        panic!("no ws response with id {want}");
    }

    /// 读到指定 method 的 ws 通知,丢弃中间帧;超时即失败。
    async fn ws_await_notification<S>(
        ws: &mut tokio_tungstenite::WebSocketStream<S>,
        method: &str,
    ) -> serde_json::Value
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        for _ in 0..16 {
            let v = ws_recv_json(ws).await;
            if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                return v;
            }
        }
        panic!("no ws notification `{method}`");
    }

    /// 合并模式的扇出证明:stdio 的 `local` 与经环回 ws 的手机跑在**同一个**
    /// `serve()` 上——stdio `thread/start` 后,ws 客户端既收到 `thread/started`
    /// 通知,`thread/listAll` 也看到同一个 thread id。
    #[tokio::test(flavor = "multi_thread")]
    async fn merged_loop_fans_out_stdio_notifications_to_ws_client() {
        let mut h = MergedHarness::new().await;
        let mut ws = h.connect_ws().await;
        crate::server::tests_support::initialize(&mut ws).await;

        // stdio:initialize + thread/start。
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let mut thread_id = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                thread_id = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
                break;
            }
        }
        let thread_id = thread_id.expect("thread/start response");

        // ws:同一个 thread 的通知必须扇出到这里。
        let notif = ws_await_notification(&mut ws, "thread/started").await;
        assert_eq!(
            notif["params"]["thread_id"].as_str(),
            Some(thread_id.as_str()),
            "ws 客户端必须收到 stdio thread/start 触发的 thread/started"
        );

        // ws:thread/listAll 读到与 stdio 同一个 thread id(同一张表 + 同一工作区)。
        ws_send_json(
            &mut ws,
            r#"{"jsonrpc":"2.0","id":2,"method":"thread/listAll","params":{}}"#,
        )
        .await;
        let v = ws_read_response(&mut ws, 2).await;
        let found = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap())
            .any(|t| t["thread_id"].as_str() == Some(thread_id.as_str()));
        assert!(
            found,
            "ws thread/listAll must see the stdio-started thread {thread_id}: {v}"
        );

        h.shutdown().await;
    }

    /// 生产路径(带 `relay_url`)也要收敛:中继端点不可达时 `run_client` 只退避
    /// 重连、不返回;stdio EOF 仍必须结束整个合并会话并返回 `Ok(())`。
    #[tokio::test(flavor = "multi_thread")]
    async fn merged_loop_returns_ok_on_stdio_eof_with_relay_configured() {
        // 未监听端口 = 中继不可达:客户端会失败并退避重连,永不阻塞启动。
        let h = MergedHarness::new_with_relay(Some(
            "ws://127.0.0.1:1/connect?session=unit".to_string(),
        ))
        .await;
        // 手机仍能经环回前端握手(relay 只影响出站桥,不影响本地前端)。
        let mut ws = h.connect_ws().await;
        crate::server::tests_support::initialize(&mut ws).await;
        h.shutdown().await;
    }

    /// 合并模式里手机仍是 `Control`:Admin-only 的 `pair/create` 必须被拒。
    #[tokio::test(flavor = "multi_thread")]
    async fn merged_loop_control_client_is_denied_admin_rpc() {
        let h = MergedHarness::new().await;
        let mut ws = h.connect_ws().await;
        crate::server::tests_support::initialize(&mut ws).await;
        ws_send_json(
            &mut ws,
            r#"{"jsonrpc":"2.0","id":2,"method":"pair/create","params":{}}"#,
        )
        .await;
        let v = ws_read_response(&mut ws, 2).await;
        assert_eq!(
            v["error"]["code"], -32014,
            "control client must be denied admin rpc: {v}"
        );
        h.shutdown().await;
    }
}

/// 主题守望者的回归测试。
///
/// 这些测试直接驱动生产里的 [`pump_theme_notifications`],不再照抄一份循环体:
/// 「Lagged 后仍存活」只有在真正跑被 spawn 的那段代码时才算数。
#[cfg(test)]
mod theme_watcher_tests {
    use super::*;

    /// 快进:让订阅者跑赢容量 16 的广播、把订阅者落在后面,`recv()` 就会返回
    /// `Lagged`(`ThemeHandle::new` 用的正是 `broadcast::channel(16)`)。
    ///
    /// 先 `yield_now` 保证 spawn 的守望者已挂上订阅:否则 17 次 `send` 一场空,
    /// 之后再订阅只会收到第 201 次的值,测不出 Lagged。
    async fn lag_the_subscriber(theme: &crate::theme_tool::ThemeHandle) {
        tokio::task::yield_now().await;
        for i in 0..(16 + 200) {
            theme.set(if i % 2 == 0 {
                crate::settings_store::Theme::Light
            } else {
                crate::settings_store::Theme::Dark
            });
        }
    }

    /// 回归:`Lagged` 不得终结守望者。
    ///
    /// 旧写法 `while let Ok(theme) = rx.recv().await` 把 `Lagged` 与 `Closed` 一并
    /// 当作退出条件;一次背压(客户端一时读得慢)就把守望者杀掉,此后本进程再也
    /// 推不出 `ui/settings/updated`——对话框或 `set_theme` 工具改的主题静默丢失。
    #[tokio::test]
    async fn theme_watcher_survives_a_lagged_broadcast() {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let local = crate::broadcast::ClientId::local();
        let mut out = hub.register_reliable(local);
        let theme = crate::theme_tool::ThemeHandle::new(std::env::temp_dir());
        let pump = tokio::spawn(pump_theme_notifications(
            theme.subscribe(),
            Arc::clone(&hub),
        ));

        lag_the_subscriber(&theme).await;

        // 让守望者重新比发送者快:它必须先处理 `Lagged`(不退出),再收到这次值。
        let mut saw_light_after_the_lag = false;
        for _ in 0..200 {
            theme.set(crate::settings_store::Theme::Light);
            match tokio::time::timeout(Duration::from_secs(5), out.recv()).await {
                Ok(Some(frame)) => {
                    if frame["method"] == "ui/settings/updated"
                        && frame["params"]["theme"] == "light"
                    {
                        saw_light_after_the_lag = true;
                        break;
                    }
                }
                Ok(None) => panic!("the local client's outbound queue closed unexpectedly"),
                Err(_) => break,
            }
        }
        pump.abort();
        assert!(
            saw_light_after_the_lag,
            "the theme watcher must stay alive across a Lagged broadcast and still push \
             ui/settings/updated; `while let Ok` returns instead"
        );
    }

    /// 守住的另一半语义:`Closed`(所有发送端都没了)才终止。
    ///
    /// 这条防止把守望者改成「永不退出」的忙循环。
    #[tokio::test]
    async fn theme_watcher_ends_when_the_broadcast_closes() {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let (tx, rx) = tokio::sync::broadcast::channel::<crate::settings_store::Theme>(16);
        let pump = tokio::spawn(pump_theme_notifications(rx, Arc::clone(&hub)));
        drop(tx);
        let finished = tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the watcher must exit once the broadcast is closed");
        assert!(finished.is_ok());
    }

    /// 投递分层：列表层/全局帧无 thread 键（恒推），内容层按 thread 键过滤。
    #[test]
    fn notification_delivery_classifies_list_and_content() {
        use crate::protocol::{Delivery, Notification};
        let status = Notification::ThreadStatusUpdated {
            thread_id: "t2".into(),
            status: crate::protocol::ThreadStatus::Running,
        };
        assert_eq!(status.delivery(), Delivery::List);
        assert_eq!(
            Notification::ThreadStarted {
                thread_id: "t2".into(),
                cwd: "/w".into(),
                model: "m".into()
            }
            .delivery(),
            Delivery::List
        );
        assert_eq!(
            Notification::ItemDelta {
                thread_id: "t2".into(),
                item_id: "i".into(),
                delta: "d".into()
            }
            .delivery(),
            Delivery::Content
        );
        assert_eq!(
            Notification::UiSettingsUpdated {
                theme: "dark".into()
            }
            .delivery(),
            Delivery::Global
        );
        assert_eq!(
            Notification::Error {
                message: "x".into()
            }
            .delivery(),
            Delivery::Global
        );
        assert_eq!(
            Notification::ToolCallApprovalResolved {
                perm_id: "p".into(),
                by: "c".into(),
                decision: "allow_once".into()
            }
            .delivery(),
            Delivery::Global
        );
    }

    /// 订阅 t1 的客户端：仍收到 t2 的 `thread/status/updated`（列表层恒推），
    /// 但收不到 t2 的 `item/delta`（内容层过滤）。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_layer_is_always_delivered_while_content_is_filtered() {
        let hub = crate::broadcast::Broadcaster::new();
        let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
        let mut rx = hub.register(sub.clone());
        hub.subscribe(&sub, vec!["t1".to_string()]);

        // 列表层：未订阅的 t2 的状态帧也必须到达。
        write_notification(
            &hub,
            &Notification::ThreadStatusUpdated {
                thread_id: "t2".into(),
                status: crate::protocol::ThreadStatus::Running,
            },
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap()["params"]["thread_id"], "t2");

        // 内容层：未订阅的 t2 的 delta 必须被挡下。
        write_notification(
            &hub,
            &Notification::ItemDelta {
                thread_id: "t2".into(),
                item_id: "i".into(),
                delta: "leak".into(),
            },
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "订阅 t1 不该收到 t2 的内容帧");
    }
}
