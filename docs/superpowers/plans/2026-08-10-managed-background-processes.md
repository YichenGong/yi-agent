# Managed Background Processes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add first-class managed background processes through process tools and a Ctrl+P TUI process tab while preserving existing `bash` foreground semantics.

**Architecture:** Introduce `yi_agent_tools::process::ProcessManager` as the runtime owner for long-running child processes, output ring buffers, readiness state, and lifecycle cleanup. Add process tools that share the manager, wire them into TUI/run setup, then extend the Ctrl+P popup from bash-only to tabbed bash/process views.

**Tech Stack:** Rust 2024, tokio process/io/time/sync, serde/serde_json, async-trait, ratatui, unicode-width, libc on Unix for process-group handling, existing yi-agent tool and TUI patterns.

---

## Reference Spec

- `docs/superpowers/specs/2026-08-10-managed-background-processes-design.md`

## File Structure

- Create `yi-agent-rs/crates/yi-agent-tools/src/process/mod.rs`: module exports and process tool registrations.
- Create `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs`: `ProcessManager`, process metadata, status model, output ring buffer, spawn/read/kill/shutdown APIs.
- Create `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`: `process_start`, `process_list`, `process_read`, `process_kill` tool implementations.
- Modify `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`: export process types and add process tool registration helpers.
- Modify `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`: add `libc = "0.2"` for Unix process groups.
- Modify `yi-agent-rs/crates/yi-agent/src/main.rs`: create one shared `Arc<ProcessManager>` per runtime and pass it to tools and TUI.
- Create `yi-agent-rs/crates/yi-agent/src/tui/process_popup.rs`: tab/list/detail rendering for managed processes.
- Modify `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`: export `process_popup`.
- Modify `yi-agent-rs/crates/yi-agent/src/tui/app.rs`: add Ctrl+P tabs, process snapshots, process kill command routing, and shutdown cleanup.
- Modify `docs/project-management/yi-agent-tools.md`, `docs/project-management/yi-agent-tui.md`, and `docs/project-management/README.md`: record completed process tooling and TUI work after tests pass.

## Implementation Notes

- Keep the existing `BashTool` unchanged except for sharing sandbox/blocklist helpers if needed.
- Use `process_id` values generated in-memory as `proc_1`, `proc_2`, ... for deterministic tests.
- Treat `process_start` and `process_kill` as mutating tools requiring confirmation.
- Treat `process_list` and `process_read` as read-only tools.
- Start Unix managed commands in their own process group via `pre_exec(|| { libc::setpgid(0, 0); Ok(()) })`.
- Kill Unix process groups with `libc::kill(-(pid as i32), libc::SIGTERM)` and fall back to killing the child handle if needed.
- Store stdout/stderr separately. Each stream has its own ring buffer and cursor.
- Do not expose stdin in the tool API.
- The MVP does not reattach kept processes after yi-agent restart.

---

### Task 1: Process Data Model And Ring Buffer

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/process/mod.rs`
- Create: `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`

- [ ] **Step 1: Add the dependency and module exports**

Modify `yi-agent-rs/crates/yi-agent-tools/Cargo.toml` under `[dependencies]`:

```toml
libc = "0.2"
```

Modify `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`:

```rust
mod process;

pub use process::{
    ManagedProcessSnapshot, OnExitPolicy, ProcessManager, ProcessReadResult, ProcessStatus,
};
```

Place the `mod process;` line with the other module declarations and the `pub use process::...` line with other exports.

Create `yi-agent-rs/crates/yi-agent-tools/src/process/mod.rs`:

```rust
pub mod manager;

pub use manager::{
    ManagedProcessSnapshot, OnExitPolicy, ProcessManager, ProcessReadResult, ProcessStatus,
};
```

- [ ] **Step 2: Write failing ring-buffer tests**

Create `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs` with this initial test module and minimal type names referenced by tests:

```rust
use serde::{Deserialize, Serialize};

const DEFAULT_STREAM_CAP_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExitPolicy {
    Kill,
    Keep,
}

impl Default for OnExitPolicy {
    fn default() -> Self {
        Self::Kill
    }
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
}

pub struct ProcessManager;

#[cfg(test)]
mod tests {
    use super::*;

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
    fn stream_ring_buffer_respects_max_bytes() {
        let mut buf = StreamRingBuffer::new(64);
        buf.push(b"abcdefghij");

        let (text, next_cursor, truncated) = buf.read(None, 4);

        assert_eq!(text, "ghij");
        assert_eq!(next_cursor, 10);
        assert!(!truncated);
    }
}
```

- [ ] **Step 3: Run the failing ring-buffer tests**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::manager::tests::stream_ring_buffer_ -- --nocapture
```

Expected: compile fails because `StreamRingBuffer::push`, `StreamRingBuffer::read`, and `StreamRingBuffer::next_cursor` are not defined.

- [ ] **Step 4: Implement the ring buffer**

In `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs`, replace the `impl StreamRingBuffer` block with:

```rust
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

        (String::from_utf8_lossy(&out).into_owned(), self.next_cursor, truncated)
    }

    fn trim_to_cap(&mut self) {
        let mut total: usize = self.chunks.iter().map(|(_, bytes)| bytes.len()).sum();
        while total > self.cap {
            let excess = total - self.cap;
            let Some((start, bytes)) = self.chunks.first_mut() else {
                break;
            };
            if excess >= bytes.len() {
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
```

- [ ] **Step 5: Run tests and commit**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::manager::tests::stream_ring_buffer_ -- --nocapture
cargo fmt --all
git add Cargo.toml crates/yi-agent-tools/Cargo.toml crates/yi-agent-tools/src/lib.rs crates/yi-agent-tools/src/process/mod.rs crates/yi-agent-tools/src/process/manager.rs
git commit -m "feat: add managed process ring buffers"
```

Expected: all four ring-buffer tests pass and the commit succeeds.

---

### Task 2: ProcessManager Start, Read, Kill, And Shutdown

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs`

- [ ] **Step 1: Add failing manager lifecycle tests**

Append these tests to `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs` inside the existing `#[cfg(test)] mod tests`:

```rust
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn manager_start_read_and_kill_process() {
    let tmp = TempDir::new().unwrap();
    let manager = ProcessManager::new(tmp.path().to_path_buf());

    let started = manager
        .start(ProcessStartOptions {
            command: "printf ready; sleep 30".into(),
            name: Some("server".into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: Some("ready".into()),
            ready_timeout_sec: Some(3),
        })
        .await
        .unwrap();

    assert_eq!(started.process_id, "proc_1");
    assert_eq!(started.name.as_deref(), Some("server"));
    assert!(started.ready);
    assert_eq!(started.status, ProcessStatus::Ready);

    let read = manager
        .read(ProcessSelector::Name("server".into()), None, 1024)
        .await
        .unwrap();
    assert!(read.stdout.contains("ready"));

    manager
        .kill(ProcessSelector::Id("proc_1".into()))
        .await
        .unwrap();
    let listed = manager.list().await;
    assert_eq!(listed[0].status, ProcessStatus::Killed);
}

#[tokio::test]
async fn manager_rejects_duplicate_names() {
    let tmp = TempDir::new().unwrap();
    let manager = ProcessManager::new(tmp.path().to_path_buf());

    manager
        .start(ProcessStartOptions {
            command: "sleep 30".into(),
            name: Some("dup".into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: None,
            ready_timeout_sec: None,
        })
        .await
        .unwrap();

    let err = manager
        .start(ProcessStartOptions {
            command: "sleep 30".into(),
            name: Some("dup".into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: None,
            ready_timeout_sec: None,
        })
        .await
        .unwrap_err();

    assert!(err.contains("process name already exists"));
    manager.shutdown().await;
}

#[tokio::test]
async fn manager_ready_timeout_keeps_process_running() {
    let tmp = TempDir::new().unwrap();
    let manager = ProcessManager::new(tmp.path().to_path_buf());

    let started = manager
        .start(ProcessStartOptions {
            command: "sleep 30".into(),
            name: None,
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: Some("never".into()),
            ready_timeout_sec: Some(1),
        })
        .await
        .unwrap();

    assert_eq!(started.status, ProcessStatus::Running);
    assert!(!started.ready);
    assert!(started.start_warning.unwrap().contains("readiness timeout"));

    manager.shutdown().await;
}

#[tokio::test]
async fn manager_shutdown_kills_kill_policy_and_keeps_keep_policy() {
    let tmp = TempDir::new().unwrap();
    let manager = ProcessManager::new(tmp.path().to_path_buf());

    let kill_proc = manager
        .start(ProcessStartOptions {
            command: "sleep 30".into(),
            name: Some("kill-me".into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: None,
            ready_timeout_sec: None,
        })
        .await
        .unwrap();
    let keep_proc = manager
        .start(ProcessStartOptions {
            command: "sleep 30".into(),
            name: Some("keep-me".into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Keep,
            ready_pattern: None,
            ready_timeout_sec: None,
        })
        .await
        .unwrap();

    let retained = manager.shutdown().await;

    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].process_id, keep_proc.process_id);
    manager
        .kill(ProcessSelector::Id(keep_proc.process_id))
        .await
        .unwrap();
    let kill_snapshot = manager
        .read(ProcessSelector::Id(kill_proc.process_id), None, 1024)
        .await
        .unwrap();
    assert_eq!(kill_snapshot.status, ProcessStatus::Killed);
}
```

- [ ] **Step 2: Run lifecycle tests to verify they fail**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::manager::tests::manager_ -- --nocapture
```

Expected: compile fails because `ProcessStartOptions`, `ProcessSelector`, `ProcessManager::new`, `start`, `list`, `read`, `kill`, and `shutdown` are not implemented.

- [ ] **Step 3: Add manager request/result types**

Add these definitions above `pub struct ProcessManager` in `manager.rs`:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex};

use crate::sandbox::{SandboxMode, SandboxPolicy};
use crate::shell::blocklist::is_blocked;

const DEFAULT_READY_TIMEOUT_SEC: u64 = 10;
const MAX_PROCESSES: usize = 16;

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub enum ProcessSelector {
    Id(String),
    Name(String),
}

#[derive(Debug, Clone)]
pub enum ProcessEvent {
    Started { process_id: String },
    Output { process_id: String },
    Ready { process_id: String },
    Exited { process_id: String },
    Killed { process_id: String },
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
    child: Option<Arc<Mutex<Child>>>,
}
```

- [ ] **Step 4: Implement ProcessManager fields and constructor**

Replace `pub struct ProcessManager;` with:

```rust
pub struct ProcessManager {
    root: PathBuf,
    sandbox: SandboxPolicy,
    next_id: AtomicU64,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    events: broadcast::Sender<ProcessEvent>,
}

impl ProcessManager {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Self::with_sandbox(
            root.clone(),
            SandboxPolicy::new(SandboxMode::DangerFullAccess, root, Vec::new()),
        )
    }

    pub fn with_sandbox(root: PathBuf, sandbox: SandboxPolicy) -> Arc<Self> {
        let (events, _) = broadcast::channel(128);
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
}
```

- [ ] **Step 5: Implement helper methods**

Add this `impl ProcessManager` block after the constructor block:

```rust
impl ProcessManager {
    fn next_process_id(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("proc_{id}")
    }

    fn resolve_cwd(&self, cwd: Option<PathBuf>) -> Result<PathBuf, String> {
        let cwd = cwd.unwrap_or_else(|| self.root.clone());
        let cwd = if cwd.is_absolute() { cwd } else { self.root.join(cwd) };
        if !cwd.exists() {
            return Err(format!("cwd does not exist: {}", cwd.display()));
        }
        if !cwd.is_dir() {
            return Err(format!("cwd is not a directory: {}", cwd.display()));
        }
        Ok(cwd)
    }

    async fn selector_to_id(&self, selector: ProcessSelector) -> Result<String, String> {
        let processes = self.processes.lock().await;
        match selector {
            ProcessSelector::Id(id) => {
                if processes.contains_key(&id) {
                    Ok(id)
                } else {
                    Err(format!("managed process not found: {id}"))
                }
            }
            ProcessSelector::Name(name) => processes
                .values()
                .find(|p| p.name.as_deref() == Some(name.as_str()))
                .map(|p| p.process_id.clone())
                .ok_or_else(|| format!("managed process not found by name: {name}")),
        }
    }

    fn snapshot(process: &ManagedProcess) -> ManagedProcessSnapshot {
        let end = process.end_time.unwrap_or_else(Instant::now);
        ManagedProcessSnapshot {
            process_id: process.process_id.clone(),
            name: process.name.clone(),
            pid: process.pid,
            command: process.command.clone(),
            cwd: process.cwd.display().to_string(),
            status: process.status.clone(),
            ready: process.ready,
            on_exit: process.on_exit,
            exit_code: process.exit_code,
            elapsed_sec: end.duration_since(process.start_time).as_secs_f32(),
        }
    }

    async fn emit(&self, event: ProcessEvent) {
        let _ = self.events.send(event);
    }
}
```

- [ ] **Step 6: Implement start/read/list/kill/shutdown**

Add this code to `manager.rs`:

