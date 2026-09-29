use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{Local, Timelike};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
use yi_agent_core::subagent::task::{
    PermissionDecision, PermissionRequestId, RootSessionId, TaskId,
};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart, WorkerWorkspace,
};

use crate::repository::RuntimeRepository;
use crate::runtime::{
    ReviewDecision, RuntimeCoordinator, RuntimeCoordinatorError, RuntimeStopOptions,
};
use crate::schedule::ScheduleDefinition;

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_PENDING_EVENT_FRAMES: usize = 1024;
const CONFIRMATION_TTL: Duration = Duration::from_secs(60);
// A command response may legitimately reach MAX_FRAME_BYTES (1 MiB). Writing
// that through a socket whose peer reads slowly takes far longer than the 1s
// timeout used to detect half-open readers, so the write side gets its own,
// much looser budget.
const COMMAND_WRITE_DEADLINE: Duration = Duration::from_secs(30);
// Invalid JSON has no trustworthy request ID to echo, so its error frame uses
// this documented stable empty identifier.
const MISSING_REQUEST_ID: &str = "";
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
struct PendingConfirmation {
    task_id: String,
    recursive: bool,
    scope: CancelScope,
    expires_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CancelScope {
    task_ids: Vec<String>,
    active_leases: Vec<IpcCancelLease>,
    unmerged_deliveries: Vec<IpcCancelDelivery>,
}

impl CancelScope {
    /// The scope of an operation that is not tied to any task or lease.
    fn empty() -> Self {
        Self {
            task_ids: Vec::new(),
            active_leases: Vec::new(),
            unmerged_deliveries: Vec::new(),
        }
    }
}

#[derive(Default)]
struct ConfirmationStore {
    pending: Mutex<HashMap<String, PendingConfirmation>>,
}

impl ConfirmationStore {
    fn issue(&self, task_id: String, recursive: bool, scope: CancelScope) -> String {
        let token = Uuid::new_v4().to_string();
        self.pending
            .lock()
            .expect("confirmation store mutex poisoned")
            .insert(
                token.clone(),
                PendingConfirmation {
                    task_id,
                    recursive,
                    scope,
                    expires_at: Instant::now() + CONFIRMATION_TTL,
                },
            );
        token
    }

    fn consume(&self, token: &str, task_id: &str, recursive: bool, scope: &CancelScope) -> bool {
        let Some(pending) = self
            .pending
            .lock()
            .expect("confirmation store mutex poisoned")
            .remove(token)
        else {
            return false;
        };
        pending.expires_at > Instant::now()
            && pending.task_id == task_id
            && pending.recursive == recursive
            && pending.scope == *scope
    }

    /// Issues the one-shot token that gates the destructive gc reclaim.
    ///
    /// `gc` has no task id and no cancellation scope of its own, so it rides the
    /// existing store with the literal id `"gc"` and an empty scope. That keeps a
    /// gc token indistinguishable-in-shape from a cancel token while making it
    /// impossible for one to satisfy the other's `consume` call.
    fn issue_gc(&self) -> String {
        self.issue("gc".into(), false, CancelScope::empty())
    }

    fn consume_gc(&self, token: &str) -> bool {
        self.consume(token, "gc", false, &CancelScope::empty())
    }
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Repository(#[from] crate::repository::RepositoryError),
    #[error(transparent)]
    Runtime(#[from] RuntimeCoordinatorError),
    #[error("a daemon is already running for {path}")]
    AlreadyRunning { path: PathBuf },
    #[error("IPC frame exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("IPC response frame is truncated after {received} bytes (the peer closed mid-frame)")]
    TruncatedFrame { received: usize },
    #[error("timed out writing response frame after {written} of {total} bytes")]
    FrameWriteTimeout { written: usize, total: usize },
    #[error("daemon listener thread panicked during shutdown")]
    ListenerPanicked,
    #[error(
        "socket path {path} exceeds this platform's {limit}-byte limit; \
         shorten the project path or set YI_AGENT_RUNTIME_DIR to a short directory"
    )]
    SocketPathTooLong { path: String, limit: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(rename = "command")]
    pub command: IpcRequest,
}

/// One versioned response frame. Subscription event frames additionally carry
/// their durable event ID while retaining the subscription request correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<IpcEventPayload>,
    pub result: IpcResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(deny_unknown_fields)]
pub enum IpcRequest {
    Status,
    Stop,
    CreateSession,
    AttachApplicationRoot {
        idempotency_key: String,
        workspace: PathBuf,
    },
    ActivateApplicationRoot {
        session_id: String,
        root_task_id: String,
        capability: String,
        objective: String,
    },
    DetachApplicationRoot {
        session_id: String,
        root_task_id: String,
        capability: String,
    },
    /// Lists reclaimable worktrees without removing anything.
    PreviewGc,
    /// Removes the reclaimable-directory scope after an explicit confirmation.
    ConfirmGc {
        confirmation_token: String,
    },
    CreateSchedule {
        cron: String,
        objective: String,
    },
    ListSchedules,
    DeleteSchedule {
        schedule_id: String,
    },
    SpawnChild {
        session_id: String,
        parent_task_id: String,
        objective: String,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    SpawnApplicationChild {
        session_id: String,
        parent_task_id: String,
        capability: String,
        objective: String,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        model: Option<String>,
    },
    StartWorker {
        session_id: String,
        task_id: String,
    },
    CancelTask {
        session_id: String,
        task_id: String,
        recursive: bool,
    },
    PreviewCancel {
        task_id: String,
        recursive: bool,
    },
    ConfirmCancel {
        task_id: String,
        recursive: bool,
        confirmation_token: String,
    },
    RetryTask {
        session_id: String,
        task_id: String,
    },
    PauseTask {
        session_id: String,
        task_id: String,
    },
    ResumeTask {
        session_id: String,
        task_id: String,
    },
    ResolvePermission {
        request_id: String,
        decision: IpcPermissionDecision,
    },
    Review {
        task_id: String,
        decision: IpcReviewDecision,
    },
    PreviewReview {
        task_id: String,
        decision: IpcReviewDecision,
    },
    ConfirmReview {
        task_id: String,
        decision: IpcReviewDecision,
        confirmation_token: String,
    },
    SendMessage {
        session_id: String,
        sender_task_id: String,
        worker_capability: String,
        recipient_task_id: String,
        message: String,
    },
    SendApplicationMessage {
        session_id: String,
        sender_task_id: String,
        capability: String,
        recipient_task_id: String,
        message: String,
    },
    SendUserMessage {
        task_id: String,
        message: String,
    },
    WaitAgent {
        session_id: String,
        caller_task_id: String,
        capability: String,
        mode: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    InspectTask {
        task_id: String,
    },
    /// A parent reading one of its own descendants. Authorized by the
    /// application-root capability or the caller's own worker capability.
    InspectChild {
        session_id: String,
        caller_task_id: String,
        capability: String,
        task_id: String,
    },
    /// A parent cancelling one of its own descendants.
    CancelChild {
        session_id: String,
        caller_task_id: String,
        capability: String,
        task_id: String,
        #[serde(default)]
        recursive: bool,
    },
    ListTaskSummaries {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default)]
        active_only: bool,
    },
    ReadTaskEvents {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_event_id: Option<i64>,
    },
    ReadTaskMailbox {
        task_id: String,
    },
    ReadTaskDiff {
        task_id: String,
    },
    SubscribeEvents {
        after_event_id: i64,
        #[serde(default)]
        filters: SubscriptionFilters,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionFilters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
}

impl SubscriptionFilters {
    fn matches(&self, event: &IpcEvent) -> bool {
        (self.task_ids.is_empty() || self.task_ids.contains(&event.task_id))
            && (self.kinds.is_empty() || self.kinds.contains(&event.kind))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcResponse {
    Status {
        high_water_event_id: i64,
    },
    Stopping,
    SessionCreated {
        session_id: String,
        root_task_id: String,
    },
    ApplicationRootAttached {
        session_id: String,
        root_task_id: String,
        message_capability: String,
        workspace: WorkerWorkspace,
    },
    ApplicationRootActivated,
    ApplicationRootDetached,
    GcPreview {
        entries: Vec<IpcGcEntry>,
        confirmation_token: String,
        expires_in_secs: u64,
    },
    GcCompleted {
        removed: usize,
    },
    ScheduleCreated {
        schedule_id: String,
    },
    Schedules {
        schedules: Vec<IpcSchedule>,
    },
    ScheduleDeleted,
    TaskSpawned {
        task_id: String,
    },
    TaskStarted,
    TaskCancelled,
    CancelPreview {
        confirmation_token: String,
        task_ids: Vec<String>,
        worktree_leases: Vec<String>,
        active_leases: Vec<IpcCancelLease>,
        unmerged_deliveries: Vec<IpcCancelDelivery>,
        expires_in_secs: u64,
    },
    TaskRetried,
    TaskPaused,
    TaskResumed,
    PermissionResolved,
    ReviewPreview {
        task_id: String,
        delivery_id: String,
        decision: IpcReviewDecision,
        confirmation_token: String,
        expires_in_secs: u64,
    },
    ReviewApproved,
    ReviewReworkRequested,
    ReviewRejected,
    MessageQueued,
    WaitCompleted {
        status: String,
        children: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reports: Vec<IpcCompletedChildReport>,
    },
    TaskDetail(IpcTaskDetail),
    TaskSummaries {
        tasks: Vec<IpcTaskSummary>,
    },
    TaskEvents {
        events: Vec<IpcEvent>,
    },
    TaskMailbox {
        messages: Vec<IpcMailboxMessage>,
    },
    TaskDiff {
        task_id: String,
        delivery_json: String,
    },
    Subscription(SubscriptionSnapshot),
    Event(IpcEvent),
    ResyncRequired,
    UnsupportedProtocol {
        supported_min: u32,
        supported_max: u32,
    },
    Error {
        code: IpcErrorCode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCancelLease {
    pub lease_id: String,
    pub task_id: String,
    pub resource_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCancelDelivery {
    pub delivery_id: String,
    pub task_id: String,
    pub payload_json: String,
}

/// Stable, actor-free decision vocabulary for local daemon controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcPermissionDecision {
    Allow,
    Deny,
}

impl From<IpcPermissionDecision> for PermissionDecision {
    fn from(decision: IpcPermissionDecision) -> Self {
        match decision {
            IpcPermissionDecision::Allow => Self::Allow,
            IpcPermissionDecision::Deny => Self::Deny,
        }
    }
}

impl From<IpcReviewDecision> for ReviewDecision {
    fn from(decision: IpcReviewDecision) -> Self {
        match decision {
            IpcReviewDecision::Accept {} => Self::Approve,
            IpcReviewDecision::Rework { feedback } => Self::Rework { feedback },
            IpcReviewDecision::Reject { reason } => Self::Reject { reason },
        }
    }
}

/// Actor-free review decisions. The daemon resolves task lineage, current
/// delivery, and the direct-parent actor from its canonical runtime state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum IpcReviewDecision {
    Accept {},
    Rework { feedback: String },
    Reject { reason: String },
}

/// Stable public error categories for IPC consumers. Error responses never
/// serialize their underlying database, environment, or implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcErrorCode {
    DaemonNotRunning,
    NotFound,
    InvalidState,
    AuthorityDenied,
    ConfirmationRequired,
    Conflict,
    Validation,
    RateLimited,
    Internal,
}

impl std::fmt::Display for IpcErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            Self::DaemonNotRunning => "daemon_not_running",
            Self::NotFound => "not_found",
            Self::InvalidState => "invalid_state",
            Self::AuthorityDenied => "authority_denied",
            Self::ConfirmationRequired => "confirmation_required",
            Self::Conflict => "conflict",
            Self::Validation => "validation",
            Self::RateLimited => "rate_limited",
            Self::Internal => "internal",
        };
        formatter.write_str(code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionSnapshot {
    pub high_water_event_id: i64,
    pub tasks: Vec<IpcTask>,
    pub events: Vec<IpcEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTask {
    pub task_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkerWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskSummary {
    pub task_id: String,
    pub state: String,
    pub is_root: bool,
}

/// One reclaimable worktree, as reported by `daemon gc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcGcEntry {
    pub task_id: String,
    pub branch: String,
    pub path: String,
    pub state: String,
    /// Whether `branch` is already contained in its parent's HEAD.
    pub merged: bool,
    /// Whether the worktree has modified or untracked files.
    pub dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSchedule {
    pub schedule_id: String,
    pub cron: String,
    pub objective: String,
    pub next_run_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskDetail {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub depth: u8,
    pub state: String,
    pub delivery_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkerWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEvent {
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcMailboxMessage {
    pub message_id: String,
    pub recipient_task_id: String,
    pub sender_task_id: Option<String>,
    pub kind: String,
    pub priority: i64,
    pub payload_json: String,
    pub delivered_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcCompletedChildReport {
    pub task_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// The child's delivered commit, when the child produced a delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
}

/// The stable wire payload for a top-level subscription event frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEventPayload {
    pub task_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload_json: String,
}

impl From<&IpcEvent> for IpcEventPayload {
    fn from(event: &IpcEvent) -> Self {
        Self {
            task_id: event.task_id.clone(),
            kind: event.kind.clone(),
            payload_json: event.payload_json.clone(),
        }
    }
}

/// A client-side event stream. The first frame is always a `Subscription` snapshot.
pub struct Subscription {
    reader: BufReader<UnixStream>,
    request_id: String,
}

impl Subscription {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Reads the next framed response while checking subscription correlation.
    pub fn next_frame(&mut self) -> Result<ResponseEnvelope, IpcError> {
        let response = read_limited_frame(&mut self.reader)?.ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed subscription",
            ))
        })?;
        let envelope: ResponseEnvelope = serde_json::from_slice(&response)?;
        if envelope.protocol_version != PROTOCOL_VERSION || envelope.request_id != self.request_id {
            return Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "subscription response envelope identity mismatch",
            )));
        }
        if let IpcResponse::Event(event) = &envelope.result {
            if envelope.event_id != Some(event.event_id)
                || envelope.event.as_ref() != Some(&IpcEventPayload::from(event))
            {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "subscription event envelope identity mismatch",
                )));
            }
        }
        Ok(envelope)
    }

    pub fn next_response(&mut self) -> Result<IpcResponse, IpcError> {
        Ok(self.next_frame()?.result)
    }
}

