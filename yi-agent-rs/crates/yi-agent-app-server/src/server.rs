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

/// 首轮 LLM 标题生成的超时:超时即回退兜底标题，绝不卡住 turn 收尾。
const TITLE_TIMEOUT: Duration = Duration::from_secs(5);

/// `image/read` 每次回传的原始字节上限。传输帧上限 1 MiB,base64 膨胀 4/3 后
/// 512 KiB 的原始块约 683 KiB,连同其它字段仍在单帧之内。
pub(crate) const IMAGE_READ_CHUNK_BYTES: u64 = 512 * 1024;

/// `image/read` 允许服务的最大文件字节数。分片只是传输形态,不代表可以无上限地
/// 服务任意大文件;与 `image_prep::MAX_IMAGE_BYTES` 同值(20 MiB)。
pub(crate) const IMAGE_READ_MAX_BYTES: u64 = 20 * 1024 * 1024;

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
    /// Where `models.json` lives. Injectable for the same reason `board_dir`
    /// is: `model/*` must be testable without writing the developer's real
    /// machine-level catalog (and its API keys).
    pub(crate) models_path: PathBuf,
    /// Where the global `.env` lives, for `onboarding/apply`. Injectable for the
    /// same reason `models_path` is: onboarding must never write the developer's
    /// real `~/.yi-agent/.env`.
    pub(crate) onboarding_env_path: PathBuf,
    /// Where `preferences.json` lives, for the `onboarding_dismissed` marker.
    /// Injectable for the same reason `onboarding_env_path` is: dismissing the
    /// first-run guide must never touch the developer's real preferences file.
    pub(crate) onboarding_preferences_path: PathBuf,
    /// Where the generic resident-daemon registry lives (`$HOME/.yi-agent`).
    /// Injectable for the same reason `board_dir` is: `board/create` must be
    /// testable without writing the developer's real resident registry.
    pub(crate) resident_dir: PathBuf,
    /// The absolute path of the `superpowers-kanban` executable written into
    /// each project's supervisor manifest. Injectable for the same reason
    /// `board_dir` is: `board/create` must be testable without the plugin being
    /// installed on the machine.
    pub(crate) plugin_command: PathBuf,
    /// How `board/create` starts a project's daemon. Injectable for the same
    /// reason `board_dir` is: a test must not spawn a real detached process,
    /// and the app's production launcher is what an integration test would
    /// otherwise have to reproduce.
    pub(crate) launcher: BoardLauncher,
    /// 桌面主题句柄：`ui/settings/read|write` 维护它，每个 thread 的
    /// `set_theme` 工具也持同一实例，故任一路径改主题都会落盘 + 广播。
    pub(crate) theme: crate::theme_tool::ThemeHandle,
    /// Git Diff 焦点句柄：`show_git_diff` 工具持它广播，watcher 翻译成
    /// `ui/gitDiff/focus` 通知。每个 thread 的工具与 watcher 共用同一实例。
    pub(crate) git_diff: crate::git_diff_tool::GitDiffHandle,
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
pub(crate) fn ensure_watchman_installed(workdir: &Path, home: &Path, install: &WatchmanInstall) {
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
    models_path: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    built: BuiltAgent,
) -> Activation {
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = PathBuf::from(cwd);
    let binding = match attach_cwd_runtime(runtimes, runtime_dir, &thread_cfg, models_path) {
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
        git_diff.clone(),
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
    models_path: &Path,
) -> Result<Arc<RuntimeBinding>, String> {
    let key = project_key(&cfg.workdir);
    if let Some(existing) = runtimes.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return existing.clone();
    }
    // Own a *usable* runtime: adopt a healthy daemon, replace a dead or wedged
    // one. The binding it returns heals itself on later calls, so a daemon that
    // dies after this point (e.g. its terminal closes) no longer strands the
    // thread's delegation tools.
    let outcome = yi_agent_subagent::attach::ensure_owned_runtime(
        cfg,
        runtime_dir.to_path_buf(),
        models_path,
    )
    .map(|attached| {
        RuntimeBinding::managed(
            cfg,
            runtime_dir.to_path_buf(),
            models_path.to_path_buf(),
            Arc::new(attached),
        )
    })
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
    git_diff: crate::git_diff_tool::GitDiffHandle,
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
    registry.register(Arc::new(crate::git_diff_tool::ShowGitDiffTool::new(
        git_diff,
        Some(thread_id.to_string()),
    )));
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
    let mut config = agent.config().clone();
    // 让重建后的 agent 沿用本次会话的 `preserve_thinking`：`build_agent_config`
    // 只认 `RuntimeConfig`（无该字段），故值经 thread-local 由本函数落到
    // `AgentConfig` 上（镜像 effort 的做法）。未设定时按默认 `true`。
    config.preserve_thinking = current_session_preserve_thinking();
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
    pub(crate) code: &'static str,
    pub(crate) message: String,
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
/// `project` is both where the plugin is installed and where its daemon runs:
/// the manifest lives at `<project>/.yi-agent/supervisors/`, the socket at
/// `<project>/.yi-agent/runtime/`, and the daemon is the project's own. Using the
/// app's cwd instead made every query for another project hit the wrong socket
/// (or none) — the "clicked and nothing happened" bug. `global` is the board
/// registry that says whether `project` has a board at all; a project without
/// one has no daemon worth dialing.
pub(crate) fn plugin_query(
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

/// 向插件问一条通用问题（当前是 `plugin/settings/*`）。
///
/// 与 `plugin_query` 的区别：不受看板登记表门控——任何插件都能走；宿主只转发
/// `method`/`params`，不解释应答。
///
/// `scope` 是**要配置的那个项目**（`<项目>/.yi-agent/supervisors/` 放清单、
/// `<项目>/.yi-agent/runtime/` 放它自己的 daemon socket）。看板按项目安装、由项目
/// 自己的 daemon 监管，桌面侧车 app-server 的 workdir 却是用户 home：固定用 workdir
/// 会既找不到清单（读成 `plugin_not_installed`）、又拨不到 socket（读成
/// `plugin_unavailable`）——这正是「看板跑着、设置页却空着」的根因。
pub(crate) fn plugin_query_self(
    scope: &Path,
    plugin: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BoardQueryError> {
    if plugin.is_empty() {
        return Err(BoardQueryError::new(
            "invalid_params",
            "plugin/settings needs a `plugin` name",
        ));
    }
    let summary = list_installed_plugins(scope)
        .into_iter()
        .find(|summary| summary.name == plugin);
    let Some(summary) = summary else {
        return Err(BoardQueryError::new(
            "plugin_not_installed",
            format!("plugin {plugin} is not installed"),
        ));
    };
    if !summary.queryable {
        return Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("plugin {plugin} does not declare a query socket"),
        ));
    }
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(scope);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        BoardQueryError::new(
            "plugin_unavailable",
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
            "plugin_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        // The plugin received the request and answered with its own rejection
        // (e.g. an invalid settings payload). That is the plugin talking, not
        // the plugin being gone: `plugin_rejected` keeps the reason apart from
        // the two genuine unavailability arms above, so the desktop can show
        // the plugin's message instead of "插件未运行".
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(BoardQueryError::new(
            "plugin_rejected",
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

/// 已装插件的一行。宿主不解释任何插件语义：`name`/`switch_key` 原样来自清单，
/// `queryable` 只是「该清单声明了 query_socket」。
pub(crate) struct PluginSummary {
    pub name: String,
    pub queryable: bool,
    pub switch_key: String,
}

/// 列出 `<scope>/.yi-agent/supervisors/` 下的插件清单。
///
/// 纯文件读取：daemon 没起也能列。目录缺失或清单损坏都只是「少一个插件」，
/// 不是错误——一个坏清单不该让整个设置页打不开。
///
/// `scope` 由调用方给出：看板插件是**按项目**安装的（清单落在
/// `<项目>/.yi-agent/supervisors/`），而桌面侧车 app-server 的 workdir 是用户
/// home。若固定读 workdir，桌面端在 home 下一个插件也看不到——这正是看板跑着、
/// 设置页却空着的根因。`plugins/list` 因此在调用方给了 `project` 时枚举该项目，
/// 否则回落到宿主自身 workdir（TUI 与未带 project 的调用保持原语义）。
pub(crate) fn list_installed_plugins(scope: &Path) -> Vec<PluginSummary> {
    let dir = scope.join(".yi-agent").join("supervisors");
    yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .map(|manifest| PluginSummary {
            name: manifest.name,
            queryable: manifest.query_socket.is_some(),
            switch_key: manifest.switch_key,
        })
        .collect()
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
    ///
    /// 由看板合并队列设计（§9）定义、桌面端经插件通道消费。宿主自身的
    /// `card_scheduler` 从不读它（合并卡不进 `running`），生产构建下显示为未读，
    /// 故显式放行，以免 `-D warnings` 把「协议字段」误判成死代码而删掉。
    #[allow(dead_code)]
    pub kind: String,
    /// 合并卡的源分支。
    #[allow(dead_code)]
    pub source: Option<String>,
    /// 合并卡的目标分支。
    #[allow(dead_code)]
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
pub(crate) fn project_arg(params: &serde_json::Value) -> Option<PathBuf> {
    let raw = params.get("project").and_then(|value| value.as_str())?;
    if raw.is_empty() {
        return None;
    }
    Some(std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw)))
}

/// 通用插件通道（`plugins/list` / `plugin/settings/*`）的作用域。
///
/// 调用方给了非空 `project` 就用它，否则回落到宿主自身 workdir。插件按项目
/// 安装、由项目自己的 daemon 监管，而桌面侧车的 workdir 是 home，所以设置页
/// 必须能把作用域指到它要配置的那个项目（清单与 socket 都在它下面）；不传
/// `project` 的调用方（TUI、旧客户端）保持「宿主 workdir」的原语义。
pub(crate) fn plugin_scope(workdir: &Path, params: &serde_json::Value) -> PathBuf {
    params
        .get("project")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf())
}

/// Serialize a board value for the wire. A value that cannot serialize is the
/// null case rather than a hang: these shapes are plain data, so this never
/// fires in practice, but an RPC must always write *some* response.
pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
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
    // 清单按指纹跟随 `models.json`:运行期改条目,新建 thread 立刻用上新值。
    let catalog =
        yi_agent_runtime::models::CatalogWatcher::new(yi_agent_runtime::models::models_path());
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
            models_path: yi_agent_runtime::models::models_path(),
            // 引导读写的全局文件。生产 = 真实的 `~/.yi-agent`;测试由 harness
            // 注入隔离 tempdir。HOME 缺失回退空路径,与 `board_dir` 同一策略。
            onboarding_env_path: yi_agent_runtime::config::resolve_global_env_path()
                .unwrap_or_default(),
            onboarding_preferences_path: home_dir().join(".yi-agent").join("preferences.json"),
            resident_dir,
            plugin_command: yi_agent_boards::scaffold::default_plugin_command().unwrap_or_default(),
            launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
        },
        production_factory(cfg, theme_for_factory, catalog),
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
    // 与 `run` 一致:清单按指纹跟随 `models.json`,工厂逐会话取当前值。
    let catalog =
        yi_agent_runtime::models::CatalogWatcher::new(yi_agent_runtime::models::models_path());
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
            models_path: yi_agent_runtime::models::models_path(),
            // 引导读写的全局文件。生产 = 真实的 `~/.yi-agent`;测试由 harness
            // 注入隔离 tempdir。HOME 缺失回退空路径,与 `board_dir` 同一策略。
            onboarding_env_path: yi_agent_runtime::config::resolve_global_env_path()
                .unwrap_or_default(),
            onboarding_preferences_path: home_dir().join(".yi-agent").join("preferences.json"),
            resident_dir,
            plugin_command: yi_agent_boards::scaffold::default_plugin_command().unwrap_or_default(),
            launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
        },
        production_factory(cfg, theme_for_factory, catalog),
        // 桌面 = Admin(与 `run`/`serve_stdio` 的 stdio 注册完全一致)。
        Scope::Admin,
        Some(listener),
        Some(relay_url),
    )
    .await
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的 `model_ref`(清单显示名)。工厂闭包是
    /// `Fn` 且被 `'static` 持有,签名里带不进这个逐会话参数,故用 thread-local
    /// 传入。`build_agent` 同步执行、期间无 `await`,故槽位不会被跨任务污染。
    static SESSION_MODEL_REF: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用工厂(`build_agent`)前设定本次会话的模型覆盖(清单显示名),供工厂解析。
