use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::time;

use crate::sandbox::{SandboxMode, SandboxPolicy};
use crate::shell::blocklist;

pub const DEFAULT_STREAM_CAP_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExitPolicy {
    #[default]
    Kill,
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessStatus {
    Starting,
    Running,
    Ready,
    Exited { code: Option<i32> },
    Killed,
    FailedToStart { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedProcessSnapshot {
    pub process_id: String,
    pub name: Option<String>,
    pub pid: Option<u32>,
    pub command: String,
    pub cwd: String,
    pub status: ProcessStatus,
    pub ready: bool,
    pub on_exit: OnExitPolicy,
    pub exit_code: Option<i32>,
    pub elapsed_sec: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessReadResult {
    pub process_id: String,
    pub name: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub next_cursor: u64,
    pub truncated: bool,
    pub status: ProcessStatus,
    pub ready: bool,
}

#[derive(Debug, Clone)]
struct StreamRingBuffer {
    cap: usize,
    chunks: Vec<(u64, Vec<u8>)>,
    next_cursor: u64,
}

impl StreamRingBuffer {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            chunks: Vec::new(),
            next_cursor: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let start = self.next_cursor;
        self.next_cursor = self.next_cursor.saturating_add(bytes.len() as u64);
        self.chunks.push((start, bytes.to_vec()));
        self.trim_to_cap();
    }

    fn next_cursor(&self) -> u64 {
        self.next_cursor
    }

    fn earliest_cursor(&self) -> u64 {
        self.chunks
            .first()
            .map(|(start, _)| *start)
            .unwrap_or(self.next_cursor)
    }

    fn read(&self, cursor: Option<u64>, max_bytes: usize) -> (String, u64, bool) {
        let earliest = self.earliest_cursor();
        let requested = cursor.unwrap_or(earliest);
        let truncated = requested < earliest;
        let effective_cursor = requested.max(earliest);

        let mut out = Vec::new();
        for (start, bytes) in &self.chunks {
            let end = start.saturating_add(bytes.len() as u64);
            if end <= effective_cursor {
                continue;
            }
            let offset = effective_cursor.saturating_sub(*start) as usize;
            if offset < bytes.len() {
                out.extend_from_slice(&bytes[offset..]);
            }
        }

        if out.len() > max_bytes {
            let keep_from = out.len() - max_bytes;
            out.drain(..keep_from);
        }

        (
            String::from_utf8_lossy(&out).into_owned(),
            self.next_cursor,
            truncated,
        )
    }

    fn trim_to_cap(&mut self) {
        let mut total: usize = self.chunks.iter().map(|(_, bytes)| bytes.len()).sum();
        while total > self.cap {
            let excess = total - self.cap;
            let Some((start, bytes)) = self.chunks.first_mut() else {
                break;
            };
            if excess > bytes.len() {
                total -= bytes.len();
                self.chunks.remove(0);
            } else {
                bytes.drain(..excess);
                *start = start.saturating_add(excess as u64);
                total -= excess;
            }
        }
    }
}

pub struct ProcessStartOptions {
    pub command: String,
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub on_exit: OnExitPolicy,
    pub ready_pattern: Option<String>,
    pub ready_timeout_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessStartResult {
    pub process_id: String,
    pub name: Option<String>,
    pub pid: Option<u32>,
    pub status: ProcessStatus,
    pub ready: bool,
    pub next_cursor: u64,
    pub start_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessSelector {
    Id(String),
    Name(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessEvent {
    Started {
        process_id: String,
    },
    Output {
        process_id: String,
    },
    Ready {
        process_id: String,
    },
    Exited {
        process_id: String,
        code: Option<i32>,
    },
    Killed {
        process_id: String,
    },
}

struct ManagedProcess {
    process_id: String,
    name: Option<String>,
    pid: Option<u32>,
    command: String,
    cwd: PathBuf,
    status: ProcessStatus,
    ready: bool,
    on_exit: OnExitPolicy,
    exit_code: Option<i32>,
    start_time: Instant,
    end_time: Option<Instant>,
    stdout: StreamRingBuffer,
    stderr: StreamRingBuffer,
    child: Option<Child>,
}

pub struct ProcessManager {
    root: PathBuf,
    sandbox: SandboxPolicy,
    next_id: AtomicU64,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    events: broadcast::Sender<ProcessEvent>,
}

impl ProcessManager {
    pub fn new(root: PathBuf) -> Arc<Self> {
        let sandbox = SandboxPolicy::new(SandboxMode::DangerFullAccess, &root, Vec::new());
        Self::with_sandbox(root, sandbox)
    }

    pub fn with_sandbox(root: PathBuf, sandbox: SandboxPolicy) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            root,
            sandbox,
            next_id: AtomicU64::new(1),
            processes: Arc::new(Mutex::new(HashMap::new())),
            events,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProcessEvent> {
        self.events.subscribe()
    }

    pub async fn start(
        self: &Arc<Self>,
        opts: ProcessStartOptions,
    ) -> Result<ProcessStartResult, String> {
        let command = opts.command.trim();
        if command.is_empty() {
            return Err("process command cannot be empty".into());
        }
        if let Some(reason) = blocklist::is_blocked(command) {
            return Err(format!("blocked command: {reason}"));
        }
        if opts.name.as_deref().is_some_and(str::is_empty) {
            return Err("process name cannot be empty".into());
        }

        let cwd = self.resolve_cwd(opts.cwd.as_ref())?;
        {
            let processes = self.lock_processes();
            if processes.len() >= 16 {
                return Err("maximum managed process count reached".into());
            }
            if let Some(name) = &opts.name {
                if processes
                    .values()
                    .any(|process| process.name.as_ref() == Some(name))
                {
                    return Err("process name already exists".into());
                }
            }
        }

        let (program, args) = self
            .sandbox
            .command(command, &cwd)
            .map_err(|err| err.to_string())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        cmd.envs(opts.env.iter());
        configure_process_group(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|err| format!("failed to start process: {err}"))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let process_id = self.next_process_id();
        let name = opts.name.clone();
        let on_exit = opts.on_exit;

        {
            let mut processes = self.lock_processes();
            processes.insert(
                process_id.clone(),
                ManagedProcess {
                    process_id: process_id.clone(),
                    name: name.clone(),
                    pid,
                    command: opts.command,
                    cwd,
                    status: ProcessStatus::Running,
                    ready: false,
                    on_exit,
                    exit_code: None,
                    start_time: Instant::now(),
                    end_time: None,
                    stdout: StreamRingBuffer::new(DEFAULT_STREAM_CAP_BYTES),
                    stderr: StreamRingBuffer::new(DEFAULT_STREAM_CAP_BYTES),
                    child: Some(child),
                },
            );
        }

        self.spawn_reader(process_id.clone(), StreamKind::Stdout, stdout);
        self.spawn_reader(process_id.clone(), StreamKind::Stderr, stderr);
        self.spawn_waiter(process_id.clone());
        self.emit(ProcessEvent::Started {
            process_id: process_id.clone(),
        });

        let mut start_warning = None;
        if let Some(pattern) = opts.ready_pattern.as_deref() {
            let timeout = Duration::from_secs(opts.ready_timeout_sec.unwrap_or(10));
            if self.wait_for_ready(&process_id, pattern, timeout).await {
                self.mark_ready_if_running(&process_id);
            } else if self.is_running(&process_id) {
                start_warning = Some(format!("readiness timeout after {}s", timeout.as_secs()));
            }
        }

        let snapshot = self.snapshot(&process_id)?;
        Ok(ProcessStartResult {
            process_id: snapshot.process_id,
            name: snapshot.name,
            pid: snapshot.pid,
            status: snapshot.status,
            ready: snapshot.ready,
            next_cursor: self.max_cursor(&process_id).unwrap_or(0),
            start_warning,
        })
    }

    pub fn list(&self) -> Vec<ManagedProcessSnapshot> {
        let processes = self.lock_processes();
        let mut snapshots = processes
            .values()
            .map(Self::snapshot_process)
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| process_id_sort_key(&snapshot.process_id));
        snapshots
    }

    pub async fn read(
        &self,
        selector: ProcessSelector,
        cursor: Option<u64>,
        max_bytes: usize,
    ) -> Result<ProcessReadResult, String> {
        let process_id = self.selector_to_id(&selector)?;
        let processes = self.lock_processes();
        let process = processes
            .get(&process_id)
            .ok_or_else(|| format!("process not found: {process_id}"))?;
        let (stdout, stdout_cursor, stdout_truncated) = process.stdout.read(cursor, max_bytes);
        let (stderr, stderr_cursor, stderr_truncated) = process.stderr.read(cursor, max_bytes);
        Ok(ProcessReadResult {
            process_id: process.process_id.clone(),
            name: process.name.clone(),
            stdout,
            stderr,
            next_cursor: stdout_cursor.max(stderr_cursor),
            truncated: stdout_truncated || stderr_truncated,
            status: process.status.clone(),
            ready: process.ready,
        })
    }

    pub async fn kill(&self, selector: ProcessSelector) -> Result<(), String> {
        let process_id = self.selector_to_id(&selector)?;
        let pid = {
            let mut processes = self.lock_processes();
            let process = processes
                .get_mut(&process_id)
                .ok_or_else(|| format!("process not found: {process_id}"))?;
            let pid = process.pid;
            if !matches!(process.status, ProcessStatus::Killed) {
                process.status = ProcessStatus::Killed;
                process.ready = false;
                process.end_time = Some(Instant::now());
            }
            if let Some(child) = process.child.as_mut() {
                let _ = child.start_kill();
            }
            pid
        };
        if let Some(pid) = pid {
            kill_process_group(pid);
        }
        self.emit(ProcessEvent::Killed { process_id });
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<Vec<ManagedProcessSnapshot>, String> {
        let ids = {
            let processes = self.lock_processes();
            processes
                .values()
                .filter(|process| process.on_exit == OnExitPolicy::Kill)
                .map(|process| process.process_id.clone())
                .collect::<Vec<_>>()
        };
        for process_id in ids {
            self.kill(ProcessSelector::Id(process_id)).await?;
        }
        let kept = {
            let processes = self.lock_processes();
            let mut kept = processes
                .values()
                .filter(|process| process.on_exit == OnExitPolicy::Keep)
                .map(Self::snapshot_process)
                .collect::<Vec<_>>();
            kept.sort_by_key(|snapshot| process_id_sort_key(&snapshot.process_id));
            kept
        };
        Ok(kept)
    }

    fn next_process_id(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("proc_{id}")
    }

    fn resolve_cwd(&self, cwd: Option<&PathBuf>) -> Result<PathBuf, String> {
        let path = match cwd {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => self.root.join(path),
            None => self.root.clone(),
        };
        let canonical = path
            .canonicalize()
            .map_err(|err| format!("invalid cwd {}: {err}", path.display()))?;
        if !canonical.is_dir() {
            return Err(format!("cwd is not a directory: {}", canonical.display()));
        }
        Ok(canonical)
    }

    fn selector_to_id(&self, selector: &ProcessSelector) -> Result<String, String> {
        match selector {
            ProcessSelector::Id(process_id) => {
                if self.lock_processes().contains_key(process_id) {
                    Ok(process_id.clone())
                } else {
                    Err(format!("process not found: {process_id}"))
                }
            }
            ProcessSelector::Name(name) => self
                .lock_processes()
                .values()
                .find(|process| process.name.as_deref() == Some(name.as_str()))
                .map(|process| process.process_id.clone())
                .ok_or_else(|| format!("process not found: {name}")),
        }
    }

    fn snapshot(&self, process_id: &str) -> Result<ManagedProcessSnapshot, String> {
        let processes = self.lock_processes();
        processes
            .get(process_id)
            .map(Self::snapshot_process)
            .ok_or_else(|| format!("process not found: {process_id}"))
    }

    fn snapshot_process(process: &ManagedProcess) -> ManagedProcessSnapshot {
        let elapsed_end = process.end_time.unwrap_or_else(Instant::now);
        ManagedProcessSnapshot {
            process_id: process.process_id.clone(),
            name: process.name.clone(),
            pid: process.pid,
            command: process.command.clone(),
            cwd: process.cwd.to_string_lossy().into_owned(),
            status: process.status.clone(),
            ready: process.ready,
            on_exit: process.on_exit,
            exit_code: process.exit_code,
            elapsed_sec: elapsed_end.duration_since(process.start_time).as_secs_f32(),
        }
    }

    fn emit(&self, event: ProcessEvent) {
        let _ = self.events.send(event);
    }

    fn spawn_reader(
        self: &Arc<Self>,
        process_id: String,
        stream: StreamKind,
        reader: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    ) {
        let Some(mut reader) = reader else {
            return;
        };
        let manager = self.clone();
        tokio::spawn(async move {
            let mut buf = [0_u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => manager.append_output(&process_id, stream, &buf[..n]),
                    Err(_) => break,
                }
            }
        });
    }

    fn append_output(&self, process_id: &str, stream: StreamKind, bytes: &[u8]) {
        {
            let mut processes = self.lock_processes();
            let Some(process) = processes.get_mut(process_id) else {
                return;
            };
            match stream {
                StreamKind::Stdout => process.stdout.push(bytes),
                StreamKind::Stderr => process.stderr.push(bytes),
            }
        }
        self.emit(ProcessEvent::Output {
            process_id: process_id.to_owned(),
        });
    }

    fn spawn_waiter(self: &Arc<Self>, process_id: String) {
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                let event = {
                    let mut processes = manager.lock_processes();
                    let Some(process) = processes.get_mut(&process_id) else {
                        return;
                    };
                    match process.child.as_mut() {
                        Some(child) => match child.try_wait() {
                            Ok(Some(status)) => {
                                process.child = None;
                                process.ready = false;
                                process.end_time.get_or_insert_with(Instant::now);
                                if matches!(process.status, ProcessStatus::Killed) {
                                    Some(ProcessEvent::Killed {
                                        process_id: process_id.clone(),
                                    })
                                } else {
                                    let code = status.code();
                                    process.status = ProcessStatus::Exited { code };
                                    process.exit_code = code;
                                    Some(ProcessEvent::Exited {
                                        process_id: process_id.clone(),
                                        code,
                                    })
                                }
                            }
                            Ok(None) => None,
                            Err(_) => {
                                process.child = None;
                                process.ready = false;
                                process.end_time.get_or_insert_with(Instant::now);
                                if matches!(process.status, ProcessStatus::Killed) {
                                    Some(ProcessEvent::Killed {
                                        process_id: process_id.clone(),
                                    })
                                } else {
                                    process.status = ProcessStatus::Exited { code: None };
                                    process.exit_code = None;
                                    Some(ProcessEvent::Exited {
                                        process_id: process_id.clone(),
                                        code: None,
                                    })
                                }
                            }
                        },
                        None => {
                            if is_terminal(&process.status) {
                                return;
                            }
                            None
                        }
                    }
                };
                if let Some(event) = event {
                    manager.emit(event);
                    return;
                }
                time::sleep(Duration::from_millis(50)).await;
            }
        });
    }

    async fn wait_for_ready(&self, process_id: &str, pattern: &str, timeout: Duration) -> bool {
        if self.output_contains(process_id, pattern) {
            return true;
        }
        let deadline = time::Instant::now() + timeout;
        let mut events = self.subscribe();
        loop {
            let now = time::Instant::now();
            if now >= deadline {
                return self.output_contains(process_id, pattern);
            }
            match time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(event)) => match event {
                    ProcessEvent::Output {
                        process_id: event_id,
                    }
                    | ProcessEvent::Started {
                        process_id: event_id,
                    }
                    | ProcessEvent::Ready {
                        process_id: event_id,
                    } => {
                        if event_id == process_id && self.output_contains(process_id, pattern) {
                            return true;
                        }
                    }
                    ProcessEvent::Exited {
                        process_id: event_id,
                        ..
                    }
                    | ProcessEvent::Killed {
                        process_id: event_id,
                    } => {
                        if event_id == process_id {
                            return self.output_contains(process_id, pattern);
                        }
                    }
                },
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    if self.output_contains(process_id, pattern) {
                        return true;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    return self.output_contains(process_id, pattern);
                }
            }
        }
    }

    fn output_contains(&self, process_id: &str, pattern: &str) -> bool {
        let processes = self.lock_processes();
        processes.get(process_id).is_some_and(|process| {
            let (stdout, _, _) = process.stdout.read(None, DEFAULT_STREAM_CAP_BYTES);
            let (stderr, _, _) = process.stderr.read(None, DEFAULT_STREAM_CAP_BYTES);
            stdout.contains(pattern) || stderr.contains(pattern)
        })
    }

    fn mark_ready_if_running(&self, process_id: &str) {
        let should_emit = {
            let mut processes = self.lock_processes();
            let Some(process) = processes.get_mut(process_id) else {
                return;
            };
            if matches!(
                process.status,
                ProcessStatus::Starting | ProcessStatus::Running | ProcessStatus::Ready
            ) {
                process.status = ProcessStatus::Ready;
                process.ready = true;
                true
            } else {
                false
            }
        };
        if should_emit {
            self.emit(ProcessEvent::Ready {
                process_id: process_id.to_owned(),
            });
        }
    }

    fn is_running(&self, process_id: &str) -> bool {
        let processes = self.lock_processes();
        processes.get(process_id).is_some_and(|process| {
            matches!(
                process.status,
                ProcessStatus::Starting | ProcessStatus::Running | ProcessStatus::Ready
            )
        })
    }

    fn max_cursor(&self, process_id: &str) -> Option<u64> {
        let processes = self.lock_processes();
        processes.get(process_id).map(|process| {
            process
                .stdout
                .next_cursor()
                .max(process.stderr.next_cursor())
        })
    }

    fn lock_processes(&self) -> std::sync::MutexGuard<'_, HashMap<String, ManagedProcess>> {
        self.processes.lock().unwrap_or_else(|err| err.into_inner())
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

fn is_terminal(status: &ProcessStatus) -> bool {
    matches!(
        status,
        ProcessStatus::Exited { .. } | ProcessStatus::Killed | ProcessStatus::FailedToStart { .. }
    )
}

fn process_id_sort_key(process_id: &str) -> (u64, String) {
    let numeric = process_id
        .strip_prefix("proc_")
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(u64::MAX);
    (numeric, process_id.to_owned())
}

#[cfg(unix)]
fn configure_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_options(command: &str, name: &str) -> ProcessStartOptions {
        ProcessStartOptions {
            command: command.into(),
            name: Some(name.into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: None,
            ready_timeout_sec: None,
        }
    }

    #[tokio::test]
    async fn manager_start_read_and_kill_process() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let mut opts = start_options("printf ready; sleep 30", "server");
        opts.ready_pattern = Some("ready".into());
        opts.ready_timeout_sec = Some(5);

        let started = manager.start(opts).await.unwrap();

        assert_eq!(started.process_id, "proc_1");
        assert_eq!(started.status, ProcessStatus::Ready);
        assert!(started.ready);
        let output = manager
            .read(ProcessSelector::Id("proc_1".into()), None, 1024)
            .await
            .unwrap();
        assert!(output.stdout.contains("ready"));

        manager
            .kill(ProcessSelector::Id("proc_1".into()))
            .await
            .unwrap();
        let processes = manager.list();
        assert_eq!(processes[0].status, ProcessStatus::Killed);
    }

    #[tokio::test]
    async fn manager_rejects_duplicate_names() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());

        manager
            .start(start_options("sleep 30", "dup"))
            .await
            .unwrap();
        let err = manager
            .start(start_options("sleep 30", "dup"))
            .await
            .unwrap_err();

        assert!(err.contains("process name already exists"));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_ready_timeout_keeps_process_running() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let mut opts = start_options("sleep 30", "slow");
        opts.ready_pattern = Some("never".into());
        opts.ready_timeout_sec = Some(1);

        let started = manager.start(opts).await.unwrap();

        assert_eq!(started.status, ProcessStatus::Running);
        assert!(!started.ready);
        assert!(
            started
                .start_warning
                .as_deref()
                .unwrap_or_default()
                .contains("readiness timeout")
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_shutdown_kills_kill_policy_and_keeps_keep_policy() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        manager
            .start(start_options("sleep 30", "kill-me"))
            .await
            .unwrap();
        let mut keep_opts = start_options("sleep 30", "keep-me");
        keep_opts.on_exit = OnExitPolicy::Keep;
        manager.start(keep_opts).await.unwrap();

        let kept = manager.shutdown().await.unwrap();

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name.as_deref(), Some("keep-me"));
        let killed = manager
            .read(ProcessSelector::Name("kill-me".into()), None, 1024)
            .await
            .unwrap();
        assert_eq!(killed.status, ProcessStatus::Killed);

        manager
            .kill(ProcessSelector::Name("keep-me".into()))
            .await
            .unwrap();
        let keep = manager
            .read(ProcessSelector::Name("keep-me".into()), None, 1024)
            .await
            .unwrap();
        assert_eq!(keep.status, ProcessStatus::Killed);
    }

    #[test]
    fn stream_ring_buffer_snapshot_returns_recent_output_and_next_cursor() {
        let mut buf = StreamRingBuffer::new(32);
        buf.push(b"hello ");
        buf.push(b"world");

        let (text, next_cursor, truncated) = buf.read(None, 64);

        assert_eq!(text, "hello world");
        assert_eq!(next_cursor, 11);
        assert!(!truncated);
    }

    #[test]
    fn stream_ring_buffer_cursor_returns_only_new_output() {
        let mut buf = StreamRingBuffer::new(64);
        buf.push(b"first\n");
        let cursor = buf.next_cursor();
        buf.push(b"second\n");

        let (text, next_cursor, truncated) = buf.read(Some(cursor), 64);

        assert_eq!(text, "second\n");
        assert_eq!(next_cursor, 13);
        assert!(!truncated);
    }

    #[test]
    fn stream_ring_buffer_old_cursor_reports_truncation() {
        let mut buf = StreamRingBuffer::new(6);
        buf.push(b"abcdef");
        buf.push(b"ghij");

        let (text, next_cursor, truncated) = buf.read(Some(0), 64);

        assert_eq!(text, "efghij");
        assert_eq!(next_cursor, 10);
        assert!(truncated);
    }

    #[test]
    fn stream_ring_buffer_retains_tail_when_single_chunk_exceeds_cap() {
        let mut buf = StreamRingBuffer::new(6);
        buf.push(b"abcdefghij");

        let (text, next_cursor, truncated) = buf.read(Some(0), 64);

        assert_eq!(text, "efghij");
        assert_eq!(next_cursor, 10);
        assert!(truncated);
    }

    #[test]
    fn stream_ring_buffer_respects_max_bytes() {
        let mut buf = StreamRingBuffer::new(64);
        buf.push(b"abcdefghij");

        let (text, next_cursor, truncated) = buf.read(None, 4);

        assert_eq!(text, "ghij");
        assert_eq!(next_cursor, 10);
        assert!(!truncated);
    }
}