/// A manually owned, current-user-only local daemon socket.
pub struct Daemon {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    coordinator: Arc<RuntimeCoordinator>,
    listener: Option<JoinHandle<()>>,
    /// Held for the daemon's whole lifetime. Dropping it releases the kernel
    /// lock, so a crashed daemon can never leave a lock behind.
    _instance_lock: InstanceLock,
}

impl Daemon {
    pub fn start(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
    ) -> Result<Self, IpcError> {
        Self::start_with_factory(
            runtime_dir,
            database_path,
            Arc::new(UnavailableWorkerFactory),
        )
    }

    /// Starts the daemon with application-owned worker construction. The
    /// default `start` remains useful for inspection-only clients, but a
    /// runnable daemon supplies its provider/tool factory here.
    pub fn start_with_factory(
        runtime_dir: impl AsRef<Path>,
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, IpcError> {
        let runtime_dir = runtime_dir.as_ref();
        fs::create_dir_all(runtime_dir)?;
        fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700))?;

        let socket_path = socket_path_for(runtime_dir)?;
        let lock_path = runtime_dir.join("runtime.lock");
        let instance_lock = InstanceLock::acquire(runtime_dir, &lock_path)?;
        // Holding the exclusive lock proves no live daemon serves this directory,
        // so any socket node still on disk was left by a killed process. `bind`
        // fails with `EADDRINUSE` on an existing path, so the stale node must go
        // before we bind our own listener.
        remove_if_exists(&socket_path)?;
        let mut repository = RuntimeRepository::open(database_path.as_ref())?;
        repository.recover_inflight_tasks()?;
        drop(repository);
        let coordinator = Arc::new(RuntimeCoordinator::open(database_path.as_ref(), factory)?);
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let daemon_coordinator = Arc::clone(&coordinator);
        let confirmations = Arc::new(ConfirmationStore::default());
        let listener_confirmations = Arc::clone(&confirmations);
        let client_handlers = Arc::new(Mutex::new(Vec::<JoinHandle<()>>::new()));
        let listener_handlers = Arc::clone(&client_handlers);
        let database_path = database_path.as_ref().to_path_buf();
        let cleanup_socket = socket_path.clone();
        let cleanup_lock = lock_path.clone();
        let listener = thread::spawn(move || {
            // Worker facts arrive independently of client traffic. Reconcile
            // them here so a parent blocked in wait_agent is always woken.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("daemon reconciliation runtime must initialize");
            let mut last_schedule_minute = None;
            while !thread_stop.load(Ordering::Acquire) {
                let _ = runtime.block_on(coordinator.reconcile_worker_events());
                let now = Local::now();
                let minute = now
                    .with_second(0)
                    .and_then(|value| value.with_nanosecond(0));
                if minute != last_schedule_minute {
                    let _ = coordinator.evaluate_schedules(now);
                    // Reclaim runs on its own thread: git is slow and this loop
                    // must stay responsive to accept().
                    let reclaim_coordinator = Arc::clone(&coordinator);
                    std::thread::spawn(move || {
                        reclaim_coordinator.reclaim_idle_worktrees(chrono::Utc::now());
                    });
                    last_schedule_minute = minute;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let database_path = database_path.clone();
                        let stop = Arc::clone(&thread_stop);
                        let coordinator = Arc::clone(&coordinator);
                        let confirmations = Arc::clone(&listener_confirmations);
                        let handler = thread::spawn(move || {
                            let _ = handle_client(
                                stream,
                                &database_path,
                                &stop,
                                &coordinator,
                                &confirmations,
                            );
                        });
                        listener_handlers
                            .lock()
                            .expect("client handler list mutex poisoned")
                            .push(handler);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let handlers = std::mem::take(
                &mut *listener_handlers
                    .lock()
                    .expect("client handler list mutex poisoned"),
            );
            for handler in handlers {
                let _ = handler.join();
            }
            let _ = remove_if_exists(&cleanup_socket);
            let _ = remove_if_exists(&cleanup_lock);
        });
        Ok(Self {
            socket_path,
            stop,
            coordinator: daemon_coordinator,
            listener: Some(listener),
            _instance_lock: instance_lock,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stop this manually started daemon and release its local runtime files.
    pub fn stop(&mut self) -> Result<(), IpcError> {
        prepare_coordinator_for_stop(&self.coordinator)?;
        self.stop_listener()
    }

    fn stop_listener(&mut self) -> Result<(), IpcError> {
        self.stop.store(true, Ordering::Release);
        // Wake the nonblocking accept loop so shutdown does not wait for its sleep interval.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(listener) = self.listener.take() {
            listener.join().map_err(|_| IpcError::ListenerPanicked)?;
        }
        Ok(())
    }

    /// Block the owning daemon process until a local or IPC stop completes.
    pub fn wait(mut self) -> Result<(), IpcError> {
        if let Some(listener) = self.listener.take() {
            listener.join().map_err(|_| IpcError::ListenerPanicked)?;
        }
        Ok(())
    }
}

/// The longest socket path `sockaddr_un.sun_path` accepts, excluding the
/// terminating NUL. Linux allows 108; macOS and the BSDs allow 104. Taking the
/// smaller bound keeps one behaviour across platforms.
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Resolves the Unix-domain socket for a runtime directory.
///
/// A runtime directory normally owns its socket (`<runtime_dir>/runtime.sock`).
/// That layout cannot always work: `sun_path` is capped at
/// [`MAX_SOCKET_PATH_BYTES`], so a deep project path — the default runtime
/// directory is `<workdir>/.yi-agent/runtime` — overflows it and `bind` fails
/// with `AF_UNIX path too long`, silently disabling subagent delegation.
///
/// When the direct path does not fit, the socket moves to a short, stable name
/// under the platform temporary directory, derived from the runtime directory
/// so that the daemon and every client agree on one location. Distinct projects
/// keep distinct sockets; an explicitly shared runtime directory keeps sharing.
pub fn socket_path_for(runtime_dir: &Path) -> Result<PathBuf, IpcError> {
    let direct = runtime_dir.join("runtime.sock");
    if direct.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return Ok(direct);
    }
    let name = socket_file_name(runtime_dir);
    let fallback_dir = std::env::temp_dir();
    let fallback = fallback_dir.join(&name);
    if fallback.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(IpcError::SocketPathTooLong {
            path: fallback.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(fallback)
}

/// The stable file name used when a socket cannot live beside its runtime
/// directory. Derived from the runtime directory, not the working directory, so
/// callers that share a runtime directory also share its socket.
fn socket_file_name(runtime_dir: &Path) -> String {
    let digest = Sha256::digest(runtime_dir.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("yi-agent-{}.sock", &hex[..16])
}

/// The exclusive instance lock for one runtime directory.
///
/// Backed by an advisory `flock` on `runtime.lock` rather than a PID file, so
/// ownership is decided by the kernel and released automatically when the
/// holding process exits — including on a crash. A PID file cannot do this: the
/// OS recycles PIDs, so a lock left by a dead daemon starts looking owned by
/// whatever unrelated process inherits that number, and `kill -0` then reports
/// it as permanently live.
///
/// The lock file itself is left in place. Removing it would race a concurrent
/// acquirer that already opened the same inode: the newcomer would hold a lock
/// on an unlinked file while a third process created a fresh one, and two
/// daemons would both believe they own the directory.
struct InstanceLock {
    /// Dropping this file closes its descriptor, and the kernel releases the
    /// `flock` with it. That is the whole unlock protocol: no `Drop` impl is
    /// needed, and a process that dies without unwinding still releases the lock.
    _file: std::fs::File,
}

impl InstanceLock {
    /// Takes the lock, or reports that another daemon already holds it.
    fn acquire(runtime_dir: &Path, lock_path: &Path) -> Result<Self, IpcError> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            return Ok(Self { _file: file });
        }
        let error = std::io::Error::last_os_error();
        // `EWOULDBLOCK` is how a held advisory lock reports contention; anything
        // else is a real failure (a bad descriptor, an unsupported filesystem)
        // and must not be misreported as "another daemon is running".
        if matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            return Err(IpcError::AlreadyRunning {
                path: runtime_dir.to_path_buf(),
            });
        }
        Err(IpcError::Io(error))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Destructors can run while Tokio is driving an async test or caller.
        // Starting a second current-thread runtime here panics, so only an
        // explicit `stop()` performs the cooperative checkpoint protocol.
        let _ = self.stop_listener();
    }
}

fn remove_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn send_request(
    socket_path: impl AsRef<Path>,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    send_request_with_version(socket_path, PROTOCOL_VERSION, request)
}

pub fn send_request_with_version(
    socket_path: impl AsRef<Path>,
    protocol_version: u32,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    let mut stream = UnixStream::connect(socket_path)?;
    let request_id = next_request_id();
    write_request(&mut stream, protocol_version, request_id.clone(), request)?;
    let response = read_limited_frame(&mut BufReader::new(stream))?.ok_or_else(|| {
        IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed request connection",
        ))
    })?;
    let envelope: ResponseEnvelope = serde_json::from_slice(&response)?;
    if envelope.protocol_version != PROTOCOL_VERSION || envelope.request_id != request_id {
        return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "response envelope identity mismatch",
        )));
    }
    Ok(envelope.result)
}