```rust
impl ProcessManager {
    pub async fn start(self: &Arc<Self>, opts: ProcessStartOptions) -> Result<ProcessStartResult, String> {
        if opts.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        if let Some(reason) = is_blocked(&opts.command) {
            return Err(format!("command blocked: {reason}"));
        }

        let cwd = self.resolve_cwd(opts.cwd.clone())?;
        let mut processes = self.processes.lock().await;
        if processes.len() >= MAX_PROCESSES {
            return Err(format!("too many managed processes; limit is {MAX_PROCESSES}"));
        }
        if let Some(name) = &opts.name {
            if processes.values().any(|p| p.name.as_ref() == Some(name)) {
                return Err(format!("process name already exists: {name}"));
            }
        }

        let process_id = self.next_process_id();
        let (program, command_args) = self
            .sandbox
            .command(&opts.command, &cwd)
            .map_err(|e| e.to_string())?;

        let mut command = Command::new(program);
        command
            .args(command_args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        for (key, value) in &opts.env {
            command.env(key, value);
        }
        configure_process_group(&mut command);

        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));

        let process = ManagedProcess {
            process_id: process_id.clone(),
            name: opts.name.clone(),
            pid,
            command: opts.command.clone(),
            cwd: cwd.clone(),
            status: if opts.ready_pattern.is_some() {
                ProcessStatus::Starting
            } else {
                ProcessStatus::Running
            },
            ready: opts.ready_pattern.is_none(),
            on_exit: opts.on_exit,
            exit_code: None,
            start_time: Instant::now(),
            end_time: None,
            stdout: StreamRingBuffer::new(DEFAULT_STREAM_CAP_BYTES),
            stderr: StreamRingBuffer::new(DEFAULT_STREAM_CAP_BYTES),
            child: Some(child.clone()),
        };
        processes.insert(process_id.clone(), process);
        drop(processes);

        self.spawn_reader(process_id.clone(), yi_agent_core::OutputStream::Stdout, stdout);
        self.spawn_reader(process_id.clone(), yi_agent_core::OutputStream::Stderr, stderr);
        self.spawn_waiter(process_id.clone(), child);
        self.emit(ProcessEvent::Started { process_id: process_id.clone() }).await;

        let mut start_warning = None;
        if let Some(pattern) = &opts.ready_pattern {
            let timeout = Duration::from_secs(opts.ready_timeout_sec.unwrap_or(DEFAULT_READY_TIMEOUT_SEC));
            let ready = self.wait_for_ready(&process_id, pattern, timeout).await;
            if ready {
                let mut processes = self.processes.lock().await;
                if let Some(process) = processes.get_mut(&process_id) {
                    process.ready = true;
                    process.status = ProcessStatus::Ready;
                }
                self.emit(ProcessEvent::Ready { process_id: process_id.clone() }).await;
            } else {
                let mut processes = self.processes.lock().await;
                if let Some(process) = processes.get_mut(&process_id) {
                    process.status = ProcessStatus::Running;
                }
                start_warning = Some(format!("readiness timeout after {}s", timeout.as_secs()));
            }
        }

        let processes = self.processes.lock().await;
        let process = processes.get(&process_id).expect("process just inserted");
        Ok(ProcessStartResult {
            process_id: process.process_id.clone(),
            name: process.name.clone(),
            pid: process.pid,
            status: process.status.clone(),
            ready: process.ready,
            next_cursor: process.stdout.next_cursor().max(process.stderr.next_cursor()),
            start_warning,
        })
    }

    pub async fn list(&self) -> Vec<ManagedProcessSnapshot> {
        let processes = self.processes.lock().await;
        let mut snapshots: Vec<_> = processes.values().map(Self::snapshot).collect();
        snapshots.sort_by(|a, b| a.process_id.cmp(&b.process_id));
        snapshots
    }

    pub async fn read(
        &self,
        selector: ProcessSelector,
        cursor: Option<u64>,
        max_bytes: usize,
    ) -> Result<ProcessReadResult, String> {
        let id = self.selector_to_id(selector).await?;
        let processes = self.processes.lock().await;
        let process = processes
            .get(&id)
            .ok_or_else(|| format!("managed process not found: {id}"))?;
        let stream_max = max_bytes.max(1);
        let (stdout, stdout_cursor, stdout_truncated) = process.stdout.read(cursor, stream_max);
        let (stderr, stderr_cursor, stderr_truncated) = process.stderr.read(cursor, stream_max);
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
        let id = self.selector_to_id(selector).await?;
        let child = {
            let mut processes = self.processes.lock().await;
            let process = processes
                .get_mut(&id)
                .ok_or_else(|| format!("managed process not found: {id}"))?;
            if matches!(process.status, ProcessStatus::Exited { .. } | ProcessStatus::Killed) {
                process.status = ProcessStatus::Killed;
                return Ok(());
            }
            kill_process_group(process.pid);
            process.status = ProcessStatus::Killed;
            process.ready = false;
            process.end_time = Some(Instant::now());
            process.child.clone()
        };
        if let Some(child) = child {
            let mut child = child.lock().await;
            let _ = child.kill().await;
        }
        self.emit(ProcessEvent::Killed { process_id: id }).await;
        Ok(())
    }

    pub async fn shutdown(&self) -> Vec<ManagedProcessSnapshot> {
        let snapshots = self.list().await;
        let mut retained = Vec::new();
        for snapshot in snapshots {
            match snapshot.on_exit {
                OnExitPolicy::Kill => {
                    let _ = self.kill(ProcessSelector::Id(snapshot.process_id)).await;
                }
                OnExitPolicy::Keep => retained.push(snapshot),
            }
        }
        retained
    }
}
```

- [ ] **Step 7: Implement reader, waiter, readiness, and platform helpers**

Add these functions to `manager.rs`:

```rust
impl ProcessManager {
    fn spawn_reader(
        self: &Arc<Self>,
        process_id: String,
        stream: yi_agent_core::OutputStream,
        pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    ) {
        let Some(mut pipe) = pipe else { return; };
        let manager = self.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match pipe.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut processes = manager.processes.lock().await;
                        if let Some(process) = processes.get_mut(&process_id) {
                            match stream {
                                yi_agent_core::OutputStream::Stdout => process.stdout.push(&buf[..n]),
                                yi_agent_core::OutputStream::Stderr => process.stderr.push(&buf[..n]),
                            }
                        }
                        drop(processes);
                        manager.emit(ProcessEvent::Output { process_id: process_id.clone() }).await;
                    }
                }
            }
        });
    }

    fn spawn_waiter(self: &Arc<Self>, process_id: String, child: Arc<Mutex<Child>>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let status = {
                let mut child = child.lock().await;
                child.wait().await.ok()
            };
            let code = status.and_then(|s| s.code());
            let mut processes = manager.processes.lock().await;
            if let Some(process) = processes.get_mut(&process_id) {
                if !matches!(process.status, ProcessStatus::Killed) {
                    process.exit_code = code;
                    process.end_time = Some(Instant::now());
                    process.status = ProcessStatus::Exited { code };
                    process.ready = false;
                }
                process.child = None;
            }
            drop(processes);
            manager.emit(ProcessEvent::Exited { process_id }).await;
        });
    }

    async fn wait_for_ready(&self, process_id: &str, pattern: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut events = self.subscribe();
        loop {
            if self.output_contains(process_id, pattern).await {
                return true;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return false,
                recv = events.recv() => {
                    if recv.is_err() {
                        return false;
                    }
                }
            }
        }
    }

    async fn output_contains(&self, process_id: &str, pattern: &str) -> bool {
        let processes = self.processes.lock().await;
        let Some(process) = processes.get(process_id) else { return false; };
        let (stdout, _, _) = process.stdout.read(None, DEFAULT_STREAM_CAP_BYTES);
        let (stderr, _, _) = process.stderr.read(None, DEFAULT_STREAM_CAP_BYTES);
        stdout.contains(pattern) || stderr.contains(pattern)
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}
```

