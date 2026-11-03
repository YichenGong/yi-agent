use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::time;

use crate::process_group::{self, configure_process_group, signal_process_group};
use crate::sandbox::{SandboxController, SandboxMode, SandboxPolicy};
use crate::shell::blocklist;

pub const DEFAULT_STREAM_CAP_BYTES: usize = 256 * 1024;

/// kill 时留给进程响应 SIGTERM 的宽限期。
const KILL_GRACE: Duration = Duration::from_secs(3);
/// 升级到 SIGKILL 后等待整组消散的上限。
const KILL_REAP_WAIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

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
    /// 启动时登记的意图，终态也保留原值。
    pub prevent_idle_sleep: bool,
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
    /// 启动时登记的意图，终态也保留原值。
    pub prevent_idle_sleep: bool,
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
    /// 调用方显式请求「在这段有限的后台工作活着期间别让机器空闲睡眠」。
    ///
    /// 只记录**意图**：它不按 `on_exit` / readiness / 已运行多久被改写，终态也
    /// 保留原值（`list`/`read` 照原样回报）。真正的电源断言只由 app-server 会话
    /// 聚合（见 `power_busy`），其它入口只保留这个意图。
    pub prevent_idle_sleep: bool,
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
    /// 原样复制启动时收到的意图，不随当前状态改写。
    pub prevent_idle_sleep: bool,
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

/// Either a child this process owns, or one the guardian owns on its behalf.
///
/// The waiter and the kill path need one shape for both, because a managed runtime
/// must not quietly change how a process is reported or stopped -- it only changes
/// *who* can stop it.
enum SpawnedChild {
    Local(tokio::process::Child),
    Managed(crate::managed_process::ManagedChild),
}

impl SpawnedChild {
    fn local_pid(&self) -> Option<u32> {
        match self {
            Self::Local(child) => child.id(),
            // The guardian owns the group; this process must not signal it.
            Self::Managed(_) => None,
        }
    }

    async fn try_exit(&mut self) -> Option<Option<i32>> {
        match self {
            Self::Local(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.code()),
                Ok(None) => None,
                // An error is terminal: the handle cannot produce an exit later.
                Err(_) => Some(None),
            },
            Self::Managed(child) => match child.try_wait().await {
                Ok(Some(exit)) => Some(exit.code),
                Ok(None) => None,
                Err(_) => Some(None),
            },
        }
    }

    /// Terminates the whole group and does not return before it is gone.
    ///
    /// A local child keeps the escalation this path always had: SIGTERM to the
    /// group, a grace period, then SIGKILL for descendants that ignore it.
    /// `start_kill()` alone would `SIGKILL` the leader before its own handler
    /// ran -- exactly what the graceful-stop test pins down.
    async fn terminate(mut self, pid: Option<u32>) {
        match &mut self {
            Self::Local(child) => {
                let Some(pid) = pid else {
                    // No pid means the spawn never completed; the handle is all
                    // there is to kill.
                    let _ = child.start_kill();
                    return;
                };
                signal_process_group(pid, libc::SIGTERM);
                if local_group_drained(child, pid, KILL_GRACE).await {
                    return;
                }
                // Grace expired: take the whole group, including grandchildren
                // that ignore SIGTERM. The extra `start_kill()` covers a leader
                // whose group signal landed nowhere (already gone, pid reused).
                signal_process_group(pid, libc::SIGKILL);
                let _ = child.start_kill();
                local_group_drained(child, pid, KILL_REAP_WAIT).await;
            }
            Self::Managed(child) => {
                // Group kill plus the proof it drained: the guardian's answer is
                // the only authority on whether this group stopped.
                let _ = child
                    .terminate_group(std::time::Duration::from_secs(10))
                    .await;
            }
        }
    }
}