pub fn subscribe(
    socket_path: impl AsRef<Path>,
    after_event_id: i64,
) -> Result<Subscription, IpcError> {
    subscribe_with_filters(socket_path, after_event_id, SubscriptionFilters::default())
}

pub fn subscribe_with_filters(
    socket_path: impl AsRef<Path>,
    after_event_id: i64,
    filters: SubscriptionFilters,
) -> Result<Subscription, IpcError> {
    let mut stream = UnixStream::connect(socket_path)?;
    let request_id = next_request_id();
    write_request(
        &mut stream,
        PROTOCOL_VERSION,
        request_id.clone(),
        IpcRequest::SubscribeEvents {
            after_event_id,
            filters,
        },
    )?;
    Ok(Subscription {
        reader: BufReader::new(stream),
        request_id,
    })
}

fn write_request(
    stream: &mut UnixStream,
    protocol_version: u32,
    request_id: String,
    request: IpcRequest,
) -> Result<(), IpcError> {
    let frame = serde_json::to_vec(&RequestEnvelope {
        protocol_version,
        request_id,
        command: request,
    })?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    stream.write_all(&frame)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn handle_client(
    mut stream: UnixStream,
    database_path: &Path,
    stop: &Arc<AtomicBool>,
    coordinator: &Arc<RuntimeCoordinator>,
    confirmations: &Arc<ConfirmationStore>,
) -> Result<(), IpcError> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    // The read timeout guards against a half-open peer and stays tight. The
    // write timeout must span a full 1 MiB response to a slow reader, so it
    // tracks COMMAND_WRITE_DEADLINE rather than the read budget.
    stream.set_write_timeout(Some(COMMAND_WRITE_DEADLINE))?;
    let frame = match read_incoming_frame(&mut BufReader::new(stream.try_clone()?))? {
        Some(IncomingFrame::TooLarge(prefix)) => {
            return write_response_frame(
                &mut stream,
                &request_id_from_prefix(&prefix),
                None,
                &IpcResponse::Error {
                    code: IpcErrorCode::Validation,
                    message: None,
                },
            );
        }
        None => return Ok(()),
        Some(IncomingFrame::Complete(frame)) => frame,
    };
    let request_id = request_id_from_frame(&frame);
    let response = match serde_json::from_slice::<RequestEnvelope>(&frame) {
        Ok(envelope) if envelope.protocol_version != PROTOCOL_VERSION => {
            IpcResponse::UnsupportedProtocol {
                supported_min: PROTOCOL_VERSION,
                supported_max: PROTOCOL_VERSION,
            }
        }
        Ok(envelope) => match envelope.command {
            IpcRequest::Stop => {
                prepare_coordinator_for_stop(coordinator)?;
                stop.store(true, Ordering::Release);
                IpcResponse::Stopping
            }
            IpcRequest::SubscribeEvents {
                after_event_id,
                filters,
            } => {
                let subscription = stream_subscription(
                    &mut stream,
                    database_path,
                    Arc::clone(stop),
                    Arc::clone(coordinator),
                    after_event_id,
                    filters,
                    &envelope.request_id,
                );
                return respond_to_subscription_result(
                    &mut stream,
                    &envelope.request_id,
                    subscription,
                );
            }
            IpcRequest::PreviewCancel { task_id, recursive } => {
                match preview_cancel(database_path, confirmations, task_id, recursive) {
                    Ok(response) => response,
                    Err(error) => error_response(&error),
                }
            }
            IpcRequest::ConfirmCancel {
                task_id,
                recursive,
                confirmation_token,
            } => match confirm_cancel(
                database_path,
                coordinator,
                confirmations,
                task_id,
                recursive,
                confirmation_token,
            ) {
                Ok(response) => response,
                Err(error) => error_response(&error),
            },
            IpcRequest::PreviewGc => match preview_gc(database_path, confirmations) {
                Ok(response) => response,
                Err(error) => error_response(&error),
            },
            IpcRequest::ConfirmGc { confirmation_token } => match confirm_gc(
                database_path,
                coordinator,
                confirmations,
                confirmation_token,
            ) {
                Ok(response) => response,
                Err(error) => error_response(&error),
            },
            request => match respond(database_path, coordinator, request) {
                Ok(response) => response,
                Err(error) => error_response(&error),
            },
        },
        Err(_) => IpcResponse::Error {
            code: IpcErrorCode::Validation,
            message: None,
        },
    };
    write_response_frame(&mut stream, &request_id, None, &response)
}

fn respond_to_subscription_result(
    stream: &mut UnixStream,
    request_id: &str,
    result: Result<(), SubscriptionFailure>,
) -> Result<(), IpcError> {
    match result {
        Ok(()) | Err(SubscriptionFailure::AfterInitialFrame) => Ok(()),
        Err(SubscriptionFailure::BeforeInitialFrame) => write_response_frame(
            stream,
            request_id,
            None,
            &IpcResponse::Error {
                code: IpcErrorCode::Internal,
                message: None,
            },
        ),
    }
}

enum SubscriptionFailure {
    BeforeInitialFrame,
    AfterInitialFrame,
}

fn preview_cancel(
    database_path: &Path,
    confirmations: &ConfirmationStore,
    task_id: String,
    recursive: bool,
) -> Result<IpcResponse, IpcError> {
    let repository = RuntimeRepository::open(database_path)?;
    let task = parse_id::<TaskId>(&task_id)?;
    let scope = cancellation_scope(&repository, &task, recursive)?;
    let confirmation_token = confirmations.issue(task_id, recursive, scope.clone());
    Ok(IpcResponse::CancelPreview {
        confirmation_token,
        worktree_leases: scope
            .active_leases
            .iter()
            .filter(|lease| lease.resource_key.starts_with("worktree:"))
            .map(|lease| lease.resource_key.clone())
            .collect(),
        task_ids: scope.task_ids,
        active_leases: scope.active_leases,
        unmerged_deliveries: scope.unmerged_deliveries,
        expires_in_secs: CONFIRMATION_TTL.as_secs(),
    })
}

fn preview_review(
    coordinator: &RuntimeCoordinator,
    task_id: String,
    decision: IpcReviewDecision,
) -> Result<IpcResponse, IpcError> {
    let task = parse_id::<TaskId>(&task_id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let preview = runtime.block_on(coordinator.preview_review(&task, decision.clone().into()))?;
    Ok(IpcResponse::ReviewPreview {
        task_id,
        delivery_id: preview.delivery_id.to_string(),
        decision,
        confirmation_token: preview.confirmation_token,
        expires_in_secs: preview.expires_in_secs,
    })
}

fn confirm_review(
    coordinator: &RuntimeCoordinator,
    task_id: String,
    decision: IpcReviewDecision,
    confirmation_token: String,
) -> Result<IpcResponse, IpcError> {
    let task = parse_id::<TaskId>(&task_id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(coordinator.confirm_review(
        &task,
        decision.clone().into(),
        &confirmation_token,
    ))?;
    match decision {
        IpcReviewDecision::Accept {} => Ok(IpcResponse::ReviewApproved),
        IpcReviewDecision::Rework { .. } => Ok(IpcResponse::ReviewReworkRequested),
        IpcReviewDecision::Reject { .. } => Ok(IpcResponse::ReviewRejected),
    }
}

fn cancellation_scope(
    repository: &RuntimeRepository,
    task: &TaskId,
    recursive: bool,
) -> Result<CancelScope, IpcError> {
    let scope = repository.cancel_scope(task, recursive)?;
    Ok(CancelScope {
        task_ids: scope
            .task_ids
            .into_iter()
            .map(|task| task.to_string())
            .collect(),
        active_leases: scope
            .active_leases
            .into_iter()
            .map(|lease| IpcCancelLease {
                lease_id: lease.lease_id,
                task_id: lease.task_id.to_string(),
                resource_key: lease.resource_key,
            })
            .collect(),
        unmerged_deliveries: scope
            .unmerged_deliveries
            .into_iter()
            .map(|delivery| IpcCancelDelivery {
                delivery_id: delivery.delivery_id,
                task_id: delivery.task_id.to_string(),
                payload_json: delivery.payload_json,
            })
            .collect(),
    })
}

fn confirm_cancel(
    database_path: &Path,
    coordinator: &RuntimeCoordinator,
    confirmations: &ConfirmationStore,
    task_id: String,
    recursive: bool,
    confirmation_token: String,
) -> Result<IpcResponse, IpcError> {
    let repository = RuntimeRepository::open(database_path)?;
    let task = parse_id::<TaskId>(&task_id)?;
    let detail = repository.task_detail(&task)?;
    let scope = cancellation_scope(&repository, &task, recursive)?;
    if !confirmations.consume(&confirmation_token, &task_id, recursive, &scope) {
        return Ok(IpcResponse::Error {
            code: IpcErrorCode::ConfirmationRequired,
            message: None,
        });
    }
    let session = parse_id::<RootSessionId>(&detail.session_id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(coordinator.cancel_task(&session, &task, recursive))?;
    Ok(IpcResponse::TaskCancelled)
}

/// Lists reclaimable worktrees and issues the token that authorizes removing them.
///
/// Read-only: the listing shells out to git and removes nothing, so it is safe to
/// call while worktrees are dirty or unmerged.
fn preview_gc(
    database_path: &Path,
    confirmations: &ConfirmationStore,
) -> Result<IpcResponse, IpcError> {
    let repository = RuntimeRepository::open(database_path)?;
    let entries = gc_entries(&repository)?;
    Ok(IpcResponse::GcPreview {
        entries,
        confirmation_token: confirmations.issue_gc(),
        expires_in_secs: CONFIRMATION_TTL.as_secs(),
    })
}

/// Reclaims the automatic-scope directories after consuming a gc token.
///
/// Scope is deliberately narrow: this removes worktree DIRECTORIES only. It never
/// deletes a branch ref and never deletes a `task_workspaces` row, because the row
/// surviving is what keeps reattachment working. Operations that forfeit
/// reattachment need their own, separately confirmed surface.
///
/// [`RuntimeCoordinator::reclaim_session_worktrees`] is synchronous, takes only
/// short internal repository locks, and never holds one across git, so it is called
/// directly rather than through an async runtime.
fn confirm_gc(
    database_path: &Path,
    coordinator: &RuntimeCoordinator,
    confirmations: &ConfirmationStore,
    confirmation_token: String,
) -> Result<IpcResponse, IpcError> {
    if !confirmations.consume_gc(&confirmation_token) {
        return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "gc confirmation token is invalid or expired",
        )));
    }
    let sessions = {
        let repository = RuntimeRepository::open(database_path)?;
        let mut sessions = repository.detached_application_roots()?;
        // No ORDER BY on the query, so duplicates need not be adjacent. `dedup`
        // alone would miss non-adjacent repeats, so key the sort on the id's
        // string form (`RootSessionId` is not `Ord` itself).
        sessions.sort_unstable_by_key(|session| session.to_string());
        sessions.dedup();
        sessions
    };
    let mut removed = 0;
    for session in sessions {
        removed += coordinator.reclaim_session_worktrees(&session);
    }
    Ok(IpcResponse::GcCompleted { removed })
}