- [ ] **Step 8: Run tests and commit**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::manager::tests -- --nocapture
cargo fmt --all
git add Cargo.toml crates/yi-agent-tools/Cargo.toml crates/yi-agent-tools/src/process/manager.rs
git commit -m "feat: manage background process lifecycle"
```

Expected: process manager tests pass. If a test hangs, follow AGENTS.md cargo guidance and inspect residual `yi_agent` test processes before rerunning.

---

### Task 3: Process Tool Implementations And Registration

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/process/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`

- [ ] **Step 1: Write failing tool tests**

Create `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs`:

```rust
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use super::manager::{OnExitPolicy, ProcessManager, ProcessSelector, ProcessStartOptions};

pub struct ProcessStartTool;
pub struct ProcessListTool;
pub struct ProcessReadTool;
pub struct ProcessKillTool;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use yi_agent_core::ContentBlock;

    fn text(result: &ToolResult) -> String {
        match &result.content[0] {
            ContentBlock::Text(s) => s.clone(),
            _ => panic!("expected text block"),
        }
    }

    #[tokio::test]
    async fn process_tools_start_list_read_kill() {
        let tmp = TempDir::new().unwrap();
        let manager = ProcessManager::new(tmp.path().to_path_buf());
        let start = ProcessStartTool::new(manager.clone());
        let list = ProcessListTool::new(manager.clone());
        let read = ProcessReadTool::new(manager.clone());
        let kill = ProcessKillTool::new(manager.clone());

        let start_result = start
            .call(serde_json::json!({
                "command": "printf ready; sleep 30",
                "name": "dev-server",
                "ready_pattern": "ready",
                "ready_timeout_sec": 3
            }))
            .await;
        assert!(!start_result.is_error, "{}", text(&start_result));
        assert!(text(&start_result).contains("proc_1"));

        let list_result = list.call(serde_json::json!({})).await;
        assert!(text(&list_result).contains("dev-server"));

        let read_result = read
            .call(serde_json::json!({"name": "dev-server"}))
            .await;
        assert!(text(&read_result).contains("ready"));

        let kill_result = kill
            .call(serde_json::json!({"process_id": "proc_1"}))
            .await;
        assert!(!kill_result.is_error, "{}", text(&kill_result));
    }

    #[test]
    fn process_tool_metadata_matches_permissions() {
        let tmp = TempDir::new().unwrap();
        let manager = ProcessManager::new(tmp.path().to_path_buf());

        assert!(ProcessStartTool::new(manager.clone()).metadata().requires_confirmation);
        assert!(!ProcessStartTool::new(manager.clone()).metadata().read_only);
        assert!(!ProcessListTool::new(manager.clone()).metadata().requires_confirmation);
        assert!(ProcessListTool::new(manager.clone()).metadata().read_only);
        assert!(!ProcessReadTool::new(manager.clone()).metadata().requires_confirmation);
        assert!(ProcessReadTool::new(manager.clone()).metadata().read_only);
        assert!(ProcessKillTool::new(manager.clone()).metadata().requires_confirmation);
        assert!(!ProcessKillTool::new(manager).metadata().read_only);
    }
}
```

- [ ] **Step 2: Run tool tests to verify they fail**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::tools::tests -- --nocapture
```

Expected: compile fails because the tool structs do not have constructors or `Tool` implementations.

- [ ] **Step 3: Implement process tools**

Replace the non-test body of `tools.rs` with:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use super::manager::{OnExitPolicy, ProcessManager, ProcessSelector, ProcessStartOptions};

#[derive(Deserialize)]
struct StartArgs {
    command: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    on_exit: OnExitPolicy,
    #[serde(default)]
    ready_pattern: Option<String>,
    #[serde(default)]
    ready_timeout_sec: Option<u64>,
}

#[derive(Deserialize)]
struct SelectArgs {
    #[serde(default)]
    process_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

fn selector(process_id: Option<String>, name: Option<String>) -> Result<ProcessSelector, String> {
    match (process_id, name) {
        (Some(id), None) => Ok(ProcessSelector::Id(id)),
        (None, Some(name)) => Ok(ProcessSelector::Name(name)),
        (None, None) => Err("one of process_id or name is required".into()),
        (Some(_), Some(_)) => Err("provide only one of process_id or name".into()),
    }
}

fn metadata(requires_confirmation: bool, read_only: bool) -> ToolMetadata {
    ToolMetadata {
        source: ToolSource::Builtin,
        requires_confirmation,
        read_only,
        version: None,
    }
}

pub struct ProcessStartTool {
    manager: Arc<ProcessManager>,
}

impl ProcessStartTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl Tool for ProcessStartTool {
    fn name(&self) -> &str { "process_start" }
    fn description(&self) -> &str { "Start a yi-agent-managed background process and return its process_id." }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "name": {"type": "string"},
                "cwd": {"type": "string"},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "on_exit": {"type": "string", "enum": ["kill", "keep"], "default": "kill"},
                "ready_pattern": {"type": "string"},
                "ready_timeout_sec": {"type": "integer"}
            },
            "required": ["command"]
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let args: StartArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        match self.manager.start(ProcessStartOptions {
            command: args.command,
            name: args.name,
            cwd: args.cwd,
            env: args.env,
            on_exit: args.on_exit,
            ready_pattern: args.ready_pattern,
            ready_timeout_sec: args.ready_timeout_sec,
        }).await {
            Ok(result) => ToolResult::text(serde_json::to_string_pretty(&result).unwrap()),
            Err(e) => ToolResult::error(e),
        }
    }
    fn metadata(&self) -> ToolMetadata { metadata(true, false) }
}

pub struct ProcessListTool {
    manager: Arc<ProcessManager>,
}

impl ProcessListTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self { Self { manager } }
}

#[async_trait]
impl Tool for ProcessListTool {
    fn name(&self) -> &str { "process_list" }
    fn description(&self) -> &str { "List yi-agent-managed background processes." }
    fn schema(&self) -> Value { serde_json::json!({"type": "object", "properties": {}}) }
    async fn call(&self, _args: Value) -> ToolResult {
        ToolResult::text(serde_json::to_string_pretty(&self.manager.list().await).unwrap())
    }
    fn metadata(&self) -> ToolMetadata { metadata(false, true) }
}

pub struct ProcessReadTool {
    manager: Arc<ProcessManager>,
}

impl ProcessReadTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self { Self { manager } }
}

#[async_trait]
impl Tool for ProcessReadTool {
    fn name(&self) -> &str { "process_read" }
    fn description(&self) -> &str { "Read stdout/stderr from a managed background process by process_id or name." }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {"type": "string"},
                "name": {"type": "string"},
                "cursor": {"type": "integer"},
                "max_bytes": {"type": "integer", "default": 65536}
            }
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let args: SelectArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let selector = match selector(args.process_id, args.name) {
            Ok(selector) => selector,
            Err(e) => return ToolResult::error(e),
        };
        match self.manager.read(selector, args.cursor, args.max_bytes.unwrap_or(64 * 1024)).await {
            Ok(result) => ToolResult::text(serde_json::to_string_pretty(&result).unwrap()),
            Err(e) => ToolResult::error(e),
        }
    }
    fn metadata(&self) -> ToolMetadata { metadata(false, true) }
}

pub struct ProcessKillTool {
    manager: Arc<ProcessManager>,
}

impl ProcessKillTool {
    pub fn new(manager: Arc<ProcessManager>) -> Self { Self { manager } }
}

#[async_trait]
impl Tool for ProcessKillTool {
    fn name(&self) -> &str { "process_kill" }
    fn description(&self) -> &str { "Kill a yi-agent-managed background process by process_id or name." }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {"type": "string"},
                "name": {"type": "string"}
            }
        })
    }
    async fn call(&self, args: Value) -> ToolResult {
        let args: SelectArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let selector = match selector(args.process_id, args.name) {
            Ok(selector) => selector,
            Err(e) => return ToolResult::error(e),
        };
        match self.manager.kill(selector).await {
            Ok(()) => ToolResult::text("killed"),
            Err(e) => ToolResult::error(e),
        }
    }
    fn metadata(&self) -> ToolMetadata { metadata(true, false) }
}
```

