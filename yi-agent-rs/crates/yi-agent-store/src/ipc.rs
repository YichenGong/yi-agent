use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::repository::RuntimeRepository;

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Repository(#[from] crate::repository::RepositoryError),
    #[error("a daemon is already running for {path}")]
    AlreadyRunning { path: PathBuf },
    #[error("IPC frame exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("daemon listener thread panicked during shutdown")]
    ListenerPanicked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RequestEnvelope {
    protocol_version: u32,
    request: IpcRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    Status,
    Stop,
    SubscribeEvents { after_event_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    Status {
        high_water_event_id: i64,
    },
    Stopping,
    Subscription(SubscriptionSnapshot),
    Event(IpcEvent),
    ResyncRequired,
    UnsupportedProtocol {
        supported_min: u32,
        supported_max: u32,
    },
    Error {
        code: String,
    },
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
pub struct IpcEvent {
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
}

/// A client-side event stream. The first frame is always a `Subscription` snapshot.
pub struct Subscription {
    reader: BufReader<UnixStream>,
}

impl Subscription {
    pub fn next_response(&mut self) -> Result<IpcResponse, IpcError> {
        let response = read_limited_frame(&mut self.reader)?.ok_or_else(|| {
            IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed subscription",
            ))
        })?;
        Ok(serde_json::from_slice(&response)?)
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
        let runtime_dir = runtime_dir.as_ref();
        fs::create_dir_all(runtime_dir)?;
        fs::set_permissions(runtime_dir, fs::Permissions::from_mode(0o700))?;

        let socket_path = runtime_dir.join("runtime.sock");
        let lock_path = runtime_dir.join("runtime.lock");
        let mut lock = acquire_lock(runtime_dir, &lock_path, &socket_path)?;
        writeln!(lock, "{}", std::process::id())?;
        let mut repository = RuntimeRepository::open(database_path.as_ref())?;
        repository.recover_inflight_tasks()?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let database_path = database_path.as_ref().to_path_buf();
        let cleanup_socket = socket_path.clone();
        let cleanup_lock = lock_path.clone();
        let listener = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let database_path = database_path.clone();
                        let stop = Arc::clone(&thread_stop);
                        thread::spawn(move || {
                            let _ = handle_client(stream, &database_path, &stop);
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
    write_request(&mut stream, protocol_version, request)?;
    let response = read_limited_frame(&mut BufReader::new(stream))?.ok_or_else(|| {
        IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed request connection",
        ))
    })?;
    Ok(serde_json::from_slice(&response)?)
}

pub fn subscribe(
    socket_path: impl AsRef<Path>,
    after_event_id: i64,
) -> Result<Subscription, IpcError> {
    let mut stream = UnixStream::connect(socket_path)?;
    write_request(
        &mut stream,
        PROTOCOL_VERSION,
        IpcRequest::SubscribeEvents { after_event_id },
    )?;
    Ok(Subscription {
        reader: BufReader::new(stream),
    })
}

fn write_request(
    stream: &mut UnixStream,
    protocol_version: u32,
    request: IpcRequest,
) -> Result<(), IpcError> {
    let frame = serde_json::to_vec(&RequestEnvelope {
        protocol_version,
        request,
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
    stop: &AtomicBool,
) -> Result<(), IpcError> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let frame = match read_limited_frame(&mut BufReader::new(stream.try_clone()?)) {
        Err(IpcError::FrameTooLarge) => {
            return write_response(
                &mut stream,
                &IpcResponse::Error {
                    code: "frame_too_large".into(),
                },
            );
        }
        Err(error) => return Err(error),
        Ok(None) => return Ok(()),
        Ok(Some(frame)) => frame,
    };
    let response = match serde_json::from_slice::<RequestEnvelope>(&frame) {
        Ok(envelope) if envelope.protocol_version != PROTOCOL_VERSION => {
            IpcResponse::UnsupportedProtocol {
                supported_min: PROTOCOL_VERSION,
                supported_max: PROTOCOL_VERSION,
            }
        }
        Ok(envelope) => match envelope.request {
            IpcRequest::Stop => {
                stop.store(true, Ordering::Release);
                IpcResponse::Stopping
            }
            IpcRequest::SubscribeEvents { after_event_id } => {
                return stream_subscription(&mut stream, database_path, stop, after_event_id);
            }
            request => match respond(database_path, request) {
                Ok(response) => response,
                Err(_) => IpcResponse::Error {
                    code: "internal".into(),
                },
            },
        },
        Err(_) => IpcResponse::Error {
            code: "invalid_request".into(),
        },
    };
    write_response(&mut stream, &response)
}

fn stream_subscription(
    stream: &mut UnixStream,
    database_path: &Path,
    stop: &AtomicBool,
    after_event_id: i64,
) -> Result<(), IpcError> {
    let mut repository = RuntimeRepository::open(database_path)?;
    let snapshot = repository.subscription_snapshot(after_event_id)?;
    let mut cursor = snapshot.high_water_event_id;
    write_response(
        stream,
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
            events: snapshot.events.into_iter().map(ipc_event).collect(),
        }),
    )?;
    while !stop.load(Ordering::Acquire) {
        let repository = RuntimeRepository::open(database_path)?;
        let events = repository.event_records_after(cursor)?;
        for event in events {
            cursor = event.id;
            write_response(stream, &IpcResponse::Event(ipc_event(event)))?;
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn write_response(stream: &mut UnixStream, response: &IpcResponse) -> Result<(), IpcError> {
    let frame = serde_json::to_vec(response)?;
    let frame = if frame.len() <= MAX_FRAME_BYTES {
        frame
    } else {
        serde_json::to_vec(&IpcResponse::ResyncRequired)?
    };
    stream.write_all(&frame)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
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
        kind: format!("{:?}", event.event),
    }
}

fn respond(database_path: &Path, request: IpcRequest) -> Result<IpcResponse, IpcError> {
    let mut repository = RuntimeRepository::open(database_path)?;
    match request {
        IpcRequest::Status => Ok(IpcResponse::Status {
            high_water_event_id: repository.latest_event_id()?,
        }),
        IpcRequest::Stop => Ok(IpcResponse::Stopping),
        IpcRequest::SubscribeEvents { after_event_id } => {
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
                .map(|event| IpcEvent {
                    event_id: event.id,
                    task_id: event.task_id.to_string(),
                    kind: format!("{:?}", event.event),
                })
                .collect();
            Ok(IpcResponse::Subscription(SubscriptionSnapshot {
                high_water_event_id: snapshot.high_water_event_id,
                tasks,
                events,
            }))
        }
    }
}
