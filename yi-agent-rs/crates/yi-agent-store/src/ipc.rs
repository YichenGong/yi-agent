use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
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
    #[error("daemon listener thread panicked during shutdown")]
    ListenerPanicked,
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
    },
    SpawnApplicationChild {
        session_id: String,
        parent_task_id: String,
        capability: String,
        objective: String,
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

        let socket_path = runtime_dir.join("runtime.sock");
        let lock_path = runtime_dir.join("runtime.lock");
        let mut lock = acquire_lock(runtime_dir, &lock_path, &socket_path)?;
        writeln!(lock, "{}", std::process::id())?;
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

fn acquire_lock(
    runtime_dir: &Path,
    lock_path: &Path,
    socket_path: &Path,
) -> Result<std::fs::File, IpcError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(lock) => Ok(lock),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if lock_owner_is_alive(lock_path) || UnixStream::connect(socket_path).is_ok() {
                return Err(IpcError::AlreadyRunning {
                    path: runtime_dir.to_path_buf(),
                });
            }
            // Both checks failed, so this is a stale local runtime left by a dead process.
            remove_if_exists(lock_path)?;
            remove_if_exists(socket_path)?;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(lock_path)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        IpcError::AlreadyRunning {
                            path: runtime_dir.to_path_buf(),
                        }
                    } else {
                        IpcError::Io(error)
                    }
                })
        }
        Err(error) => Err(IpcError::Io(error)),
    }
}

fn lock_owner_is_alive(lock_path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(lock_path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|output| output.status.success())
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
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
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
    stream.write_all(&frame)?;
    stream.write_all(b"\n")?;
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
        IpcError::Json(_) | IpcError::FrameTooLarge => IpcErrorCode::Validation,
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
    if frame.len() > MAX_FRAME_BYTES || !frame.ends_with(b"\n") {
        return Err(IpcError::FrameTooLarge);
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
    coordinator: &RuntimeCoordinator,
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
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let task_id = runtime.block_on(coordinator.spawn_child_and_admit(
                &session_id,
                &parent_task_id,
                objective,
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
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let task_id = runtime.block_on(coordinator.spawn_application_child(
                &session_id,
                &parent_task_id,
                &capability,
                objective,
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