Keep the test module from Step 1 below this code.

- [ ] **Step 4: Export tools and registration helpers**

Modify `yi-agent-rs/crates/yi-agent-tools/src/process/mod.rs`:

```rust
pub mod manager;
pub mod tools;

pub use manager::{
    ManagedProcessSnapshot, OnExitPolicy, ProcessEvent, ProcessManager, ProcessReadResult,
    ProcessSelector, ProcessStartOptions, ProcessStartResult, ProcessStatus,
};
pub use tools::{ProcessKillTool, ProcessListTool, ProcessReadTool, ProcessStartTool};
```

Modify `yi-agent-rs/crates/yi-agent-tools/src/lib.rs` exports:

```rust
pub use process::{
    ManagedProcessSnapshot, OnExitPolicy, ProcessEvent, ProcessKillTool, ProcessListTool,
    ProcessManager, ProcessReadResult, ProcessReadTool, ProcessSelector, ProcessStartOptions,
    ProcessStartResult, ProcessStartTool, ProcessStatus,
};
```

Add this helper below `register_builtin_tools_with_sandbox` in `lib.rs`:

```rust
pub fn register_process_tools(registry: &mut ToolRegistry, manager: Arc<ProcessManager>) {
    registry.register(Arc::new(ProcessStartTool::new(manager.clone())));
    registry.register(Arc::new(ProcessListTool::new(manager.clone())));
    registry.register(Arc::new(ProcessReadTool::new(manager.clone())));
    registry.register(Arc::new(ProcessKillTool::new(manager)));
}
```

- [ ] **Step 5: Run process tool tests and commit**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process::tools::tests -- --nocapture
cargo test -p yi-agent-tools --lib process::manager::tests -- --nocapture
cargo fmt --all
git add crates/yi-agent-tools/src/lib.rs crates/yi-agent-tools/src/process/mod.rs crates/yi-agent-tools/src/process/tools.rs crates/yi-agent-tools/src/process/manager.rs
git commit -m "feat: add managed process tools"
```

Expected: manager and tool tests pass.

---

### Task 4: Runtime Wiring And Permission Coverage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

- [ ] **Step 1: Add failing runtime registration test**

In `yi-agent-rs/crates/yi-agent/src/main.rs`, find the existing tests that assert builtin tool names and add:

```rust
#[test]
fn default_mode_registers_process_tools_with_expected_permissions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_tools::register_builtin_tools_with_sandbox(
        &mut registry,
        tmp.path().to_path_buf(),
        yi_agent_tools::SandboxMode::DangerFullAccess,
        Vec::new(),
    );
    let manager = yi_agent_tools::ProcessManager::new(tmp.path().to_path_buf());
    yi_agent_tools::register_process_tools(&mut registry, manager);

    let start = registry.get("process_start").expect("process_start registered");
    let list = registry.get("process_list").expect("process_list registered");
    let read = registry.get("process_read").expect("process_read registered");
    let kill = registry.get("process_kill").expect("process_kill registered");

    assert!(start.metadata().requires_confirmation);
    assert!(!start.metadata().read_only);
    assert!(!list.metadata().requires_confirmation);
    assert!(list.metadata().read_only);
    assert!(!read.metadata().requires_confirmation);
    assert!(read.metadata().read_only);
    assert!(kill.metadata().requires_confirmation);
    assert!(!kill.metadata().read_only);
}
```

- [ ] **Step 2: Run test to verify the runtime path still needs wiring**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent default_mode_registers_process_tools_with_expected_permissions -- --exact
```

Expected: test passes once Task 3 is complete. This test locks metadata behavior before main runtime wiring changes.

- [ ] **Step 3: Wire process manager into TUI and run mode setup**

In `main.rs`, after each call to `register_builtin_tools_with_sandbox(...)`, create a manager and register process tools:

```rust
let process_manager = yi_agent_tools::ProcessManager::with_sandbox(
    config.workdir.clone(),
    yi_agent_tools::SandboxPolicy::new(
        config.sandbox,
        config.workdir.clone(),
        config.sandbox_writable_roots.clone(),
    ),
);
yi_agent_tools::register_process_tools(&mut registry, process_manager.clone());
```

Use this in both normal TUI setup and `yi-agent run` setup. Pass `process_manager.clone()` into `run_tui_agent` in the TUI path. In `run` mode, keep the manager local and call `let retained = process_manager.shutdown().await;` before returning the exit code. For each retained process, write a line to stderr:

```rust
eprintln!(
    "retained process: id={} name={} pid={:?}",
    process.process_id,
    process.name.unwrap_or_else(|| "-".into()),
    process.pid
);
```

- [ ] **Step 4: Update `run_tui_agent` signature**

Change the `run_tui_agent` function signature to accept:

```rust
process_manager: Arc<yi_agent_tools::ProcessManager>,
```

Pass it through to `tui::run(...)`. If `tui::run` does not yet accept it, Task 6 will add that parameter; temporarily keep it available at the call site and let Task 6 complete the compile.

- [ ] **Step 5: Run focused registration tests and commit after Task 6 compiles**

Do not commit this task until Task 6 completes the TUI signature wiring. After Task 6, run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent default_mode_registers_process_tools_with_expected_permissions -- --exact
cargo test -p yi-agent-tools --lib process::tools::tests -- --nocapture
cargo fmt --all
git add crates/yi-agent/src/main.rs
git commit -m "feat: register managed process tools"
```

Expected: tests pass and `main.rs` compiles with the TUI signature changes from Task 6.

---

### Task 5: Process Popup Rendering Unit

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/process_popup.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`

- [ ] **Step 1: Add failing popup tests and types**

