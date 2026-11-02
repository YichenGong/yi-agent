use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::broadcast;
use tokio::time;

use crate::process_group::{configure_process_group, signal_process_group};
use crate::sandbox::{SandboxController, SandboxMode, SandboxPolicy};
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
struct StreamPage {
    text: String,
    /// Earlier bytes exist that this call cannot return.
    data_lost: bool,
}

#[derive(Debug, Clone)]
struct StreamRingBuffer {
    cap: usize,
    chunks: Vec<(u64, Vec<u8>)>,
    next_cursor: u64,
    /// Offset of the first byte ever written to this stream. Positions below it
    /// belong to the *other* stream, so "cursor < origin" is not data loss.
    origin: Option<u64>,
}

impl StreamRingBuffer {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            chunks: Vec::new(),
            next_cursor: 0,
            origin: None,
        }
    }

    /// First offset this stream ever wrote at (its own start), if any.
    fn origin(&self) -> u64 {
        self.origin.unwrap_or(self.next_cursor)
    }

    #[cfg(test)]
    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let start = self.next_cursor;
        self.push_at(start, bytes);
    }

    fn push_at(&mut self, start: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.origin.get_or_insert(start);
        self.next_cursor = self
            .next_cursor
            .max(start.saturating_add(bytes.len() as u64));
        self.chunks.push((start, bytes.to_vec()));
        self.trim_to_cap();
    }

    fn earliest_cursor(&self) -> u64 {
        self.chunks
            .first()
            .map(|(start, _)| *start)
            .unwrap_or(self.next_cursor)
    }

    /// Lowest cursor this stream can serve *without losing its own bytes*.
    ///
    /// A stream that has never been trimmed can serve from offset 0 — it simply
    /// has nothing in the early part of the shared timeline, because those bytes
    /// belong to the other stream. Only a trimmed stream pushes the floor up.
    fn loss_floor(&self) -> u64 {
        if self.origin() < self.earliest_cursor() {
            self.earliest_cursor()
        } else {
            0
        }
    }

    /// Bytes in `[from, to)` concatenated.
    fn slice_range(&self, from: u64, to: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for (start, bytes) in &self.chunks {
            let end = start.saturating_add(bytes.len() as u64);
            let lo = (*start).max(from);
            let hi = end.min(to);
            if lo >= hi {
                continue;
            }
            out.extend_from_slice(&bytes[(lo - start) as usize..(hi - start) as usize]);
        }
        out
    }

    /// True when bytes this stream wrote at or after `offset` were already
    /// discarded by the cap. Offsets before `origin` belong to the *other*
    /// stream on the shared timeline, so they are not loss for us.
    fn lost_before(&self, offset: u64) -> bool {
        self.origin() < self.earliest_cursor() && offset < self.earliest_cursor()
    }

    /// Page over the shared window `[from, to)`.
    fn read_window(&self, from: u64, to: u64, data_lost: bool) -> StreamPage {
        StreamPage {
            text: String::from_utf8_lossy(&self.slice_range(from, to)).into_owned(),
            data_lost,
        }
    }

    /// Recent bytes of this stream, for snapshot (cursor-less) reads.
    fn read_tail(&self, max_bytes: usize) -> StreamPage {
        let all = self.slice_range(self.earliest_cursor(), self.next_cursor);
        let dropped = all.len() > max_bytes;
        let text = if dropped {
            let start = tail_start_at_boundary(&all, max_bytes);
            String::from_utf8_lossy(&all[start..]).into_owned()
        } else {
            String::from_utf8_lossy(&all).into_owned()
        };
        StreamPage {
            text,
            data_lost: dropped || self.origin() < self.earliest_cursor(),
        }
    }

    /// Shrink `to` so this stream's bytes in `[from, to)` end on a character
    /// boundary. Returns `None` when the stream has no bytes in the window.
    fn boundary_end(&self, from: u64, to: u64) -> Option<u64> {
        let bytes = self.slice_range(from, to);
        if bytes.is_empty() {
            return None;
        }
        match std::str::from_utf8(&bytes) {
            // Incomplete character at the very end: cut it off and let the next
            // page carry it whole.
            Err(err) if err.error_len().is_none() => {
                Some(from.saturating_add(err.valid_up_to() as u64))
            }
            _ => None,
        }
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
    output_cursor: u64,
    stdout: StreamRingBuffer,
    stderr: StreamRingBuffer,
    child: Option<Child>,
}