/// Lists the reclaimable-but-not-reclaimed worktrees of every detached session.
///
/// It reads rows and runs `git status` / `git merge-base` with the relevant
/// worktree as the working directory. It removes nothing, so a dirty or unmerged
/// worktree can be reported safely.
///
/// Both columns answer the SAME question the reclaim answers, because a preview
/// that disagrees with the action is worse than no preview on a destructive path:
///
/// * `dirty` is `git status --porcelain`, the probe `reclaim_directory` gates on.
/// * `merged` is `merge-base --is-ancestor <branch> <parent_branch>` against the
///   recorded `parent_branch`, matching `DaemonWorkspaceService::is_merged_into`.
///   Judging it against the owner worktree's CURRENT branch would let a worker
///   that ran `git checkout` in the owner change the answer, and would let the
///   preview print `merged = false` for a directory the confirm then reclaims.
///
/// The probe needs a working directory that exists: the owner worktree when it is
/// still there, otherwise the repository root. That fallback is deliberate — an
/// owner reclaimed by an earlier pass makes `git` fail to `chdir`, and
/// `merge-base --is-ancestor` resolves both refs from the ref database, so any
/// directory inside the repository answers identically. Do not "simplify" it back
/// to the owner path alone; that would silently restore a false `merged`.
///
/// A candidate whose directory is already gone is skipped: the confirm removes
/// directories, so listing a directory-less row would promise work it cannot do.
///
/// A root is reported with whatever the same probe yields (`parent_branch` is the
/// main branch). The reclaim applies no merge gate to a root, so the listing does
/// not pretend a gate ran; it reports the probe rather than inventing a value the
/// confirm would not honour.
fn gc_entries(repository: &RuntimeRepository) -> Result<Vec<IpcGcEntry>, IpcError> {
    let mut entries = Vec::new();
    let mut sessions = repository.detached_application_roots()?;
    sessions.sort_unstable_by_key(|session| session.to_string());
    sessions.dedup();
    for session in sessions {
        for candidate in repository.reclaim_candidates(&session)? {
            let Some(workspace) = candidate.workspace.clone() else {
                continue;
            };
            // The confirm removes a directory, so a row whose directory is gone
            // must not be advertised as reclaimable.
            if !workspace.path.exists() {
                continue;
            }
            let dirty = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&workspace.path)
                .output()
                .map(|output| !output.stdout.is_empty())
                .unwrap_or(false);
            let merged = if workspace.branch.is_empty() || workspace.parent_branch.is_empty() {
                false
            } else {
                // Prefer the owner worktree, but it may already have been
                // reclaimed; fall back to the repository root (see doc comment).
                let probe_directory = candidate
                    .parent_task_id
                    .as_ref()
                    .and_then(|parent| parent.parse::<TaskId>().ok())
                    .and_then(|parent| repository.task_workspace_optional(&parent).ok().flatten())
                    .map(|owner| owner.path)
                    .filter(|path| path.exists())
                    .unwrap_or_else(|| workspace.repository_root.clone());
                Command::new("git")
                    .args([
                        "merge-base",
                        "--is-ancestor",
                        &workspace.branch,
                        &workspace.parent_branch,
                    ])
                    .current_dir(&probe_directory)
                    .output()
                    .map(|output| output.status.success())
                    .unwrap_or(false)
            };
            entries.push(IpcGcEntry {
                task_id: candidate.task_id.clone(),
                branch: workspace.branch.clone(),
                path: workspace.path.display().to_string(),
                state: candidate.state.clone(),
                merged,
                dirty,
            });
        }
    }
    Ok(entries)
}

fn prepare_coordinator_for_stop(coordinator: &RuntimeCoordinator) -> Result<(), IpcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(IpcError::Io)?;
    runtime.block_on(async {
        coordinator
            .graceful_stop(RuntimeStopOptions::default())
            .await
    })?;
    Ok(())
}

fn stream_subscription(
    stream: &mut UnixStream,
    database_path: &Path,
    stop: Arc<AtomicBool>,
    coordinator: Arc<RuntimeCoordinator>,
    after_event_id: i64,
    filters: SubscriptionFilters,
    request_id: &str,
) -> Result<(), SubscriptionFailure> {
    let mut repository = RuntimeRepository::open(database_path)
        .map_err(|_| SubscriptionFailure::BeforeInitialFrame)?;
    let snapshot = repository
        .subscription_snapshot(after_event_id)
        .map_err(|_| SubscriptionFailure::BeforeInitialFrame)?;
    let mut cursor = snapshot.high_water_event_id;
    write_response_frame(
        stream,
        request_id,
        None,
        &IpcResponse::Subscription(SubscriptionSnapshot {
            high_water_event_id: snapshot.high_water_event_id,
            tasks: snapshot
                .tasks
                .into_iter()
                .map(|task| IpcTask {
                    task_id: task.task_id,
                    state: task.state,
                    workspace: task.workspace,
                })
                .collect(),
            events: snapshot
                .events
                .into_iter()
                .map(ipc_event)
                .filter(|event| filters.matches(event))
                .collect(),
        }),
    )
    .map_err(|_| SubscriptionFailure::AfterInitialFrame)?;

    let pending = Arc::new(PendingSubscriptionFrames::new(
        request_id,
        MAX_PENDING_EVENT_FRAMES,
    ));
    let subscription_stop = Arc::new(AtomicBool::new(false));
    let producer_pending = Arc::clone(&pending);
    let producer_stop = Arc::clone(&subscription_stop);
    let producer_daemon_stop = Arc::clone(&stop);
    let database_path = database_path.to_path_buf();
    let producer = thread::spawn(move || -> Result<(), IpcError> {
        let result = (|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            while !producer_daemon_stop.load(Ordering::Acquire)
                && !producer_stop.load(Ordering::Acquire)
            {
                runtime.block_on(coordinator.reconcile_worker_events())?;
                let repository = RuntimeRepository::open(&database_path)?;
                let events = repository.event_records_after(cursor)?;
                for event in events {
                    cursor = event.id;
                    let event = ipc_event(event);
                    if filters.matches(&event) && !producer_pending.push_event(event) {
                        return Ok(());
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        producer_pending.close();
        result
    });

    let write_result = write_subscription_frames(stream, &pending, &stop);
    subscription_stop.store(true, Ordering::Release);
    pending.close();
    let producer_result = producer
        .join()
        .map_err(|_| SubscriptionFailure::AfterInitialFrame)?
        .map_err(|_| SubscriptionFailure::AfterInitialFrame);
    write_result.map_err(|_| SubscriptionFailure::AfterInitialFrame)?;
    producer_result
}

struct PendingSubscriptionFrames {
    request_id: String,
    capacity: usize,
    state: Mutex<PendingSubscriptionState>,
    available: Condvar,
}

struct PendingSubscriptionState {
    frames: VecDeque<ResponseEnvelope>,
    close_reason: SubscriptionQueueClose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubscriptionQueueClose {
    Open,
    ProducerClosed,
    Overflowed,
}

#[derive(Debug, Eq, PartialEq)]
enum SubscriptionFrameWrite {
    Dropped,
    Written(usize),
}

impl PendingSubscriptionFrames {
    fn new(request_id: impl Into<String>, capacity: usize) -> Self {
        assert!(capacity > 0, "subscription queue capacity must be positive");
        Self {
            request_id: request_id.into(),
            capacity,
            state: Mutex::new(PendingSubscriptionState {
                frames: VecDeque::new(),
                close_reason: SubscriptionQueueClose::Open,
            }),
            available: Condvar::new(),
        }
    }

    /// Returns false once this event caused overflow or the subscription was closed.
    fn push_event(&self, event: IpcEvent) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.close_reason != SubscriptionQueueClose::Open {
            return false;
        }
        if state.frames.len() == self.capacity {
            state.frames.clear();
            state.frames.push_back(response_envelope(
                &self.request_id,
                None,
                IpcResponse::ResyncRequired,
            ));
            state.close_reason = SubscriptionQueueClose::Overflowed;
            self.available.notify_one();
            return false;
        }
        let event_id = event.event_id;
        state.frames.push_back(response_envelope(
            &self.request_id,
            Some(event_id),
            IpcResponse::Event(event),
        ));
        self.available.notify_one();
        true
    }

    fn pop_wait(&self) -> Option<ResponseEnvelope> {
        let mut state = self.state.lock().unwrap();
        while state.frames.is_empty() && state.close_reason == SubscriptionQueueClose::Open {
            state = self.available.wait(state).unwrap();
        }
        state.frames.pop_front()
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        if state.close_reason == SubscriptionQueueClose::Open {
            state.close_reason = SubscriptionQueueClose::ProducerClosed;
        }
        self.available.notify_all();
    }

    fn close_reason(&self) -> SubscriptionQueueClose {
        self.state.lock().unwrap().close_reason
    }

    fn write_frame_chunk(
        &self,
        writer: &mut impl Write,
        frame: &PendingFrameWrite,
    ) -> Result<SubscriptionFrameWrite, IpcError> {
        if frame.written == 0 {
            let state = self.state.lock().unwrap();
            if state.close_reason == SubscriptionQueueClose::ProducerClosed
                || (frame.is_event && state.close_reason == SubscriptionQueueClose::Overflowed)
            {
                return Ok(SubscriptionFrameWrite::Dropped);
            }
            return writer
                .write(&frame.bytes)
                .map(SubscriptionFrameWrite::Written)
                .map_err(IpcError::Io);
        }
        writer
            .write(&frame.bytes[frame.written..])
            .map(SubscriptionFrameWrite::Written)
            .map_err(IpcError::Io)
    }

    #[cfg(test)]
    fn drain_for_test(&self) -> Vec<ResponseEnvelope> {
        self.state.lock().unwrap().frames.drain(..).collect()
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().close_reason != SubscriptionQueueClose::Open
    }
}

struct PendingFrameWrite {
    bytes: Vec<u8>,
    written: usize,
    is_event: bool,
    is_resync: bool,
}

impl PendingFrameWrite {
    fn new(envelope: &ResponseEnvelope) -> Result<Self, IpcError> {
        let (mut bytes, is_event, is_resync) = encode_subscription_envelope(envelope)?;
        bytes.push(b'\n');
        Ok(Self {
            bytes,
            written: 0,
            is_event,
            is_resync,
        })
    }
}

fn write_subscription_frames(
    stream: &mut UnixStream,
    pending: &PendingSubscriptionFrames,
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    stream.set_write_timeout(None)?;
    stream.set_nonblocking(true)?;
    let mut current: Option<PendingFrameWrite> = None;
    loop {
        let close_reason = pending.close_reason();
        if close_reason == SubscriptionQueueClose::ProducerClosed
            || (stop.load(Ordering::Acquire) && close_reason != SubscriptionQueueClose::Overflowed)
        {
            stream.shutdown(Shutdown::Write)?;
            return Ok(());
        }
        if current.is_none() {
            let Some(envelope) = pending.pop_wait() else {
                stream.shutdown(Shutdown::Write)?;
                return Ok(());
            };
            current = Some(PendingFrameWrite::new(&envelope)?);
        }

        let frame = current.as_mut().expect("pending frame was initialized");
        match pending.write_frame_chunk(stream, frame) {
            Ok(SubscriptionFrameWrite::Dropped) => {
                current = None;
            }
            Ok(SubscriptionFrameWrite::Written(0)) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write subscription frame",
                )));
            }
            Ok(SubscriptionFrameWrite::Written(written)) => {
                frame.written += written;
                if frame.written == frame.bytes.len() {
                    let sent_resync = frame.is_resync;
                    current = None;
                    if sent_resync {
                        stream.shutdown(Shutdown::Write)?;
                        return Ok(());
                    }
                }
            }
            Err(IpcError::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn write_response_frame(
    stream: &mut UnixStream,
    request_id: &str,
    event_id: Option<i64>,
    response: &IpcResponse,
) -> Result<(), IpcError> {
    write_envelope_frame(
        stream,
        &response_envelope(request_id, event_id, response.clone()),
    )
}

fn response_envelope(
    request_id: &str,
    event_id: Option<i64>,
    response: IpcResponse,
) -> ResponseEnvelope {
    ResponseEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.into(),
        event_id,
        event: match &response {
            IpcResponse::Event(event) => Some(IpcEventPayload::from(event)),
            _ => None,
        },
        result: response,
    }
}

/// Writes a whole frame, tolerating partial writes and backpressure.
///
/// The accepted connection inherits `O_NONBLOCK` from the non-blocking
/// listener, so a single `write` consumes only what fits in the socket send
/// buffer (8192 bytes on macOS) and then reports `WouldBlock`. `write_all`
/// treats that as fatal and abandons the response mid-frame; this loops
/// instead, sleeping briefly until the peer drains the buffer or the deadline
/// expires.
fn write_frame_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), IpcError> {
    let mut written = 0usize;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write response frame",
                )));
            }
            Ok(step) => written += step,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(IpcError::FrameWriteTimeout {
                        written,
                        total: bytes.len(),
                    });
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(IpcError::Io(error)),
        }
    }
    Ok(())
}