Create `yi-agent-rs/crates/yi-agent/src/tui/process_popup.rs`:

```rust
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use yi_agent_tools::{ManagedProcessSnapshot, ProcessReadResult, ProcessStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTab {
    BashTasks,
    Processes,
}

#[derive(Debug, Clone)]
pub enum ProcessPopup {
    List(ProcessListPopup),
    Detail(ProcessDetailPopup),
    ConfirmKill(ConfirmProcessKill),
}

#[derive(Debug, Clone)]
pub struct ProcessListPopup {
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct ProcessDetailPopup {
    pub process_id: String,
    pub scroll: usize,
    pub scroll_locked: bool,
}

#[derive(Debug, Clone)]
pub struct ConfirmProcessKill {
    pub process_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_tools::OnExitPolicy;

    fn snapshot(id: &str, name: Option<&str>, status: ProcessStatus) -> ManagedProcessSnapshot {
        ManagedProcessSnapshot {
            process_id: id.into(),
            name: name.map(str::to_string),
            pid: Some(1234),
            command: "python -m http.server".into(),
            cwd: "/tmp".into(),
            status,
            ready: true,
            on_exit: OnExitPolicy::Kill,
            exit_code: None,
            elapsed_sec: 1.2,
        }
    }

    #[test]
    fn process_list_popup_selects_and_moves() {
        let mut popup = ProcessListPopup::new();
        let processes = vec![
            snapshot("proc_1", Some("a"), ProcessStatus::Running),
            snapshot("proc_2", Some("b"), ProcessStatus::Ready),
        ];

        assert_eq!(popup.selected_id(&processes), Some("proc_1"));
        popup.move_down(processes.len());
        assert_eq!(popup.selected_id(&processes), Some("proc_2"));
        popup.move_up();
        assert_eq!(popup.selected_id(&processes), Some("proc_1"));
    }

    #[test]
    fn render_process_list_includes_name_status_and_pid() {
        let popup = ProcessListPopup::new();
        let processes = vec![snapshot("proc_1", Some("dev"), ProcessStatus::Ready)];
        let paragraph = render_process_list_popup(&popup, &processes, Rect::new(0, 0, 80, 8));
        let text = format!("{:?}", paragraph);

        assert!(text.contains("dev"));
        assert!(text.contains("ready"));
        assert!(text.contains("1234"));
    }

    #[test]
    fn detail_lines_include_stdout_and_stderr() {
        let process = snapshot("proc_1", Some("dev"), ProcessStatus::Ready);
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: "listening\n".into(),
            stderr: "warn\n".into(),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Ready,
            ready: true,
        };

        let lines = process_detail_lines(&process, Some(&output), 80);
        let text = format!("{:?}", lines);

        assert!(text.contains("listening"));
        assert!(text.contains("warn"));
        assert!(text.contains("proc_1"));
    }
}
```

Modify `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`:

```rust
pub mod process_popup;
```

- [ ] **Step 2: Run popup tests to verify they fail**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::process_popup::tests -- --nocapture
```

Expected: compile fails because constructors and rendering functions are missing.

- [ ] **Step 3: Implement process popup logic**

Add this code above the test module in `process_popup.rs`:

```rust
impl RuntimeTab {
    pub fn next(self) -> Self {
        match self {
            Self::BashTasks => Self::Processes,
            Self::Processes => Self::BashTasks,
        }
    }
}

impl ProcessListPopup {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn selected_id<'a>(&self, processes: &'a [ManagedProcessSnapshot]) -> Option<&'a str> {
        processes.get(self.selected).map(|p| p.process_id.as_str())
    }
}

impl Default for ProcessListPopup {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessDetailPopup {
    pub fn new(process_id: String) -> Self {
        Self {
            process_id,
            scroll: 0,
            scroll_locked: true,
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        self.scroll_locked = false;
    }

    pub fn scroll_down(&mut self, n: usize, max: usize) {
        self.scroll = (self.scroll + n).min(max);
        if self.scroll >= max {
            self.scroll_locked = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_locked = true;
    }
}

fn status_word(status: &ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Starting => "starting",
        ProcessStatus::Running => "running",
        ProcessStatus::Ready => "ready",
        ProcessStatus::Exited { .. } => "exited",
        ProcessStatus::Killed => "killed",
        ProcessStatus::FailedToStart { .. } => "failed",
    }
}

fn status_color(status: &ProcessStatus) -> Color {
    match status {
        ProcessStatus::Starting | ProcessStatus::Running => Color::Yellow,
        ProcessStatus::Ready => Color::Green,
        ProcessStatus::Exited { .. } => Color::DarkGray,
        ProcessStatus::Killed | ProcessStatus::FailedToStart { .. } => Color::Red,
    }
}

pub fn render_process_list_popup<'a>(
    popup: &'a ProcessListPopup,
    processes: &'a [ManagedProcessSnapshot],
    _area: Rect,
) -> Paragraph<'a> {
    let lines: Vec<Line> = processes
        .iter()
        .enumerate()
        .map(|(i, process)| {
            let name = process.name.as_deref().unwrap_or("-");
            let pid = process.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
            let style = if i == popup.selected {
                Style::new().bg(Color::Blue).fg(Color::White)
            } else {
                Style::new().fg(status_color(&process.status))
            };
            Line::styled(
                format!(
                    " {:<10} {:<16} pid={:<8} {:>6.1}s {}",
                    process.process_id,
                    name,
                    pid,
                    process.elapsed_sec,
                    status_word(&process.status)
                ),
                style,
            )
        })
        .collect();

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("processes (↑↓ select, Enter open, Tab switch, q close)"),
    )
}

pub fn render_process_detail_popup(
    popup: &ProcessDetailPopup,
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    area: Rect,
) -> Paragraph<'static> {
    Paragraph::new(process_detail_lines(process, output, area.width))
        .alignment(Alignment::Left)
        .scroll((popup.scroll as u16, 0))
}

pub fn process_detail_line_count(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    width: u16,
) -> usize {
    process_detail_lines(process, output, width).len()
}

pub fn process_detail_lines(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    _width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let name = process.name.as_deref().unwrap_or("-");
    lines.push(Line::styled(
        format!(
            " process {} name={} pid={:?} status={} ready={}",
            process.process_id,
            name,
            process.pid,
            status_word(&process.status),
            process.ready
        ),
        Style::new().fg(status_color(&process.status)),
    ));
    lines.push(Line::raw(format!(" cwd: {}", process.cwd)));
    lines.push(Line::raw(format!(" cmd: {}", process.command)));
    lines.push(Line::raw(format!(" on_exit: {:?}", process.on_exit)));
    lines.push(Line::raw(""));
    lines.push(Line::styled("stdout:", Style::new().fg(Color::DarkGray)));
    match output.map(|o| o.stdout.as_str()).filter(|s| !s.is_empty()) {
        Some(stdout) => lines.extend(stdout.lines().map(|line| Line::raw(line.to_string()))),
        None => lines.push(Line::styled("(empty)", Style::new().fg(Color::DarkGray))),
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("stderr:", Style::new().fg(Color::DarkGray)));
    match output.map(|o| o.stderr.as_str()).filter(|s| !s.is_empty()) {
        Some(stderr) => lines.extend(stderr.lines().map(|line| Line::raw(line.to_string()))),
        None => lines.push(Line::styled("(empty)", Style::new().fg(Color::DarkGray))),
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " [q] back  [k] kill  [↑↓] scroll  [f] follow  [Tab] switch",
        Style::new().fg(Color::DarkGray),
    ));
    lines
}
```

- [ ] **Step 4: Run popup tests and commit**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::process_popup::tests -- --nocapture
cargo fmt --all
git add crates/yi-agent/src/tui/mod.rs crates/yi-agent/src/tui/process_popup.rs
git commit -m "feat(tui): render managed process popup"
```

