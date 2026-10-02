//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::protocol::{
    ClientResponse, JSONRPC_VERSION, Notification, NotificationEnvelope, PROTOCOL_VERSION,
    RequestEnvelope, RequestId, ResponseEnvelope, ReverseRequest, RpcError, ThreadStatus,
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
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

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
struct BuiltAgent {
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
struct RuntimeAttachments {
    runtimes: ProjectRuntimes,
    thread_roots: ThreadRoots,
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
fn attach_delegation(
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
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
    match build_runtime_tooling(&thread_cfg, &root, thread_id, built.yolo.clone()) {
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
async fn require_known_thread<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    id: RequestId,
) -> anyhow::Result<bool> {
    if threads.contains_key(thread_id) {
        return Ok(true);
    }
    write_response(
        writer,
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

/// Cancels every live child a conversation owns, before its files go away.
///
/// The children live in the shared project runtime, where the conversation's
/// files are not the authority, so deleting them alone would leave the agents
/// running where nobody can see them. The cancellation is scoped by the
/// conversation marker rather than the root: several conversations share one
/// attached root, and a root-scoped cancel would stop a sibling's work too.
///
/// Best effort by construction: the thread is being deleted either way, so an
/// unreachable daemon is reported to the log and never blocks the deletion.
fn cancel_thread_children(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
) -> Result<usize, String> {
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
        },
    )?;
    match response {
        yi_agent_store::ipc::IpcResponse::ThreadTasksCancelled { task_ids } => Ok(task_ids.len()),
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
async fn watch_children<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(
    writer: Arc<MessageWriter<W>>,
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
        if write_notification(&writer, &notification).await.is_err() {
            return;
        }
    }
}

/// 把 manager 的状态广播转成 `process/updated` 通知。
///
/// 只在状态变化时推送(`Output` 丢弃):高频输出走 `process/read` 按需拉取。
/// 关闭 thread 时由调用方 abort。
async fn watch_processes<W>(
    writer: Arc<MessageWriter<W>>,
    thread_id: String,
    manager: Arc<yi_agent_tools::ProcessManager>,
    mut rx: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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
                    &writer,
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
async fn stream_trace<W: tokio::io::AsyncWrite + Unpin + Send + 'static>(
    writer: Arc<MessageWriter<W>>,
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
                if write_notification(&writer, &notification).await.is_err() {
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
    // The conversation marker is bound here, not inferred later: the model that
    // calls `spawn_agent` never sees a thread id, so each thread's tools carry
    // their own so every child they spawn is tagged with this conversation.
    yi_agent_subagent::register_attached_root_tools_in_thread(
        &mut registry,
        Arc::clone(root),
        controller,
        Some(thread_id.to_string()),
    );
    let permission =
        yi_agent_runtime::bootstrap::load_permission_checker_with_switch(&workspace_root, yolo)
            .map_err(|error| error.to_string())?;
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
        process_manager,
    })
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
    let session = agent.session();
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
            .with_session(session);
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
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

/// Ask a supervised plugin a question and return its answer verbatim.
///
/// The host attaches no meaning to `method` or `params`, and unwraps no board
/// shape here: this is a generic channel, so the UI (not the server) owns what a
/// card or a switch field means. A plugin the daemon does not supervise, or one
/// that refuses, surfaces as an RPC error the client can show.
fn plugin_query(
    workdir: &Path,
    method: &str,
    plugin: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if plugin.is_empty() {
        return Err("plugin/query needs a `plugin` name".to_string());
    }
    // The board RPCs used to read `<workdir>/.yi-agent/...` directly. The daemon
    // that runs the plugin lives at the same project root, so that is where the
    // question has to go.
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(workdir);
    let socket =
        yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| error.to_string())?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| format!("daemon is unavailable: {error}"))?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "the plugin rejected the query: {code:?} {}",
            message.unwrap_or_default()
        )),
        other => Err(format!("daemon returned an unexpected response: {other:?}")),
    }
}

/// app-server 入口:在 stdio(或任意读写流)上跑 JSON-RPC 主循环。
///
/// `cfg` 同时用于 `config/read` 响应与(每个 thread 的)`bootstrap_agent`。
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let cfg_for_factory = cfg.clone();
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    run_with(
        reader,
        writer,
        cfg,
        PERMISSION_TIMEOUT,
        workspaces,
        RuntimeAttachments {
            runtimes,
            thread_roots,
        },
        move |session, cwd, mode| {
            let mut thread_cfg = cfg_for_factory.clone();
            thread_cfg.workdir = cwd.to_path_buf();
            thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
            let built = yi_agent_runtime::bootstrap::bootstrap_agent(
                &thread_cfg,
                yi_agent_runtime::bootstrap::PermissionMode::Interactive,
            )?;
            // The provider stays the single object the bootstrap built; a second
            // one would re-read the credential and duplicate the client.
            let config = built.agent.config().clone();
            Ok(BuiltAgent {
                agent: apply_session(built.agent, session),
                provider: built.provider,
                config,
                decision_tx: built.decision_tx,
                decision_rx: built.decision_rx,
                catalog: built.catalog,
                yolo: built.yolo,
                process_manager: built.process_manager,
            })
        },
    )
    .await
}