fn write_envelope_frame(
    stream: &mut UnixStream,
    envelope: &ResponseEnvelope,
) -> Result<(), IpcError> {
    let frame = match encode_envelope(envelope) {
        Ok(frame) => frame,
        // Resynchronization is meaningful only for event subscriptions. A
        // regular command reply must retain its typed error semantics.
        Err(IpcError::FrameTooLarge) => encode_envelope(&response_envelope(
            &envelope.request_id,
            None,
            IpcResponse::Error {
                code: IpcErrorCode::Internal,
                message: None,
            },
        ))?,
        Err(error) => return Err(error),
    };
    let deadline = Instant::now() + COMMAND_WRITE_DEADLINE;
    write_frame_until(stream, &frame, deadline)?;
    write_frame_until(stream, b"\n", deadline)?;
    stream.flush()?;
    Ok(())
}

fn encode_envelope(envelope: &ResponseEnvelope) -> Result<Vec<u8>, IpcError> {
    let frame = serde_json::to_vec(envelope)?;
    if frame.len() <= MAX_FRAME_BYTES {
        return Ok(frame);
    }
    Err(IpcError::FrameTooLarge)
}

fn encode_subscription_envelope(
    envelope: &ResponseEnvelope,
) -> Result<(Vec<u8>, bool, bool), IpcError> {
    if let Ok(frame) = encode_envelope(envelope) {
        return Ok((
            frame,
            matches!(envelope.result, IpcResponse::Event(_)),
            matches!(envelope.result, IpcResponse::ResyncRequired),
        ));
    }
    Ok((
        serde_json::to_vec(&ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: envelope.request_id.clone(),
            event_id: None,
            event: None,
            result: IpcResponse::ResyncRequired,
        })?,
        false,
        true,
    ))
}