/// Waits until the direct child has been reaped *and* the process group is empty.
///
/// Both conditions are needed: reaping the leader says nothing about its
/// descendants, which is exactly the case this wait exists for.
async fn local_group_drained(
    child: &mut tokio::process::Child,
    pid: u32,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let reaped = !matches!(child.try_wait(), Ok(None));
        if reaped && !process_group::group_exists(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        time::sleep(POLL_INTERVAL).await;
    }
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
    child: Option<SpawnedChild>,
    /// 启动时登记的休眠保护意图。只在构造时写，之后不因状态变化改写。
    prevent_idle_sleep: bool,
}

pub struct ProcessManager {
    root: PathBuf,
    sandbox: SandboxPolicy,
    next_id: AtomicU64,
    shutting_down: AtomicBool,
    processes: Arc<Mutex<HashMap<String, ManagedProcess>>>,
    events: broadcast::Sender<ProcessEvent>,
    /// Who owns processes this manager starts.
    ///
    /// `Unmanaged` keeps the original behaviour: a manual daemon or a test has no
    /// guardian to register with and makes no promise that quitting stops
    /// anything.
    policy: crate::managed_process::ManagedProcessPolicy,
    spawner: Option<crate::managed_process::ManagedSpawner>,
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
            policy: crate::managed_process::ManagedProcessPolicy::Unmanaged,
            spawner: None,
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

    /// The same manager, with every process it starts owned by one frontend.
    ///
    /// Consumption is deliberate: switching owners mid-session would leave earlier
    /// processes under the old owner, and "which client owns this?" would stop
    /// having an answer.
    pub fn into_managed(
        self: Arc<Self>,
        policy: crate::managed_process::ManagedProcessPolicy,
        spawner: crate::managed_process::ManagedSpawner,
    ) -> Arc<Self> {
        // The fields are immutable after construction, so a managed manager is a
        // rebuilt one; there is no window where it is half-managed.
        Arc::new(Self {
            root: self.root.clone(),
            sandbox: self.sandbox.clone(),
            next_id: AtomicU64::new(self.next_id.load(Ordering::SeqCst)),
            shutting_down: AtomicBool::new(self.shutting_down.load(Ordering::SeqCst)),
            processes: Arc::new(Mutex::new(HashMap::new())),
            events: self.events.clone(),
            policy,
            spawner: Some(spawner),
        })
    }

    fn managed_policy(&self) -> &crate::managed_process::ManagedProcessPolicy {
        &self.policy
    }