Expected: process popup tests pass.

---

### Task 6: Ctrl+P Tab Integration And Process Kill UI

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

- [ ] **Step 1: Add failing TUI state-machine tests**

Add tests to the existing `#[cfg(test)] mod tests` in `tui/app.rs`:

```rust
#[test]
fn ctrl_p_process_tab_kill_confirmation_sends_process_id() {
    let mut runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
        crate::tui::process_popup::ProcessDetailPopup::new("proc_1".into()),
    ));
    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('k'),
        crossterm::event::KeyModifiers::NONE,
    );

    handle_runtime_popup_key_for_test(key, &mut runtime_popup, &[], &[]);

    assert!(matches!(
        runtime_popup,
        RuntimePopup::Processes(ProcessPopup::ConfirmKill(_))
    ));
}

#[test]
fn runtime_popup_tab_switches_between_bash_and_processes() {
    let mut popup = RuntimePopup::Bash(crate::tui::bash_popup::BashPopup::List(
        crate::tui::bash_popup::ListPopup::new(vec!["bash_1".into()]),
    ));

    popup.switch_tab(Vec::new());

    assert!(matches!(popup, RuntimePopup::Processes(ProcessPopup::List(_))));
}
```

These tests require adding `RuntimePopup`, `ProcessPopup` imports, and test helper functions in later steps.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent runtime_popup_ ctrl_p_process_tab_ -- --nocapture
```

Expected: compile fails because `RuntimePopup` and helper functions are missing.

- [ ] **Step 3: Add runtime popup state**

At the top of `tui/app.rs`, import process popup types:

```rust
use super::process_popup::{
    ConfirmProcessKill, ProcessDetailPopup, ProcessListPopup, ProcessPopup,
};
```

Add this enum near the existing popup state helpers:

```rust
#[derive(Debug, Clone)]
enum RuntimePopup {
    None,
    Bash(BashPopup),
    Processes(ProcessPopup),
}

impl RuntimePopup {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    fn switch_tab(&mut self, process_ids: Vec<String>) {
        *self = match self {
            Self::Bash(_) => Self::Processes(ProcessPopup::List(ProcessListPopup::new())),
            Self::Processes(_) => Self::Bash(BashPopup::List(ListPopup::new(process_ids))),
            Self::None => Self::None,
        };
    }
}
```

- [ ] **Step 4: Change `run` and `run_tui` signatures**

Update TUI entry functions in `app.rs` to accept:

```rust
process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
```

Thread that parameter from `main.rs` into `run_tui`. Inside the event loop, create:

```rust
let mut runtime_popup = RuntimePopup::None;
let mut process_snapshots: Vec<yi_agent_tools::ManagedProcessSnapshot> = Vec::new();
let mut process_outputs: std::collections::HashMap<String, yi_agent_tools::ProcessReadResult> = std::collections::HashMap::new();
let mut process_events = process_manager.subscribe();
```

- [ ] **Step 5: Poll process events and refresh snapshots**

In the event loop before drawing, drain process events:

```rust
while process_events.try_recv().is_ok() {
    process_snapshots = process_manager.list().await;
    for snapshot in &process_snapshots {
        if let Ok(output) = process_manager
            .read(
                yi_agent_tools::ProcessSelector::Id(snapshot.process_id.clone()),
                None,
                64 * 1024,
            )
            .await
        {
            process_outputs.insert(snapshot.process_id.clone(), output);
        }
    }
}
```

Also refresh `process_snapshots` when Ctrl+P opens the process tab.

- [ ] **Step 6: Render runtime popup tabs**

Replace bash-only popup rendering with a match on `runtime_popup`:

```rust
match &runtime_popup {
    RuntimePopup::None => {}
    RuntimePopup::Bash(bash_popup) => {
        render_existing_bash_popup(f, bash_popup, &task_registry, chunks[0]);
    }
    RuntimePopup::Processes(ProcessPopup::List(p)) => {
        f.render_widget(
            super::process_popup::render_process_list_popup(p, &process_snapshots, chunks[0]),
            chunks[0],
        );
    }
    RuntimePopup::Processes(ProcessPopup::Detail(p)) => {
        if let Some(process) = process_snapshots.iter().find(|p2| p2.process_id == p.process_id) {
            let output = process_outputs.get(&p.process_id);
            f.render_widget(
                super::process_popup::render_process_detail_popup(p, process, output, chunks[0]),
                chunks[0],
            );
        }
    }
    RuntimePopup::Processes(ProcessPopup::ConfirmKill(ck)) => {
        if let Some(process) = process_snapshots.iter().find(|p| p.process_id == ck.process_id) {
            let detail = ProcessDetailPopup::new(ck.process_id.clone());
            let output = process_outputs.get(&ck.process_id);
            f.render_widget(
                super::process_popup::render_process_detail_popup(&detail, process, output, chunks[0]),
                chunks[0],
            );
            render_kill_confirmation_overlay(f, chunks[0], "kill this managed process?");
        }
    }
}
```

Extract existing bash rendering into `render_existing_bash_popup(...)` and existing kill overlay drawing into `render_kill_confirmation_overlay(...)` so the match stays small.

- [ ] **Step 7: Add key handling for runtime popup**

Add a helper in `app.rs`:

```rust
fn handle_runtime_popup_key_for_test(
    key: KeyEvent,
    runtime_popup: &mut RuntimePopup,
    bash_ids: &[String],
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
) {
    match runtime_popup {
        RuntimePopup::None => {}
        RuntimePopup::Bash(bash_popup) => {
            if key.code == KeyCode::Tab {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
            } else {
                handle_bash_popup_key(key, bash_popup, &RunningTaskRegistry::new(), 80);
            }
        }
        RuntimePopup::Processes(ProcessPopup::List(p)) => match key.code {
            KeyCode::Tab => {
                *runtime_popup = RuntimePopup::Bash(BashPopup::List(ListPopup::new(bash_ids.to_vec())));
            }
            KeyCode::Up => p.move_up(),
            KeyCode::Down => p.move_down(processes.len()),
            KeyCode::Enter => {
                if let Some(id) = p.selected_id(processes) {
                    *runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(ProcessDetailPopup::new(id.to_string())));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => *runtime_popup = RuntimePopup::None,
            _ => {}
        },
        RuntimePopup::Processes(ProcessPopup::Detail(d)) => match key.code {
            KeyCode::Tab => {
                *runtime_popup = RuntimePopup::Bash(BashPopup::List(ListPopup::new(bash_ids.to_vec())));
            }
            KeyCode::Char('k') => {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::ConfirmKill(ConfirmProcessKill {
                    process_id: d.process_id.clone(),
                }));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
            }
            KeyCode::Up => d.scroll_up(1),
            KeyCode::Down => d.scroll_down(1, 1000),
            KeyCode::Char('f') => d.scroll_to_bottom(),
            _ => {}
        },
        RuntimePopup::Processes(ProcessPopup::ConfirmKill(ck)) => match key.code {
            KeyCode::Char('n') | KeyCode::Esc => {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(ProcessDetailPopup::new(ck.process_id.clone())));
            }
            KeyCode::Char('y') => {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
            }
            _ => {}
        },
    }
}
```

In production key handling, use the same state transitions but when `ConfirmKill` receives `y`, call:

```rust
let _ = process_manager
    .kill(yi_agent_tools::ProcessSelector::Id(ck.process_id.clone()))
    .await;