fn next_request_id() -> String {
    format!(
        "request-{}-{}",
        std::process::id(),
        NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod subscription_queue_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;

    fn event(event_id: i64) -> IpcEvent {
        IpcEvent {
            event_id,
            task_id: format!("task-{event_id}"),
            kind: "task_started".into(),
            payload_json: "{}".into(),
        }
    }

    #[test]
    fn pending_event_overflow_emits_one_correlated_resync_and_closes() {
        assert_eq!(MAX_PENDING_EVENT_FRAMES, 1024);
        let queue = PendingSubscriptionFrames::new("slow-client", 2);
        assert!(queue.push_event(event(1)));
        assert!(queue.push_event(event(2)));
        assert!(!queue.push_event(event(3)));
        assert!(!queue.push_event(event(4)));

        let frames = queue.drain_for_test();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].protocol_version, PROTOCOL_VERSION);
        assert_eq!(frames[0].request_id, "slow-client");
        assert_eq!(frames[0].event_id, None);
        assert_eq!(frames[0].event, None);
        assert_eq!(frames[0].result, IpcResponse::ResyncRequired);
        assert!(queue.is_closed());
    }

    #[test]
    fn slow_subscriber_receives_resync_then_socket_closes() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let queue = Arc::new(PendingSubscriptionFrames::new("slow-client", 2));
        assert!(queue.push_event(event(1)));
        assert!(queue.push_event(event(2)));
        assert!(!queue.push_event(event(3)));

        let writer_queue = Arc::clone(&queue);
        let writer = thread::spawn(move || {
            while let Some(frame) = writer_queue.pop_wait() {
                write_envelope_frame(&mut writer, &frame).unwrap();
            }
        });
        let mut subscription = Subscription {
            reader: BufReader::new(reader),
            request_id: "slow-client".into(),
        };

        assert_eq!(
            subscription.next_response().unwrap(),
            IpcResponse::ResyncRequired
        );
        assert!(matches!(
            subscription.next_response(),
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        writer.join().unwrap();
    }

    #[test]
    fn partial_event_frame_finishes_before_resync_frame() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        set_send_buffer(&writer, 4 * 1024);
        reader
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let queue = Arc::new(PendingSubscriptionFrames::new("partial-client", 2));
        let mut large_event = event(1);
        large_event.task_id = "x".repeat(256 * 1024);
        assert!(queue.push_event(large_event));

        let writer_queue = Arc::clone(&queue);
        let writer = thread::spawn(move || {
            write_subscription_frames(&mut writer, &writer_queue, &AtomicBool::new(false)).unwrap();
        });
        assert!(wait_until_socket_has_bytes(&reader));
        assert!(queue.push_event(event(2)));
        assert!(queue.push_event(event(3)));
        assert!(!queue.push_event(event(4)));

        let mut subscription = Subscription {
            reader: BufReader::new(reader),
            request_id: "partial-client".into(),
        };
        let first = subscription.next_frame().unwrap();
        assert!(matches!(
            first.result,
            IpcResponse::Event(IpcEvent { event_id: 1, .. })
        ));
        assert_eq!(
            subscription.next_response().unwrap(),
            IpcResponse::ResyncRequired
        );
        assert!(matches!(
            subscription.next_response(),
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        writer.join().unwrap();
    }

    #[test]
    fn producer_close_interrupts_a_blocked_partial_frame_writer() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        set_send_buffer(&writer, 4 * 1024);
        let queue = Arc::new(PendingSubscriptionFrames::new("closed-client", 2));
        let mut large_event = event(1);
        large_event.task_id = "x".repeat(256 * 1024);
        assert!(queue.push_event(large_event));

        let writer_queue = Arc::clone(&queue);
        let (finished_tx, finished_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            let result =
                write_subscription_frames(&mut writer, &writer_queue, &AtomicBool::new(false));
            let _ = finished_tx.send(result);
        });
        assert!(wait_until_socket_has_bytes(&reader));

        queue.close();
        assert!(
            finished_rx.recv_timeout(Duration::from_millis(250)).is_ok(),
            "producer close must stop a writer blocked on an unread peer"
        );
        drop(reader);
        writer.join().unwrap();
    }

    #[test]
    fn overflow_cannot_race_between_the_initial_check_and_first_write() {
        struct LockCheckingWriter<'a> {
            state: &'a Mutex<PendingSubscriptionState>,
            observed_locked_state: bool,
            bytes: Vec<u8>,
        }

        impl Write for LockCheckingWriter<'_> {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                if self.state.try_lock().is_err() {
                    self.observed_locked_state = true;
                    return Err(std::io::ErrorKind::WouldBlock.into());
                }
                self.bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let queue = PendingSubscriptionFrames::new("racing-client", 1);
        assert!(queue.push_event(event(1)));
        let envelope = queue.pop_wait().unwrap();
        let frame = PendingFrameWrite::new(&envelope).unwrap();
        let mut writer = LockCheckingWriter {
            state: &queue.state,
            observed_locked_state: false,
            bytes: Vec::new(),
        };

        let first = queue.write_frame_chunk(&mut writer, &frame);
        assert!(matches!(
            first,
            Err(IpcError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert!(writer.observed_locked_state);

        assert!(queue.push_event(event(2)));
        assert!(!queue.push_event(event(3)));
        assert_eq!(
            queue.write_frame_chunk(&mut writer, &frame).unwrap(),
            SubscriptionFrameWrite::Dropped
        );
        assert!(writer.bytes.is_empty());
    }

    #[test]
    fn producer_close_drops_a_frame_that_has_not_started_writing() {
        let queue = PendingSubscriptionFrames::new("closing-client", 1);
        assert!(queue.push_event(event(1)));
        let envelope = queue.pop_wait().unwrap();
        let frame = PendingFrameWrite::new(&envelope).unwrap();
        queue.close();

        let mut writer = Vec::new();
        assert_eq!(
            queue.write_frame_chunk(&mut writer, &frame).unwrap(),
            SubscriptionFrameWrite::Dropped
        );
        assert!(writer.is_empty());
    }

    #[test]
    fn normal_oversized_response_uses_a_typed_error_not_subscription_resync() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let response = response_envelope(
            "normal-client",
            None,
            IpcResponse::TaskDetail(IpcTaskDetail {
                task_id: "task".into(),
                session_id: "session".into(),
                parent_task_id: None,
                depth: 0,
                state: "queued".into(),
                delivery_json: "x".repeat(MAX_FRAME_BYTES),
                terminal_json: None,
                workspace: None,
            }),
        );

        write_envelope_frame(&mut writer, &response).unwrap();
        let mut reader = BufReader::new(reader);
        let frame = read_limited_frame(&mut reader).unwrap().unwrap();
        let response: ResponseEnvelope = serde_json::from_slice(&frame).unwrap();
        assert_eq!(response.request_id, "normal-client");
        assert_eq!(
            response.result,
            IpcResponse::Error {
                code: IpcErrorCode::Internal,
                message: None,
            }
        );
    }

    #[test]
    fn a_truncated_response_is_reported_as_truncated_not_as_oversized() {
        // A peer that closes mid-frame yields a frame without its terminator.
        // That is a *transport* failure, not a size violation; conflating the two
        // sends operators chasing a payload-size limit that was never hit.
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"{\"partial\":").unwrap();
        writer.shutdown(Shutdown::Write).unwrap();

        let mut reader = BufReader::new(reader);
        let error = read_limited_frame(&mut reader).unwrap_err();

        assert!(
            matches!(error, IpcError::TruncatedFrame { .. }),
            "a closed mid-frame response must be TruncatedFrame, got: {error:?}"
        );
    }

    #[test]
    fn a_frame_over_the_cap_is_reported_as_too_large_not_truncated() {
        // The genuine size violation keeps its own distinct error.
        //
        // The payload is larger than the socket buffer, so it has to be written
        // from its own thread: a same-thread write fills the buffer and blocks
        // before this thread ever reaches read_limited_frame, hanging the test
        // instead of exercising the error path.
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let pump = std::thread::spawn(move || {
            let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
            writer.write_all(&oversized).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();
        });

        let mut reader = BufReader::new(reader);
        let error = read_limited_frame(&mut reader).unwrap_err();

        assert!(
            matches!(error, IpcError::FrameTooLarge),
            "a frame past the cap must stay FrameTooLarge, got: {error:?}"
        );
        pump.join().unwrap();
    }

    #[test]
    fn truncation_and_write_timeout_map_to_validation_not_internal() {
        assert_eq!(
            ipc_error_code(&IpcError::TruncatedFrame { received: 12 }),
            IpcErrorCode::Validation
        );
        assert_eq!(
            ipc_error_code(&IpcError::FrameWriteTimeout {
                written: 8192,
                total: 26000,
            }),
            IpcErrorCode::Validation
        );
    }

    #[test]
    fn subscription_failure_after_initial_frame_does_not_append_an_error_frame() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(br#"{"partial":"#).unwrap();

        respond_to_subscription_result(
            &mut writer,
            "subscription-client",
            Err(SubscriptionFailure::AfterInitialFrame),
        )
        .unwrap();
        writer.shutdown(Shutdown::Write).unwrap();

        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, br#"{"partial":"#);
    }

    #[test]
    fn draining_admission_error_is_a_typed_invalid_state_response() {
        assert_eq!(
            ipc_error_code(&IpcError::Runtime(RuntimeCoordinatorError::Draining)),
            IpcErrorCode::InvalidState
        );
    }

    fn set_send_buffer(stream: &UnixStream, bytes: libc::c_int) {
        // SAFETY: the file descriptor and option pointer are valid for this call.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::addr_of!(bytes).cast(),
                std::mem::size_of_val(&bytes) as libc::socklen_t,
            )
        };
        assert_eq!(result, 0);
    }

    #[test]
    fn a_frame_larger_than_the_send_buffer_is_written_in_full() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        set_send_buffer(&writer, 4 * 1024);

        let payload = vec![b'x'; 64 * 1024];
        let deadline = Instant::now() + Duration::from_secs(10);

        // Drain until EOF rather than a byte count: the frame is payload plus
        // its terminator, so stopping at `payload.len()` closes the reader
        // before the trailing newline is written and the writer sees BrokenPipe.
        let pump = std::thread::spawn(move || {
            let mut drained = 0usize;
            let mut scratch = [0u8; 8192];
            loop {
                match reader.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(n) => drained += n,
                    Err(_) => break,
                }
            }
            drained
        });

        write_frame_until(&mut writer, &payload, deadline).unwrap();
        write_frame_until(&mut writer, b"\n", deadline).unwrap();
        writer.flush().unwrap();
        drop(writer);

        let drained = pump.join().unwrap();
        assert_eq!(
            drained,
            payload.len() + 1,
            "the whole frame, terminator included, must reach the peer"
        );
    }

    fn wait_until_socket_has_bytes(stream: &UnixStream) -> bool {
        for _ in 0..1_000 {
            let mut byte = 0_u8;
            // SAFETY: the one-byte output buffer and file descriptor are valid.
            let received = unsafe {
                libc::recv(
                    stream.as_raw_fd(),
                    std::ptr::addr_of_mut!(byte).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if received > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }
}

fn request_id_from_frame(frame: &[u8]) -> String {
    request_id_from_json(frame).unwrap_or_else(|| MISSING_REQUEST_ID.into())
}

fn request_id_from_json(frame: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(frame)
        .ok()
        .and_then(|value| value.get("request_id")?.as_str().map(str::to_owned))
}

fn request_id_from_prefix(prefix: &[u8]) -> String {
    request_id_from_json(prefix)
        .or_else(|| json_string_field(prefix, b"\"request_id\""))
        .unwrap_or_else(|| MISSING_REQUEST_ID.into())
}

fn json_string_field(frame: &[u8], key: &[u8]) -> Option<String> {
    for key_start in frame
        .windows(key.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == key).then_some(index))
    {
        let mut value_start = key_start + key.len();
        while frame.get(value_start).is_some_and(u8::is_ascii_whitespace) {
            value_start += 1;
        }
        if frame.get(value_start) != Some(&b':') {
            continue;
        }
        value_start += 1;
        while frame.get(value_start).is_some_and(u8::is_ascii_whitespace) {
            value_start += 1;
        }
        if frame.get(value_start) != Some(&b'\"') {
            continue;
        }
        let mut escaped = false;
        for value_end in value_start + 1..frame.len() {
            match frame[value_end] {
                b'\\' if !escaped => escaped = true,
                b'\"' if !escaped => {
                    if let Ok(value) = serde_json::from_slice(&frame[value_start..=value_end]) {
                        return Some(value);
                    }
                    break;
                }
                _ => escaped = false,
            }
        }
    }
    None
}

fn error_response(error: &IpcError) -> IpcResponse {
    IpcResponse::Error {
        code: ipc_error_code(error),
        message: ipc_error_message(error),
    }
}

fn ipc_error_message(error: &IpcError) -> Option<String> {
    match error {
        IpcError::Runtime(RuntimeCoordinatorError::Supervisor(message)) => Some(message.clone()),
        IpcError::Runtime(RuntimeCoordinatorError::Spawn(
            yi_agent_core::subagent::supervisor::SpawnError::DirectChildLimitReached,
        )) => Some("an agent may have at most four direct children".into()),
        _ => None,
    }
}

fn ipc_error_code(error: &IpcError) -> IpcErrorCode {
    match error {
        IpcError::Repository(error) => repository_error_code(error),
        IpcError::Runtime(RuntimeCoordinatorError::SessionNotFound(_)) => IpcErrorCode::NotFound,
        IpcError::Runtime(RuntimeCoordinatorError::Repository(error)) => {
            repository_error_code(error)
        }
        IpcError::Runtime(RuntimeCoordinatorError::Spawn(
            yi_agent_core::subagent::supervisor::SpawnError::ParentNotFound,
        )) => IpcErrorCode::NotFound,
        IpcError::Runtime(RuntimeCoordinatorError::Supervisor(_))
        | IpcError::Runtime(RuntimeCoordinatorError::Spawn(_)) => IpcErrorCode::InvalidState,
        IpcError::Runtime(RuntimeCoordinatorError::ResidentCapacityExhausted) => {
            IpcErrorCode::RateLimited
        }
        IpcError::Runtime(RuntimeCoordinatorError::Draining) => IpcErrorCode::InvalidState,
        IpcError::Runtime(RuntimeCoordinatorError::AuthorityDenied(_)) => {
            IpcErrorCode::AuthorityDenied
        }
        IpcError::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            IpcErrorCode::Validation
        }
        // Transport-layer failures stay Validation: a truncated or timed-out
        // response is not a daemon fault, and reporting it as Internal sends
        // operators hunting for a crashed daemon that is running fine.
        IpcError::Json(_)
        | IpcError::FrameTooLarge
        | IpcError::TruncatedFrame { .. }
        | IpcError::FrameWriteTimeout { .. } => IpcErrorCode::Validation,
        _ => IpcErrorCode::Internal,
    }
}

enum IncomingFrame {
    Complete(Vec<u8>),
    TooLarge(Vec<u8>),
}

/// Retain at most one frame prefix; oversized input is never buffered in full.
fn read_incoming_frame<R: BufRead>(reader: &mut R) -> Result<Option<IncomingFrame>, IpcError> {
    let mut prefix = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let remaining = (MAX_FRAME_BYTES + 1).saturating_sub(prefix.len());
        if remaining == 0 {
            return Ok(Some(IncomingFrame::TooLarge(prefix)));
        }
        match reader.take(remaining as u64).read_until(b'\n', &mut prefix) {
            Ok(0) if prefix.is_empty() => return Ok(None),
            Ok(0) => return Ok(Some(IncomingFrame::TooLarge(prefix))),
            Ok(_) if prefix.ends_with(b"\n") => {
                prefix.pop();
                return Ok(Some(IncomingFrame::Complete(prefix)));
            }
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out reading IPC frame",
                    )));
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(IpcError::Io(error)),
        }
    }
}

