use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use yi_agent_core::subagent::task::{RootSessionId, TaskId};
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};

use crate::repository::RuntimeRepository;
use crate::runtime::{RuntimeCoordinator, RuntimeCoordinatorError};

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_PENDING_EVENT_FRAMES: usize = 1024;
// Invalid JSON has no trustworthy request ID to echo, so its error frame uses
// this documented stable empty identifier.
const MISSING_REQUEST_ID: &str = "";
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

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
pub enum IpcRequest {
    Status,
    Stop,
    CreateSession,
    SpawnChild {
        session_id: String,
        parent_task_id: String,
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
    SendMessage {
        session_id: String,
        sender_task_id: String,
        worker_capability: String,
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
        mode: String,
    },
    InspectTask {
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
    TaskSpawned {
        task_id: String,
    },
    TaskStarted,
    TaskCancelled,
    TaskRetried,
    TaskPaused,
    TaskResumed,
    MessageQueued,
    WaitCompleted {
        status: String,
        children: Vec<String>,
    },
    TaskDetail(IpcTaskDetail),
    Subscription(SubscriptionSnapshot),
    Event(IpcEvent),
    ResyncRequired,
    UnsupportedProtocol {
        supported_min: u32,
        supported_max: u32,
    },
    Error {
        code: IpcErrorCode,
    },
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTaskDetail {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub depth: u8,
    pub state: String,
    pub delivery_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEvent {
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
}

/// The stable wire payload for a top-level subscription event frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcEventPayload {
    pub task_id: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl From<&IpcEvent> for IpcEventPayload {
    fn from(event: &IpcEvent) -> Self {
        Self {
            task_id: event.task_id.clone(),
            kind: event.kind.clone(),
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
            while !thread_stop.load(Ordering::Acquire) {
                let _ = runtime.block_on(coordinator.reconcile_worker_events());
                match listener.accept() {
                    Ok((stream, _)) => {
                        let database_path = database_path.clone();
                        let stop = Arc::clone(&thread_stop);
                        let coordinator = Arc::clone(&coordinator);
                        thread::spawn(move || {
                            let _ = handle_client(stream, &database_path, &stop, &coordinator);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = remove_if_exists(&cleanup_socket);
            let _ = remove_if_exists(&cleanup_lock);
        });
        Ok(Self {
            socket_path,
            stop,
            listener: Some(listener),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stop this manually started daemon and release its local runtime files.
    pub fn stop(&mut self) -> Result<(), IpcError> {
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
        let _ = self.stop();
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
                stop.store(true, Ordering::Release);
                IpcResponse::Stopping
            }
            IpcRequest::SubscribeEvents {
                after_event_id,
                filters,
            } => {
                return match stream_subscription(
                    &mut stream,
                    database_path,
                    Arc::clone(stop),
                    Arc::clone(coordinator),
                    after_event_id,
                    filters,
                    &envelope.request_id,
                ) {
                    Ok(()) => Ok(()),
                    Err(_) => write_response_frame(
                        &mut stream,
                        &envelope.request_id,
                        None,
                        &IpcResponse::Error {
                            code: IpcErrorCode::Internal,
                        },
                    ),
                };
            }
            request => match respond(database_path, coordinator, request) {
                Ok(response) => response,
                Err(error) => error_response(&error),
            },
        },
        Err(_) => IpcResponse::Error {
            code: IpcErrorCode::Validation,
        },
    };
    write_response_frame(&mut stream, &request_id, None, &response)
}

fn stream_subscription(
    stream: &mut UnixStream,
    database_path: &Path,
    stop: Arc<AtomicBool>,
    coordinator: Arc<RuntimeCoordinator>,
    after_event_id: i64,
    filters: SubscriptionFilters,
    request_id: &str,
) -> Result<(), IpcError> {
    let mut repository = RuntimeRepository::open(database_path)?;
    let snapshot = repository.subscription_snapshot(after_event_id)?;
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
                })
                .collect(),
            events: snapshot
                .events
                .into_iter()
                .map(ipc_event)
                .filter(|event| filters.matches(event))
                .collect(),
        }),
    )?;

    let pending = Arc::new(PendingSubscriptionFrames::new(
        request_id,
        MAX_PENDING_EVENT_FRAMES,
    ));
    let subscription_stop = Arc::new(AtomicBool::new(false));
    let producer_pending = Arc::clone(&pending);
    let producer_stop = Arc::clone(&subscription_stop);
    let database_path = database_path.to_path_buf();
    let producer = thread::spawn(move || -> Result<(), IpcError> {
        let result = (|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            while !stop.load(Ordering::Acquire) && !producer_stop.load(Ordering::Acquire) {
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

    let write_result = loop {
        match pending.pop_wait() {
            Some(envelope) => {
                if let Err(error) = write_envelope_frame(stream, &envelope) {
                    break Err(error);
                }
            }
            None => break Ok(()),
        }
    };
    subscription_stop.store(true, Ordering::Release);
    pending.close();
    let producer_result = producer.join().map_err(|_| {
        IpcError::Io(std::io::Error::other(
            "subscription producer thread panicked",
        ))
    })?;
    write_result?;
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
    closed: bool,
}

impl PendingSubscriptionFrames {
    fn new(request_id: impl Into<String>, capacity: usize) -> Self {
        assert!(capacity > 0, "subscription queue capacity must be positive");
        Self {
            request_id: request_id.into(),
            capacity,
            state: Mutex::new(PendingSubscriptionState {
                frames: VecDeque::new(),
                closed: false,
            }),
            available: Condvar::new(),
        }
    }

    /// Returns false once this event caused overflow or the subscription was closed.
    fn push_event(&self, event: IpcEvent) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }
        if state.frames.len() == self.capacity {
            state.frames.clear();
            state.frames.push_back(response_envelope(
                &self.request_id,
                None,
                IpcResponse::ResyncRequired,
            ));
            state.closed = true;
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
        while state.frames.is_empty() && !state.closed {
            state = self.available.wait(state).unwrap();
        }
        state.frames.pop_front()
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        self.available.notify_all();
    }

    #[cfg(test)]
    fn drain_for_test(&self) -> Vec<ResponseEnvelope> {
        self.state.lock().unwrap().frames.drain(..).collect()
    }

    #[cfg(test)]
    fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
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
    let frame = serde_json::to_vec(&envelope)?;
    let frame = if frame.len() <= MAX_FRAME_BYTES {
        frame
    } else {
        serde_json::to_vec(&ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: envelope.request_id.clone(),
            event_id: None,
            event: None,
            result: IpcResponse::ResyncRequired,
        })?
    };
    stream.write_all(&frame)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
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

    fn event(event_id: i64) -> IpcEvent {
        IpcEvent {
            event_id,
            task_id: format!("task-{event_id}"),
            kind: "task_started".into(),
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
        | crate::repository::RepositoryError::MailboxMessageNotFound { .. } => {
            IpcErrorCode::NotFound
        }
        crate::repository::RepositoryError::Sql(_)
        | crate::repository::RepositoryError::Json(_)
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
    }
}

fn runtime_event_name(event: crate::repository::RuntimeEvent) -> &'static str {
    match event {
        crate::repository::RuntimeEvent::TaskQueued => "task_queued",
        crate::repository::RuntimeEvent::TaskStarted => "task_started",
        crate::repository::RuntimeEvent::TaskCancelled => "task_cancelled",
        crate::repository::RuntimeEvent::TaskPauseRequested => "task_pause_requested",
        crate::repository::RuntimeEvent::TaskPaused => "task_paused",
        crate::repository::RuntimeEvent::TaskFailed => "task_failed",
        crate::repository::RuntimeEvent::TaskRecoveryRequired => "task_recovery_required",
        crate::repository::RuntimeEvent::MailboxMessageQueued => "mailbox_message_queued",
        crate::repository::RuntimeEvent::MailboxMessageConsumed => "mailbox_message_consumed",
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
        IpcRequest::CancelTask {
            session_id,
            task_id,
            recursive,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let task_id = parse_id::<TaskId>(&task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.cancel_task(&session_id, &task_id, recursive))?;
            Ok(IpcResponse::TaskCancelled)
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
            mode,
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
            let outcome = runtime.block_on(coordinator.wait_for_children(
                &session_id,
                &caller_task_id,
                mode,
            ))?;
            let (status, children) = match outcome {
                yi_agent_core::subagent::supervisor::WaitOutcome::NeedsAttention => {
                    ("needs_attention".into(), Vec::new())
                }
                yi_agent_core::subagent::supervisor::WaitOutcome::Completed(children) => (
                    "completed".into(),
                    children
                        .into_iter()
                        .map(|child| child.to_string())
                        .collect(),
                ),
            };
            Ok(IpcResponse::WaitCompleted { status, children })
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
            }))
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