///
/// 调用返回后恢复调用前的值,故嵌套调用(如 `thread/start` 内再建其它 agent)不会
/// 泄漏覆盖。`thread/start` 取请求参数、`thread/resume` 取 `loaded.meta.model_ref`。
pub(crate) fn with_session_model_ref<T>(model_ref: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_MODEL_REF.with(|slot| {
        let prev = slot.replace(model_ref);
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的 `model_ref`;不在 [`with_session_model_ref`] 内时为空。
fn current_session_model_ref() -> Option<String> {
    SESSION_MODEL_REF.with(|slot| slot.borrow().clone())
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的 effort 覆盖（七档之一）。与
    /// [`SESSION_MODEL_REF`] 同一理由：工厂闭包签名带不进这个逐会话参数。
    /// 槽位为 `Some` 时**覆盖** `resolve_effective` 从条目带出的默认值；为 `None`
    /// 时跟随条目默认。解析链「会话覆盖 → 条目默认 → 未设置」由此收敛：工厂只认
    /// 「有没有覆盖」，**没有**「强制不发」这一态——清除覆盖等同于未设置(都跟随
    /// 条目默认),这样 resume 重建与活线程经 provider 自补的路径手感一致。
    static SESSION_REASONING_EFFORT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用工厂(`build_agent`)前设定本次会话的 effort 覆盖，供工厂解析。
///
/// `Some(tier)` 覆盖条目默认；`None` 表示本次会话没有覆盖，工厂沿用
/// `resolve_effective` 带出的条目默认。调用返回后恢复调用前的值，故嵌套调用不会
/// 泄漏覆盖。`thread/start` 传 `None`（新会话无覆盖），`thread/resume` 传
/// `meta.reasoning_effort`（清过的即为 `None` ⇒ 同样回落条目默认）。
pub(crate) fn with_session_reasoning_effort<T>(effort: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_REASONING_EFFORT.with(|slot| {
        let prev = slot.replace(effort);
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的 effort 覆盖；不在
/// [`with_session_reasoning_effort`] 内时为 `None`（无覆盖，跟随条目默认）。
fn current_session_reasoning_effort() -> Option<String> {
    SESSION_REASONING_EFFORT.with(|slot| slot.borrow().clone())
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的 `preserve_thinking`（跨轮 CoT 保留）。
    /// 与 [`SESSION_REASONING_EFFORT`] 同一理由：工厂闭包签名带不进这个逐会话
    /// 参数，而 `build_agent_config` 只认 `RuntimeConfig`（其中没有该字段）。
    ///
    /// `Some(v)` = 本次会话显式指定的值（`thread/resume` 传
    /// `meta.preserve_thinking`）；`None` = 未指定（直接构建与测试），此时按默认
    /// `true` 解释，与 `AgentConfig::default()` 一致 ⇒ 零回归。
    static SESSION_PRESERVE_THINKING: std::cell::RefCell<Option<bool>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用工厂(`build_agent`)前设定本次会话的 `preserve_thinking`，供工厂解析。
///
/// 调用返回后恢复调用前的值，故嵌套调用不会泄漏。`thread/start` 传 `true`
/// （新会话默认保留）；`thread/resume` 传 `meta.preserve_thinking`——让重开的
/// 会话沿用落盘的开关，否则被关闭保留的会话重开后会静默恢复为「保留」，与
/// 磁盘和已广播给各端的值相悖。
pub(crate) fn with_session_preserve_thinking<T>(preserve: bool, f: impl FnOnce() -> T) -> T {
    SESSION_PRESERVE_THINKING.with(|slot| {
        let prev = slot.replace(Some(preserve));
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的 `preserve_thinking`；未设定时按默认 `true` 解释。
fn current_session_preserve_thinking() -> bool {
    SESSION_PRESERVE_THINKING.with(|slot| slot.borrow().unwrap_or(true))
}

/// 生产环境的 agent 工厂:按 thread 的 cwd 覆盖 workdir 与 yolo 后引导一个 agent。
///
/// `catalog` 在进程启动时经 `load_catalog()` 读入一次并共享;每次调用再按本次会话
/// 的 `model_ref`(由 [`with_session_model_ref`] 设定)解析 provider/api_url/
/// api_key/model 四个字段,使**整条会话**跑在清单条目的 url/key/model 上,而非
/// 全局 `cfg`(解析不中则原样回退 `cfg`)。
pub(crate) fn production_factory(
    cfg: RuntimeConfig,
    theme: crate::theme_tool::ThemeHandle,
    catalog: Arc<yi_agent_runtime::models::CatalogWatcher>,
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
        let model_ref = current_session_model_ref();
        // 取「当前」清单:运行期新增/修改的条目立刻可见(原先持启动快照)。
        let catalog = catalog.current();
        // 按本次会话解析 provider/api_url/api_key/model;不中则原样回退 cfg。
        let mut thread_cfg = yi_agent_runtime::models::resolve_effective(
            &thread_cfg,
            &catalog,
            model_ref.as_deref(),
        );
        // 会话 effort 覆盖（若有）优先于条目默认：`resolve_effective` 已带出条目
        // 的 `reasoning_effort`，仅当槽位**确有覆盖**（`Some(tier)`）时才覆盖它。
        // 槽位为 `None` 时一律保留条目默认 —— 这同时覆盖「新会话无覆盖」与「显式
        // 清除覆盖」：清除 = 回落条目默认（解析链第 2 级），而非不发该字段。二者
        // 必须一致，否则同一个 meta 在「活线程清除」（provider 自补条目档）与
        // 「resume 重建」两条路径上会有两种线上行为。
        if let Some(effort) = current_session_reasoning_effort() {
            thread_cfg.reasoning_effort = Some(effort);
        }
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

fn parse_relay_url(relay_url: &str) -> anyhow::Result<(String, String)> {
    yi_agent_relay::parse_relay_parts(relay_url)
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

/// Git Diff 焦点守望者：把 [`crate::git_diff_tool::GitDiffHandle`] 的广播翻译成
/// `ui/gitDiff/focus` 通知扇出。
///
/// 与 [`pump_theme_notifications`] 同构，`Lagged` 同样当作**继续**：它只是订阅者
/// 一时落后，`recv()` 仍可继续调用；只有 `Closed`（所有发送端都没了）才结束。
async fn pump_git_diff_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::git_diff_tool::GitDiffFocus>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    loop {
        let focus = match rx.recv().await {
            Ok(focus) => focus,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "git diff watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let n = Notification::UiGitDiffFocus {
            thread_id: focus.thread_id,
            base: focus.base,
            note: focus.note,
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
        models_path,
        onboarding_env_path,
        onboarding_preferences_path,
        resident_dir,
        plugin_command,
        launcher: board_launcher,
        theme,
        git_diff,
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
    // 分片上传的会话登记表（iOS 客户端）。与 `threads` 同属主循环局部量，串行
    // 访问故无需加锁；每轮循环开头 `sweep_expired()` 收走超时会话。
    let mut uploads = crate::image_upload::UploadRegistry::new();
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
    // Git Diff 焦点 → ui/gitDiff/focus。与主题守望者同构：Lagged 继续，
    // 只有 Closed 才结束。
    tokio::spawn(pump_git_diff_notifications(
        git_diff.subscribe(),
        Arc::clone(&hub),
    ));
    // 建 thread 的工具仍需该句柄(与 watcher 同一实例,故通知不落空)。
    let git_diff_handle = git_diff.clone();

    loop {
        // 上传登记表的 TTL 清理：每轮循环开头收走超时的 staging 会话。放在这里
        // 而不是另起定时器，理由同看板调度器——登记表是主循环局部量，`&mut`
        // 借用必须串行。
        uploads.sweep_expired();
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
                //
                // 注:`thread/setPermissionMode` 原在此列,现按 `Control` 放行
                // （见下方方法分支内的门禁）。设计
                // （2026-10-02-mobile-remote-access-design.md §5.4）承诺的「引导去
                // 桌面端授权」入口在代码里并不存在,admin-only 使手机端的 YOLO 切换
                // 结构性不可用。它与 `thread/setModel` 同档:Control 客户端本就能
                // `turn/start` 并在 YOLO 会话上批准任意工具,单开此门不扩大既有能力面。
                //
                // 注:`thread/delete` 原在此列,现同样按 `Control` 放行（见下方分支
                // 内的门禁,2026-10-11 修订）。手机端「长按 = 右键」菜单的「删除」
                // 对齐桌面右键/悬停删除按钮,admin-only 使它必然 `-32014`、与 YOLO
                // 同一死路。它属与 `thread/rename` 同级的会话管理,Control 本就能
                // `thread/start`/`turn/start`,单开此门不扩大既有能力面。`process/kill`
                // (杀进程)仍严格 admin。
                const ADMIN_METHODS: [&str; 3] = ["process/kill", "pair/create", "device/revoke"];
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

                // 域 handler 需要主循环的局部量;每个域臂按需构造一次
                // [`crate::rpc::AppState`],借用到臂末(通常 `continue`)为止,故主循环
                // 各局部量的所有权与借用关系与搬移前完全一致。
                macro_rules! app_state {
                    () => {
                        crate::rpc::AppState::new(
                            &hub, &client, client_scope, &cfg, &theme_handle, &models_path,
                            &onboarding_env_path, &onboarding_preferences_path, &board_dir,
                            &resident_dir, &plugin_command, &watchman_home, &watchman_install,
                            &watchman_uninstall, &board_launcher, &mut uploads, &mut threads,
                            &workspaces,
                        )
                    };
                }
                match crate::rpc::domain_for(method.as_str()) {
                    crate::rpc::Domain::Ui => {
                        let mut state = app_state!();
                        match crate::rpc::ui::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Model => {
                        let mut state = app_state!();
                        match crate::rpc::model::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Onboarding => {
                        let mut state = app_state!();
                        match crate::rpc::onboarding::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Board => {
                        let mut state = app_state!();
                        match crate::rpc::board::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Plugin => {
                        let mut state = app_state!();
                        match crate::rpc::plugin::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Image => {
                        let mut state = app_state!();
                        match crate::rpc::image::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Workspace => {
                        let mut state = app_state!();
                        match crate::rpc::workspace::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Process => {
                        let mut state = app_state!();
                        match crate::rpc::process::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    // 尚未搬移的域(以及 `initialize`)仍走原始 dispatch。
                    // 逐域搬移期间,还没搬的域也落到这里,故用 `_` 一并兜住
                    // (`Domain::Unknown` 覆盖 `initialize` 与所有未识别前缀)。
                    _ => {}
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

                        // 可选的会话级模型覆盖(清单显示名)。wire 用 camelCase
                        // `modelRef`,兼容 snake_case `model_ref`;二者都缺省即
                        // `None`(跟随清单全局默认,再回退 cfg)。
                        let model_ref = req
                            .params
                            .get("modelRef")
                            .or_else(|| req.params.get("model_ref"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string);

                        // 本次会话的 `model_ref` 经 thread-local 交给工厂解析 provider /
                        // api_url / api_key / model(命中清单默认则用条目,否则回退 cfg)。
                        let built = match with_session_model_ref(model_ref.clone(), || {
                            with_session_reasoning_effort(None, || {
                                with_session_preserve_thinking(true, || {
                                    build_agent(None, Path::new(&cwd), mode)
                                })
                            })
                        }) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(&hub, &client, err_response(id, RpcError::internal(e.to_string()))).await?;
                                continue;
                            }
                        };

                        // 落盘/上报的生效串取自**实际建出的 agent**,而非另按清单解析一次。
                        // `production_factory` 只在进程启动时快照一次清单,运行期经
                        // `model/upsert` 新增的条目不在其快照内;此处若再读一次磁盘,就可能
                        // 上报一个工厂实际未采用的模型(任务 7 评审 Finding 1)。用
                        // `agent.config().model` 才能保证「上报 == 实际运行」。
                        let model = built.agent.config().model.clone();

                        // 与 `model` 同源:上报**实际建出**的 agent 的生效档位。
                        // 用硬编码 null 会在条目带默认档时谎报(本会话其实跑着该档),
                        // 且与 resume 的口径不一致。
                        let reasoning_effort =
                            built.agent.config().gen_params.reasoning_effort.clone();

                        // 与 `model` 同源:上报**实际建出**的 agent 的开关值。
                        let preserve_thinking = built.agent.config().preserve_thinking;

                        // 内核负责「attach_delegation → 建 driver 通道 → insert 到
                        // `threads` → spawn 守望者与 driver」;RPC 层的通知与响应仍在
                        // 本分支完成,与抽取前逐字一致。
                        start_thread_core(
                            &mut threads,
                            &mut pending_activation,
                            &mut process_watches,
                            &runtimes,
                            &thread_roots,
                            &models_path,
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
                            &git_diff_handle,
                            &workspaces,
                            model.clone(),
                            model_ref.clone(),
                            None,
                            None,
                        )
                        .await?;

                        write_notification(&hub, &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                model_ref: model_ref.clone(),
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
                                    "model_ref": model_ref,
                                    "permission_mode": mode,
                                    // 新会话无覆盖 ⇒ 上报实际运行值(解析出的条目默认
                                    // 或 cfg;都没有则 null),与实际建出的 agent 同源。
                                    "reasoning_effort": reasoning_effort,
                                    "preserve_thinking": preserve_thinking,
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

                        let built = match with_session_model_ref(loaded.meta.model_ref.clone(), || {
                            with_session_reasoning_effort(loaded.meta.reasoning_effort.clone(), || {
                                with_session_preserve_thinking(loaded.meta.preserve_thinking, || {
                                    build_agent(Some(session), Path::new(&cwd), mode)
                                })
                            })
                        }) {
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
                        // 与 `thread/start` 同款:上报/落盘的生效串取自**实际建出的 agent**
                        // (`agent.config().model`),而非按清单再解析一次。工厂持有的是启动时
                        // 快照,运行期新增的条目会让「另解析一次」与实际运行不一致
                        // (任务 7 评审 Finding 1);不照抄 meta.model 也是为了让旧库/异常库里
                        // 一个与当前清单不符的陈旧串被本次实际生效值取代。
                        let model = built.agent.config().model.clone();
                        // 与 `model` 同源:上报**实际建出**的 agent 的生效档位。清除覆盖
                        // 后的 meta 是 `None`,但工厂此时已回落到条目默认,故响应必须报
                        // 该实际值,而非照抄 meta 的 null(否则客户端挂着的控件会骗人)。
                        let reasoning_effort =
                            built.agent.config().gen_params.reasoning_effort.clone();
                        // 与 `reasoning_effort` 同源:上报**实际建出**的 agent 的开关
                        // 值(取自 meta,经工厂落到 config),让重开的客户端立刻拿到
                        // 初始状态,而不必等一次广播。
                        let preserve_thinking = built.agent.config().preserve_thinking;
                        let runtime_dir =
                            yi_agent_subagent::attach::project_runtime_directory(Path::new(&cwd));
                        let activation = attach_delegation(
                            &runtimes,
                            &thread_roots,
                            &runtime_dir,
                            &models_path,
                            &cfg,
                            &cwd,
                            &thread_id,
                            &theme,
                            &git_diff_handle,
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

                        // 回放:thread/started → 若干批量 items/completed → 最近用量 → 响应。
                        write_notification(&hub, &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                model_ref: loaded.meta.model_ref.clone(),
                            },
                        )
                        .await?;
                        // 回放：分块批量下发（每块一条 `items/completed`），经带背压的
                        // 投递——本地 reliable 客户端队列满时等待而非丢帧。旧实现逐条
                        // try_send,超过 256 格队列即静默丢帧,长会话尾部就此消失。
                        for chunk in chunk_items_for_replay(loaded.items) {
                            if let Err(e) = write_notification_await(
                                &hub,
                                &Notification::ItemsCompleted {
                                    thread_id: thread_id.clone(),
                                    items: chunk,
                                },
                            )
                            .await
                            {
                                eprintln!(
                                    "[app-server] replay aborted for {thread_id}: {e}"
                                );
                                break;
                            }
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
                                    "model_ref": loaded.meta.model_ref,
                                    "permission_mode": loaded.meta.permission_mode,
                                    "reasoning_effort": reasoning_effort,
                                    "preserve_thinking": preserve_thinking,
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
                    "thread/setModel" => {
                        // 与 `model/*` 写路径同档:切模型会改 provider/凭据,需 `Control`。
                        if client_scope < Scope::Control {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // `name: null`(或缺失)= 清覆盖;字符串 = 选显示名。非字符串/非
                        // null 一律 `-32602`,不静默当作清覆盖。
                        let name = match req.params.get("name") {
                            None | Some(serde_json::Value::Null) => None,
                            Some(serde_json::Value::String(n)) => Some(n.clone()),
                            Some(_) => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("name must be a string or null"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 会话覆盖必须命中清单条目,否则 `model_not_found` 且零副作用。
                        // 读清单路径走注入的 `models_path`(与 `model/*` 同一份),
                        // 故 upsert 刚落盘的条目立刻可见。
                        let catalog = yi_agent_runtime::models::load_catalog_from(&models_path);
                        if let Some(n) = &name {
                            if !catalog.models.iter().any(|m| &m.name == n) {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::model_not_found(n)),
                                )
                                .await?;
                                continue;
                            }
                        }
                        // 生效串按 清单条目 → 全局默认 → cfg 解析;校验通过后再落盘。
                        let effective =
                            yi_agent_runtime::models::resolve_effective(&cfg, &catalog, name.as_deref());
                        // 先让在场 driver 用新覆盖重建 agent,成功后再落盘。铁律
                        // 「上报/落盘 == 实际运行」:若 provider 重建失败(build_provider
                        // 抛错),agent 仍跑旧模型,此时若已落盘就会让磁盘与响应谎报新模型
                        // (任务 7 评审 Finding 3)。故先重建、后落盘;失败即回错误且零磁盘
                        // 副作用,由客户端重试。
                        //
                        // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
                        // meta.model_ref 重建,故跳过本段。
                        if threads.contains_key(&thread_id) {
                            let (reply_tx, reply_rx) = oneshot::channel();
                            // 借用只活在下面这个块内(send 的 future 借 `session`),
                            // 出块即释放,后续 `threads.get_mut` 才不被占用。
                            let send_result = {
                                let session = threads
                                    .get(&thread_id)
                                    .expect("contains_key was just checked");
                                session
                                    .session_tx
                                    .send(SessionCommand::SetModel {
                                        effective: Box::new(effective.clone()),
                                        model_ref: name.clone(),
                                        reply: reply_tx,
                                    })
                                    .await
                            };
                            // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                            // 同样意味着无法保证生效,一并按失败处理。
                            let switched =
                                send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                            if !switched {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::internal(
                                            "the live session could not switch to the requested \
                                             model; it is still running the previous one"
                                                .to_string(),
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        }
                        // 重建成功后(冷 thread 则直接)落盘:一次原子写把 `model_ref` 与
                        // 生效 `model` 同进同出,避免并发 `touch` 读到自相矛盾的组合。
                        let store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        if let Err(e) = store.set_model(&thread_id, name.as_deref(), &effective.model) {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            } else {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                            continue;
                        }
                        // 让内存里的 `ThreadSession.model` 与实际生效值保持一致。注意:
                        // `thread/list(All)` 目前是从**盘上 meta**(`thread_summary_json`)
                        // 渲染 model 的,`ThreadSession.model` 在当前 crate 内尚无读取方;
                        // 这里仍同步刷新,是为了让内存视图与刚落盘的 meta 同源、不残留旧串
                        // (供将来的读取方或排查时使用),而不是因为 list 会读它。
                        if let Some(session) = threads.get_mut(&thread_id) {
                            session.model = effective.model.clone();
                        }
                        write_response(
                            &hub, &client,
                            ok_response(id, json!({ "ok": true, "model": effective.model })),
                        )
                        .await?;
                    }
                    "thread/setReasoningEffort" => {
                        // 与 `thread/setModel` 同档:effort 会改推理行为(能力面/计费),
                        // 需 `Control`。
                        if client_scope < Scope::Control {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // `effort: null`(或缺失)= 清覆盖;字符串必须在七档白名单内,
                        // 否则 `-32602`;其余 JSON 类型同样 `-32602`,不静默当作清覆盖。
                        // 白名单与 `model_rpc::upsert` 的条目校验逐字一致。
                        let effort = match req.params.get("effort") {
                            None | Some(serde_json::Value::Null) => None,
                            Some(serde_json::Value::String(s)) => {
                                if !matches!(
                                    s.as_str(),
                                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                                ) {
                                    write_response(
                                        &hub, &client,
                                        err_response(
                                            id,
                                            RpcError::invalid_params(
                                                "effort must be one of \
                                                 none|minimal|low|medium|high|xhigh|max",
                                            ),
                                        ),
                                    )
                                    .await?;
                                    continue;
                                }
                                Some(s.clone())
                            }
                            Some(_) => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("effort must be a string or null"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 先让在场 driver 用新 effort 重建 agent（复用当前 provider——
                        // effort 不烤进 URL/key），成功后再落盘。铁律「上报/落盘 == 实际
                        // 运行」同 setModel:若重建失败,agent 仍跑旧档位,此时落盘就会让
                        // 磁盘与响应谎报。故先重建、后落盘;失败即回错误且零磁盘副作用。
                        //
                        // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
                        // meta.reasoning_effort 重建,故跳过本段。
                        if threads.contains_key(&thread_id) {
                            let (reply_tx, reply_rx) = oneshot::channel();
                            // 借用只活在下面这个块内(send 的 future 借 `session`),
                            // 出块即释放,后续 `threads.get_mut` 才不被占用。
                            let send_result = {
                                let session = threads
                                    .get(&thread_id)
                                    .expect("contains_key was just checked");
                                session
                                    .session_tx
                                    .send(SessionCommand::SetReasoningEffort {
                                        effort: effort.clone(),
                                        reply: reply_tx,
                                    })
                                    .await
                            };
                            // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                            // 同样意味着无法保证生效,一并按失败处理。
                            let applied =
                                send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                            if !applied {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::internal(
                                            "the live session could not switch to the requested \
                                             reasoning effort; it is still running the previous one"
                                                .to_string(),
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        }
                        // 重建成功后(冷 thread 则直接)落盘:读-改-写 + 原子写,与
                        // `set_model` 同锁,避免并发 `touch` / `rename` 互相覆盖。
                        let store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        if let Err(e) = store.set_reasoning_effort(&thread_id, effort.as_deref()) {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            } else {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                            continue;
                        }
                        // 列表层恒推(同 `thread/permissionModeChanged`):桌面改 → 手机
                        // effort 控件同步,反之亦然。
                        let _ = write_notification(
                            &hub,
                            &Notification::ReasoningEffortChanged {
                                thread_id: thread_id.clone(),
                                effort: effort.clone(),
                            },
                        )
                        .await;
                        write_response(
                            &hub, &client,
                            ok_response(id, json!({ "ok": true, "effort": effort })),
                        )
                        .await?;
                    }
                    "thread/setPreserveThinking" => {
                        // 与 `thread/setReasoningEffort` 同档:开关会改请求体(是否回传
                        // 历史轮的推理凭证),需 `Control`。
                        if client_scope < Scope::Control {
                            write_response(
                                &hub, &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // `preserve` 必须是 JSON 布尔;缺失/null/字符串/数字一律
                        // `-32602`,**不**静默取默认值——静默默认会让调用方以为开关
                        // 已按其所愿设置,而实际可能停在相反的值。
                        let preserve = match req.params.get("preserve") {
                            Some(serde_json::Value::Bool(b)) => *b,
                            _ => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("preserve must be a boolean"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 先让在场 driver 用新开关重建 agent(复用当前 provider——
                        // 开关不烤进 URL/key),成功后再落盘。铁律「上报/落盘 == 实际
                        // 运行」同 setReasoningEffort:若重建失败,agent 仍跑旧值,此时
                        // 落盘就会让磁盘与响应谎报。故先重建、后落盘;失败即回错误且
                        // 零磁盘副作用。
                        //
                        // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
                        // meta.preserve_thinking 重建,故跳过本段。
                        if threads.contains_key(&thread_id) {
                            let (reply_tx, reply_rx) = oneshot::channel();
                            // 借用只活在下面这个块内(send 的 future 借 `session`),
                            // 出块即释放,后续 `threads.get_mut` 才不被占用。
                            let send_result = {
                                let session = threads
                                    .get(&thread_id)
                                    .expect("contains_key was just checked");
                                session
                                    .session_tx
                                    .send(SessionCommand::SetPreserveThinking {
                                        preserve,
                                        reply: reply_tx,
                                    })
                                    .await
                            };
                            // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                            // 同样意味着无法保证生效,一并按失败处理。
                            let applied =
                                send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                            if !applied {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        id,
                                        RpcError::internal(
                                            "the live session could not switch the \
                                             preserve-thinking flag; it is still running the \
                                             previous value"
                                                .to_string(),
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        }
                        // 重建成功后(冷 thread 则直接)落盘:读-改-写 + 原子写,与
                        // `set_reasoning_effort` 同锁,避免并发 `touch` / `rename` 互相覆盖。
                        let store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        if let Err(e) = store.set_preserve_thinking(&thread_id, preserve) {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            } else {
                                write_response(
                                    &hub, &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                            continue;
                        }
                        // 列表层恒推(同 `thread/reasoningEffortChanged`):桌面改 → 手机
                        // 控件同步,反之亦然。
                        let _ = write_notification(
                            &hub,
                            &Notification::PreserveThinkingChanged {
                                thread_id: thread_id.clone(),
                                preserve,
                            },
                        )
                        .await;
                        write_response(
                            &hub, &client,
                            ok_response(id, json!({ "ok": true, "preserve": preserve })),
                        )
                        .await?;
                    }
                    "thread/setPermissionMode" => {
                        // 与 `thread/setModel` 同档:Control 起。Observe 拿到的是
                        // 掩码级视图,不得放开 OS 沙箱。
                        if client_scope < Scope::Control {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
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
                        // 广播给所有客户端(列表层恒推):桌面改 → 手机 chip 同步,
                        // 手机改 → 桌面 chip 同步。与 `setModel` 的 ModelChanged
                        // 广播同一手法。
                        let _ = write_notification(
                            &hub,
                            &Notification::PermissionModeChanged {
                                thread_id: thread_id.clone(),
                                mode,
                            },
                        )
                        .await;
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
                        // 与 `thread/rename` / `thread/setModel` 同档:Control 起。
                        // Observe 是掩码级只读视图,不得删除会话。
                        if client_scope < Scope::Control {
                            write_response(
                                &hub,
                                &client,
                                err_response(id, RpcError::insufficient_scope(Scope::Control)),
                            )
                            .await?;
                            continue;
                        }
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
                        // 卡片会话的 worktree 判定：只读会话自带 meta，不碰插件。
                        // 顺手把 board_project 拷到局部变量：会话文件随后即被删除，
                        // 到执行删除时再 load 只会失败，故必须在此刻留下。
                        let mut board_project: Option<String> = None;
                        let reclaim = match thread_store.load(&thread_id) {
                            Ok(Some(loaded)) => {
                                board_project = loaded.meta.board_project.clone();
                                crate::worktree_reclaim::decide(
                                    loaded.meta.card_id.as_deref(),
                                    loaded.meta.board_project.as_deref(),
                                    Path::new(&loaded.meta.cwd),
                                )
                            }
                            _ => crate::worktree_reclaim::WorktreeReclaim::None,
                        };
                        // 「需确认」= 有活跃子代理（既有）或 worktree 处置为破坏性/硬保留（新）。
                        let children = if force {
                            None
                        } else {
                            match cancel_thread_children(&runtimes, &threads, &thread_id, false) {
                                Ok(ThreadCancelOutcome::NeedsConfirmation(count)) => Some(count),
                                // 无事可取消时按 0 处理，交给 worktree 判定决定是否确认。
                                Ok(ThreadCancelOutcome::Cancelled(_)) => Some(0),
                                // 查询失败（如会话不在内存）按 0 处理，但照旧记日志——与既有行为一致。
                                Err(cause) => {
                                    eprintln!(
                                        "[app-server] could not cancel subagents for {thread_id}: {cause}"
                                    );
                                    Some(0)
                                }
                            }
                        };
                        let worktree_needs_confirm = matches!(
                            reclaim,
                            crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete { .. }
                                | crate::worktree_reclaim::WorktreeReclaim::Keep { .. }
                        );
                        if !force && (children.unwrap_or(0) > 0 || worktree_needs_confirm) {
                            let mut payload = json!({
                                "status": "needs_confirmation",
                                "active_children": children.unwrap_or(0),
                            });
                            if let Some(worktree) = worktree_json(&reclaim) {
                                payload["worktree"] = worktree;
                            }
                            write_response(&hub, &client, ok_response(id, payload)).await?;
                            continue;
                        }
                        if force {
                            // force 下真的回收子代理（既有语义）。
                            match cancel_thread_children(&runtimes, &threads, &thread_id, true) {
                                Ok(ThreadCancelOutcome::Cancelled(cancelled)) if cancelled > 0 => {
                                    eprintln!(
                                        "[app-server] cancelled {cancelled} subagent task(s) for {thread_id}"
                                    );
                                }
                                Err(cause) => eprintln!(
                                    "[app-server] could not cancel subagents for {thread_id}: {cause}"
                                ),
                                _ => {}
                            }
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
                        // 收走该会话的附件:与 thread 文件同一生命周期。放在 delete
                        // 之后(driver 已结束落盘),避免竞态——工作区被挪走时
                        // `remove_attachments` 尽力而为,不让删除失败。这里拿不到
                        // cwd(见 `detach_unused_runtimes` 的注释:不为它改
                        // `store_lookup` 签名),故由 store 的 root
                        // (`<cwd>/.yi-agent/threads`)上溯两级得到工作区。
                        if let Some(cwd) = thread_store.root().parent().and_then(|p| p.parent()) {
                            crate::attachments::remove_attachments(cwd, &thread_id);
                        }
                        // 上传登记表也随 thread 一起 prune：否则一条 `Committed`
                        // 记录会活到 10 分钟 TTL，让被删 thread 的 `uploadId` 仍能
                        // 反查到已被删掉的附件路径（随后 `prepare_image_file` 读空），
                        // 未 commit 的 staging 槽与进程级会话名额也被死 thread 白占。
                        // `drop_thread` 只需内存与每条会话已记的 staging 路径，故这里
                        // 拿不到 cwd 也无妨（已落盘的附件由上面 `remove_attachments` 收）。
                        uploads.drop_thread(&thread_id);
                        // 会话文件已摘除：按判定处置 worktree。删除失败只记日志，不阻断会话删除（D9）。
                        // board_project 已在进分支时留好——此处绝不能再 load（会话文件已不存在）。
                        // 但失败这件事本身必须回报：用户刚被告知该 worktree 会被删掉，此刻会话
                        // 已摘除、应用内再也删不掉它，又按 D10 让卡片上的 thread_id 悬空——
                        // 若默默失败，worktree 就无声滞留、用户永远看不见。故除宿主日志外，
                        // 把这一次"该删却没删成"如实回给客户端（失败才现身，成功仍为空）。
                        let mut worktree_failure: Option<serde_json::Value> = None;
                        if let (
                            crate::worktree_reclaim::WorktreeReclaim::Delete { path }
                            | crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete {
                                path, ..
                            },
                            Some(project),
                        ) = (&reclaim, board_project.as_deref())
                        {
                            if let Err(error) =
                                crate::worktree_reclaim::remove(path, Path::new(project), force)
                            {
                                eprintln!(
                                    "[app-server] could not remove the worktree for {thread_id}: {error}"
                                );
                                worktree_failure = Some(json!({
                                    "action": "remove",
                                    "removed": false,
                                    "path": path.to_string_lossy(),
                                    "reason": error,
                                }));
                            }
                        }
                        // 只有"被承诺会删、且确实没删成"才带字段；普通会话（reclaim=None）、
                        // Keep、以及删除成功一律仍是空成功体，既有形状零变化。
                        let body = match worktree_failure {
                            Some(worktree) => json!({ "worktree": worktree }),
                            None => json!({}),
                        };
                        write_response(&hub, &client, ok_response(id, body)).await?;
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
                            &mut uploads,
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
                    "thread/diff/read" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone())
                            .await?
                        {
                            continue;
                        }
                        // cwd 是 diff 的唯一来源：thread 的 cwd，缺省兜底 cfg.workdir。
                        let cwd = threads
                            .get(&thread_id)
                            .map(|t| t.cwd.clone())
                            .filter(|c| !c.is_empty())
                            .unwrap_or_else(|| cfg.workdir.display().to_string());
                        let repo = Path::new(&cwd);
                        let base = req.params.get("base").and_then(|v| v.as_str());
                        let result =
                            if let Some(commit) = req.params.get("commit").and_then(|v| v.as_str()) {
                                let (unified_diff, truncated) =
                                    crate::git_diff::commit_diff(repo, commit);
                                json!({ "unifiedDiff": unified_diff, "truncated": truncated })
                            } else if let Some(path) =
                                req.params.get("path").and_then(|v| v.as_str())
                            {
                                let d = crate::git_diff::thread_diff(repo, base);
                                let (unified_diff, truncated) = crate::git_diff::file_diff(
                                    repo,
                                    d.merge_base.as_deref(),
                                    path,
                                );
                                json!({ "unifiedDiff": unified_diff, "truncated": truncated })
                            } else {
                                serde_json::to_value(crate::git_diff::thread_diff(repo, base))
                                    .unwrap_or(json!(null))
                            };
                        write_response(&hub, &client, ok_response(id, result)).await?;
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
                    &mut uploads,
                    &runtimes,
                    &thread_roots,
                    &models_path,
                    &cfg,
                    &hub,
                    &turn_tx,
                    &pending,
                    permission_timeout,
                    &perm_seq,
                    &theme,
                    &git_diff_handle,
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
pub(crate) fn ws_client_for_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
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
pub(crate) fn forget_ws_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
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

pub(crate) fn ok_response(id: RequestId, result: serde_json::Value) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: Some(result),
        error: None,
    }
}

pub(crate) fn err_response(id: RequestId, error: RpcError) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: None,
        error: Some(error),
    }
}

pub(crate) async fn write_response(
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

/// 与 [`write_notification`] 同款序列化与投递键，但走**带背压**的
/// [`crate::broadcast::Broadcaster::broadcast_await`]：本地 reliable 客户端队列
/// 满时等待而非丢帧。用于历史回放这类「必须完整送达」的批量下发。
async fn write_notification_await(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_await(key, frame)
        .await
        .map_err(|_| anyhow::anyhow!("replay client closed"))?;
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

/// 把 worktree 判定渲染成 `thread/delete` 的 `needs_confirmation.worktree` 字段。
///
/// `None` 表示"不适用/无需呈现"（普通会话、或不回收）——此时响应不带该字段，
/// 普通会话的既有响应因此零变化。
fn worktree_json(reclaim: &crate::worktree_reclaim::WorktreeReclaim) -> Option<serde_json::Value> {
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

/// 把 thread meta 渲染成 wire 上的 ThreadSummary（含 `pinned`）。
fn thread_summary_json(
    m: &crate::thread_store::ThreadMeta,
    threads: &HashMap<String, ThreadSession>,
) -> serde_json::Value {
    json!({
        "thread_id": m.thread_id,
        "cwd": m.cwd,
        "model": m.model,
        "model_ref": m.model_ref,
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
    user_attachments: &[crate::protocol::Attachment],
    user_image_refs: &[crate::protocol::ImageRef],
    completed_items: &[crate::protocol::Item],
    messages: Vec<yi_agent_core::Message>,
    usage: Option<crate::thread_store::TurnUsage>,
) -> crate::thread_store::PartialTurn {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    items.push(opening_user_item(
        turn_id,
        user_prompt,
        user_attachments.to_vec(),
        user_image_refs.to_vec(),
    ));
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
    user_attachments: &[crate::protocol::Attachment],
    user_image_refs: &[crate::protocol::ImageRef],
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    provider: &Arc<dyn yi_agent_core::Provider>,
    config: &yi_agent_core::AgentConfig,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    if let Some(prompt) = user_prompt {
        // 基线 server 不 emit userMessage，必须在落盘时补齐，否则 resume 会丢用户提问。
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{}", turn_id.unwrap_or("session-command")),
            text: prompt.to_string(),
            attachments: user_attachments.to_vec(),
            images: user_image_refs.to_vec(),
        });
    }
    items.extend(completed_items);

    // 先取助手侧材料：`items` 接下来会 move 进 `record`，此后无法再借用。
    let assistant_text = completed_items_agent_text(&items);
    // 落盘副本：关闭跨轮保留（`preserve_thinking == false`）时剥离思考块，使磁盘
    // 与「已放弃跨轮保留」的会话状态一致——否则每轮追加的日志行仍携带刚产出的
    // 凭证，重开会话即「复活」（spec §4.1/§7.4）。session 本体（展示与请求的权威）
    // 不受影响：这里只剥**落盘副本**，与请求路径的剥离互不干扰。
    // 默认 `true` 时逐字取会话内容 ⇒ 与今日落盘完全一致（零回归）。
    let persisted_messages = if agent.config().preserve_thinking {
        agent.session().messages().to_vec()
    } else {
        yi_agent_core::strip_thinking(agent.session().messages())
    };
    let record = crate::thread_store::TurnLine::Turn {
        items,
        usage: last_usage,
        messages: persisted_messages,
    };
    // append 失败则跳过 touch：避免出现"幽灵" thread。
    let mut first_turn = false;
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {thread_id}: {e}");
    } else if let Some(prompt) = user_prompt {
        match store.touch(thread_id, Some(prompt)) {
            // 本次刚写入兜底截断标题 = 该 thread 的首轮。
            Ok(true) => first_turn = true,
            Ok(false) => {}
            Err(e) => eprintln!("[app-server] failed to update meta for {thread_id}: {e}"),
        }
    }

    // 先广播「本轮已落盘」，再做标题生成：标题是锦上添花，绝不能拖住 turn 收尾。
    // delete/interrupt 路径正是靠这枚 Finished 判定落盘完成，而标题生成最长可等
    // TITLE_TIMEOUT——若排在它前面，会让收尾凭空多出 5 秒延迟。
    let _ = update_status(hub, status, thread_id, ThreadStatus::Idle).await;
    if let Some(turn_id) = turn_id {
        let _ = turn_tx.send(finished_event(thread_id, turn_id)).await;
    }

    // 仅首轮生成标题：拿 touch 写入的兜底截断值作 CAS 凭据，尝试用 LLM 标题覆盖。
    // 任何失败/超时/空结果都静默保留兜底标题。clear / compact 传 `user_prompt = None`，
    // 天然不会走到这里。
    if first_turn {
        if let Some(prompt) = user_prompt {
            let fallback = crate::thread_store::title_from(prompt);
            match tokio::time::timeout(
                TITLE_TIMEOUT,
                yi_agent_core::title::generate_title(
                    provider,
                    config,
                    prompt,
                    assistant_text.as_deref(),
                ),
            )
            .await
            {
                Ok(Ok(Some(title))) => {
                    // 仅在 CAS **确实替换**（`Ok(true)`）后广播：`Ok(false)` 表示标题
                    // 已被手动改名抢占或 thread 不存在，磁盘上不是这个值，广播它会让
                    // 客户端显示一个不存在的标题。广播失败只记 stderr——标题是锦上
                    // 添花，绝不能拖住 turn 收尾。
                    match store.set_title_if_unchanged(thread_id, &fallback, &title) {
                        Ok(true) => {
                            if let Err(e) = write_notification(
                                hub,
                                &Notification::TitleChanged {
                                    thread_id: thread_id.to_string(),
                                    title,
                                },
                            )
                            .await
                            {
                                eprintln!(
                                    "[app-server] failed to broadcast title change {thread_id}: {e}"
                                );
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("[app-server] failed to set LLM title {thread_id}: {e}")
                        }
                    }
                }
                // 模型产出无法作为标题：保留兜底标题。
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    tracing::info!(%e, %thread_id, "title generation failed; keeping fallback");
                }
                Err(_) => {
                    tracing::info!(%thread_id, "title generation timed out; keeping fallback");
                }
            }
        }
    }
}

/// 从本轮 items 里取第一条助手文本：作为标题生成材料的"助手回复"侧。
///
/// 空文本的 `AgentMessage` 视为不存在：`build_title_material` 对 `Some("")` 会渲染
/// 一个悬空的"助手回复："标签，反而污染标题模型的输入。
fn completed_items_agent_text(items: &[crate::protocol::Item]) -> Option<String> {
    items.iter().find_map(|item| match item {
        crate::protocol::Item::AgentMessage { text, .. } if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    })
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
                &[],
                &[],
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                provider,
                config,
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
                &[],
                &[],
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                provider,
                config,
                status,
            )
            .await;
            let _ = reply.send(outcome);
        }
        SessionCommand::SetModel {
            effective,
            model_ref,
            reply,
        } => {
            // provider 构造放在最前：失败就在这里短路，`agent` 尚未被消费，原样返回。
            // 这是「绝不半重建」的关键——绝不在换了 config/句柄之后才发现造不出 provider。
            let provider = match yi_agent_runtime::bootstrap::build_provider(&effective) {
                Ok(provider) => provider,
                Err(error) => {
                    tracing::warn!(%error, ?model_ref, "session model switch failed; keeping the old agent");
                    let _ = reply.send(Err(error.to_string()));
                    return agent;
                }
            };
            // 新 cfg 只覆盖 model（provider/api_url/api_key 已烤进新 provider；其余
            // budget/system_prompt 等沿用原 config，避免切模型顺带重置别的设置）。
            let mut next_config = agent.config().clone();
            next_config.model = effective.model.clone();
            // 三样必须原样保留：session `Arc`（否则委派 caller 读不到本会话）、
            // 工具注册表、审批路径（checker + decision_rx）。与
            // [`rebuild_thread_agent_with_theme`] 的语义一致。
            let session_handle = agent.session_handle();
            let tools = agent.tools();
            let permission = agent.permission_checker();
            let decision = agent.decision_rx();
            let mut rebuilt = yi_agent_core::Agent::new(provider, tools, next_config)
                .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            tracing::info!(model = %effective.model, ?model_ref, "session model switched");
            // Design §5.3 step 4:重建成功后广播本会话的生效模型,让所有订阅该
            // thread 的客户端(桌面 + 手机)立刻刷新,无需轮询。
            //
            // 在此直接广播、而非让 driver 经事件流走 Translator:`SetModel` 只在
            // **没有 turn 在跑**时经本函数执行,此刻不存在 `agent.run()` 的事件流
            // 来承载 `AgentEvent::ModelChanged`(translator 仅在本轮 stream 上被
            // 驱动)。直接广播与「重建完成」同一时刻发生;Translator 侧那条臂保留,
            // 供将来事件流真的承载该事件时复用。
            let _ = write_notification(
                hub,
                &Notification::ModelChanged {
                    thread_id: thread_id.to_string(),
                    model: effective.model.clone(),
                },
            )
            .await;
            let _ = reply.send(Ok(()));
            agent = rebuilt;
        }
        SessionCommand::SetReasoningEffort { effort, reply } => {
            // effort 不烤进 provider（URL/key 未变），故复用当前 `provider`，
            // 只换 `AgentConfig.gen_params.reasoning_effort` 后重建 agent。
            // 三样必须原样保留：session `Arc`、工具注册表、审批路径
            // （checker + decision_rx）。与 `SetModel` 的重建语义一致。
            let mut next_config = agent.config().clone();
            next_config.gen_params.reasoning_effort = effort.clone();
            let session_handle = agent.session_handle();
            let tools = agent.tools();
            let permission = agent.permission_checker();
            let decision = agent.decision_rx();
            let mut rebuilt = yi_agent_core::Agent::new(provider.clone(), tools, next_config)
                .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            tracing::info!(?effort, "session reasoning effort switched");
            let _ = reply.send(Ok(()));
            agent = rebuilt;
        }
        SessionCommand::SetPreserveThinking { preserve, reply } => {
            // provider 不变（开关不烤进 URL/key），故复用当前 `provider`，只把
            // `AgentConfig.preserve_thinking` 换掉后重建 agent。
            //
            // **先**把 strip 落到共享 session 本体：`preserve_thinking` 只影响请求
            // 副本（见 core 的 `run_loop`），若不在此就地剥离，落盘/展示的权威
            // transcript 仍带着凭证，重开会话后它们会「复活」——与「关闭保留」的
            // 开关语义直接矛盾（spec §4.1）。`set_session_messages` 与
            // `session_handle()` 摸的是同一个 `Arc<Mutex<Session>>`，故先写后取。
            if !preserve {
                let stripped = yi_agent_core::strip_thinking(agent.session().messages());
                agent.set_session_messages(stripped);
            }
            let mut next_config = agent.config().clone();
            next_config.preserve_thinking = preserve;
            // 三样必须原样保留：session `Arc`（否则委派 caller 读不到本会话）、
            // 工具注册表、审批路径（checker + decision_rx）。与
            // `SetReasoningEffort` 的重建语义一致。
            let session_handle = agent.session_handle();
            let tools = agent.tools();
            let permission = agent.permission_checker();
            let decision = agent.decision_rx();
            let mut rebuilt = yi_agent_core::Agent::new(provider.clone(), tools, next_config)
                .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            tracing::info!(preserve, "session preserve-thinking switched");
            agent = rebuilt;
            // 立即把（可能已剥离的）会话状态落盘：否则「关闭后、下一轮开始前」
            // 重开会话，日志最后一行仍是切换前那条带凭证的记录，凭证就此「复活」
            // ——与「关闭保留」的语义矛盾（spec §7.4：关闭时落盘结果同样被剥离）。
            // 必须在 `agent = rebuilt` **之后**调用：`persist_and_finish_turn` 会读
            // `agent.config().preserve_thinking` 决定落盘副本是否剥离，读到旧值就
            // 会在关闭时仍把凭证写盘。与 `Clear` / `Compact` 同款：`turn_id` 传
            // `None`（不发 Finished），也不 touch meta（meta 已由 RPC 层落盘）。
            persist_and_finish_turn(
                thread_id,
                None,
                None,
                &[],
                &[],
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                provider,
                config,
                status,
            )
            .await;
            let _ = reply.send(Ok(()));
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
            attachments: turn_attachments,
            image_blocks,
            image_refs,
            display_text,
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
        // 气泡正文=用户原话(不含注入的清单);`prompt`(带清单)只送模型。
        let user_prompt = display_text;
        // 本轮附件暂存:落盘时写进开启的 userMessage item。
        let user_attachments = turn_attachments;
        // 本轮图片暂存:落盘时写进开启的 userMessage item 的 `images`。
        // 直接取 `TurnPrompt` 上 `prepare_turn_core` 算好的那一份引用,不再从
        // 内容块里反推元数据:反推就得重新求 `size`,而实时开启项（`start_turn_core`
        // 里的 `opening_user_item`）用的是 prepare 阶段那一份——两处各算各的,
        // 一旦口径不同,同一个图在实时气泡与回放气泡上会显示成两个大小。
        let user_image_refs = image_refs;
        // 送给模型的内容块:文本(若有)在前,图片在后。
        let mut blocks: Vec<yi_agent_core::ContentBlock> = Vec::new();
        if !prompt.is_empty() {
            blocks.push(yi_agent_core::ContentBlock::Text(prompt.clone()));
        }
        blocks.extend(image_blocks.iter().cloned());
        // 防御:空 blocks 会给模型塞一条没有任何内容的消息。正常路径够不到,
        // 但有一条例外:一条**只有图片**的消息里那张图被丢弃时(上传句柄未知 /
        // 已过期——见 `prepare_turn_core` 的丢弃分支),文本与图片就都空了。
        // 此时这里必须自己兜住,而不是把边界甩给模型。
        if blocks.is_empty() {
            blocks.push(yi_agent_core::ContentBlock::Text(String::new()));
        }
        translator.set_turn(turn_id.clone());

        // turn 开始就把提问落进 checkpoint：即使这一轮随后立刻崩溃，
        // 至少提问不会丢。
        checkpoint(
            &store,
            &thread_id,
            build_partial(
                &turn_id,
                &user_prompt,
                &user_attachments,
                &user_image_refs,
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

        let mut stream = match agent.run_blocks(blocks).await {
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
                                // **屏障**：任何**非 delta** 通知到达时，先刷它所属 thread
                                // 的待发 delta，保证「先吐字、再标记这项完成」的顺序不变。
                                // 缺这道屏障时，某 item 的 `item/completed` 会**越过**自己
                                // 那条尚未刷出的 delta 先发出：客户端先按 id 整条替换成
                                // 完整文本，随后迟到的 delta 又被就地追加一遍，尾部于是
                                // 重复。注释与实现必须同时成立，这里补上被漏掉的分支。
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
                                        // 非 delta 通知：先刷**它自己 thread** 的待发
                                        // delta（若有），再发这条通知。无订阅者时
                                        // 缓冲恒空，这里是 no-op、字节不变。
                                        if let Some(t) = n.thread_key() {
                                            if let Some((pid, text)) = coalescer.take(t) {
                                                to_send.push(Notification::ItemDelta {
                                                    thread_id: t.to_string(),
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
                            &user_attachments,
                            &user_image_refs,
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
                            SessionCommand::SetModel { reply, .. } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::SetReasoningEffort { reply, .. } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::SetPreserveThinking { reply, .. } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
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
            &user_attachments,
            &user_image_refs,
            &agent,
            std::mem::take(&mut completed_items),
            last_usage.take(),
            &store,
            &hub,
            &turn_tx,
            &provider,
            &config,
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
    uploads: &mut crate::image_upload::UploadRegistry,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    models_path: &Path,
    cfg: &RuntimeConfig,
    hub: &Arc<crate::broadcast::Broadcaster>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
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
            uploads,
            runtimes,
            thread_roots,
            models_path,
            cfg,
            hub,
            turn_tx,
            pending,
            permission_timeout,
            perm_seq,
            theme,
            git_diff,
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
    /// 上传登记表：看板会话不起于 RPC，但 `prepare_turn_core` 的签名要求它
    /// （`{type:"uploaded_image"}` 只可能来自 RPC；看板卡永远不带上传句柄）。
    uploads: &'a mut crate::image_upload::UploadRegistry,
    runtimes: &'a ProjectRuntimes,
    thread_roots: &'a ThreadRoots,
    /// 机器级模型清单路径:看板会话的委派链路与手工会话读同一份,而非各自
    /// 重读 `$HOME`。
    models_path: &'a Path,
    cfg: &'a RuntimeConfig,
    hub: &'a Arc<crate::broadcast::Broadcaster>,
    turn_tx: &'a mpsc::Sender<TurnEvent>,
    pending: &'a Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &'a Arc<AtomicU64>,
    theme: &'a crate::theme_tool::ThemeHandle,
    git_diff: &'a crate::git_diff_tool::GitDiffHandle,
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
        // 生效串取自实际建出的 agent,与 `thread/start`/`thread/resume` 同口径,避免
        // 工厂快照与另读磁盘不一致而上报一个未真正采用的模型。
        let model = built.agent.config().model.clone();

        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.models_path,
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
            self.git_diff,
            self.workspaces,
            // 看板会话不带 model_ref;生效串取自上面实际建出的 agent,与
            // `thread/start`/`thread/resume` 同口径。
            model,
            None,
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
        let prepared =
            match prepare_turn_core(self.threads, self.pending_activation, self.uploads, &params)
                .await
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
            opening_user_item(
                &prepared.turn_id,
                &prepared.display_text,
                prepared.attachments.clone(),
                prepared.image_refs.clone(),
            ),
        )
        .await?;
        if prepared
            .prompt_tx
            .send(TurnPrompt {
                turn_id: prepared.turn_id,
                prompt: prepared.prompt,
                activate: prepared.activate,
                attachments: prepared.attachments,
                image_blocks: prepared.image_blocks,
                image_refs: prepared.image_refs,
                display_text: prepared.display_text,
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
    models_path: &Path,
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
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    workspaces: &WorkspaceIndex,
    // 该会话的生效模型串与覆盖名(由调用方按清单解析后传入),落进 ThreadMeta
    // 并写进 ThreadSession,与 `thread/start`/`thread/resume` 同一来源。
    model: String,
    model_ref: Option<String>,
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
        models_path,
        cfg,
        cwd,
        &thread_id,
        theme,
        git_diff,
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
        model_ref,
        reasoning_effort: None,
        // 新会话默认开启跨轮保留(与 `ThreadMeta` 的缺省语义一致)。
        preserve_thinking: true,
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
    /// 附件不可读 / 超出上限 / 不是文件。
    InvalidAttachment(String),
    /// 显式路径的图片输入（`{type:"image", path}`）不可用。带 `data.code =
    /// "invalid_image"`，客户端据此指认是哪张图被拒（见 `invalid_image`）。
    InvalidImage(String),
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
    /// 送给模型的 prompt（含附件清单注入）。
    pub(crate) prompt: String,
    /// 气泡正文:用户原话，**不含**注入的清单。
    pub(crate) display_text: String,
    /// 本轮附件元数据；回显在开启项上，并随 `TurnPrompt` 交给 driver 落盘。
    pub(crate) attachments: Vec<crate::protocol::Attachment>,
    /// 本轮图片内容块（含 base64），随 `TurnPrompt` 进模型上下文。
    pub(crate) image_blocks: Vec<yi_agent_core::ContentBlock>,
    /// 本轮图片元数据；回显在开启项上（`Item::UserMessage.images`）。
    pub(crate) image_refs: Vec<crate::protocol::ImageRef>,
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
    uploads: &mut crate::image_upload::UploadRegistry,
    params: &serde_json::Value,
) -> Result<PreparedTurn, TurnPrepareError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or(TurnPrepareError::MissingThreadId)?
        .to_string();
    // 一次性拆出「文本 + 附件源路径 + 图片输入」:文本为空但有附件或图片时仍然
    // 放行(附件-only / 图片-only)。
    let (text, source_paths) = crate::attachments::parse_input(params);
    let image_inputs = crate::attachments::parse_image_inputs(params);
    let text_opt = if text.trim().is_empty() {
        None
    } else {
        Some(text)
    };
    if text_opt.is_none() && source_paths.is_empty() && image_inputs.is_empty() {
        return Err(TurnPrepareError::EmptyInput);
    }

    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    // 内层作用域:让 `&mut threads` 的借用先结束(下面复制附件还要再借一次)。
    let (prompt_tx, status_handle, cwd) = {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        (
            session.prompt_tx.clone(),
            Arc::clone(&session.status),
            session.cwd.clone(),
        )
    };

    // 复制附件进工作区。放在占 `active_turn_id` **之前**:失败要能干净地拒绝,
    // 不留一个「已占用但起不来」的 turn。
    let max_bytes = crate::attachments::resolve_max_bytes();
    let mut attachments = Vec::with_capacity(source_paths.len());
    for source in &source_paths {
        match crate::attachments::store_attachment(
            Path::new(&cwd),
            &thread_id,
            Path::new(source),
            max_bytes,
        ) {
            Ok(attachment) => attachments.push(attachment),
            Err(crate::attachments::AttachmentError::TooLarge { size, max }) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {source} is {size} bytes, over the {max} byte limit"
                )));
            }
            Err(crate::attachments::AttachmentError::NotAFile(path)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {} is not a regular file",
                    path.display()
                )));
            }
            Err(crate::attachments::AttachmentError::Io(e)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "could not read attachment {source}: {e}"
                )));
            }
        }
    }

    // 复制 + 编码图片。同样放在占 `active_turn_id` **之前**。
    //
    // 两种来源在这里**收敛**到同一条管线（裁定 R3）：
    // - `Path`（桌面）：文件选择器给的根外绝对路径，先 `store_attachment` 复制进
    //   工作区；
    // - `Upload`（iOS 分片上传）：`image/upload/*` 已经把 staging 收拢成同一份
    //   `store_attachment` 结果（含 `size` = 存盘文件字节长度），这里只反查登记表。
    //
    // 之后一律读**根内副本**再编码：`prepare_image_file` 在根外读原文件也可以，
    // 但那样回显的 `path` 与真正读到的字节可能来自两个位置；统一读根内副本，
    // `path` 与内容才必然同源，`ImageRef.size` 也才必然等于该文件的字节长度。
    //
    // 坏图的两条处置**刻意不同**，判据是「失败能不能靠重试自愈」：
    // - `Upload`：`uploadId` 未知 / 未 commit / 已过期（10 分钟 TTL）都是**不可
    //   自愈**的——服务端手里已没有重试所需的任何东西，客户端每重发一次都只会拿
    //   到一模一样的失败，而它还会把 chip 留着，于是整轮对话被这一张图钉死。故
    //   **丢弃这张图**（warn 一条）并继续起 turn：用户照样能把消息发出去。
    //   注：上传路径的图已由 `begin` 的 `MAX_IMAGE_BYTES` 设闸，故「超大」不会
    //   走到这里；但「上传的字节其实不是图」会——`commit` 只收拢字节，判图是这
    //   一步的事。
    // - `Path`：路径是这次请求里的显式参数，指不到/不是图是**调用方请求本身**的
    //   问题（桌面端的图在本地盘上，重发同一路径多半就是同一个结果），仍按
    //   `InvalidImage` 硬拒，与文档附件的 `InvalidAttachment` 同口径。
    let mut image_blocks = Vec::with_capacity(image_inputs.len());
    let mut image_refs = Vec::with_capacity(image_inputs.len());
    for input in &image_inputs {
        let (label, stored) = match input {
            crate::attachments::ImageInput::Path(source) => {
                let stored = match crate::attachments::store_attachment(
                    Path::new(&cwd),
                    &thread_id,
                    Path::new(source),
                    max_bytes,
                ) {
                    Ok(stored) => stored,
                    Err(e) => {
                        return Err(TurnPrepareError::InvalidImage(format!(
                            "image {source}: {e:?}"
                        )));
                    }
                };
                (source.clone(), stored)
            }
            crate::attachments::ImageInput::Upload(upload_id) => {
                // 未 commit / 未知 / 已过期都落到这里。
                //
                // 反查必须带 `thread_id`：`uploadId` 是客户端手里的不透明句柄，
                // 只看 id 不看归属，就能把别条 thread 的图摄进本 turn（跨 thread
                // 取数）。归属不符与「未知」一样丢弃，不向调用方区分两者。
                match uploads.resolve_for(upload_id, &thread_id) {
                    Some(attachment) => (upload_id.clone(), attachment),
                    None => {
                        tracing::warn!(
                            %thread_id,
                            upload_id = %upload_id,
                            "dropping unresolvable uploaded image; continuing the turn",
                        );
                        continue;
                    }
                }
            }
        };
        let stored_abs = Path::new(&cwd).join(&stored.path);
        match yi_agent_tools::image_prep::prepare_image_file(
            &stored_abs,
            yi_agent_core::ImageDetail::High,
            yi_agent_tools::image_prep::resolve_budget(),
        ) {
            Ok(yi_agent_tools::image_prep::PreparedImage::Ready {
                media_type, data, ..
            }) => {
                image_blocks.push(yi_agent_core::ContentBlock::Image {
                    source: yi_agent_core::ImageSource::Base64 {
                        media_type: media_type.clone(),
                        data,
                    },
                    detail: yi_agent_core::ImageDetail::High,
                    path: Some(stored.path.clone()),
                });
                image_refs.push(crate::protocol::ImageRef {
                    path: stored.path,
                    media_type,
                    size: stored.size,
                    detail: Some(yi_agent_core::ImageDetail::High.as_wire_str().to_string()),
                });
            }
            // 已落盘、但读不出/超预算的上传图同样丢：它和「未知 uploadId」一样属于
            // 重发必然复现的失败，不该毒死整轮。
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message })
                if matches!(input, crate::attachments::ImageInput::Upload(_)) =>
            {
                tracing::warn!(
                    %thread_id,
                    image = %label,
                    %message,
                    "dropping an unusable uploaded image; continuing the turn",
                );
            }
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message }) => {
                return Err(TurnPrepareError::InvalidImage(format!(
                    "image {label}: {message}"
                )));
            }
            Err(e) if matches!(input, crate::attachments::ImageInput::Upload(_)) => {
                tracing::warn!(
                    %thread_id,
                    image = %label,
                    error = ?e,
                    "dropping an unusable uploaded image; continuing the turn",
                );
            }
            Err(e) => {
                return Err(TurnPrepareError::InvalidImage(format!(
                    "image {label}: {e:?}"
                )));
            }
        }
    }

    // 占位(复制成功之后)。
    {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        session.active_turn_id = Some(turn_id.clone());
    }
    let activate = pending_activation.get(&thread_id).cloned().flatten();
    // prompt 送进模型时带上附件清单;气泡文本仍是原话(见 `display_text`)。
    let prompt = crate::attachments::prompt_with_attachments(
        text_opt.as_deref().unwrap_or(""),
        &attachments,
    );
    Ok(PreparedTurn {
        thread_id,
        turn_id,
        prompt_tx,
        prompt,
        display_text: text_opt.unwrap_or_default(),
        attachments,
        image_blocks,
        image_refs,
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
        TurnPrepareError::InvalidAttachment(msg) => RpcError::invalid_params(msg),
        TurnPrepareError::InvalidImage(msg) => RpcError::invalid_image(msg),
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
fn opening_user_item(
    turn_id: &str,
    text: &str,
    attachments: Vec<crate::protocol::Attachment>,
    images: Vec<crate::protocol::ImageRef>,
) -> crate::protocol::Item {
    crate::protocol::Item::UserMessage {
        id: format!("user-{turn_id}"),
        text: text.to_string(),
        attachments,
        images,
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
pub(crate) async fn start_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    uploads: &mut crate::image_upload::UploadRegistry,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    let prepared = match prepare_turn_core(threads, pending_activation, uploads, params).await {
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
        opening_user_item(
            &prepared.turn_id,
            &prepared.display_text,
            prepared.attachments.clone(),
            prepared.image_refs.clone(),
        ),
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
            attachments: prepared.attachments,
            image_blocks: prepared.image_blocks,
            image_refs: prepared.image_refs,
            display_text: prepared.display_text,
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

/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
///
/// 非 text 的 block（含 attachment）由 `crate::attachments::parse_input` 另行处理；
/// 这里只看文本，空文本返回 `None`。**注意**：附件-only 的消息在这里是 `None`，
/// 是否放行由调用方结合附件列表决定（见 `prepare_turn_core`）。
pub(crate) fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let (text, _paths) = crate::attachments::parse_input(params);
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
    pub(super) fn fake_daemon(
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

    /// A daemon that reached the plugin and relayed the plugin's OWN rejection
    /// (an invalid settings payload, say) is the plugin talking, not the plugin
    /// being gone. Reporting `plugin_unavailable` here makes the desktop say
    /// "插件未运行" and discards the plugin's reason; the code must be
    /// `plugin_rejected`. This is the app-server's own-workdir channel
    /// (`plugin_query_self`), which is what `plugin/settings/*` dispatch uses.
    #[test]
    fn a_plugin_side_rejection_is_not_reported_as_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("bad.json"),
            r#"{"name":"bad","command":"x","switch_key":"bad_on","query_socket":"{state_dir}/bad.sock"}"#,
        )
        .unwrap();
        let (_socket, _seen, handle) = fake_daemon(
            dir.path(),
            json!({
                "type": "Error",
                "code": "validation",
                "message": "interval_secs must be in [1, 3600], got 0",
            }),
        );

        let error = plugin_query_self(dir.path(), "bad", "settings.write", json!({})).unwrap_err();
        handle.join().unwrap();

        assert_eq!(error.code, "plugin_rejected", "{error}");
        assert_ne!(
            error.code, "plugin_unavailable",
            "别把拒绝说成没运行:{error}"
        );
        assert!(
            error.message.contains("interval_secs"),
            "插件的具体理由要带出来:{error}"
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

#[cfg(test)]
mod plugins_tests {
    use super::{list_installed_plugins, plugin_scope};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    #[test]
    fn an_absent_supervisors_directory_lists_no_plugins() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_installed_plugins(dir.path()).is_empty());
    }

    /// 通用插件通道的作用域：给了非空 `project` 就用它，否则回落宿主 workdir。
    /// 看板按项目安装，桌面侧车 workdir 是 home，这一条是「看板跑着、设置页却
    /// 空着」能修好的前提。
    #[test]
    fn the_plugin_scope_follows_an_explicit_project_and_defaults_to_the_workdir() {
        let workdir = Path::new("/tmp/yi-agent-host-workdir");
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({})),
            workdir.to_path_buf()
        );
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({ "project": "/p/proj" })),
            PathBuf::from("/p/proj")
        );
        // 空/全空白都不算「指定了项目」，回落到 workdir。
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({ "project": "" })),
            workdir.to_path_buf()
        );
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({ "project": "   " })),
            workdir.to_path_buf()
        );
    }

    /// 端到端复现并锁死根因：宿主 workdir（= 桌面侧车的 home）下没有清单，所以
    /// 不带 `project` 的 `plugins/list` 必须为空；带上项目后才能看到该项目里装的
    /// 看板。修复前第二段断言为空——正是用户看到的现象。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_project_scoped_plugins_list_finds_a_project_installed_plugin() {
        use super::tests::{Harness, initialize, test_config};

        let home = tempfile::tempdir().unwrap();
        let mut cfg = test_config();
        cfg.workdir = home.path().to_path_buf();
        let mut h = Harness::with_cfg(cfg).await;
        initialize(&mut h).await;

        let project = tempfile::tempdir().unwrap();
        let sup = project.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();

        // 不带 project：读的是宿主 workdir，这里没有清单。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"plugins/list","params":{}}"#)
            .await;
        let bare = read_response(&mut h, 2).await;
        assert_eq!(
            bare["result"]["plugins"].as_array().unwrap().len(),
            0,
            "宿主 workdir 下不该凭空冒出插件：{bare}"
        );

        // 带 project：枚举该项目自己的清单，看到看板。
        h.send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "plugins/list",
                "params": { "project": project.path().to_string_lossy() },
            })
            .to_string(),
        )
        .await;
        let scoped = read_response(&mut h, 3).await;
        let plugins = scoped["result"]["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1, "按项目作用域应看到看板：{scoped}");
        assert_eq!(plugins[0]["name"], "superpowers-kanban");
        assert_eq!(plugins[0]["queryable"], true);
        h.shutdown().await;
    }

    /// 与 `plugins/list` 同源的根因回归：`plugin/settings/read` 也必须按项目
    /// 作用域。不带 `project` 时，宿主在 home 下找不到清单，读成
    /// `plugin_not_installed`（正是用户看到「设置页里没有看板」的那一步）；带上
    /// 项目后才会拨该项目 daemon 的 socket 并拿回设置。
    #[tokio::test(flavor = "multi_thread")]
    async fn project_scoped_settings_read_reaches_the_project_daemon() {
        use super::plugin_query_tests::fake_daemon;
        use super::tests::{Harness, initialize, test_config};

        let home = tempfile::tempdir().unwrap();
        let mut cfg = test_config();
        cfg.workdir = home.path().to_path_buf();
        let mut h = Harness::with_cfg(cfg).await;
        initialize(&mut h).await;

        let project = tempfile::tempdir().unwrap();
        let sup = project.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        let (_socket, _seen, handle) = fake_daemon(
            project.path(),
            json!({
                "type": "PluginResult",
                "value": { "settings": { "default_max_tasks": 3, "interval_secs": 10, "windows": [] } },
            }),
        );

        // 不带 project：宿主 workdir（= home）下没有清单 → 读成没装。
        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"plugin/settings/read","params":{"plugin":"superpowers-kanban"}}"#,
        )
        .await;
        let bare = read_response(&mut h, 2).await;
        assert_eq!(
            bare["error"]["data"]["code"], "plugin_not_installed",
            "home 作用域下不该声称看板已装：{bare}"
        );

        // 带 project：拨该项目的 daemon，拿回它自己的设置。
        h.send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "plugin/settings/read",
                "params": {
                    "plugin": "superpowers-kanban",
                    "project": project.path().to_string_lossy(),
                },
            })
            .to_string(),
        )
        .await;
        let scoped = read_response(&mut h, 3).await;
        assert_eq!(
            scoped["result"]["settings"]["interval_secs"], 10,
            "按项目作用域应读到设置：{scoped}"
        );
        handle.join().unwrap();
        h.shutdown().await;
    }

    /// 读到 id 匹配的响应，跳过中间的通知帧。
    async fn read_response(h: &mut super::tests::Harness, id: i64) -> serde_json::Value {
        loop {
            let value = h.read_value().await;
            if value.get("id") == Some(&serde_json::json!(id)) {
                return value;
            }
        }
    }

    #[test]
    fn a_manifest_with_a_query_socket_is_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        std::fs::write(
            sup.join("plain.json"),
            r#"{"name":"plain","command":"y","switch_key":"plain_on"}"#,
        )
        .unwrap();
        let mut plugins = list_installed_plugins(dir.path());
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "plain");
        assert!(!plugins[0].queryable);
        assert_eq!(plugins[1].name, "superpowers-kanban");
        assert!(plugins[1].queryable);
        assert_eq!(plugins[1].switch_key, "superpowers_kanban");
    }

    #[test]
    fn a_plugin_not_in_the_manifests_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            super::plugin_query_self(dir.path(), "ghost", "settings.read", serde_json::json!({}))
                .unwrap_err();
        assert_eq!(error.code, "plugin_not_installed", "{error}");
    }

    #[test]
    fn an_installed_plugin_without_a_query_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("quiet.json"),
            r#"{"name":"quiet","command":"y","switch_key":"quiet_on"}"#,
        )
        .unwrap();
        let error =
            super::plugin_query_self(dir.path(), "quiet", "settings.read", serde_json::json!({}))
                .unwrap_err();
        assert_eq!(error.code, "plugin_unavailable", "{error}");
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
    use crate::test_support::isolated_models_path;
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
        uploads: crate::image_upload::UploadRegistry,
        runtimes: ProjectRuntimes,
        thread_roots: ThreadRoots,
        cfg: RuntimeConfig,
        hub: Arc<crate::broadcast::Broadcaster>,
        turn_tx: mpsc::Sender<TurnEvent>,
        turn_rx: mpsc::Receiver<TurnEvent>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
        perm_seq: Arc<AtomicU64>,
        theme: crate::theme_tool::ThemeHandle,
        git_diff: crate::git_diff_tool::GitDiffHandle,
        workspaces: WorkspaceIndex,
        workdir: PathBuf,
        /// 委派链路要读的机器清单路径。指向本测试的隔离目录,避免读到开发者
        /// 真实的 `~/.yi-agent/models.json` 而改变解析出的 provider。
        models_path: PathBuf,
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
                uploads: crate::image_upload::UploadRegistry::new(),
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                cfg: test_config(),
                hub: Arc::new(crate::broadcast::Broadcaster::new()),
                turn_tx,
                turn_rx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                perm_seq: Arc::new(AtomicU64::new(1)),
                theme: crate::theme_tool::ThemeHandle::new(theme_dir.path().to_path_buf()),
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                workspaces: WorkspaceIndex::new(workspace_dir.path().join("workspaces.json")),
                workdir,
                models_path: isolated_models_path(theme_dir.path()),
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
                uploads: &mut self.uploads,
                runtimes: &self.runtimes,
                thread_roots: &self.thread_roots,
                models_path: &self.models_path,
                cfg: &self.cfg,
                hub: &self.hub,
                turn_tx: &self.turn_tx,
                pending: &self.pending,
                permission_timeout: PERMISSION_TIMEOUT,
                perm_seq: &self.perm_seq,
                theme: &self.theme,
                git_diff: &self.git_diff,
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
    /// `board.mark_terminal(awaiting_merge)`,但**仍保留跟踪**——它的会话可能追问续跑。
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
        assert_eq!(
            tracked.get("card-1").map(|t| t.state),
            Some(crate::card_scheduler::TrackState::AwaitingMerge),
            "the reconciled card stays tracked so a later turn can revive it"
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

#[cfg(test)]
#[path = "rpc_tests.rs"]
pub(crate) mod tests;

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
            match tokio::time::timeout(crate::test_support::EVENT_WAIT, out.recv()).await {
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
        let finished = tokio::time::timeout(crate::test_support::EVENT_WAIT, pump)
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
                model: "m".into(),
                model_ref: None
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
        // 回合结束是一桩**列表层/注意力**事实，不是会话正文：未读蓝点唯一的
        // 驱动。远程客户端只订阅寥寥几条会话，若把它按内容层过滤，窗口外跑完的
        // 会话就永远不亮蓝点——用户以为「没跑完」。
        assert_eq!(
            Notification::TurnCompleted {
                thread_id: "t2".into(),
                turn_id: "u".into(),
                status: crate::protocol::TurnStatus::Completed,
                error: None
            }
            .delivery(),
            Delivery::List
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

        // 列表层：未订阅的 t2 的**回合结束**同样必须到达——它是未读蓝点唯一的
        // 驱动，被过滤掉就再没有第二次机会（内容层被挡下，客户端无从得知跑完了）。
        // 用 `try_recv` 而非 `recv().await`：投递是同步 `try_send`，帧已在该分支
        // 入队或丢弃；`await` 在 RED 下会挂死而不是快速失败。
        write_notification(
            &hub,
            &Notification::TurnCompleted {
                thread_id: "t2".into(),
                turn_id: "u1".into(),
                status: crate::protocol::TurnStatus::Completed,
                error: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rx.try_recv().expect("未订阅的 t2 的回合结束帧也必须到达")["method"],
            "turn/completed"
        );

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