fn repository_error_code(error: &crate::repository::RepositoryError) -> IpcErrorCode {
    match error {
        crate::repository::RepositoryError::TaskNotFound { .. }
        | crate::repository::RepositoryError::TaskWorkspaceNotFound { .. }
        | crate::repository::RepositoryError::MailboxMessageNotFound { .. }
        | crate::repository::RepositoryError::PermissionRequestNotFound { .. } => {
            IpcErrorCode::NotFound
        }
        crate::repository::RepositoryError::PermissionRequestNotPending { .. } => {
            IpcErrorCode::InvalidState
        }
        crate::repository::RepositoryError::DeliveryRequiresParent { .. }
        | crate::repository::RepositoryError::TaskNotReadyForDelivery { .. }
        | crate::repository::RepositoryError::DeliveryNotAwaitingReview { .. } => {
            IpcErrorCode::InvalidState
        }
        crate::repository::RepositoryError::ReviewActorMismatch { .. } => {
            IpcErrorCode::AuthorityDenied
        }
        crate::repository::RepositoryError::IntegrationNotValidated
        | crate::repository::RepositoryError::ReviewFeedbackRequired
        | crate::repository::RepositoryError::ReviewReasonRequired => IpcErrorCode::Validation,
        crate::repository::RepositoryError::Sql(_)
        | crate::repository::RepositoryError::Json(_)
        | crate::repository::RepositoryError::InvalidWorkerRecoveryContext { .. }
        | crate::repository::RepositoryError::InvalidAdmissionCursor { .. }
        | crate::repository::RepositoryError::InvalidTaskWorkspace { .. }
        | crate::repository::RepositoryError::InvalidWatchdogSnapshot { .. }
        | crate::repository::RepositoryError::UnknownEventKind { .. } => IpcErrorCode::Internal,
    }
}

fn read_limited_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, IpcError> {
    let mut frame = Vec::new();
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)?;
    if read == 0 {
        return Ok(None);
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    if !frame.ends_with(b"\n") {
        return Err(IpcError::TruncatedFrame {
            received: frame.len(),
        });
    }
    frame.pop();
    Ok(Some(frame))
}

fn ipc_event(event: crate::repository::PersistedEvent) -> IpcEvent {
    IpcEvent {
        event_id: event.id,
        task_id: event.task_id.to_string(),
        kind: runtime_event_name(event.event).into(),
        payload_json: event.payload_json,
    }
}

fn runtime_event_name(event: crate::repository::RuntimeEvent) -> &'static str {
    match event {
        crate::repository::RuntimeEvent::RuntimeDraining => "runtime_draining",
        crate::repository::RuntimeEvent::RuntimeRecovered => "runtime_recovered",
        crate::repository::RuntimeEvent::TaskQueued => "task_queued",
        crate::repository::RuntimeEvent::TaskStarted => "task_started",
        crate::repository::RuntimeEvent::TaskCompleted => "task_completed",
        crate::repository::RuntimeEvent::TaskCancelled => "task_cancelled",
        crate::repository::RuntimeEvent::TaskPauseRequested => "task_pause_requested",
        crate::repository::RuntimeEvent::TaskPaused => "task_paused",
        crate::repository::RuntimeEvent::TaskProgress => "task_progress",
        crate::repository::RuntimeEvent::TaskBlocked => "task_blocked",
        crate::repository::RuntimeEvent::TaskStalled => "task_stalled",
        crate::repository::RuntimeEvent::TaskTimedOut => "task_timed_out",
        crate::repository::RuntimeEvent::TaskBudgetExhausted => "task_budget_exhausted",
        crate::repository::RuntimeEvent::TaskFailed => "task_failed",
        crate::repository::RuntimeEvent::TaskRecoveryRequired => "task_recovery_required",
        crate::repository::RuntimeEvent::TaskRecoveryAttested => "task_recovery_attested",
        crate::repository::RuntimeEvent::TaskDelivered => "task_delivered",
        crate::repository::RuntimeEvent::MailboxMessageQueued => "mailbox_message_queued",
        crate::repository::RuntimeEvent::MailboxMessageConsumed => "mailbox_message_consumed",
        crate::repository::RuntimeEvent::PermissionRequested => "permission_requested",
        crate::repository::RuntimeEvent::PermissionResolved => "permission_resolved",
        crate::repository::RuntimeEvent::ReviewApproved => "review_approved",
        crate::repository::RuntimeEvent::ReviewAccepted => "review_accepted",
        crate::repository::RuntimeEvent::ReviewRework => "review_rework",
        crate::repository::RuntimeEvent::ReviewRejected => "review_rejected",
        crate::repository::RuntimeEvent::TaskWorkspaceRecycled => "task_workspace_recycled",
        crate::repository::RuntimeEvent::TaskWorkspaceRecycleFailed => {
            "task_workspace_recycle_failed"
        }
    }
}