    fn managed_spawner(&self) -> Option<&crate::managed_process::ManagedSpawner> {
        self.spawner.as_ref()
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

        // `keep` asks for a process that outlives the work that started it. In a
        // managed runtime that is precisely the promise the product makes and then
        // must not break, so it is refused here rather than silently turned into
        // `kill`.
        let on_exit_value = match opts.on_exit {
            OnExitPolicy::Kill => "kill",
            OnExitPolicy::Keep => "keep",
        };
        crate::managed_process::validate_on_exit(self.managed_policy(), on_exit_value)?;
        let process_id = self.next_process_id();
        let name = opts.name.clone();
        let on_exit = opts.on_exit;
        let prevent_idle_sleep = opts.prevent_idle_sleep;
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
            if let Some(name) = &name
                && processes
                    .values()
                    .any(|process| process.name.as_ref() == Some(name))
            {
                return Err("process name already exists".into());
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
                    prevent_idle_sleep,
                },
            );
        }

        // The unmanaged command is still built: when the runtime is managed it is
        // simply not spawned, and building it keeps one code path for the sandbox
        // wrapper's arguments.
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        cmd.envs(opts.env.iter());
        configure_process_group(&mut cmd);

        let (child, pid, stdout, stderr) = match self.managed_spawner() {
            Some(spawner) => {
                // The guardian execs exactly the sandbox wrapper the unmanaged path
                // would have run, so file permissions are unchanged; only
                // ownership moves.
                let executable = crate::managed_process::resolve_program(&program)
                    .map_err(|error| error.to_string())?;
                let env: Vec<(String, String)> = opts
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                let mut managed = spawner
                    .spawn(&executable, &args, &env, &cwd)
                    .await
                    .inspect_err(|_| {
                        self.lock_processes().remove(&process_id);
                    })
                    .map_err(|err| format!("failed to start process: {err}"))?;
                let stdout = managed
                    .stdout
                    .take()
                    .map(crate::managed_process::into_reader);
                let stderr = managed
                    .stderr
                    .take()
                    .map(crate::managed_process::into_reader);
                (SpawnedChild::Managed(managed), None, stdout, stderr)
            }
            None => {
                let mut child = cmd
                    .spawn()
                    .inspect_err(|_| {
                        self.lock_processes().remove(&process_id);
                    })
                    .map_err(|err| format!("failed to start process: {err}"))?;
                let pid = child.id();
                let stdout = child.stdout.take().map(crate::managed_process::into_reader);
                let stderr = child.stderr.take().map(crate::managed_process::into_reader);
                (SpawnedChild::Local(child), pid, stdout, stderr)
            }
        };

        // The child is moved out so both branches can own it: the rejection path
        // has to terminate it, and the accepted path hands it to the record.
        let mut pending = Some(child);
        let rejected = {
            let mut processes = self.lock_processes();
            if self.shutting_down.load(Ordering::SeqCst) {
                processes.remove(&process_id);
                true
            } else {
                let process = processes
                    .get_mut(&process_id)
                    .ok_or_else(|| format!("process reservation missing: {process_id}"))?;
                process.pid = pid;
                process.status = ProcessStatus::Running;
                process.child = pending.take();
                false
            }
        };
        if rejected {
            // Nothing was recorded, so this is only cleanup: stop whatever was just
            // started, through whichever authority owns it.
            if let Some(child) = pending.take() {
                child.terminate(pid).await;
            }
            return Err("process manager is shutting down".into());
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
            prevent_idle_sleep: snapshot.prevent_idle_sleep,
        })
    }

    /// 是否存在**仍在活跃**、且显式请求了防休眠的后台进程。
    ///
    /// 只读内存表的轻量谓词（不 spawn、不读盘、不 fork）：主循环每 10 秒采样一次
    /// 电源 busy，这里必须是常数级工作。`Starting` 也算活跃——它已经占了位，正
    /// 在等 readiness；`Ready` 不是终态，`on_exit` / readiness 都不改变意图。
    pub fn has_sleep_preventing_processes(&self) -> bool {
        self.lock_processes().values().any(|p| {
            p.prevent_idle_sleep
                && matches!(
                    p.status,
                    ProcessStatus::Starting | ProcessStatus::Running | ProcessStatus::Ready
                )
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
                    prevent_idle_sleep: process.prevent_idle_sleep,
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
                    prevent_idle_sleep: process.prevent_idle_sleep,
                })
            }
        }
    }

    pub async fn kill(&self, selector: ProcessSelector) -> Result<(), String> {
        let process_id = self.selector_to_id(&selector)?;
        let (pid, child, child_was_running) = {
            let mut processes = self.lock_processes();
            let process = processes
                .get_mut(&process_id)
                .ok_or_else(|| format!("process not found: {process_id}"))?;
            if is_terminal(&process.status) {
                return Ok(());
            }
            let pid = process
                .child
                .as_ref()
                .and_then(|child| child.local_pid())
                .or(process.pid);
            process.status = ProcessStatus::Killed;
            process.ready = false;
            process.end_time = Some(Instant::now());
            // Whether there was a child at all decides the event: a process whose
            // waiter already observed the exit is not "killed" by this call.
            let had_child = process.child.is_some();
            let child = process.child.take();
            (pid, child, had_child)
        };
        // Signalling belongs to `terminate` for both shapes. Signalling here as
        // well would send SIGTERM twice and let the escalation run behind its back.
        if let Some(child) = child {
            child.terminate(pid).await;
        }
        if child_was_running {
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
            prevent_idle_sleep: process.prevent_idle_sleep,
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
                // The probe happens outside the lock. A managed `try_exit` sends a
                // request to the guardian, and holding the process mutex across
                // that await would both block every other tool and make this task
                // non-`Send`.
                let outcome = {
                    // A local child is polled under the lock (a `try_wait` does not
                    // yield); a managed one has to await the guardian, so its
                    // request runs outside the lock and the result is written back
                    // afterwards.
                    enum Step {
                        Local(Option<Option<i32>>),
                        Remote,
                        Gone,
                    }
                    let step = {
                        let mut processes = manager.lock_processes();
                        match processes.get_mut(&process_id) {
                            Some(process) => match process.child.as_mut() {
                                Some(SpawnedChild::Local(child)) => {
                                    Step::Local(match child.try_wait() {
                                        Ok(Some(status)) => Some(status.code()),
                                        Ok(None) => None,
                                        Err(_) => Some(None),
                                    })
                                }
                                Some(SpawnedChild::Managed(_)) => Step::Remote,
                                None => Step::Gone,
                            },
                            None => Step::Gone,
                        }
                    };
                    match step {
                        Step::Local(value) => value,
                        Step::Gone => return,
                        Step::Remote => {
                            // Poll the guardian without the lock. The managed child
                            // is taken out, asked, and put back, so no other path
                            // can observe a half-updated handle.
                            let taken = {
                                let mut processes = manager.lock_processes();
                                processes
                                    .get_mut(&process_id)
                                    .and_then(|process| process.child.take())
                            };
                            let Some(mut child) = taken else {
                                return;
                            };
                            let outcome = child.try_exit().await;
                            let mut processes = manager.lock_processes();
                            match processes.get_mut(&process_id) {
                                Some(process) => {
                                    if outcome.is_none() {
                                        process.child = Some(child);
                                    }
                                    outcome
                                }
                                None => outcome,
                            }
                        }
                    }
                };

                let event = {
                    let mut processes = manager.lock_processes();
                    let Some(process) = processes.get_mut(&process_id) else {
                        return;
                    };
                    match outcome {
                        Some(code) => {
                            process.child = None;
                            process.ready = false;
                            process.end_time.get_or_insert_with(Instant::now);
                            // `Killed` was recorded by `kill`, so the exit must be
                            // reported as a kill rather than a normal exit with the
                            // signal-derived code.
                            if matches!(process.status, ProcessStatus::Killed) {
                                Some(ProcessEvent::Killed {
                                    process_id: process_id.clone(),
                                })
                            } else {
                                process.status = ProcessStatus::Exited { code };
                                process.exit_code = code;
                                Some(ProcessEvent::Exited {
                                    process_id: process_id.clone(),
                                    code,
                                })
                            }
                        }
                        // Still running: keep polling. Returning here would stop
                        // the waiter on the first tick and leave the process
                        // reported as running forever.
                        None => None,
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
            prevent_idle_sleep: false,
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

    /// 进程组 `pgid` 里仍存活的成员。组长被 reap 之后仍能用来探测子孙。
    /// 僵尸（`Z`）不算存活——它已被内核回收，不再占资源。
    #[cfg(unix)]
    fn group_members(pgid: u32) -> Vec<u32> {
        let output = std::process::Command::new("ps")
            .args(["-o", "pid=", "-o", "stat=", "-g", &pgid.to_string()])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid = fields.next()?.parse().ok()?;
                let stat = fields.next()?;
                (!stat.starts_with('Z')).then_some(pid)
            })
            .collect()
    }

    #[cfg(unix)]
    fn group_is_alive(pgid: u32) -> bool {
        group_members(pgid)
            .iter()
            .any(|pid| *pid != std::process::id())
    }

    /// 等到进程组里出现至少 `min` 个成员，返回观测到的成员数。
    #[cfg(unix)]
    async fn wait_until_group_size(pgid: u32, min: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let size = group_members(pgid).len();
            if size >= min {
                return size;
            }
            assert!(
                Instant::now() < deadline,
                "process group {pgid} never reached {min} members, saw {size}"
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// 进程是否仍存在（不含僵尸：僵尸已被内核回收，不再占资源）。
    #[cfg(unix)]
    fn pid_exists(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps");
        let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
        !stat.is_empty() && !stat.starts_with('Z')
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

    #[cfg(unix)]
    #[tokio::test]
    async fn manager_kill_lets_the_child_shut_down_gracefully_before_killing() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let started = manager
            .start(start_options(
                "trap 'echo GRACEFUL_SHUTDOWN' TERM; sleep 300",
                "graceful",
            ))
            .await
            .unwrap();
        // 保证 child 已装好 handler 再发信号（未命中默认动作会直接终止）。
        sleep(Duration::from_millis(500)).await;

        manager
            .kill(ProcessSelector::Id(started.process_id.clone()))
            .await
            .unwrap();

        // 直接子进程必须先拿到 SIGTERM 并跑完自己的 handler，而不是一上来就被 SIGKILL。
        let out = manager
            .read(ProcessSelector::Id(started.process_id.clone()), None, 4096)
            .await
            .unwrap();
        assert!(
            out.stdout.contains("GRACEFUL_SHUTDOWN"),
            "kill must let the child handle SIGTERM, stdout was {:?}",
            out.stdout
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manager_kill_reaps_grandchildren_that_ignore_sigterm() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // shell 自任组长；孙进程刻意忽略 SIGTERM，只有整组升级到 SIGKILL 才能收掉它。
        let started = manager
            .start(start_options(
                "(trap '' TERM; exec sleep 300) & wait",
                "stubborn",
            ))
            .await
            .unwrap();
        let pid = started.pid.expect("pid while running");
        // 必须等到孙进程真的出现（组内比子孙只有组长时更多），否则「组为空」可能
        // 只是子孙还没起来，测试会假通过。
        let group_size_before = wait_until_group_size(pid, 2).await;

        manager
            .kill(ProcessSelector::Id(started.process_id.clone()))
            .await
            .unwrap();

        // 对整组发信号 0：还有任何成员存活就返回 0，整组清空才返回 -1。
        let deadline = Instant::now() + Duration::from_secs(10);
        while group_is_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "kill must reap the whole process group (had {group_size_before} members), survivors: {:?}",
                group_members(pid)
            );
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manager_kill_does_not_return_until_the_process_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // 子进程收到 TERM 后要花 300ms 收尾：若 kill 发完信号就返回，断言会抓到它
        // 还在跑。用 `sleep 300` 抓不到——它被 SIGKILL 时是瞬时的。
        let started = manager
            .start(start_options(
                "trap 'sleep 0.3; exit 0' TERM; sleep 300",
                "slow",
            ))
            .await
            .unwrap();
        let pid = started.pid.expect("pid while running");
        sleep(Duration::from_millis(500)).await;

        manager
            .kill(ProcessSelector::Id(started.process_id.clone()))
            .await
            .unwrap();

        // `kill` 返回即意味着进程已经不在：状态说是终态、进程也必须真没了。
        assert!(
            !pid_exists(pid),
            "kill must not return while the child is still alive"
        );
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

#[cfg(test)]
mod sleep_hardening_tests {
    use super::*;
    use tokio::time::{Duration, sleep};

    fn options(command: &str, name: &str) -> ProcessStartOptions {
        ProcessStartOptions {
            command: command.into(),
            name: Some(name.into()),
            cwd: None,
            env: Default::default(),
            on_exit: OnExitPolicy::Kill,
            ready_pattern: None,
            ready_timeout_sec: None,
            prevent_idle_sleep: false,
        }
    }

    /// B2/B3：意图只按状态判定活跃，不因终态被抹掉。
    ///
    /// 直接改记录里的 `status` 覆盖全部 6 态：这是纯状态谓词，不需要真实进程。
    #[tokio::test]
    async fn sleep_hardening_predicate_covers_every_status() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        // 先起一个真实进程拿到一条记录，再在锁内直接改它的状态与意图——
        // 这样不依赖 `start` 之外的任何新入口。
        manager.start(options("sleep 300", "st")).await.unwrap();

        let cases = [
            (ProcessStatus::Starting, true),
            (ProcessStatus::Running, true),
            (ProcessStatus::Ready, true),
            (ProcessStatus::Exited { code: Some(0) }, false),
            (ProcessStatus::Killed, false),
            (
                ProcessStatus::FailedToStart {
                    reason: "boom".into(),
                },
                false,
            ),
        ];
        for (status, expected) in cases {
            {
                let mut processes = manager.lock_processes();
                let process = processes.values_mut().next().expect("one record");
                process.prevent_idle_sleep = true;
                process.status = status.clone();
            }
            assert_eq!(
                manager.has_sleep_preventing_processes(),
                expected,
                "status {status:?} with prevent_idle_sleep=true must yield {expected}"
            );
            // 终态也保留原意图：这是「只记录意图、不按状态抹掉」的判据。
            assert!(
                manager.list()[0].prevent_idle_sleep,
                "the intent must survive the terminal status {status:?}"
            );
        }

        // false 的任何活跃状态都不算忙。
        {
            let mut processes = manager.lock_processes();
            let process = processes.values_mut().next().expect("one record");
            process.prevent_idle_sleep = false;
            process.status = ProcessStatus::Running;
        }
        assert!(!manager.has_sleep_preventing_processes());

        manager.shutdown().await.unwrap();
    }

    /// B1–B4 的真实路径：门闩进程保持活跃 ⇒ true；写完门闩到终态 ⇒ false；
    /// 且 ready 终态前后 `list` / `read` / `start` 三处读回都等于 `true`。
    ///
    /// 用「读一行 stdin 再退出」会因 `stdin` 是 null 而立即结束，**不能**用它保持
    /// 活跃；这里用临时文件门闩。
    #[tokio::test]
    async fn sleep_hardening_gated_process_reports_the_intent_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let gate = temp.path().join("done");
        let manager = ProcessManager::new(temp.path().to_path_buf());

        let mut opts = options(
            "printf READY; while [ ! -e \"$DONE\" ]; do sleep 0.02; done",
            "gated",
        );
        opts.env.insert("DONE".into(), gate.display().to_string());
        opts.ready_pattern = Some("READY".into());
        opts.ready_timeout_sec = Some(5);
        opts.prevent_idle_sleep = true;

        let started = manager.start(opts).await.expect("the gated process starts");
        assert!(
            started.prevent_idle_sleep,
            "start result must carry the intent"
        );
        assert!(started.ready, "the gate printed READY");
        assert!(
            manager.has_sleep_preventing_processes(),
            "a ready, gated, opted-in process must count as busy"
        );

        let listed = manager.list();
        assert!(listed[0].prevent_idle_sleep, "list must carry the intent");
        assert!(
            matches!(listed[0].status, ProcessStatus::Ready),
            "readiness is not a terminal state, got {:?}",
            listed[0].status
        );

        let read = manager
            .read(ProcessSelector::Name("gated".into()), None, 4096)
            .await
            .unwrap();
        assert!(read.prevent_idle_sleep, "read must carry the intent");

        // 放行：写门闩，等它真的进入终态。
        std::fs::write(&gate, b"go").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let terminal = manager
                .list()
                .first()
                .is_some_and(|p| matches!(p.status, ProcessStatus::Exited { .. }));
            if terminal {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the gated process must exit after the gate is written"
            );
            sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !manager.has_sleep_preventing_processes(),
            "a terminal process must not keep the machine awake"
        );
        assert!(
            manager.list()[0].prevent_idle_sleep,
            "the intent is retained even after exit"
        );

        manager.shutdown().await.unwrap();
    }

    /// B4：默认 false 的常驻进程不算忙。
    #[tokio::test]
    async fn sleep_hardening_default_false_process_is_not_busy() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::new(temp.path().to_path_buf());
        let started = manager
            .start(options("sleep 30", "resident"))
            .await
            .unwrap();
        assert!(!started.prevent_idle_sleep, "the default is false");
        assert!(
            !manager.has_sleep_preventing_processes(),
            "an opted-out process must never count as busy"
        );
        manager.shutdown().await.unwrap();
    }
}
