//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
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
}

/// 一个 cwd 的工具集与其权限检查器。
struct RuntimeTooling {
    registry: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
}

/// 每 cwd 的已 attach runtime。app-server 是长驻多 cwd 进程,而 runtime 是按项目
/// 划分的,因此 attach 以 canonical cwd 为键、懒初始化。
///
/// 失败也缓存(存 `Err(原因)`):否则纯目录下每开一个 thread 都会重跑一遍注定失败的
/// bring-up(git 检查 + daemon 起停尝试)。
type ProjectRuntimes = Arc<StdMutex<HashMap<PathBuf, Result<Arc<RuntimeBinding>, String>>>>;

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
    runtime: Option<Arc<RuntimeBinding>>,
}

/// 为该 cwd 的 agent 接上委派能力。失败即降级:保留原 agent,只记 trace。
///
/// 调用方必须已拿到 `build_agent` 的产物,且**不得**在 `threads` 的可变借用内调用:
/// 本函数只经 `runtimes` 的 `Mutex` 访问,不触碰 `threads`,所以先调用、再
/// `threads.insert(..)` 是安全的。
fn attach_delegation(
    runtimes: &ProjectRuntimes,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
    built: BuiltAgent,
) -> Activation {
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = PathBuf::from(cwd);
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&thread_cfg.workdir);
    let runtime = match attach_cwd_runtime(runtimes, &runtime_dir, &thread_cfg) {
        Ok(runtime) => runtime,
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
    match build_runtime_tooling(&thread_cfg, &runtime, thread_id, built.yolo.clone()) {
        Ok(tooling) => Activation {
            built: wrap_for_delegation(built, tooling),
            runtime: Some(runtime),
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
    binding: &Arc<RuntimeBinding>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
) -> Result<RuntimeTooling, String> {
    // The tools get the binding itself, not its snapshot: they re-resolve the
    // live socket and root on every call.
    let workspace_root = binding.current()?.workspace_root;
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
    // The conversation marker is bound here, not inferred later: the model that
    // calls `spawn_agent` never sees a thread id, so each thread's tools carry
    // their own so every child they spawn is tagged with this conversation.
    yi_agent_subagent::register_attached_root_tools_in_thread(
        &mut registry,
        Arc::clone(binding),
        controller,
        Some(thread_id.to_string()),
    );
    let permission =
        yi_agent_runtime::bootstrap::load_permission_checker_with_switch(&workspace_root, yolo)
            .map_err(|error| error.to_string())?;
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
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
    }
}

/// Ask a supervised plugin a question and return its answer verbatim.
///
/// The host attaches no meaning to `method` or `params`, and unwraps no board
/// shape here: this is a generic channel, so the UI (not the server) owns what a
/// card or a switch field means. A plugin the daemon does not supervise, or one
/// that refuses, surfaces as an RPC error the client can show.
fn plugin_query(workdir: &Path, method: &str, plugin: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    if plugin.is_empty() {
        return Err("plugin/query needs a `plugin` name".to_string());
    }
    // The board RPCs used to read `<workdir>/.yi-agent/...` directly. The daemon
    // that runs the plugin lives at the same project root, so that is where the
    // question has to go.
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(workdir);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| error.to_string())?;
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
    run_with(
        reader,
        writer,
        cfg,
        PERMISSION_TIMEOUT,
        workspaces,
        runtimes,
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
    runtimes: ProjectRuntimes,
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
    let mut pending_activation: HashMap<String, Option<Arc<RuntimeBinding>>> = HashMap::new();
    // 每个 thread 至多一条被关注的轨迹流。换任务即替换(不并存),thread 删除即收尾。
    let mut trace_watches: HashMap<String, TraceWatch> = HashMap::new();
    // 每个 thread 至多一个子任务列表守望者,首次 agent/children/list 时建立。
    let mut children_watches: HashMap<String, ChildrenWatch> = HashMap::new();

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
                                .map(|m| {
                                    json!({
                                        "thread_id": m.thread_id,
                                        "cwd": m.cwd,
                                        "model": m.model,
                                        "created_at": m.created_at,
                                        "updated_at": m.updated_at,
                                        "title": m.title,
                                        "permission_mode": m.permission_mode,
                                        "status": thread_status(&threads, &m.thread_id),
                                    })
                                })
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
                                        .map(|m| {
                                            json!({
                                                "thread_id": m.thread_id,
                                                "cwd": m.cwd,
                                                "model": m.model,
                                                "created_at": m.created_at,
                                                "updated_at": m.updated_at,
                                                "title": m.title,
                                                "permission_mode": m.permission_mode,
                                                "status": thread_status(&threads, &m.thread_id),
                                            })
                                        })
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
                        write_response(&writer, ok_response(id, json!({ "groups": groups })))
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
                        let activation = attach_delegation(&runtimes, &cfg, &cwd, &thread_id, built);
                        let BuiltAgent { agent, provider, config, decision_tx, catalog, yolo, .. } =
                            activation.built;
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
                                prompt_tx,
                                interrupt_tx,
                                interject_tx,
                                session_tx,
                                store: Arc::clone(&thread_store),
                                status: Arc::clone(&store_status),
                            },
                        );

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
                        let activation = attach_delegation(&runtimes, &cfg, &cwd, &thread_id, built);
                        let BuiltAgent { agent, provider, config, decision_tx, catalog, yolo, .. } =
                            activation.built;
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
                                prompt_tx,
                                interrupt_tx,
                                interject_tx,
                                session_tx,
                                store: Arc::clone(&thread_store),
                                status: Arc::clone(&store_status),
                            },
                        );

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
                        if let Some(watch) = trace_watches.remove(&thread_id) {
                            watch.stop().await;
                        }
                        if let Some(watch) = children_watches.remove(&thread_id) {
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
                        write_response(&writer, ok_response(id, json!({ "stopped": true }))).await?;
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
    ) -> (PathBuf, Arc<StdMutex<serde_json::Value>>, std::thread::JoinHandle<()>) {
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