fn respond(
    database_path: &Path,
    coordinator: &Arc<RuntimeCoordinator>,
    request: IpcRequest,
) -> Result<IpcResponse, IpcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(coordinator.reconcile_worker_events())?;
    let mut repository = RuntimeRepository::open(database_path)?;
    match request {
        IpcRequest::Status => Ok(IpcResponse::Status {
            high_water_event_id: repository.latest_event_id()?,
        }),
        IpcRequest::Stop => Ok(IpcResponse::Stopping),
        IpcRequest::CreateSession => {
            let session_id = coordinator.create_session()?;
            let root_task_id = coordinator.root_task_id(&session_id)?;
            Ok(IpcResponse::SessionCreated {
                session_id: session_id.to_string(),
                root_task_id: root_task_id.to_string(),
            })
        }
        IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace,
        } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let attached = runtime
                .block_on(coordinator.attach_application_root(&idempotency_key, &workspace))?;
            Ok(IpcResponse::ApplicationRootAttached {
                session_id: attached.session_id.to_string(),
                root_task_id: attached.root_task_id.to_string(),
                message_capability: attached.message_capability,
                workspace: attached.workspace,
            })
        }
        IpcRequest::ActivateApplicationRoot {
            session_id,
            root_task_id,
            capability,
            objective,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let root_task_id = parse_id::<TaskId>(&root_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.activate_application_root(
                &session_id,
                &root_task_id,
                &capability,
                objective,
            ))?;
            Ok(IpcResponse::ApplicationRootActivated)
        }
        IpcRequest::DetachApplicationRoot {
            session_id,
            root_task_id,
            capability,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let root_task_id = parse_id::<TaskId>(&root_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.detach_application_root(
                &session_id,
                &root_task_id,
                &capability,
            ))?;
            // Reclaim on a background thread: git is a synchronous subprocess and
            // the client's `send_request` sets no read timeout, so doing this
            // inline would make the TUI wait for the full duration. Detached is
            // safe because the reclaim is idempotent and re-runnable, and
            // `prepare_task_workspace` rebuilds any directory it removes.
            let reclaim_coordinator = Arc::clone(coordinator);
            let reclaim_session = session_id.clone();
            std::thread::spawn(move || {
                reclaim_coordinator.reclaim_session_worktrees(&reclaim_session);
            });
            Ok(IpcResponse::ApplicationRootDetached)
        }
        IpcRequest::CreateSchedule { cron, objective } => {
            let definition = ScheduleDefinition::new(cron, objective).map_err(|error| {
                IpcError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
            })?;
            let next_run_at = definition.next_run_after(Local::now()).map_err(|error| {
                IpcError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
            })?;
            let schedule = repository.create_schedule(&definition, next_run_at)?;
            Ok(IpcResponse::ScheduleCreated {
                schedule_id: schedule.id,
            })
        }
        IpcRequest::ListSchedules => Ok(IpcResponse::Schedules {
            schedules: repository
                .schedules()?
                .into_iter()
                .map(|schedule| IpcSchedule {
                    schedule_id: schedule.id,
                    cron: schedule.definition.cron,
                    objective: schedule.definition.objective,
                    next_run_at: schedule.next_run_at.to_rfc3339(),
                })
                .collect(),
        }),
        IpcRequest::DeleteSchedule { schedule_id } => {
            if !repository.delete_schedule(&schedule_id)? {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "schedule does not exist",
                )));
            }
            Ok(IpcResponse::ScheduleDeleted)
        }
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id,
            objective,
            mode,
            model,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            let workspace_mode = parse_workspace_mode(mode)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let task_id = runtime.block_on(coordinator.spawn_child_and_admit(
                &session_id,
                &parent_task_id,
                objective,
                workspace_mode,
                model,
            ))?;
            Ok(IpcResponse::TaskSpawned {
                task_id: task_id.to_string(),
            })
        }
        IpcRequest::SpawnApplicationChild {
            session_id,
            parent_task_id,
            capability,
            objective,
            mode,
            model,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            let workspace_mode = parse_workspace_mode(mode)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let task_id = runtime.block_on(coordinator.spawn_application_child(
                &session_id,
                &parent_task_id,
                &capability,
                objective,
                workspace_mode,
                model,
            ))?;
            Ok(IpcResponse::TaskSpawned {
                task_id: task_id.to_string(),
            })
        }
        IpcRequest::StartWorker {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.start_worker(&session_id, &task_id))?;
            Ok(IpcResponse::TaskStarted)
        }
        IpcRequest::CancelTask { .. } => Ok(IpcResponse::Error {
            code: IpcErrorCode::ConfirmationRequired,
            message: None,
        }),
        IpcRequest::PreviewCancel { .. } | IpcRequest::ConfirmCancel { .. } => {
            Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "confirmation requests require daemon state",
            )))
        }
        IpcRequest::PreviewGc | IpcRequest::ConfirmGc { .. } => {
            Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "confirmation requests require daemon state",
            )))
        }
        IpcRequest::RetryTask {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.retry_task(&session_id, &task_id))?;
            Ok(IpcResponse::TaskRetried)
        }
        IpcRequest::PauseTask {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.pause_task(&session_id, &task_id))?;
            Ok(IpcResponse::TaskPaused)
        }
        IpcRequest::ResumeTask {
            session_id,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.resume_task(&session_id, &task_id))?;
            Ok(IpcResponse::TaskResumed)
        }
        IpcRequest::ResolvePermission {
            request_id,
            decision,
        } => {
            let request_id = parse_id::<PermissionRequestId>(&request_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.resolve_permission(&request_id, decision.into()))?;
            Ok(IpcResponse::PermissionResolved)
        }
        IpcRequest::Review { .. } => Ok(IpcResponse::Error {
            code: IpcErrorCode::ConfirmationRequired,
            message: None,
        }),
        IpcRequest::PreviewReview { task_id, decision } => {
            preview_review(coordinator, task_id, decision)
        }
        IpcRequest::ConfirmReview {
            task_id,
            decision,
            confirmation_token,
        } => confirm_review(coordinator, task_id, decision, confirmation_token),
        IpcRequest::SendMessage {
            session_id,
            sender_task_id,
            worker_capability,
            recipient_task_id,
            message,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let sender_task_id = parse_id::<TaskId>(&sender_task_id)?;
            let recipient_task_id = parse_id::<TaskId>(&recipient_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.send_message(
                &session_id,
                &sender_task_id,
                &worker_capability,
                recipient_task_id,
                message,
            ))?;
            Ok(IpcResponse::MessageQueued)
        }
        IpcRequest::SendApplicationMessage {
            session_id,
            sender_task_id,
            capability,
            recipient_task_id,
            message,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let sender_task_id = parse_id::<TaskId>(&sender_task_id)?;
            let recipient_task_id = parse_id::<TaskId>(&recipient_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.send_application_message(
                &session_id,
                &sender_task_id,
                &capability,
                recipient_task_id,
                message,
            ))?;
            Ok(IpcResponse::MessageQueued)
        }
        IpcRequest::SendUserMessage { task_id, message } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.send_user_override(&task_id, message))?;
            Ok(IpcResponse::MessageQueued)
        }
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id,
            capability,
            mode,
            timeout_ms,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            let mode = match mode.as_str() {
                "one" | "any" => yi_agent_core::subagent::supervisor::WaitMode::Any,
                "all" => yi_agent_core::subagent::supervisor::WaitMode::All,
                _ => {
                    return Err(IpcError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "mode must be one, any, or all",
                    )));
                }
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let outcome = if let Some(timeout_ms) = timeout_ms {
                runtime.block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(timeout_ms),
                        coordinator.wait_for_children_authorized(
                            &session_id,
                            &caller_task_id,
                            &capability,
                            mode,
                        ),
                    )
                    .await
                })
            } else {
                Ok(runtime.block_on(coordinator.wait_for_children_authorized(
                    &session_id,
                    &caller_task_id,
                    &capability,
                    mode,
                )))
            };
            let (status, children, reports) = match outcome {
                Err(_) => {
                    let (children, reports) =
                        coordinator.child_completion_snapshot(&session_id, &caller_task_id)?;
                    (
                        "timeout".into(),
                        children
                            .into_iter()
                            .map(|child| child.to_string())
                            .collect(),
                        reports
                            .into_iter()
                            .map(|report| IpcCompletedChildReport {
                                task_id: report.task_id.to_string(),
                                state: report.state,
                                report: report.report,
                                delivery: report.delivery,
                            })
                            .collect(),
                    )
                }
                Ok(outcome) => match outcome? {
                    yi_agent_core::subagent::supervisor::WaitOutcome::NeedsAttention => {
                        ("needs_attention".into(), Vec::new(), Vec::new())
                    }
                    yi_agent_core::subagent::supervisor::WaitOutcome::Completed {
                        children,
                        reports,
                    } => (
                        "completed".into(),
                        children
                            .into_iter()
                            .map(|child| child.to_string())
                            .collect(),
                        reports
                            .into_iter()
                            .map(|report| IpcCompletedChildReport {
                                task_id: report.task_id.to_string(),
                                state: report.state,
                                report: report.report,
                                delivery: report.delivery,
                            })
                            .collect(),
                    ),
                },
            };
            Ok(IpcResponse::WaitCompleted {
                status,
                children,
                reports,
            })
        }
        IpcRequest::InspectTask { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let detail = repository.task_detail(&task_id)?;
            Ok(IpcResponse::TaskDetail(IpcTaskDetail {
                task_id: detail.task_id,
                session_id: detail.session_id,
                parent_task_id: detail.parent_task_id,
                depth: detail.depth,
                state: detail.state,
                delivery_json: detail.delivery_json,
                terminal_json: detail.terminal_json,
                workspace: detail.workspace,
            }))
        }
        IpcRequest::InspectChild {
            session_id,
            caller_task_id,
            capability,
            task_id,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let detail = runtime.block_on(coordinator.inspect_child_authorized(
                &session_id,
                &caller_task_id,
                &capability,
                &task_id,
            ))?;
            Ok(IpcResponse::TaskDetail(IpcTaskDetail {
                task_id: detail.task_id,
                session_id: detail.session_id,
                parent_task_id: detail.parent_task_id,
                depth: detail.depth,
                state: detail.state,
                delivery_json: detail.delivery_json,
                terminal_json: detail.terminal_json,
                workspace: detail.workspace,
            }))
        }
        IpcRequest::CancelChild {
            session_id,
            caller_task_id,
            capability,
            task_id,
            recursive,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let caller_task_id = parse_id::<TaskId>(&caller_task_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.cancel_child_authorized(
                &session_id,
                &caller_task_id,
                &capability,
                &task_id,
                recursive,
            ))?;
            Ok(IpcResponse::TaskCancelled)
        }
        IpcRequest::ListTaskSummaries {
            session_id,
            active_only,
        } => {
            let session_id = session_id
                .as_deref()
                .map(parse_id::<RootSessionId>)
                .transpose()?;
            let tasks = repository
                .task_summaries(session_id.as_ref(), active_only)?
                .into_iter()
                .map(|task| IpcTaskSummary {
                    task_id: task.task_id,
                    state: task.state,
                    is_root: task.is_root,
                })
                .collect();
            Ok(IpcResponse::TaskSummaries { tasks })
        }
        IpcRequest::ReadTaskEvents {
            task_id,
            after_event_id,
        } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let events = repository
                .event_records_for_task_after(&task_id, after_event_id.unwrap_or(0))?
                .into_iter()
                .map(ipc_event)
                .collect();
            Ok(IpcResponse::TaskEvents { events })
        }
        IpcRequest::ReadTaskMailbox { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let messages = repository
                .mailbox_messages_for_task(&task_id)?
                .into_iter()
                .map(|message| IpcMailboxMessage {
                    message_id: message.message_id,
                    recipient_task_id: message.recipient_task_id.to_string(),
                    sender_task_id: message.sender_task_id.map(|task_id| task_id.to_string()),
                    kind: message.kind,
                    priority: message.priority,
                    payload_json: message.payload_json,
                    delivered_at: message.delivered_at,
                    created_at: message.created_at,
                })
                .collect();
            Ok(IpcResponse::TaskMailbox { messages })
        }
        IpcRequest::ReadTaskDiff { task_id } => {
            let task_id = parse_id::<TaskId>(&task_id)?;
            let detail = repository.task_detail(&task_id)?;
            Ok(IpcResponse::TaskDiff {
                task_id: detail.task_id,
                delivery_json: detail.delivery_json,
            })
        }
        IpcRequest::SubscribeEvents {
            after_event_id,
            filters,
        } => {
            let snapshot = repository.subscription_snapshot(after_event_id)?;
            let tasks = snapshot
                .tasks
                .into_iter()
                .map(|task| IpcTask {
                    task_id: task.task_id,
                    state: task.state,
                    workspace: task.workspace,
                })
                .collect();
            let events = snapshot
                .events
                .into_iter()
                .map(ipc_event)
                .filter(|event| filters.matches(event))
                .collect();
            Ok(IpcResponse::Subscription(SubscriptionSnapshot {
                high_water_event_id: snapshot.high_water_event_id,
                tasks,
                events,
            }))
        }
    }
}

fn parse_id<T>(value: &str) -> Result<T, IpcError>
where
    T: FromStr,
{
    value.parse().map_err(|_| {
        IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid ID",
        ))
    })
}

/// Resolves the requested workspace mode for a spawn request. An omitted mode
/// defaults to read-only so an existing client cannot accidentally request a
/// writable worktree; an explicit but unknown value is rejected.
fn parse_workspace_mode(
    mode: Option<String>,
) -> Result<yi_agent_core::TaskWorkspaceMode, IpcError> {
    match mode.as_deref() {
        None => Ok(yi_agent_core::TaskWorkspaceMode::ReadOnly),
        Some(value) => yi_agent_core::TaskWorkspaceMode::parse(value).ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("mode must be 'coding' or 'read_only', got {value}"),
            ))
        }),
    }
}

struct UnavailableWorkerFactory;

impl AgentWorkerFactory for UnavailableWorkerFactory {
    fn is_available(&self) -> bool {
        false
    }

    fn start(
        &self,
        _request: WorkerStart,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async {
            Err(WorkerError::Startup(
                "no application worker factory registered".into(),
            ))
        })
    }
}

#[cfg(test)]
mod socket_path_tests {
    use super::*;

    /// The real project path that produced `AF_UNIX path too long` in practice:
    /// `<repo>/.yi-agent/runtime/runtime.sock` measured 108 bytes.
    const LONG_RUNTIME_DIR: &str = "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.yi-agent/runtime";

    #[test]
    fn a_short_runtime_directory_keeps_the_socket_inside_it() {
        let runtime_dir = Path::new("/tmp/project/.yi-agent/runtime");

        let socket = socket_path_for(runtime_dir).expect("short paths resolve");

        assert_eq!(
            socket,
            runtime_dir.join("runtime.sock"),
            "a runtime directory that fits must keep its existing layout"
        );
    }

    #[test]
    fn a_long_runtime_directory_yields_a_socket_that_fits_the_platform_limit() {
        let runtime_dir = Path::new(LONG_RUNTIME_DIR);
        assert!(
            runtime_dir.join("runtime.sock").as_os_str().len() > MAX_SOCKET_PATH_BYTES,
            "precondition: the direct socket path must overflow the limit"
        );

        let socket = socket_path_for(runtime_dir).expect("long paths fall back, not fail");

        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
            "socket must fit sockaddr_un.sun_path, got {} bytes: {}",
            socket.as_os_str().len(),
            socket.display()
        );
    }

    #[test]
    fn the_fallback_is_deterministic_for_one_runtime_directory() {
        let runtime_dir = Path::new(LONG_RUNTIME_DIR);

        let first = socket_path_for(runtime_dir).expect("resolves");
        let second = socket_path_for(runtime_dir).expect("resolves");

        assert_eq!(
            first, second,
            "the daemon and every client must resolve the identical socket"
        );
    }

    #[test]
    fn distinct_runtime_directories_get_distinct_sockets() {
        let one = socket_path_for(Path::new(LONG_RUNTIME_DIR)).expect("resolves");
        let other =
            socket_path_for(Path::new(&format!("{LONG_RUNTIME_DIR}-other"))).expect("resolves");

        assert_ne!(
            one, other,
            "project isolation must survive the fallback: different projects need different sockets"
        );
    }

    #[test]
    fn a_shared_runtime_directory_resolves_to_one_socket() {
        // An explicitly shared runtime directory must still yield a single socket,
        // so two projects pointed at it attach to the same daemon.
        let shared = Path::new(
            "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/shared-runtime-dir/runtime",
        );

        let first = socket_path_for(shared).expect("resolves");
        let second = socket_path_for(shared).expect("resolves");

        assert_eq!(first, second);
    }
}