/// 主循环的可测核心:agent 工厂由调用方注入(测试用 mock provider)。
///
/// **取消安全**:`MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,
/// 因此这里不直接在 `select!` 上轮询它;而是 spawn 一个独占 reader 的读取任务,
/// 把完整行转发到 channel,主循环只 select 两个 `recv`(均 cancel-safe)。
async fn run_with<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
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
        + 'static,
{
    let RuntimeAttachments {
        runtimes,
        thread_roots,
    } = attachments;
    // channel 里携带 `Result`,区分「读到一行」「EOF(channel 关闭)」与
    // 「读/传输错误」。若不区分,超大帧或 broken pipe 会被误当成干净 EOF。
    let (req_tx, mut req_rx) = mpsc::channel::<anyhow::Result<String>>(64);
    tokio::spawn(async move {
        let mut reader = MessageReader::new(reader);
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if req_tx.send(Ok(line)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break, // EOF → 丢弃 req_tx → 主循环优雅退出
                Err(e) => {
                    tracing::error!("app-server read error: {e}");
                    let _ = req_tx.send(Err(e)).await;
                    break;
                }
            }
        }
        // 丢弃 req_tx → 主循环的 req_rx.recv() 返回 None,触发优雅退出。
    });

    let writer = Arc::new(MessageWriter::new(writer));
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

    let mut initialized = false;
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

    loop {
        tokio::select! {
            line = req_rx.recv() => {
                let Some(item) = line else { break }; // EOF → graceful exit
                let line = match item {
                    Ok(l) => l,
                    Err(e) => {
                        // 尽力告知客户端我们为何退出,再把失败向上抛出,
                        // 避免传输错误被伪装成干净退出。
                        let _ = write_response(
                            &writer,
                            err_response(
                                RequestId::Num(0),
                                RpcError::invalid_request(format!("transport error: {e}")),
                            ),
                        )
                        .await;
                        return Err(e);
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
                            &writer,
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
                                route_client_response(resp, &pending).await;
                            }
                            _ => {
                                write_response(
                                    &writer,
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

                // 未 initialize 前,除 `initialize` 外的请求一律拒绝。
                if !initialized && method != "initialize" {
                    write_response(&writer, err_response(id, RpcError::not_initialized())).await?;
                    continue;
                }

                match method.as_str() {
                    "initialize" => {
                        initialized = true;
                        write_response(
                            &writer,
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
                        write_response(&writer, ok_response(id, cfg.redacted_view())).await?;
                    }
                    "plugin/query" => {
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
                        match plugin_query(&cfg.workdir, method, plugin, params) {
                            Ok(value) => write_response(&writer, ok_response(id, value)).await?,
                            Err(message) => {
                                write_response(&writer, err_response(id, RpcError::internal(message)))
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
                        write_response(&writer, ok_response(id, json!({ "workspaces": list })))
                            .await?;
                    }
                    "workspace/add" => {
                        let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        match std::fs::canonicalize(raw) {
                            Ok(p) if p.is_dir() => {
                                let value = p.to_string_lossy().to_string();
                                match workspaces.add(&p) {
                                    Ok(()) => {
                                        write_response(&writer, ok_response(id, json!({ "path": value })))
                                            .await?
                                    }
                                    Err(e) => {
                                        write_response(
                                            &writer,
                                            err_response(id, RpcError::internal(e.to_string())),
                                        )
                                        .await?
                                    }
                                }
                            }
                            _ => {
                                write_response(
                                    &writer,
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
                            Ok(()) => write_response(&writer, ok_response(id, json!({}))).await?,
                            Err(e) => {
                                write_response(
                                    &writer,
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
                            write_response(&writer, ok_response(id, json!({ "threads": threads })))
                                .await?;
                        }
                        Err(e) => {
                            write_response(&writer, err_response(id, RpcError::internal(e.to_string())))
                                .await?;
                        }
                        }
                    }
                    "thread/listAll" => {
                        // 跨目录汇总:按索引顺序(最近的在前)遍历每个 workspace,
                        // 组内是该目录 store 的 thread(updated_at 降序)。
                        // 失效目录先 stat 跳过,不做深扫,但仍报表该组(exists:false),
                        // 供侧栏置灰展示。
                        let mut groups: Vec<serde_json::Value> = Vec::new();
                        for dir in workspaces.list() {
                            let path = Path::new(&dir);
                            let exists = path.is_dir();
                            let threads: Vec<serde_json::Value> = if exists {
                                // 读取错误(如权限拒绝)不能静默等同于「无 thread」:
                                // 记 stderr 后再降级为空组,与 thread/list 的错误可见性一致。
                                match crate::thread_store::ThreadStore::new(path).list() {
                                    Ok(metas) => metas
                                        .into_iter()
                                        .map(|m| thread_summary_json(&m, &threads))
                                        .collect(),
                                    Err(e) => {
                                        eprintln!(
                                            "[app-server] thread/listAll failed to list {dir}: {e}"
                                        );
                                        Vec::new()
                                    }
                                }
                            } else {
                                Vec::new()
                            };
                            groups.push(json!({
                                "workspace": dir,
                                "exists": exists,
                                "threads": threads,
                            }));
                        }
                        let pinned: Vec<serde_json::Value> = collect_pinned(&workspaces)
                            .iter()
                            .map(|m| thread_summary_json(m, &threads))
                            .collect();
                        write_response(
                            &writer,
                            ok_response(id, json!({ "groups": groups, "pinned": pinned })),
                        )
                        .await?;
                    }
                    "thread/start" => {
                        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

                        // 目录决定 agent / store / 权限 / 沙箱 / skills 的根。
                        let cwd = match resolve_thread_cwd(&req.params, &cfg, &writer, id.clone()).await? {
                            Some(c) => c,
                            None => continue,
                        };
                        let thread_store =
                            Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

                        // 新线程一律以 Normal 起步;同一值既用于建 agent,也落盘 meta,
                        // 抽成局部量避免两处字面量漂移。
                        let mode = crate::thread_store::ThreadMode::Normal;

                        let built = match build_agent(None, Path::new(&cwd), mode) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(&writer, err_response(id, RpcError::internal(e.to_string()))).await?;
                                continue;
                            }
                        };
                        // 委派是可选能力:项目 runtime 起不来就保留原 agent,只记 trace。
                        // 接线刻意放在这里(而非 `threads` 守卫之内),避免与其可变借用冲突。
                        let runtime_dir =
                            yi_agent_subagent::attach::project_runtime_directory(Path::new(&cwd));
                        let activation = attach_delegation(
                            &runtimes,
                            &thread_roots,
                            &runtime_dir,
                            &cfg,
                            &cwd,
                            &thread_id,
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

                        let model = cfg.model.clone();

                        let now = crate::thread_store::now_millis();
                        let meta = crate::thread_store::ThreadMeta {
                            thread_id: thread_id.clone(),
                            cwd: cwd.clone(),
                            model: model.clone(),
                            created_at: now,
                            updated_at: now,
                            title: None,
                            permission_mode: mode,
                            pin_seq: None,
                        };
                        if let Err(e) = thread_store.create(&meta) {
                            // 持久化是尽力而为:写失败不阻断 thread 创建。
                            eprintln!("[app-server] failed to create thread meta for {thread_id}: {e}");
                        }
                        // 记录到全局「最近目录」索引,供 thread/listAll 与侧栏复用。
                        // 索引键统一为 canonical,与 workspace/remove 的规范化对齐;
                        // thread 的 cwd / meta 仍保持 `resolve_thread_cwd` 的原样。
                        let index_path =
                            std::fs::canonicalize(&cwd).unwrap_or_else(|_| PathBuf::from(&cwd));
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
                            Arc::clone(&writer),
                            thread_id.clone(),
                            Arc::clone(&process_manager),
                            rx,
                        ));
                        process_watches.insert(thread_id.clone(), ProcessWatch { task: handle });

                        // 每个 thread 一个 driver task:独占 agent 与两个 receiver,
                        // 串行驱动 turn。
                        let driver_writer = Arc::clone(&writer);
                        let driver_turn_tx = turn_tx.clone();
                        let driver_thread_id = thread_id.clone();
                        tokio::spawn(run_thread_driver(
                            driver_thread_id,
                            agent,
                            prompt_rx,
                            interrupt_rx,
                            interject_rx,
                            session_rx,
                            driver_writer,
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

                        write_notification(
                            &writer,
                            &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        write_response(
                            &writer,
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
                            require_thread_id(&writer, &req.params, id.clone()).await?
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
                                    &writer,
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
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
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
                                    &writer,
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
                            Arc::clone(&writer),
                            thread_id.clone(),
                            Arc::clone(&process_manager),
                            rx,
                        ));
                        process_watches.insert(thread_id.clone(), ProcessWatch { task: handle });

                        let driver_writer = Arc::clone(&writer);
                        let driver_turn_tx = turn_tx.clone();
                        let driver_thread_id = thread_id.clone();
                        tokio::spawn(run_thread_driver(
                            driver_thread_id,
                            agent,
                            prompt_rx,
                            interrupt_rx,
                            interject_rx,
                            session_rx,
                            driver_writer,
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
                        write_notification(
                            &writer,
                            &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        for item in loaded.items {
                            write_notification(
                                &writer,
                                &Notification::ItemCompleted {
                                    thread_id: thread_id.clone(),
                                    item,
                                },
                            )
                            .await?;
                        }
                        if let Some(u) = loaded.usage {
                            write_notification(
                                &writer,
                                &Notification::TokenUsage {
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
                        write_response(
                            &writer,
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
                            require_thread_id(&writer, &req.params, id.clone()).await?
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
                                &writer,
                                err_response(id, RpcError::invalid_params("title must not be empty")),
                            )
                            .await?;
                            continue;
                        }
                        match store_lookup(&threads, &workspaces, &cfg, &thread_id)
                            .rename(&thread_id, &title)
                        {
                            Ok(true) => {
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
                    "thread/setPermissionMode" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
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
                                    &writer,
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
                                &writer,
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
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    "thread/setPinned" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // 参数校验先于 thread 存在性:非布尔一律 `-32602`。
                        let Some(pinned) = req.params.get("pinned").and_then(|v| v.as_bool())
                        else {
                            write_response(
                                &writer,
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
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
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
                                    &writer,
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
                                    &writer,
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
                                &writer,
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
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Some(e) => {
                                write_response(&writer, err_response(id, e)).await?;
                            }
                        }
                    }
                    "thread/delete" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
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
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        // 活跃 thread:先中断,再等 driver 落盘完成才删文件。若像旧
                        // 实现那样在 driver 落盘前就删文件,driver 的 `append_turn`
                        // (`create(true)`)会把 `<id>.jsonl` 复活,导致删除后
                        // `store.exists` 仍为真、`thread/resume` 能把已删 thread 拉回。
                        // 故复用 resume 的等待模式,等落盘后再删。
                        interrupt_and_wait_for_persist(&mut threads, &mut turn_rx, &thread_id).await;
                        // 在删文件之前先取消该会话名下的子代理。子代理跑在共享的项目
                        // runtime 里,对话文件不是它的权威,只删文件会把它留在无人可见
                        // 的地方继续跑。按会话标记取消,不动同目录其它会话的子代理
                        // (它们共享同一个 root)。
                        match cancel_thread_children(&runtimes, &threads, &thread_id) {
                            Ok(cancelled) if cancelled > 0 => eprintln!(
                                "[app-server] cancelled {cancelled} subagent task(s) for {thread_id}"
                            ),
                            Ok(_) => {}
                            Err(cause) => eprintln!(
                                "[app-server] could not cancel subagents for {thread_id}: {cause}"
                            ),
                        }
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
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    "thread/clear" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 清空跑在半个 turn 上会产出不自洽的历史，直接拒绝。
                        if session.active_turn_id.is_some() {
                            write_response(
                                &writer,
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
                                &writer,
                                err_response(id, RpcError::internal("thread driver is gone")),
                            )
                            .await?;
                            continue;
                        }
                        match reply_rx.await {
                            Ok(Ok(())) => {
                                write_response(&writer, ok_response(id, json!({}))).await?
                            }
                            Ok(Err(message)) => write_response(
                                &writer,
                                err_response(id, RpcError::internal(message)),
                            )
                            .await?,
                            Err(_) => write_response(
                                &writer,
                                err_response(id, RpcError::internal("thread driver dropped")),
                            )
                            .await?,
                        }
                    }
                    "thread/compact" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 压缩会重写整段历史,turn 进行中不接受。
                        if session.active_turn_id.is_some() {
                            write_response(
                                &writer,
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
                                &writer,
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
                        write_response(&writer, ok_response(id, result)).await?;
                    }
                    "turn/start" => {
                        // `id` 后续响应仍需使用,故传 clone。
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let prompt = match extract_prompt(&req.params) {
                            Some(p) => p,
                            None => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("missing or empty input text"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };

                        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
                        // 内层作用域:让 `&mut threads` 的借用先结束,后续错误
                        // 路径才能再次 `threads.get_mut`。
                        let (prompt_tx, status_handle) = {
                            let Some(session) = threads.get_mut(&thread_id) else {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            if session.active_turn_id.is_some() {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            session.active_turn_id = Some(turn_id.clone());
                            (session.prompt_tx.clone(), Arc::clone(&session.status))
                        };

                        // 顺序确定:先 turn/started 通知,再推 Running 状态,再响应,
                        // 最后投递 prompt。
                        write_notification(
                            &writer,
                            &Notification::TurnStarted {
                                thread_id: thread_id.clone(),
                                turn_id: turn_id.clone(),
                            },
                        )
                        .await?;
                        update_status(&writer, &status_handle, &thread_id, ThreadStatus::Running)
                            .await?;
                        write_response(
                            &writer,
                            ok_response(id, json!({ "turn_id": turn_id.clone() })),
                        )
                        .await?;

                        let activate = pending_activation
                            .get(&thread_id)
                            .cloned()
                            .flatten();
                        if prompt_tx
                            .send(TurnPrompt {
                                turn_id,
                                prompt,
                                activate,
                            })
                            .await
                            .is_err()
                        {
                            // driver 已退出(理论上不会):清掉活跃标记,
                            // 避免后续 turn 永远报 turn_in_progress。
                            if let Some(s) = threads.get_mut(&thread_id) {
                                s.active_turn_id = None;
                            }
                        }
                    }
                    "turn/interrupt" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        if let Some(turn_id) = session.active_turn_id.clone() {
                            // 幂等 level 信号:用 try_send 避免主循环在满队列上阻塞。
                            let _ = session.interrupt_tx.try_send(turn_id);
                        }
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    "turn/interject" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let text = match extract_prompt(&req.params) {
                            Some(p) => p,
                            None => {
                                write_response(
                                    &writer,
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
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            match session.active_turn_id.clone() {
                                Some(turn_id) => (session.interject_tx.clone(), turn_id),
                                None => {
                                    write_response(
                                        &writer,
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
                            write_response(&writer, err_response(id, RpcError::not_running()))
                                .await?;
                            continue;
                        }
                        write_response(
                            &writer,
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
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
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
                                    Arc::clone(&writer),
                                    thread_id.clone(),
                                    socket,
                                    children.clone(),
                                ));
                                children_watches.insert(thread_id.clone(), ChildrenWatch { task });
                            }
                        }
                        write_response(&writer, ok_response(id, json!({ "children": children })))
                            .await?;
                    }
                    "agent/trace/read" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let Some(task_id) =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("missing taskId")),
                            )
                            .await?;
                            continue;
                        };
                        match read_task_trace(&runtimes, &threads, &thread_id, &task_id) {
                            Ok((rows, high_water_id)) => {
                                write_response(
                                    &writer,
                                    ok_response(
                                        id,
                                        json!({ "rows": rows, "highWaterId": high_water_id }),
                                    ),
                                )
                                .await?;
                            }
                            Err(error) => {
                                write_response(&writer, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/trace/watch" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let Some(task_id) =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
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
                                    Arc::clone(&writer),
                                    thread_id.clone(),
                                    task_id.clone(),
                                    socket_for_thread(&runtimes, &threads, &thread_id)
                                        .unwrap_or_default(),
                                    high_water_id,
                                ));
                                trace_watches.insert(thread_id.clone(), TraceWatch { task: handle });
                                write_response(
                                    &writer,
                                    ok_response(id, json!({ "rows": rows, "highWaterId": high_water_id })),
                                )
                                .await?;
                            }
                            Err(error) => {
                                write_response(&writer, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/message" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let task_id =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string);
                        let message =
                            req.params.get("message").and_then(|v| v.as_str()).map(str::to_string);
                        let (Some(task_id), Some(message)) = (task_id, message) else {
                            write_response(
                                &writer,
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
                                write_response(&writer, ok_response(id, json!({ "queued": true })))
                                    .await?;
                            }
                            Err(error) => {
                                write_response(&writer, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/cancel/preview" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
                            continue;
                        }
                        let Some(task_id) =
                            req.params.get("taskId").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
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
                                    &writer,
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
                                    &writer,
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
                                write_response(&writer, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/cancel" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
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
                                &writer,
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
                                write_response(&writer, ok_response(id, json!({ "cancelled": true })))
                                    .await?;
                            }
                            Err(error) => {
                                write_response(&writer, err_response(id, error)).await?;
                            }
                        }
                    }
                    "agent/trace/unwatch" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&writer, &threads, &thread_id, id.clone()).await? {
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
                        write_response(&writer, ok_response(id, json!({ "stopped": true }))).await?;
                    }
                    "process/list" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
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
                        write_response(&writer, ok_response(id, json!({ "processes": processes })))
                            .await?;
                    }
                    "process/read" => {
                        let Some(thread_id) =
                            req.params.get("thread_id").and_then(|v| v.as_str()).map(str::to_string)
                        else {
                            write_response(
                                &writer,
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
                                &writer,
                                err_response(id, RpcError::invalid_params("missing process_id")),
                            )
                            .await?;
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
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
                                    &writer,
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
                                    &writer,
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
                                &writer,
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
                                &writer,
                                err_response(id, RpcError::invalid_params("missing process_id")),
                            )
                            .await?;
                            continue;
                        };
        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
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
                                write_response(&writer, ok_response(id, json!({ "ok": true })))
                                    .await?
                            }
                            Err(message) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::invalid_params(message)),
                                )
                                .await?
                            }
                        }
                    }
                    _ => {
                        write_response(&writer, err_response(id, RpcError::method_not_found(&method)))
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
        }
    }

    // 进程退出前断开所有项目 root。同步调用:已经不在热路径上,而进程即将结束。
    for binding in attached_runtimes(&runtimes) {
        binding.detach();
    }

    Ok(())
}

/// 恢复会话:`Some` 时用载入的历史覆盖 agent 的 session,`None` 时保持新建的空 session。
fn apply_session(
    agent: yi_agent_core::Agent,
    session: Option<yi_agent_core::Session>,
) -> yi_agent_core::Agent {
    match session {
        Some(s) => agent.with_session(s),
        None => agent,
    }
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

async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    writer.write_value(&resp).await
}

async fn write_notification<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    n: &Notification,
) -> anyhow::Result<()> {
    writer.write_value(&NotificationEnvelope::new(n)).await
}

/// 更新共享状态句柄并推送 `thread/status/updated`。
///
/// 加锁是同步的、不跨 `.await`；锁在写通知前即释放。锁中毒时沿用
/// `workspace_index.rs` 的恢复约定：取回内部值而非 panic(状态只是 UI 提示,
/// 不应因一次 panic 永久失效)。
async fn update_status<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    handle: &std::sync::Mutex<ThreadStatus>,
    thread_id: &str,
    next: ThreadStatus,
) -> anyhow::Result<()> {
    *handle.lock().unwrap_or_else(|p| p.into_inner()) = next;
    write_notification(
        writer,
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
        "status": thread_status(threads, &m.thread_id),
    })
}

/// 把客户端对反向请求的响应路由到等待中的 driver。
async fn route_client_response(
    resp: ClientResponse,
    pending: &Mutex<HashMap<String, oneshot::Sender<Decision>>>,
) {
    let RequestId::Str(key) = resp.id else {
        tracing::warn!("ignoring client response with non-string id");
        return;
    };
    let Some(tx) = pending.lock().await.remove(&key) else {
        tracing::warn!("no pending approval request for id {key}");
        return;
    };
    let _ = tx.send(parse_client_decision(resp.result.as_ref()));
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

/// 一个 turn 结束后（或被 clear / compact 改动后）的收尾：把当前 session 快照
/// 落盘、置 Idle，并在 `turn_id` 为 `Some` 时上报 Finished。
///
/// clear / compact 之后必须调用它：`/clear` 要落一条空快照（否则 resume 回放的是
/// 旧日志），`/compact` 要把压缩结果写回 `.jsonl`。此时**没有 turn 在收尾**，
/// 传 `turn_id = None`：不发 `TurnEvent::Finished`，也不 touch meta。
#[allow(clippy::too_many_arguments)]
async fn persist_and_finish_turn<W>(
    thread_id: &str,
    turn_id: Option<&str>,
    user_prompt: Option<&str>,
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    writer: &MessageWriter<W>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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

    let _ = update_status(writer, status, thread_id, ThreadStatus::Idle).await;
    if let Some(turn_id) = turn_id {
        let _ = turn_tx.send(finished_event(thread_id, turn_id)).await;
    }
}

/// 在**没有 turn 在跑**的时刻执行一条会话命令，返回（可能被重建过的）agent。
///
/// 按值传入/返回是刻意的：`Agent::with_session` 消费 self，而 `Agent` 既不是
/// `Clone` 也没有便宜的占位值，所以无法用 `&mut Agent` 调用它。
#[allow(clippy::too_many_arguments)]
async fn apply_session_command<W>(
    mut agent: yi_agent_core::Agent,
    command: SessionCommand,
    provider: &Arc<dyn yi_agent_core::Provider>,
    config: &yi_agent_core::AgentConfig,
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    writer: &MessageWriter<W>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) -> yi_agent_core::Agent
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match command {
        SessionCommand::Clear { reply } => {
            agent = agent.with_session(yi_agent_core::Session::new());
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
                writer,
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
                    agent = agent.with_session(compacted);
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
                writer,
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
async fn run_thread_driver<W>(
    thread_id: String,
    mut agent: yi_agent_core::Agent,
    mut prompt_rx: mpsc::Receiver<TurnPrompt>,
    mut interrupt_rx: mpsc::Receiver<String>,
    mut interject_rx: mpsc::Receiver<InterjectionRequest>,
    mut session_rx: mpsc::Receiver<SessionCommand>,
    writer: Arc<MessageWriter<W>>,
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
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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
                    agent, command, &provider, &config, &store, &thread_id, &writer, &turn_tx,
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
                    let _ = write_notification(&writer, &n).await;
                }
                let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                // run() 失败即本轮结束:thread 立刻回 Idle(失败是事件不是状态)。
                let _ = update_status(&writer, &status, &thread_id, ThreadStatus::Idle).await;
                continue;
            }
        };

        // 必须在 run() 之后捕获:run() 内部会重建 cancel token。
        let cancel_token = agent.cancel_token();
        // 同一次 run 的投递句柄;run() 结束即失效,下一轮重新取。
        let inbox = agent.inbox_handle();
        let mut cancel_sent = false;

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
                            if writer.write_value(&reverse).await.is_err() {
                                pending.lock().await.remove(&perm_id);
                                let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                return;
                            }
                            // 反向请求已写出:进入等待审批状态。
                            let _ = update_status(
                                &writer,
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
                            let _ = update_status(&writer, &status, &thread_id, ThreadStatus::Running)
                                .await;

                            if let Some(tx) = &decision_tx {
                                let _ = tx.send((request_id, decision)).await;
                            }
                        }
                        Some(e) => {
                            for n in translator.on_event(e) {
                                if let crate::protocol::Notification::ItemCompleted { item, .. } = &n {
                                    completed_items.push(item.clone());
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
                                if write_notification(&writer, &n).await.is_err() {
                                    // 客户端可能已断开;先上报 Finished,
                                    // 避免 active_turn_id 永久卡住。
                                    let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                    return;
                                }
                            }
                        }
                        None => break,
                    }
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
                            let _ = write_notification(&writer, &n).await;
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
                agent, command, &provider, &config, &store, &thread_id, &writer, &turn_tx, &status,
            )
            .await;
            // 命令路径已 append 过最终状态;这里只需为这个 turn 发 Finished 让主循环清
            // active_turn_id,且**不要**再 append 一次(否则会用 turn 前的 session 覆盖)。
            let _ = update_status(&writer, &status, &thread_id, ThreadStatus::Idle).await;
            let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
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
            &writer,
            &turn_tx,
            &status,
        )
        .await;
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

/// 解析 `thread/start` 的目标目录:显式 `params.cwd` 优先,缺省用 `cfg.workdir`。
///
/// 显式 `cwd` canonicalize + 校验是目录;失败写 `-32602` 并返回 `Ok(None)`
/// (调用方 continue)。缺省时沿用 `cfg.workdir` 原样,不 canonicalize:保持旧的
/// 单目录行为,避免 macOS `/var` → `/private/var` 之类改写破坏既有路径语义。
async fn resolve_thread_cwd<W: tokio::io::AsyncWrite + Unpin>(
    params: &serde_json::Value,
    cfg: &RuntimeConfig,
    writer: &MessageWriter<W>,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("cwd").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => match std::fs::canonicalize(s) {
            Ok(p) if p.is_dir() => Ok(Some(p.to_string_lossy().to_string())),
            _ => {
                write_response(
                    writer,
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
async fn require_thread_id<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("threadId").and_then(|v| v.as_str()) {
        Some(s) => Ok(Some(s.to_string())),
        None => {
            write_response(
                writer,
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

#[cfg(test)]
mod plugin_query_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    /// A fake daemon that records the request it received and replies with one
    /// frame, so the test can prove the host forwards the plugin name, method
    /// and params verbatim without interpreting any of them.
    fn fake_daemon(
        dir: &Path,
    ) -> (
        PathBuf,
        Arc<StdMutex<serde_json::Value>>,
        std::thread::JoinHandle<()>,
    ) {
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
                    "result": { "type": "PluginResult", "value": { "cards": [] } },
                });
                let mut stream = stream;
                let _ = stream.write_all(reply.to_string().as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        (socket, seen, handle)
    }

    #[test]
    fn a_query_reaches_the_daemon_with_the_plugin_and_method_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (_socket, seen, handle) = fake_daemon(dir.path());

        plugin_query(
            dir.path(),
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

    #[test]
    fn a_missing_plugin_name_is_refused_before_touching_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let error = plugin_query(dir.path(), "list", "", json!({})).unwrap_err();
        assert!(error.contains("plugin"), "{error}");
    }

    #[test]
    fn an_unreachable_daemon_reports_that_the_plugin_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let error = plugin_query(dir.path(), "list", "superpowers-kanban", json!({})).unwrap_err();
        assert!(error.contains("daemon is unavailable"), "{error}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

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
            build_runtime_tooling(&cfg, &root, "thread-test", switch.clone()).expect("tooling");
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
            build_test_agent(None, &cfg.workdir, crate::thread_store::ThreadMode::Normal).unwrap(),
        );
        let second = attach_delegation(
            &runtimes,
            &thread_roots,
            runtime.path(),
            &cfg,
            &cwd,
            "thread-b",
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

    fn build_test_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
        let config = yi_agent_core::AgentConfig::default();
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    provider.clone(),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    config.clone(),
                ),
                session,
            ),
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
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    provider.clone(),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    config.clone(),
                ),
                session,
            ),
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
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    provider.clone(),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    config.clone(),
                ),
                session,
            ),
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }

    fn test_config() -> RuntimeConfig {
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

    /// 用两条 `duplex` 管道把 server 与测试客户端对接。
    struct Harness {
        client_w: tokio::io::DuplexStream,
        client_r: BufReader<tokio::io::DuplexStream>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
        /// 隔离的全局目录索引;持有它保证 tempdir 存活到 harness 结束。
        _index_dir: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_factory(build_test_agent, PERMISSION_TIMEOUT)
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
                + 'static,
        {
            Self::with_config(test_config(), build, permission_timeout)
        }

        /// 用自定义 config + agent 工厂搭建 harness(持久化测试需要自定义 workdir)。
        fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + 'static,
        {
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let index_dir = tempfile::TempDir::new().unwrap();
            let workspaces = Arc::new(WorkspaceIndex::new(
                index_dir.path().join("workspaces.json"),
            ));
            let handle = tokio::spawn(run_with(
                server_r,
                server_w,
                cfg,
                permission_timeout,
                workspaces,
                RuntimeAttachments {
                    runtimes: Arc::new(StdMutex::new(HashMap::new())),
                    thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                },
                build,
            ));
            Self {
                client_w,
                client_r: BufReader::new(client_r),
                handle,
                _index_dir: index_dir,
            }
        }

        async fn send(&mut self, line: &str) {
            self.client_w.write_all(line.as_bytes()).await.unwrap();
            self.client_w.write_all(b"\n").await.unwrap();
            self.client_w.flush().await.unwrap();
        }

        async fn read_value(&mut self) -> serde_json::Value {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), self.client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a message")
                .expect("read_line failed");
            assert!(n > 0, "unexpected EOF while waiting for a message");
            serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
        }

        /// 关掉客户端写端(触发 EOF)并等待 server 任务结束。
        async fn shutdown(self) {
            let Harness {
                client_w, handle, ..
            } = self;
            drop(client_w);
            let res = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("server must shut down within the timeout")
                .expect("server task must not panic");
            assert!(res.is_ok(), "run_with should return Ok on EOF: {res:?}");
        }
    }

    async fn initialize(h: &mut Harness) {
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
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
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
            "run_with should return Ok on EOF: {result:?}"
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
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
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
        assert!(res.is_ok(), "run_with should return Ok on EOF: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_returns_err() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
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
        for _ in 0..12 {
            let v = h.read_value().await;
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                methods.push(m.to_string());
                if m == "thread/status/updated" {
                    running_status = v["params"]["status"].as_str().map(|s| s.to_string());
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
        let writer = Arc::new(MessageWriter::new(server_w));

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
            writer,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_reports_finished_when_writer_fails() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (_interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        drop(client_r); // 断开读端 → 写通知失败
        let writer = Arc::new(MessageWriter::new(server_w));

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
            writer,
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
        let writer = Arc::new(MessageWriter::new(server_w));

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
            writer,
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
        let writer = Arc::new(MessageWriter::new(server_w));

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
            writer,
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
        let agent = apply_session(
            yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
                .with_permission(checker.clone(), rx_arc.clone()),
            session,
        );
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
        let writer = Arc::new(MessageWriter::new(server_w));
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
            writer,
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
            Ok(BuiltAgent {
                agent: apply_session(
                    yi_agent_core::Agent::new(
                        provider.clone(),
                        Arc::new(yi_agent_core::ToolRegistry::new()),
                        config.clone(),
                    ),
                    session,
                ),
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
        assert!(bad["error"]["message"]
            .as_str()
            .unwrap()
            .contains("process not found"));

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
        assert!(names.contains(&"t4-probe"), "thread must see the held manager: {listed}");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"process/read","params":{{"thread_id":"{thread_id}","process_id":"{}"}}}}"#,
            started.process_id
        ))
        .await;
        let first = read_response(&mut h, 4).await;
        assert!(first["error"].is_null(), "{first}");
        assert!(
            first["result"]["stdout"].as_str().unwrap().contains("alpha"),
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
        assert_eq!(resumed["result"]["thread_id"], thread_id.as_str(), "{resumed}");

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
            note["params"]["process_id"], started.process_id.as_str(),
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
}