```

- [ ] **Step 8: Run TUI tests and commit Tasks 4 and 6 together**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent runtime_popup_ ctrl_p_process_tab_ -- --nocapture
cargo test -p yi-agent --bin yi-agent tui::process_popup::tests -- --nocapture
cargo test -p yi-agent --bin yi-agent default_mode_registers_process_tools_with_expected_permissions -- --exact
cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): add managed process tab"
```

Expected: focused TUI and registration tests pass.

---

### Task 7: Shutdown Reporting For Retained Processes

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

- [ ] **Step 1: Add focused shutdown formatting test**

In `main.rs` tests, add:

```rust
#[test]
fn retained_process_message_includes_id_name_and_pid() {
    let snapshot = yi_agent_tools::ManagedProcessSnapshot {
        process_id: "proc_1".into(),
        name: Some("dev".into()),
        pid: Some(1234),
        command: "sleep 30".into(),
        cwd: "/tmp".into(),
        status: yi_agent_tools::ProcessStatus::Running,
        ready: true,
        on_exit: yi_agent_tools::OnExitPolicy::Keep,
        exit_code: None,
        elapsed_sec: 1.0,
    };

    let line = retained_process_message(&snapshot);

    assert!(line.contains("proc_1"));
    assert!(line.contains("dev"));
    assert!(line.contains("1234"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent retained_process_message_includes_id_name_and_pid -- --exact
```

Expected: compile fails because `retained_process_message` is missing.

- [ ] **Step 3: Implement retained process message helper and shutdown use**

Add to `main.rs`:

```rust
fn retained_process_message(process: &yi_agent_tools::ManagedProcessSnapshot) -> String {
    format!(
        "retained process: id={} name={} pid={:?}",
        process.process_id,
        process.name.as_deref().unwrap_or("-"),
        process.pid
    )
}
```

Use this helper anywhere shutdown reports retained `on_exit="keep"` processes:

```rust
for process in retained {
    eprintln!("{}", retained_process_message(&process));
}
```

In TUI shutdown, after the terminal is restored, print retained process messages to stderr so the user sees what stayed alive.

- [ ] **Step 4: Run focused tests and commit**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent retained_process_message_includes_id_name_and_pid -- --exact
cargo fmt --all
git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/app.rs
git commit -m "feat: report retained managed processes"
```

Expected: retained-process message test passes.

---

### Task 8: Project Management Docs

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/README.md`

- [ ] **Step 1: Update yi-agent-tools feature tracking**

In `docs/project-management/yi-agent-tools.md`, add one completed feature under `## Features`:

```markdown
- [x] Managed process tools — `crates/yi-agent-tools/src/process/` provides `ProcessManager` plus `process_start` / `process_list` / `process_read` / `process_kill`, with bounded stdout/stderr buffers, readiness matching, unique names, and managed kill; verification: `cargo test -p yi-agent-tools --lib process::`
```

- [ ] **Step 2: Update yi-agent-tui feature tracking**

In `docs/project-management/yi-agent-tui.md`, add one completed feature under `## Features`:

```markdown
- [x] Ctrl+P managed process tab — `tui/process_popup.rs` and `tui/app.rs` add a `Processes` tab beside Bash Tasks, showing managed process status/output and kill confirmation; verification: `cargo test -p yi-agent --bin yi-agent tui::process_popup::tests` and `cargo test -p yi-agent --bin yi-agent runtime_popup_`
```

- [ ] **Step 3: Update README counts**

In `docs/project-management/README.md`, update counts:

```markdown
| yi-agent-tools | 9 / 9 | [详情](./yi-agent-tools.md) |
| yi-agent-tui | 22 / 22 | [详情](./yi-agent-tui.md) |
```

If another branch changed these counts, increment the current totals by one for each module instead of forcing the numbers above.

- [ ] **Step 4: Run docs grep and commit**

Run:

```bash
rg -n "\[~\]|Managed process tools|Ctrl\+P managed process tab" docs/project-management
cd yi-agent-rs
cargo fmt --all
git add ../docs/project-management/yi-agent-tools.md ../docs/project-management/yi-agent-tui.md ../docs/project-management/README.md
git commit -m "docs: track managed process support"
```

Expected: `rg` shows no forbidden in-progress status markers, and it shows the two new feature rows.

---

### Task 9: Final Verification

**Files:**
- No source changes expected.

- [ ] **Step 1: Check for residual cargo/test processes**

Run:

```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || true
```

Expected: no unrelated cargo/rustc/yi_agent test processes are running.

- [ ] **Step 2: Run focused verification suite**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent-tools --lib process:: -- --nocapture
cargo test -p yi-agent --bin yi-agent tui::process_popup::tests -- --nocapture
cargo test -p yi-agent --bin yi-agent runtime_popup_ -- --nocapture
cargo test -p yi-agent --bin yi-agent default_mode_registers_process_tools_with_expected_permissions -- --exact
cargo test -p yi-agent --bin yi-agent retained_process_message_includes_id_name_and_pid -- --exact
cargo fmt --all
```

Expected: all focused tests pass and formatting completes without changes.

- [ ] **Step 3: Inspect git history and status**

Run:

```bash
git status --short
git log --oneline -8
```

Expected: clean status and recent commits for the managed process implementation.

- [ ] **Step 4: Summarize implementation evidence**

Prepare a final handoff note listing:

```text
Implemented:
- ProcessManager start/list/read/kill/shutdown
- process_start/process_list/process_read/process_kill tools
- Ctrl+P Processes tab and kill confirmation
- project-management docs

Verified:
- cargo test -p yi-agent-tools --lib process:: -- --nocapture
- cargo test -p yi-agent --bin yi-agent tui::process_popup::tests -- --nocapture
- cargo test -p yi-agent --bin yi-agent runtime_popup_ -- --nocapture
- cargo fmt --all
```
