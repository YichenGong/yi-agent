use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RequestEnvelope {
    protocol_version: u32,
    request: IpcRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    Status,
    SubscribeEvents { after_event_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    Status {
        high_water_event_id: i64,
    },
    Subscription(SubscriptionSnapshot),
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

/// A manually owned, current-user-only local daemon socket.
pub struct Daemon {
    socket_path: PathBuf,
    lock_path: PathBuf,
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

        let lock_path = runtime_dir.join("runtime.lock");
        let mut lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    IpcError::AlreadyRunning {
                        path: runtime_dir.to_path_buf(),
                    }
                } else {
                    IpcError::Io(error)
                }
            })?;
        writeln!(lock, "{}", std::process::id())?;

        let socket_path = runtime_dir.join("runtime.sock");
        if socket_path.exists() {
            let _ = fs::remove_file(&lock_path);
            return Err(IpcError::AlreadyRunning {
                path: runtime_dir.to_path_buf(),
            });
        }
        let mut repository = RuntimeRepository::open(database_path.as_ref())?;
        repository.recover_inflight_tasks()?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let database_path = database_path.as_ref().to_path_buf();
        let listener = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_client(stream, &database_path);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            socket_path,
            lock_path,
            stop,
            listener: Some(listener),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the nonblocking accept loop so shutdown does not wait for its sleep interval.
        let _ = UnixStream::connect(&self.socket_path);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.lock_path);
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
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response)?;
    Ok(serde_json::from_str(&response)?)
}

fn handle_client(mut stream: UnixStream, database_path: &Path) -> Result<(), IpcError> {
    let mut frame = String::new();
    let bytes = BufReader::new(stream.try_clone()?).read_line(&mut frame)?;
    let response = if bytes > MAX_FRAME_BYTES {
        IpcResponse::Error {
            code: "frame_too_large".into(),
        }
    } else {
        match serde_json::from_str::<RequestEnvelope>(&frame) {
            Ok(envelope) if envelope.protocol_version != PROTOCOL_VERSION => {
                IpcResponse::UnsupportedProtocol {
                    supported_min: PROTOCOL_VERSION,
                    supported_max: PROTOCOL_VERSION,
                }
            }
            Ok(envelope) => match respond(database_path, envelope.request) {
                Ok(response) => response,
                Err(_) => IpcResponse::Error {
                    code: "internal".into(),
                },
            },
            Err(_) => IpcResponse::Error {
                code: "invalid_request".into(),
            },
        }
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn respond(database_path: &Path, request: IpcRequest) -> Result<IpcResponse, IpcError> {
    let repository = RuntimeRepository::open(database_path)?;
    let high_water_event_id = repository.latest_event_id()?;
    match request {
        IpcRequest::Status => Ok(IpcResponse::Status {
            high_water_event_id,
        }),
        IpcRequest::SubscribeEvents { after_event_id } => {
            let tasks = repository
                .task_snapshots()?
                .into_iter()
                .map(|task| IpcTask {
                    task_id: task.task_id,
                    state: task.state,
                })
                .collect();
            let events = repository
                .event_records_after(after_event_id)?
                .into_iter()
                .map(|event| IpcEvent {
                    event_id: event.id,
                    task_id: event.task_id.to_string(),
                    kind: format!("{:?}", event.event),
                })
                .collect();
            Ok(IpcResponse::Subscription(SubscriptionSnapshot {
                high_water_event_id,
                tasks,
                events,
            }))
        }
    }
}