pub struct ProcessManager {
    root: PathBuf,
    sandbox: SandboxPolicy,
    next_id: AtomicU64,
    shutting_down: AtomicBool,
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
            shutting_down: AtomicBool::new(false),
            processes: Arc::new(Mutex::new(HashMap::new())),
            events,
        })
    }

    /// Build a process manager backed by an externally-owned sandbox controller.
    ///
    /// Sharing the controller with the shell tool keeps a live YOLO switch in
    /// sync across both execution paths.
    pub fn with_controller(
        root: PathBuf,
        controller: SandboxController,
        extra_writable_roots: Vec<PathBuf>,
    ) -> Arc<Self> {
        let sandbox = SandboxPolicy::with_controller(&root, extra_writable_roots, controller);
        Self::with_sandbox(root, sandbox)
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
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("process manager is shutting down".into());
        }
        if let Some(reason) = blocklist::is_blocked(command) {
            return Err(format!("blocked command: {reason}"));
        }
        if opts.name.as_deref().is_some_and(str::is_empty) {
            return Err("process name cannot be empty".into());
        }

        let cwd = self.resolve_cwd(opts.cwd.as_ref())?;
        let (program, args) = self
            .sandbox
            .command(command, &cwd)
            .map_err(|err| err.to_string())?;
        let process_id = self.next_process_id();
        let name = opts.name.clone();
        let on_exit = opts.on_exit;
        {
            let mut processes = self.lock_processes();
            if self.shutting_down.load(Ordering::SeqCst) {
                return Err("process manager is shutting down".into());
            }
            // Terminal processes never produce more output or change state, so
            // they must not hold a slot forever: a long session that runs many
            // short commands would otherwise permanently hit the cap.
            processes.retain(|_, process| !is_terminal(&process.status));
            if processes.len() >= 16 {
                return Err("maximum managed process count reached".into());
            }
            if let Some(name) = &name {
                if processes
                    .values()
                    .any(|process| process.name.as_ref() == Some(name))
                {
                    return Err("process name already exists".into());
                }
            }
            processes.insert(
                process_id.clone(),
                ManagedProcess {
                    process_id: process_id.clone(),
                    name: name.clone(),
                    pid: None,
                    command: opts.command.clone(),
                    cwd: cwd.clone(),
                    status: ProcessStatus::Starting,
                    ready: false,
                    on_exit,
                    exit_code: None,
                    start_time: Instant::now(),
                    end_time: None,
                    output_cursor: 0,
                    stdout: StreamRingBuffer::new(DEFAULT_STREAM_CAP_BYTES),
                    stderr: StreamRingBuffer::new(DEFAULT_STREAM_CAP_BYTES),
                    child: None,
                },
            );
        }

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
            .inspect_err(|_| {
                self.lock_processes().remove(&process_id);
            })
            .map_err(|err| format!("failed to start process: {err}"))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        {
            let mut processes = self.lock_processes();
            if self.shutting_down.load(Ordering::SeqCst) {
                processes.remove(&process_id);
                drop(processes);
                terminate_child(child, pid);
                return Err("process manager is shutting down".into());
            }
            let process = processes
                .get_mut(&process_id)
                .ok_or_else(|| format!("process reservation missing: {process_id}"))?;
            process.pid = pid;
            process.status = ProcessStatus::Running;
            process.child = Some(child);
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
            next_cursor: self.next_output_cursor(&process_id).unwrap_or(0),
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
        match cursor {
            None => {
                // Snapshot: newest `max_bytes` of each stream, for a detail view.
                let stdout_page = process.stdout.read_tail(max_bytes);
                let stderr_page = process.stderr.read_tail(max_bytes);
                Ok(ProcessReadResult {
                    process_id: process.process_id.clone(),
                    name: process.name.clone(),
                    stdout: stdout_page.text,
                    stderr: stderr_page.text,
                    next_cursor: process.output_cursor,
                    truncated: stdout_page.data_lost || stderr_page.data_lost,
                    status: process.status.clone(),
                    ready: process.ready,
                })
            }
            Some(requested) => {
                // Incremental: one window over the shared timeline. Both streams
                // are served from the same `[start, end)` window, so a caller
                // looping on `next_cursor` sees every byte exactly once.
                let floor = process.stdout.loss_floor().max(process.stderr.loss_floor());
                let start = requested.max(floor);
                let data_lost =
                    process.stdout.lost_before(start) || process.stderr.lost_before(start);

                let available = process.output_cursor.saturating_sub(start) as usize;
                let window = max_bytes.min(available);
                let mut end = start.saturating_add(window as u64);

                // Cap the page at `max_bytes` of *rendered* bytes and keep both
                // streams on character boundaries: an incomplete trailing
                // character moves to the next page instead of becoming U+FFFD.
                while end > start {
                    let stdout_bytes = process.stdout.slice_range(start, end);
                    let stderr_bytes = process.stderr.slice_range(start, end);
                    if stdout_bytes.len() + stderr_bytes.len() <= max_bytes {
                        break;
                    }
                    end -= 1;
                }
                for stream in [&process.stdout, &process.stderr] {
                    if let Some(boundary) = stream.boundary_end(start, end) {
                        end = end.min(boundary);
                    }
                }
                // A single character wider than `max_bytes` would collapse the
                // window to empty and stall the cursor forever. Return that one
                // character whole (slightly over budget) so paging stays alive.
                if end == start && window > 0 {
                    let mut widened = start.saturating_add(1);
                    while widened < process.output_cursor
                        && process
                            .stdout
                            .boundary_end(start, widened)
                            .or_else(|| process.stderr.boundary_end(start, widened))
                            .is_some()
                    {
                        widened += 1;
                    }
                    end = widened.min(process.output_cursor);
                }

                let stdout_page = process.stdout.read_window(start, end, data_lost);
                let stderr_page = process.stderr.read_window(start, end, data_lost);
                Ok(ProcessReadResult {
                    process_id: process.process_id.clone(),
                    name: process.name.clone(),
                    stdout: stdout_page.text,
                    stderr: stderr_page.text,
                    next_cursor: end,
                    truncated: data_lost,
                    status: process.status.clone(),
                    ready: process.ready,
                })
            }
        }
    }

    pub async fn kill(&self, selector: ProcessSelector) -> Result<(), String> {
        let process_id = self.selector_to_id(&selector)?;
        let (pid, killed) = {
            let mut processes = self.lock_processes();
            let process = processes
                .get_mut(&process_id)
                .ok_or_else(|| format!("process not found: {process_id}"))?;
            if is_terminal(&process.status) {
                return Ok(());
            }
            let pid = process.child.as_ref().and(process.pid);
            process.status = ProcessStatus::Killed;
            process.ready = false;
            process.end_time = Some(Instant::now());
            if let Some(child) = process.child.as_mut() {
                let _ = child.start_kill();
            }
            (pid, true)
        };
        if let Some(pid) = pid {
            signal_process_group(pid, libc::SIGTERM);
        }
        if killed {
            self.emit(ProcessEvent::Killed { process_id });
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<Vec<ManagedProcessSnapshot>, String> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let ids = {
            let processes = self.lock_processes();
            processes
                .values()
                .filter(|process| {
                    process.on_exit == OnExitPolicy::Kill && !is_terminal(&process.status)
                })
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
            let start = process.output_cursor;
            process.output_cursor = process.output_cursor.saturating_add(bytes.len() as u64);
            match stream {
                StreamKind::Stdout => process.stdout.push_at(start, bytes),
                StreamKind::Stderr => process.stderr.push_at(start, bytes),
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
        let mut events = self.subscribe();
        if self.output_contains(process_id, pattern) {
            return true;
        }
        let deadline = time::Instant::now() + timeout;
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
            let stdout = process.stdout.read_tail(DEFAULT_STREAM_CAP_BYTES);
            let stderr = process.stderr.read_tail(DEFAULT_STREAM_CAP_BYTES);
            stdout.text.contains(pattern) || stderr.text.contains(pattern)
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

    fn next_output_cursor(&self, process_id: &str) -> Option<u64> {
        let processes = self.lock_processes();
        processes
            .get(process_id)
            .map(|process| process.output_cursor)
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

/// Start offset of the last `max_bytes` of `bytes`, pushed forward to a UTF-8
/// character boundary so a tail snapshot never begins with half a character.
fn tail_start_at_boundary(bytes: &[u8], max_bytes: usize) -> usize {
    let mut start = bytes.len().saturating_sub(max_bytes);
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    start
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

fn terminate_child(mut child: Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        signal_process_group(pid, libc::SIGTERM);
    }
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep};

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

    async fn wait_until<F>(timeout: Duration, mut predicate: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if predicate() {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert!(predicate());
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

    #[tokio::test]
    async fn manager_shutdown_prevents_late_start_leak() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());

        manager.shutdown().await.unwrap();
        let err = manager
            .start(start_options("sleep 30", "late"))
            .await
            .unwrap_err();

        assert!(err.contains("process manager is shutting down"));
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn manager_read_cursor_does_not_skip_slower_stderr() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let mut opts = start_options("printf out; sleep 1; printf err >&2; sleep 30", "mixed");
        opts.ready_pattern = Some("out".into());
        opts.ready_timeout_sec = Some(2);
        manager.start(opts).await.unwrap();

        let first = manager
            .read(ProcessSelector::Name("mixed".into()), None, 1024)
            .await
            .unwrap();
        assert!(first.stdout.contains("out"));
        assert!(first.stderr.is_empty());

        wait_until(Duration::from_secs(3), || {
            let output = futures::executor::block_on(manager.read(
                ProcessSelector::Name("mixed".into()),
                Some(first.next_cursor),
                1024,
            ))
            .unwrap();
            output.stderr.contains("err")
        })
        .await;
        let second = manager
            .read(
                ProcessSelector::Name("mixed".into()),
                Some(first.next_cursor),
                1024,
            )
            .await
            .unwrap();
        assert!(second.stdout.is_empty());
        assert!(second.stderr.contains("err"));

        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_shutdown_leaves_exited_kill_policy_process_exited() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        manager
            .start(start_options("printf done", "short"))
            .await
            .unwrap();

        wait_until(Duration::from_secs(1), || {
            manager
                .list()
                .iter()
                .any(|process| matches!(process.status, ProcessStatus::Exited { code: Some(0) }))
        })
        .await;

        let kept = manager.shutdown().await.unwrap();

        assert!(kept.is_empty());
        let processes = manager.list();
        assert_eq!(processes.len(), 1);
        assert_eq!(processes[0].status, ProcessStatus::Exited { code: Some(0) });
    }

    #[test]
    fn stream_ring_buffer_snapshot_returns_recent_output_and_next_cursor() {
        let mut buf = StreamRingBuffer::new(32);
        buf.push(b"hello ");
        buf.push(b"world");

        let page = buf.read_tail(64);

        assert_eq!(page.text, "hello world");
        assert!(!page.data_lost);
    }

    #[test]
    fn stream_ring_buffer_snapshot_returns_tail_and_reports_dropped_prefix() {
        let mut buf = StreamRingBuffer::new(64);
        buf.push(b"abcdefghij");

        let page = buf.read_tail(4);

        // Snapshot 取最近 4 字节,前面的 6 字节确实读不回来了,必须如实报告。
        assert_eq!(page.text, "ghij");
        assert!(page.data_lost);
    }

    #[test]
    fn stream_ring_buffer_window_does_not_skip_or_repeat() {
        let mut buf = StreamRingBuffer::new(1024);
        buf.push_at(0, b"0123");
        buf.push_at(4, b"4567");
        buf.push_at(8, b"89");

        assert_eq!(buf.read_window(0, 4, false).text, "0123");
        assert_eq!(buf.read_window(4, 8, false).text, "4567");
        assert_eq!(buf.read_window(8, 10, false).text, "89");
    }

    #[test]
    fn stream_ring_buffer_reports_loss_only_when_cursor_left_trimmed_bytes() {
        let mut buf = StreamRingBuffer::new(6);
        buf.push_at(0, b"abcdef");
        buf.push_at(6, b"ghij");

        // "ab" 已被 cap 丢掉,从 0 读会真的漏数据。
        assert!(buf.lost_before(0));
        // 从现存最早位置读,没有丢失。
        assert!(!buf.lost_before(buf.loss_floor()));
    }

    #[test]
    fn stream_ring_buffer_window_ends_on_a_char_boundary() {
        let mut buf = StreamRingBuffer::new(1024);
        buf.push_at(0, "中文字".as_bytes()); // 每个字 3 字节,共 9 字节

        // 窗口末落在"中"的字中间:回退到 3,让下一个窗口从"文"整字开始。
        assert_eq!(buf.boundary_end(0, 4), Some(3));
        // 正好落在字符边界上:无需调整。
        assert_eq!(buf.boundary_end(0, 3), None);

        assert_eq!(buf.read_window(0, 3, false).text, "中");
        assert_eq!(buf.read_window(3, 6, false).text, "文");
        assert_eq!(buf.read_window(6, 9, false).text, "字");
    }

    #[test]
    fn stream_ring_buffer_tail_does_not_split_a_multibyte_char() {
        let mut buf = StreamRingBuffer::new(1024);
        buf.push_at(0, "中文字".as_bytes());

        // 只保留 4 字节会切在"文"中间,必须往前退到"字"。
        let page = buf.read_tail(4);
        assert_eq!(page.text, "字");
    }

    #[tokio::test]
    async fn manager_read_pages_incrementally_without_gaps() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        manager
            .start(start_options("printf '0123456789'", "paged"))
            .await
            .unwrap();
        wait_until(Duration::from_secs(3), || {
            manager
                .list()
                .iter()
                .any(|p| p.status == ProcessStatus::Exited { code: Some(0) })
        })
        .await;

        let mut collected = String::new();
        let mut cursor = Some(0u64);
        for _ in 0..8 {
            let out = manager
                .read(ProcessSelector::Name("paged".into()), cursor, 4)
                .await
                .unwrap();
            if out.stdout.is_empty() {
                break;
            }
            collected.push_str(&out.stdout);
            cursor = Some(out.next_cursor);
        }

        assert_eq!(collected, "0123456789");
    }

    #[tokio::test]
    async fn manager_read_always_advances_the_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // 非法 UTF-8 字节渲染成 3 字节的 U+FFFD,max_bytes=1 时按渲染长度收窗口
        // 会把它整段裁掉。游标必须仍然前进,否则轮询方会永远停在同一位置。
        manager
            .start(start_options(r#"printf '\xff\xfe'"#, "binary"))
            .await
            .unwrap();
        wait_until(Duration::from_secs(5), || {
            manager
                .list()
                .iter()
                .any(|p| matches!(p.status, ProcessStatus::Exited { .. }))
        })
        .await;

        let mut cursor = 0u64;
        for _ in 0..4 {
            let out = manager
                .read(ProcessSelector::Name("binary".into()), Some(cursor), 1)
                .await
                .unwrap();
            if out.next_cursor == cursor {
                assert_eq!(
                    out.next_cursor, 2,
                    "游标停住了: {} (text={:?})",
                    out.next_cursor, out.stdout
                );
                break;
            }
            cursor = out.next_cursor;
        }
        assert_eq!(cursor, 2, "两个非法字节必须被分页读完");
    }

    #[tokio::test]
    async fn manager_read_advances_past_a_char_wider_than_max_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // 单个汉字 3 字节。max_bytes=1 时窗口装不下它:无论如何都要让游标前进,
        // 否则轮询方永远读同一位置(死循环),而整字返回只是稍微超一点上限。
        manager
            .start(start_options("printf '中'", "wide-char"))
            .await
            .unwrap();
        wait_until(Duration::from_secs(5), || {
            manager
                .list()
                .iter()
                .any(|p| matches!(p.status, ProcessStatus::Exited { .. }))
        })
        .await;

        let out = manager
            .read(ProcessSelector::Name("wide-char".into()), Some(0), 1)
            .await
            .unwrap();

        assert_eq!(out.stdout, "中", "整字返回,不得切成 U+FFFD");
        assert_eq!(out.next_cursor, 3, "游标必须越过这个字");
    }

    #[tokio::test]
    async fn manager_read_does_not_mark_truncated_for_fresh_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // stdout 在前、stderr 在后:两条流都完整保留,游标 0 一定能读到全部。
        manager
            .start(start_options(
                "printf 'O1\n'; sleep 0.2; printf 'E1\n' >&2",
                "interleaved",
            ))
            .await
            .unwrap();
        wait_until(Duration::from_secs(5), || {
            manager
                .list()
                .iter()
                .any(|p| matches!(p.status, ProcessStatus::Exited { .. }))
        })
        .await;

        let out = manager
            .read(ProcessSelector::Name("interleaved".into()), Some(0), 1024)
            .await
            .unwrap();

        assert_eq!(out.stdout, "O1\n");
        assert_eq!(out.stderr, "E1\n");
        assert!(
            !out.truncated,
            "两条流都完整返回,不应误报 truncated: {out:?}"
        );
    }

    #[tokio::test]
    async fn manager_read_keeps_stderr_after_stdout_exceeds_cap() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // stdout 远超单流 256 KiB 上限,stderr 写在最后。共享游标若跳过头,
        // 这条错误信息会被永久跳过。
        manager
            .start(start_options(
                r#"sh -c 'head -c 300000 /dev/zero | tr "\0" "o"; printf "ERRMARK" >&2'"#,
                "overflow",
            ))
            .await
            .unwrap();
        wait_until(Duration::from_secs(10), || {
            manager
                .list()
                .iter()
                .any(|p| matches!(p.status, ProcessStatus::Exited { .. }))
        })
        .await;

        let mut stderr = String::new();
        let mut cursor = Some(0u64);
        for _ in 0..12 {
            let out = manager
                .read(ProcessSelector::Name("overflow".into()), cursor, 64 * 1024)
                .await
                .unwrap();
            stderr.push_str(&out.stderr);
            if out.next_cursor <= cursor.unwrap_or(0) && out.stdout.is_empty() {
                break;
            }
            cursor = Some(out.next_cursor);
        }

        assert!(
            stderr.contains("ERRMARK"),
            "stdout 溢出后 stderr 仍必须可读,实际: {stderr:?}"
        );
    }

    #[tokio::test]
    async fn manager_start_reclaims_slots_from_exited_processes() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());

        for i in 0..16 {
            manager
                .start(start_options("printf done", &format!("short-{i}")))
                .await
                .unwrap();
        }
        wait_until(Duration::from_secs(10), || {
            manager
                .list()
                .iter()
                .filter(|p| matches!(p.status, ProcessStatus::Exited { .. }))
                .count()
                == 16
        })
        .await;

        // 已退出的进程占着槽位却永远不再产生输出,必须能被回收。
        manager
            .start(start_options("sleep 30", "after-eviction"))
            .await
            .expect("terminal processes must not exhaust the slot budget");

        manager.shutdown().await.unwrap();
    }
}
