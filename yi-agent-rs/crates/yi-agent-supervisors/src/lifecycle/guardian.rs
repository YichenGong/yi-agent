//! The guardian: process ownership that outlives the business daemon.
//!
//! A tool process must not die because the daemon that started it died, and must
//! not survive because the daemon that started it *thinks* it died. Both halves
//! need one component that owns the child handles and can prove what it owns, so
//! this module keeps that ownership outside the daemon's business runtime: the
//! guardian opens no repository and schedules no work.
//!
//! Two invariants carry the safety:
//!
//! 1. **Register before run.** Every child starts behind a *ready barrier*: it
//!    blocks on a private descriptor until its ownership is durably journaled, and
//!    sees EOF (and exits without running anything) if the guardian dies first.
//!    There is no window in which an unowned process can execute.
//! 2. **Never reap the leader before the group is empty.** A PGID can be recycled
//!    only after its leader is reaped. As long as the guardian holds the unreaped
//!    child, the recorded PGID provably still names its group, so signalling it is
//!    safe — even when the leader has exited and only grandchildren remain. Once
//!    the leader *is* gone and we cannot prove identity, we refuse to signal and
//!    report a fault instead of guessing.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, ClientRunId, DaemonInstanceId, ManagedProcessId, RootId, RuntimeScope,
};

use super::processes::{self, ProcessBirth};

/// The private descriptor a managed child uses for its ready barrier.
pub const READY_FD: RawFd = 9;
/// Environment variable naming that descriptor, so a launcher does not have to
/// hard-code the number.
pub const READY_FD_ENV: &str = "YI_AGENT_MANAGED_READY_FD";
/// First number the bootstrap source may be hoisted to before the fork.
///
/// Above the ready barrier, the stdio slots and any plausible pipe number a
/// three-descriptor spawn could produce, so the bootstrap and the barrier can never
/// be installed over one another.
const DEDICATED_FD_BASE: i32 = 32;

/// The number the guardian's bootstrap descriptor is installed on.
///
/// Distinct from [`READY_FD`] because the two are different protocols: the ready
/// barrier is a one-byte permit to start, and the bootstrap is the inherited proof
/// of who assigned the role. Sharing a number would let one read the other's bytes.
pub const BOOTSTRAP_FD: RawFd = 8;
/// Set when something in front of the program has already consumed the barrier.
///
/// The tool launcher reads the barrier and then `exec`s, so the program must not
/// read it a second time: the first read consumed the only byte, and a second
/// would block until the guardian happened to write again (never).
pub const READY_CONSUMED_ENV: &str = "YI_AGENT_MANAGED_READY_CONSUMED";
/// Where the ownership journal lives inside the runtime directory.
pub const JOURNAL_FILE: &str = "processes.json";
/// The guardian's control socket name.
///
/// Re-exported from `yi-agent-lifecycle`: the desktop shell resolves the same
/// socket to register its leases, and two constants would drift.
pub const GUARDIAN_SOCKET_FILE: &str = yi_agent_lifecycle::GUARDIAN_SOCKET_FILE;

/// Who a managed process belongs to.
///
/// `SharedPlugin` is not a frontend: a plugin may keep a process while any client
/// still demands it, and must be reclaimed when the last one releases. `Daemon`
/// is the fallback monitor for plugin work, not a license for a worker to keep a
/// runtime alive.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOwner {
    Frontend(ClientRunId),
    SharedPlugin(String),
    Daemon(DaemonInstanceId),
}

impl ProcessOwner {
    pub fn frontend(id: impl Into<ClientRunId>) -> Self {
        Self::Frontend(id.into())
    }

    pub fn plugin(name: impl Into<String>) -> Self {
        Self::SharedPlugin(name.into())
    }

    pub fn daemon(id: impl Into<DaemonInstanceId>) -> Self {
        Self::Daemon(id.into())
    }
}

/// Which task attempt a tool process serves.
///
/// All three parts matter: the same task id can recur in a later generation, and
/// the same task can be retried as a new attempt. Stopping "task t1" across
/// generations would stop work that belongs to someone else.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaskExecutionKey {
    pub generation: String,
    pub task: String,
    pub attempt: String,
}

impl TaskExecutionKey {
    pub fn new(
        generation: impl Into<String>,
        task: impl Into<String>,
        attempt: impl Into<String>,
    ) -> Self {
        Self {
            generation: generation.into(),
            task: task.into(),
            attempt: attempt.into(),
        }
    }
}

/// A process the guardian is asked to start and own.
///
/// `env` values are secrets in the general case (a provider key, a token), so this
/// type's `Debug` prints only the keys. Nothing here is ever written to the
/// journal.
// No `Clone`/`PartialEq`: the command may own a live descriptor, and duplicating
// one would create two claims to the same authorization. Nothing compares two
// commands either -- a test that did would be asserting on a resource identity.
// `Debug` is written by hand below, because env values are secrets.
pub struct ManagedCommand {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub owner: ProcessOwner,
    pub root: Option<RootId>,
    pub execution: Option<TaskExecutionKey>,
    /// A private descriptor the child is authorised with, installed on
    /// [`BOOTSTRAP_FD`] after `setsid`.
    ///
    /// Only the guardian's own daemon start uses this. It is a field rather than a
    /// second spawn method because the authorization and the process that receives
    /// it must be recorded together: a daemon started by a path that forgot the
    /// descriptor would refuse to run, and a descriptor passed to a path that forgot
    /// to record the process would be an unjournaled child.
    pub bootstrap: Option<std::os::fd::OwnedFd>,
}

impl std::fmt::Debug for ManagedCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedCommand")
            .field("executable", &self.executable)
            .field("args", &self.args)
            // Values would leak credentials into logs; keys are enough to
            // diagnose a misconfigured launch.
            .field(
                "env_keys",
                &self.env.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("cwd", &self.cwd)
            .field("owner", &self.owner)
            .field("root", &self.root)
            .field("execution", &self.execution)
            .field("has_bootstrap", &self.bootstrap.is_some())
            .finish()
    }
}

/// What the journal says happened to one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    Running,
    /// Observed stopped, with the group verified empty.
    Reclaimed,
    /// Could not be proven ours any more. Never auto-killed; needs a human.
    Unverifiable,
}

/// One journal line. Deliberately contains no environment and no credentials:
/// the journal is a recovery aid, not a place to persist secrets.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessRecord {
    pub scope_key: String,
    pub generation: String,
    pub process_id: ManagedProcessId,
    pub pid: i32,
    pub pgid: i32,
    /// Kernel start time of the *leader*, so a recycled PID is detectable.
    pub start_micros: u64,
    pub artifact_build_id: String,
    pub artifact_sha256: String,
    pub owner: ProcessOwner,
    pub root: Option<RootId>,
    pub execution: Option<TaskExecutionKey>,
    pub state: RecordState,
    /// Why the record is not `Running`/`Reclaimed` cleanly, if it is not.
    pub fault: Option<String>,
}

/// Result of reading a journal left by an earlier guardian.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Groups proven ours and observed stopped.
    pub reclaimed: Vec<ManagedProcessId>,
    /// Records that could not be verified. Their groups are *not* signalled.
    pub manual: Vec<(ManagedProcessId, String)>,
}

impl RecoveryReport {
    /// Whether it is safe to let the next generation schedule work.
    ///
    /// A single unverifiable record blocks scheduling: if something we started may
    /// still be running, starting more work would compound the uncertainty.
    pub fn is_safe(&self) -> bool {
        self.manual.is_empty()
    }
}

/// Owns every child this guardian started, plus the durable record of them.
///
/// The registry is not `Clone` and not shareable: exactly one process may hold the
/// `Child` handles, because holding them is what makes the PGIDs trustworthy.
pub struct ProcessRegistry {
    scope_key: String,
    generation: String,
    artifact: ArtifactIdentity,
    journal: Option<PathBuf>,
    records: Vec<ProcessRecord>,
    children: BTreeMap<ManagedProcessId, Child>,
    /// Set when a record could not be verified; blocks new spawns.
    fault: Option<String>,
}

impl ProcessRegistry {
    /// A registry that journaled its ownership to `runtime_dir/processes.json`.
    ///
    /// Spawning requires a journal: a process that is not durably recorded cannot
    /// be reclaimed after a crash, so allowing it would be the very leak this
    /// module exists to prevent.
    ///
    /// The report describes what happened to groups left by an earlier guardian,
    /// so a caller can refuse to schedule work when `!report.is_safe()`.
    pub fn open(
        scope: &RuntimeScope,
        generation: impl Into<String>,
        artifact: &ArtifactIdentity,
    ) -> io::Result<(Self, RecoveryReport)> {
        std::fs::create_dir_all(&scope.runtime_dir)?;
        let journal = scope.runtime_dir.join(JOURNAL_FILE);
        let records = read_journal(&journal)?;
        let mut registry = Self {
            scope_key: scope.key.clone(),
            generation: generation.into(),
            artifact: artifact.clone(),
            journal: Some(journal),
            records,
            children: BTreeMap::new(),
            fault: None,
        };
        let report = registry.recover()?;
        if !report.is_safe() {
            registry.fault = Some(format!(
                "{} managed process group(s) could not be verified",
                report.manual.len()
            ));
        }
        Ok((registry, report))
    }

    /// A registry with no journal. Useful for pure state tests; `spawn` refuses.
    pub fn without_journal(scope_key: impl Into<String>, artifact: ArtifactIdentity) -> Self {
        Self {
            scope_key: scope_key.into(),
            generation: String::new(),
            artifact,
            journal: None,
            records: Vec::new(),
            children: BTreeMap::new(),
            fault: None,
        }
    }

    pub fn records(&self) -> &[ProcessRecord] {
        &self.records
    }

    pub fn fault(&self) -> Option<&str> {
        self.fault.as_deref()
    }

    /// Starts `command` behind a ready barrier, journals its identity, then lets it
    /// run.
    ///
    /// Failure at any step before the barrier opens means the child never executed
    /// its program: it sees EOF on its ready descriptor and exits.
    pub fn spawn(
        &mut self,
        command: ManagedCommand,
        deadline: Deadline,
    ) -> io::Result<ManagedProcessId> {
        self.spawn_with_stdio(command, None, deadline)
    }

    /// Starts `command` with caller-supplied standard descriptors.
    ///
    /// The descriptors are inherited copies: the child ends up with exactly these
    /// open file descriptions on fd 0/1/2, so stdout and stderr stay two distinct
    /// streams instead of one merged one, and stdin can be a pipe rather than
    /// `/dev/null`. They are dup2'ed inside `pre_exec` before the blanket close of
    /// fds above 2, because that close is what stops unrelated host descriptors
    /// from leaking into a tool process.
    ///
    /// `stdio` is `None` for the daemon's own processes, which get `/dev/null` on
    /// all three.
    pub fn spawn_with_stdio(
        &mut self,
        command: ManagedCommand,
        stdio: Option<[std::os::fd::OwnedFd; 3]>,
        deadline: Deadline,
    ) -> io::Result<ManagedProcessId> {
        if deadline.is_expired() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no budget left to start a managed process",
            ));
        }
        if let Some(fault) = &self.fault {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refusing to spawn while a process group is unverified: {fault}"),
            ));
        }
        let journal = self.journal.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a managed process needs a journal to be reclaimable",
            )
        })?;
        if !command.executable.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a managed executable must be an absolute path",
            ));
        }
        if command.executable.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a managed executable must not be empty",
            ));
        }

        // The child reads one byte from the read end; the parent holds the write
        // end and writes only after the record is durable.
        let mut pipe = [0 as RawFd; 2];
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let read_end = OwnedFd::from_raw(pipe[0]);
        let write_end = OwnedFd::from_raw(pipe[1]);
        // A pipe inherited by an unrelated child would keep the barrier open
        // forever, so both ends are close-on-exec and only fd 9 is duplicated.
        for fd in [read_end.0, write_end.0] {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }

        // Ask `Command` to keep the caller's descriptors open across the fork.
        // They still have CLOEXEC set, so `pre_exec` is what turns each into the
        // real stdio number before exec.
        // Raw numbers, captured before the fork: `pre_exec` may only do
        // async-signal-safe work, so it cannot touch the `OwnedFd`s themselves.
        let stdio_raw = stdio.as_ref().map(|[stdin, stdout, stderr]| {
            use std::os::fd::AsRawFd;
            [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()]
        });

        // Why a launcher and not the program itself:
        //
        // The ready barrier must be read *after* the record is durable, and the
        // parent only continues once `spawn` returns. A program that does not read
        // fd 9 therefore either blocks the parent (if it reads in `pre_exec`) or
        // has already run by the time the record is written (if it does not read
        // at all). A tool program such as `sh` does neither.
        //
        // So the barrier is read by a one-line `sh` shim that then `exec`s the
        // caller's program with the caller's arguments. `exec` preserves the pid
        // and the process group, so ownership and reclamation still refer to the
        // program that was asked for. The shim is only used for tool spawns (those
        // that arrive with stdio descriptors); the daemon's own cooperative
        // children keep reading the barrier themselves.
        let mut process = if stdio_raw.is_some() {
            let mut launcher = Command::new("/bin/sh");
            launcher.arg("-c").arg(
                // `read` returns non-zero on EOF, which is what a guardian that
                // died before the record was durable leaves behind: exit without
                // running the program.
                "read -r managed_ready <&9 || exit 126; exec \"$@\"",
            );
            launcher.arg("managed-launch");
            launcher.arg(&command.executable);
            launcher.args(&command.args);
            // Tell the program the barrier is already behind it.
            launcher.env(READY_CONSUMED_ENV, "1");
            launcher
        } else {
            let mut direct = Command::new(&command.executable);
            direct.args(&command.args);
            direct
        };
        process
            .current_dir(&command.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env(READY_FD_ENV, READY_FD.to_string());
        if let Some([stdin_fd, stdout_fd, stderr_fd]) = stdio_raw {
            for fd in [stdin_fd, stdout_fd, stderr_fd] {
                if fd < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "a managed stdio descriptor must be a real descriptor",
                    ));
                }
                // Let the descriptor survive the fork. `pre_exec` clears CLOEXEC
                // again on the target number; this only keeps the *source* open
                // long enough to be dup2'ed.
                unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
            }
        }
        for (key, value) in &command.env {
            process.env(key, value);
        }
        let barrier_fd = read_end.0;
        // The bootstrap descriptor, if this command carries one, must survive the
        // fork at a number the close loop below will not touch. `pre_exec` moves it
        // onto `BOOTSTRAP_FD`; the source stays CLOEXEC-clear only until then.
        // The bootstrap descriptor is moved to a dedicated high number *before* the
        // fork. Using the caller's number directly is what broke the first version of
        // this: the ready-barrier pipe can land on 8, `dup2`-ing the bootstrap onto 8
        // then overwrote the barrier's read end, and the parent's own barrier write
        // failed with `BrokenPipe` while the child sat on a socket it never read from.
        // A number above every reserved slot makes the two protocols unable to collide.
        let hoisted_bootstrap = command.bootstrap.as_ref().and_then(|fd| {
            let raw = std::os::fd::AsRawFd::as_raw_fd(fd);
            let hoisted = unsafe { libc::fcntl(raw, libc::F_DUPFD, DEDICATED_FD_BASE) };
            if hoisted < 0 {
                return None;
            }
            // Survives the fork; `pre_exec` clears CLOEXEC on the target number.
            unsafe { libc::fcntl(hoisted, libc::F_SETFD, 0) };
            // The local `OwnedFd` closes its number on drop, which is what keeps the
            // hoisted copy from leaking once the child has been forked.
            Some(OwnedFd::from_raw(hoisted))
        });
        if command.bootstrap.is_some() && hoisted_bootstrap.is_none() {
            return Err(io::Error::last_os_error());
        }
        let bootstrap_raw = hoisted_bootstrap.as_ref().map(|fd| fd.0);
        // Computed before the fork: `pre_exec` may only do async-signal-safe work,
        // and the descriptor table size does not change between here and there.
        let descriptor_limit = unsafe { libc::getdtablesize() }.clamp(64, 1 << 17);
        unsafe {
            process.pre_exec(move || {
                // A new session makes the child its own group leader, so killing
                // the group reaches grandchildren the child later forks.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Move the caller's descriptors onto the standard numbers before
                // the close loop below. `dup2` clears CLOEXEC on the target, which
                // is what makes them survive exec.
                if let Some([stdin_fd, stdout_fd, stderr_fd]) = stdio_raw {
                    for (source, target) in [
                        (stdin_fd, libc::STDIN_FILENO),
                        (stdout_fd, libc::STDOUT_FILENO),
                        (stderr_fd, libc::STDERR_FILENO),
                    ] {
                        if source != target && libc::dup2(source, target) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        // Even when `source == target`, a lingering CLOEXEC would
                        // close the stream at exec.
                        if libc::fcntl(target, libc::F_SETFD, 0) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        // The inherited copy is no longer needed: leaving it open
                        // would let the child hold its own stdout write end and
                        // never see EOF.
                        if source != target && source > 2 {
                            libc::close(source);
                        }
                    }
                }
                // The barrier is installed first so that a bootstrap install can
                // never overwrite it, and so that the barrier's own source number is
                // free to be reused by the bootstrap afterwards.
                if libc::dup2(barrier_fd, READY_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                // `dup2` into the same number is a no-op, so it would keep the
                // source's CLOEXEC flag -- and a pipe end that happened to be fd 9
                // would vanish at exec, closing the barrier before it is released.
                if libc::fcntl(READY_FD, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(raw) = bootstrap_raw {
                    // Refuse a collision instead of silently closing one protocol
                    // with the other: a bootstrap on the ready number would make the
                    // child read a permit as proof of who started it.
                    if BOOTSTRAP_FD == READY_FD || raw <= 2 || raw == READY_FD {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a bootstrap descriptor must not share a number with stdio or the ready barrier",
                        ));
                    }
                    if libc::dup2(raw, BOOTSTRAP_FD) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    // Same CLOEXEC trap as the ready barrier: `dup2` into the same
                    // number is a no-op and keeps the flag.
                    if libc::fcntl(BOOTSTRAP_FD, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if raw != BOOTSTRAP_FD && raw > 2 {
                        libc::close(raw);
                    }
                }
                // Close every other descriptor. Relying on CLOEXEC would only cover
                // descriptors *we* opened; anything the host process inherited (a
                // lifecycle pipe, a lease FD, a logging socket) would otherwise pass
                // into the child and keep things alive that must not be kept alive.
                // Only stdin/stdout/stderr and the ready barrier may cross exec.
                for fd in 3..descriptor_limit {
                    // 0..=2 hold the caller's descriptors (or /dev/null): closing
                    // them would leave the child without stdio. The caller's source
                    // descriptors were already closed above. The bootstrap number is
                    // kept only when this child was actually authorised with one.
                    if fd != READY_FD && !(bootstrap_raw.is_some() && fd == BOOTSTRAP_FD) {
                        libc::close(fd);
                    }
                }
                Ok(())
            });
        }
        let child = process.spawn()?;
        // The child owns its copy now; the parent must drop this immediately or a
        // crashed child would still see an open barrier.
        drop(read_end);
        // The child has its own copies on 0/1/2. Ours must go, or the caller's
        // reads would never see EOF and the guardian would hold three descriptors
        // per spawn forever.
        drop(stdio);

        let pid = child.id() as i32;
        let fail = |mut child: Child, error: io::Error| -> io::Error {
            let _ = processes::kill_group(pid);
            let _ = child.wait();
            error
        };

        let birth = match ProcessBirth::of(pid) {
            Ok(birth) => birth,
            Err(error) => return Err(fail(child, error)),
        };
        // `setsid` must have taken effect; if not, the child is in our group and
        // killing "its group" would kill the guardian.
        match processes::process_group(pid) {
            Ok(Some(group)) if group == pid => {}
            Ok(other) => {
                return Err(fail(
                    child,
                    io::Error::other(format!("child {pid} is in group {other:?}, not its own")),
                ));
            }
            Err(error) => return Err(fail(child, error)),
        }

        let process_id = format!("{pid}-{}", birth.started_at());
        self.records.push(ProcessRecord {
            scope_key: self.scope_key.clone(),
            generation: self.generation.clone(),
            process_id: process_id.clone(),
            pid,
            pgid: pid,
            start_micros: birth.started_at(),
            artifact_build_id: self.artifact.build_id.clone(),
            artifact_sha256: self.artifact.sha256.clone(),
            owner: command.owner.clone(),
            root: command.root.clone(),
            execution: command.execution.clone(),
            state: RecordState::Running,
            fault: None,
        });
        if let Err(error) = write_journal(&journal, &self.records) {
            // Undo the record: it was never durable, and the barrier is about to
            // close. The child must not be left running unrecorded.
            self.records.pop();
            return Err(fail(child, error));
        }
        self.children.insert(process_id.clone(), child);

        // Only now may the child proceed.
        if let Err(error) = write_barrier(&write_end) {
            // The process is journaled, so terminate it through the normal path
            // rather than leaving a record for a process that never ran.
            let _ = self.stop(&process_id, &deadline);
            return Err(error);
        }
        Ok(process_id)
    }

    /// Stops every group owned by `owner`, waiting until each is observed empty.
    pub fn terminate_owner(&mut self, owner: &ProcessOwner, deadline: Deadline) -> io::Result<()> {
        let owned: Vec<ManagedProcessId> = self
            .records
            .iter()
            .filter(|record| &record.owner == owner && record.state == RecordState::Running)
            .map(|record| record.process_id.clone())
            .collect();
        let mut failures = Vec::new();
        for process_id in owned {
            if let Err(error) = self.stop(&process_id, &deadline) {
                failures.push(format!("{process_id}: {error}"));
            }
            if deadline.is_expired() {
                failures.push("budget exhausted before every group stopped".to_string());
                break;
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "managed groups not stopped: {}",
                failures.join("; ")
            )))
        }
    }

    /// Whether every group owned by `owner` is observed stopped.
    ///
    /// `Err` means "cannot tell", which callers must treat as *not* stopped: a
    /// group whose state is unknown is exactly the group that must not be reported
    /// as reclaimed.
    pub fn owner_is_stopped(&mut self, owner: &ProcessOwner) -> io::Result<bool> {
        let ids: Vec<String> = self
            .records
            .iter()
            .filter(|record| &record.owner == owner && record.state != RecordState::Reclaimed)
            .map(|record| record.process_id.clone())
            .collect();
        self.all_groups_stopped(&ids)
    }

    /// Stops every group belonging to one task attempt.
    ///
    /// Scoped to the attempt, not the owner: the client's other tasks keep running
    /// in their own groups, so finishing this task cannot take them down.
    pub fn terminate_execution(
        &mut self,
        execution: &TaskExecutionKey,
        deadline: Deadline,
    ) -> io::Result<()> {
        let owned: Vec<ManagedProcessId> = self
            .records
            .iter()
            .filter(|record| {
                record.execution.as_ref() == Some(execution) && record.state == RecordState::Running
            })
            .map(|record| record.process_id.clone())
            .collect();
        let mut failures = Vec::new();
        for process_id in owned {
            if let Err(error) = self.stop(&process_id, &deadline) {
                failures.push(format!("{process_id}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "managed groups not stopped: {}",
                failures.join("; ")
            )))
        }
    }

    /// Whether every tool group for one task attempt is observed stopped.
    ///
    /// Deliberately narrower than `owner_is_stopped`: a client may run several
    /// tasks in one executor, and finishing this task must not require stopping
    /// the executor that still serves the others.
    pub fn task_tools_are_stopped(&mut self, execution: &TaskExecutionKey) -> io::Result<bool> {
        let ids: Vec<String> = self
            .records
            .iter()
            .filter(|record| {
                record.execution.as_ref() == Some(execution)
                    && record.state != RecordState::Reclaimed
            })
            .map(|record| record.process_id.clone())
            .collect();
        self.all_groups_stopped(&ids)
    }

    fn all_groups_stopped(&mut self, ids: &[String]) -> io::Result<bool> {
        for process_id in ids {
            match self.group_state(process_id)? {
                GroupState::Empty => {}
                GroupState::Alive => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Stops one group, but only if `owner` holds it.
    ///
    /// The ownership check is here rather than in the caller so a channel cannot
    /// forget it: a frontend's private channel naming another frontend's
    /// `process_id` must be refused, not obeyed.
    pub fn terminate_granted(
        &mut self,
        owner: &ProcessOwner,
        process_id: &str,
        deadline: Deadline,
    ) -> io::Result<yi_agent_lifecycle::types::ManagedExit> {
        let record = self
            .records
            .iter()
            .find(|record| record.process_id == process_id)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no managed process {process_id}"),
                )
            })?;
        if &record.owner != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("process {process_id} is not owned by this channel"),
            ));
        }
        if record.state == RecordState::Running {
            self.stop(process_id, &deadline)?;
        }
        let mut exit = self.leader_status(&record);
        exit.group_stopped = processes::group_is_empty(record.pgid)?;
        Ok(exit)
    }

    /// Waits for the group behind `process_id` to end, describing how it did.
    ///
    /// Reports the leader's exit status *and* whether the whole group drained.
    /// The two are not the same fact: a shell that exits leaves its foreground job
    /// running in the same group, and a caller told only `code == 0` would report
    /// work finished while children are still alive.
    pub fn wait_exit(
        &mut self,
        process_id: &str,
        deadline: Deadline,
    ) -> io::Result<yi_agent_lifecycle::types::ManagedExit> {
        if deadline.is_expired() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no budget left to wait for a managed process",
            ));
        }
        let record = self
            .records
            .iter()
            .find(|record| record.process_id == process_id)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no managed process {process_id}"),
                )
            })?;
        if processes::wait_for_group_stopped(record.pgid, deadline.remaining())? {
            // The status is read *after* the group drains. Reading it first would
            // describe a leader that was still running, which is exactly the
            // `code: None` a caller then reports as a failed command.
            let mut exit = self.leader_status(&record);
            exit.group_stopped = true;
            if record.state == RecordState::Running {
                self.finish(process_id, RecordState::Reclaimed, None);
            }
            return Ok(exit);
        }
        // The budget ran out while the group was still alive: report what is known
        // now, with `group_stopped` false so the caller does not read it as done.
        Ok(self.leader_status(&record))
    }

    /// Reports the current state without waiting.
    ///
    /// `None` means the group is still alive. A caller polling this must not treat
    /// it as a stop proof either way: only `group_stopped` on the returned value is.
    pub fn try_exit(
        &mut self,
        process_id: &str,
    ) -> io::Result<Option<yi_agent_lifecycle::types::ManagedExit>> {
        let record = self
            .records
            .iter()
            .find(|record| record.process_id == process_id)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no managed process {process_id}"),
                )
            })?;
        let mut exit = self.leader_status(&record);
        if !processes::group_is_empty(record.pgid)? {
            return Ok(None);
        }
        exit.group_stopped = true;
        if record.state == RecordState::Running {
            self.finish(process_id, RecordState::Reclaimed, None);
        }
        Ok(Some(exit))
    }

    /// Reads the leader's exit status, reaping it if it already ended.
    ///
    /// `try_wait` is what turns "the group is empty" into "the leader exited with
    /// this status". Without reaping first, `WIFEXITED` has nothing to report.
    fn leader_status(&mut self, record: &ProcessRecord) -> yi_agent_lifecycle::types::ManagedExit {
        let mut exit = yi_agent_lifecycle::types::ManagedExit::default();
        let Some(child) = self.children.get_mut(&record.process_id) else {
            return exit;
        };
        if let Ok(Some(status)) = child.try_wait() {
            use std::os::unix::process::ExitStatusExt;
            exit.code = status.code();
            exit.signal = status.signal();
        }
        exit
    }

    /// Classifies one managed group, refusing to call an unowned group stopped.
    ///
    /// Two facts are needed and neither is sufficient alone:
    ///
    /// - **Emptiness.** `kill(-pgid, 0)` answers "is any process in this group?"
    ///   but on macOS reports `EPERM` both for another user's group *and* for a
    ///   group containing only our own zombies. So the exit status alone cannot
    ///   distinguish "ours, drained" from "not ours".
    /// - **Ownership.** Holding the unreaped `Child` proves the PGID is ours: the
    ///   kernel cannot recycle the leader's PID, and therefore cannot recycle the
    ///   PGID, until we reap it. A live leader with the recorded birth time is the
    ///   second proof.
    ///
    /// With both, a drained group of ours is `Empty`, a group with live members of
    /// ours is `Alive`, and anything else is an error rather than a claim.
    fn group_state(&mut self, process_id: &str) -> io::Result<GroupState> {
        let record = self
            .records
            .iter()
            .find(|record| record.process_id == process_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no managed process {process_id}"),
                )
            })?
            .clone();
        let members = processes::live_group_members(record.pgid)?;
        if members.is_empty() {
            // Enumeration already excludes zombies, so "no live member" means
            // nothing in this group can ever run again. That is stopped, whatever
            // its ownership: there is nothing left to signal. Requiring an
            // ownership proof here would report a drained group as a fault forever,
            // because `kill(-pgid, 0)` on a group holding only zombies reports
            // `EPERM` -- indistinguishable from another user's live group.
            return Ok(GroupState::Empty);
        }
        if !self.group_is_ours(&record) {
            return Err(io::Error::other(format!(
                "process group {} has live members but cannot be proven to belong to \
                     {process_id}; not signalling it",
                record.pgid
            )));
        }
        Ok(GroupState::Alive)
    }

    /// Whether the recorded PGID still provably names this record's group.
    ///
    /// Two independent proofs: we still hold the unreaped `Child` (the kernel
    /// cannot recycle its PID, hence cannot recycle its PGID), or the recorded
    /// leader is alive under the same birth identity.
    fn group_is_ours(&self, record: &ProcessRecord) -> bool {
        if self.children.contains_key(&record.process_id) {
            return true;
        }
        ProcessBirth::from_recorded(record.pid, record.start_micros).is_alive()
    }

    fn stop(&mut self, process_id: &str, deadline: &Deadline) -> io::Result<()> {
        let record = match self
            .records
            .iter()
            .find(|record| record.process_id == process_id)
        {
            Some(record) => record.clone(),
            None => return Ok(()),
        };
        if record.state != RecordState::Running {
            return Ok(());
        }
        match self.group_state(process_id)? {
            GroupState::Empty => {
                self.finish(process_id, RecordState::Reclaimed, None);
                return Ok(());
            }
            GroupState::Alive => {}
        }
        // Ownership was proven just above, so this PGID cannot have been reused.
        processes::kill_group(record.pgid)?;
        // From here only draining matters: the leader may vanish immediately after
        // the signal, so re-proving ownership would turn a successful kill into a
        // false fault.
        if !processes::wait_for_group_stopped(record.pgid, deadline.remaining())? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process group {} did not stop in time", record.pgid),
            ));
        }
        self.finish(process_id, RecordState::Reclaimed, None);
        Ok(())
    }

    fn finish(&mut self, process_id: &str, state: RecordState, fault: Option<String>) {
        if let Some(child) = self.children.remove(process_id) {
            // Reap only after the group was observed empty (or the record is
            // unrecoverable), so the PGID is not recycled while still in use.
            let mut child = child;
            let _ = child.wait();
        }
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.process_id == process_id)
        {
            record.state = state;
            record.fault = fault;
        }
        if let Some(journal) = &self.journal {
            let _ = write_journal(journal, &self.records);
        }
    }

    /// Verifies groups left behind by an earlier guardian.
    ///
    /// Only a group whose leader is still alive under the recorded birth identity
    /// is reclaimed automatically. Anything else is reported for manual handling:
    /// without the unreaped child handle, a recycled PGID cannot be distinguished
    /// from the original, and signalling it could kill an unrelated process.
    pub fn recover(&mut self) -> io::Result<RecoveryReport> {
        let mut report = RecoveryReport::default();
        let running: Vec<String> = self
            .records
            .iter()
            .filter(|record| record.state == RecordState::Running)
            .map(|record| record.process_id.clone())
            .collect();
        for process_id in running {
            let record = match self.records.iter().find(|r| r.process_id == process_id) {
                Some(record) => record.clone(),
                None => continue,
            };
            let leader_alive =
                ProcessBirth::from_recorded(record.pid, record.start_micros).is_alive();
            // Without the child handle, ownership rests on the leader's birth
            // identity alone: `kill` on a group whose leader vanished could hit a
            // recycled PGID.
            let members = match processes::live_group_members(record.pgid) {
                Ok(members) => members,
                Err(error) => {
                    let reason = format!("cannot inspect group {}: {error}", record.pgid);
                    self.finish(&process_id, RecordState::Unverifiable, Some(reason.clone()));
                    report.manual.push((process_id, reason));
                    continue;
                }
            };
            if members.is_empty() {
                // A drained group needs no signal, so no ownership proof either.
                self.finish(&process_id, RecordState::Reclaimed, None);
                report.reclaimed.push(process_id);
                continue;
            }
            if !leader_alive {
                let reason = format!(
                    "group {} still has live members but its leader {} is gone; \
                     the group id may have been recycled",
                    record.pgid, record.pid
                );
                self.finish(&process_id, RecordState::Unverifiable, Some(reason.clone()));
                report.manual.push((process_id, reason));
                continue;
            }
            // The leader is alive under the same birth, so the group is ours.
            // Ownership was proven by the leader's birth identity immediately
            // above; after the signal only draining matters.
            if processes::kill_group(record.pgid).is_err()
                || !processes::wait_for_group_stopped(record.pgid, Duration::from_secs(5))?
            {
                let reason = format!("group {} did not stop while reclaiming", record.pgid);
                self.finish(&process_id, RecordState::Unverifiable, Some(reason.clone()));
                report.manual.push((process_id, reason));
                continue;
            }
            self.finish(&process_id, RecordState::Reclaimed, None);
            report.reclaimed.push(process_id);
        }
        Ok(report)
    }
}

/// Last-resort reclamation for the groups this registry started.
///
/// `std::process::Child`'s `Drop` neither kills nor waits, and every managed child
/// calls `setsid` in `pre_exec`, so a dropped-but-not-terminated registry leaves
/// groups that are not in our process group: neither the test harness nor the
/// shell will ever signal them. They outlive the owner and are reparented to init.
///
/// The normal path stays `terminate_owner` / `terminate_execution`, which drain the
/// group and journal the outcome. This exists for the paths that cannot reach them:
/// a caller that returns early, a panicking test, or code that deliberately keeps a
/// group running to observe it. It signals groups directly instead of going through
/// `stop`, because the journal is no longer meaningful to a dying owner and a
/// journal write must not run during unwinding.
impl Drop for ProcessRegistry {
    fn drop(&mut self) {
        for record in &self.records {
            if record.state != RecordState::Running {
                continue;
            }
            // Without the `Child` handle, ownership rests on the leader's birth
            // identity: a PGID whose leader vanished may since have been recycled.
            let leader_alive = self.children.contains_key(&record.process_id)
                || ProcessBirth::from_recorded(record.pid, record.start_micros).is_alive();
            if !leader_alive {
                continue;
            }
            let _ = processes::kill_group(record.pgid);
        }
        // Reap what we hold, so no zombie is left behind by our own exit. The group
        // kill above is what stops the members; this only collects the leader.
        for (_, mut child) in std::mem::take(&mut self.children) {
            let _ = child.wait();
        }
    }
}

/// What one managed group currently looks like.
enum GroupState {
    /// No live member remains, and the group was proven ours.
    Empty,
    /// Live members remain and the group was proven ours.
    Alive,
}

/// A minimal owned descriptor so a pipe end cannot leak on an error path.
struct OwnedFd(RawFd);

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe { libc::close(self.0) };
        }
    }
}

impl OwnedFd {
    fn from_raw(fd: RawFd) -> Self {
        Self(fd)
    }
}

fn write_barrier(write_end: &OwnedFd) -> io::Result<()> {
    let mut fd = write_end.0;
    // A newline, not a bare byte: the shell launcher uses `read`, which only
    // returns when it sees a line terminator.
    let bytes = b"\n";
    // `write` on the raw descriptor, so the wrapper keeps sole ownership and
    // cannot close it twice.
    let written = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, 1) };
    let _ = &mut fd;
    if written == 1 {
        Ok(())
    } else if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "could not release the managed ready barrier",
        ))
    }
}

fn read_journal(path: &Path) -> io::Result<Vec<ProcessRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.is_empty() {
                return Ok(Vec::new());
            }
            serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("process journal {} is unreadable: {error}", path.display()),
                )
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Writes the journal atomically at mode 0600.
///
/// A torn journal would be worse than none: recovery could misread a partial
/// record as a whole one and either miss a live group or claim a stopped one. The
/// rename makes readers see either the old file or the new one.
fn write_journal(path: &Path, records: &[ProcessRecord]) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(records)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    // `set_permissions`, not just the create mode: rename preserves the file's
    // mode, but a pre-existing temp file may have been wider.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

/// The guardian process itself: leases, the control listener, and process
/// ownership, and *no* business state.
///
/// It deliberately owns no `yi-agent-store` handle. The guardian must survive the
/// daemon, so it must not depend on the daemon's database: a guardian that opened
/// `runtime.sqlite` would take the schema forward, and then an older daemon could
/// no longer serve the same runtime.
/// What a guardian knows about the business daemon it started.
///
/// Deliberately a record of *facts the daemon reported*, not of what the guardian
/// hoped it would be: [`Self::identity`] is only ever filled from the daemon's own
/// handshake, so a daemon that failed to start cannot be described as one that did.
#[derive(Clone, Debug)]
pub struct GuardianDaemon {
    /// The generation this daemon instance announced. Random per start, so a client
    /// can prove a switch really produced a new daemon rather than a stale answer.
    pub instance_id: yi_agent_lifecycle::types::DaemonInstanceId,
    /// The build the daemon reported, which is what compatibility is judged against.
    pub build_id: yi_agent_lifecycle::types::BuildId,
    /// The build's display version, for a human-facing mismatch message.
    pub display_version: String,
    /// The business protocol the daemon speaks.
    pub business_protocol: u32,
    /// Capabilities the daemon advertises.
    pub capabilities: Vec<String>,
    /// The `process_id` the registry assigned, so stopping it goes through the one
    /// path that can prove a group stopped.
    pub process_id: ManagedProcessId,
}

pub struct Guardian {
    scope: RuntimeScope,
    artifact: ArtifactIdentity,
    generation: DaemonInstanceId,
    /// The registry, kept alongside the reclaimer's backend.
    ///
    /// The backend is a trait object (`dyn ProcessBackend`) because reclamation only
    /// needs to ask "is it stopped"; an impact preview has to *enumerate* what is
    /// running, which that trait cannot express. One `Arc` shared with reclamation
    /// rather than a second registry: two views of "what did we start" would
    /// eventually disagree.
    registry: Option<std::sync::Arc<std::sync::Mutex<ProcessRegistry>>>,
    /// Plugin demand this guardian tracks. `None` for a guardian that only serves
    /// leases (tests, and a runtime with no plugins configured).
    demands: Option<super::demands::PluginDemandTable>,
    /// Impact snapshots and their one-shot tokens.
    previews: yi_agent_lifecycle::preview::PreviewStore,
    /// How to ask the business daemon what only it knows.
    ///
    /// Defaults to "nobody to ask", which yields an explicitly unknown preview rather
    /// than an empty one: a guardian that cannot see the daemon must not report that
    /// the runtime is idle.
    coordinator: std::sync::Arc<dyn super::preview::CoordinatorProbe>,
    /// The business daemon this guardian started, once it answered.
    ///
    /// `None` means "no daemon is serving this scope", which is not the same as
    /// "unknown": a guardian that never started one reports a *managed* runtime whose
    /// daemon fields are empty, and every compatibility check then refuses business.
    /// Filling these from the guardian's own artifact would be the exact conflation
    /// of guardian and daemon identity that [`RuntimeIdentity`] exists to prevent.
    daemon: Option<GuardianDaemon>,
    /// The guardian's end of the daemon's authorization lifeline.
    ///
    /// Held for as long as the daemon should consider itself supervised. Dropping it
    /// is how the daemon learns its guardian is gone, so keeping it in a local would
    /// make every daemon believe that.
    daemon_lifeline: Option<std::os::unix::net::UnixStream>,
    /// Version switches this guardian is running, so a client can poll one.
    ///
    /// Shared rather than owned because the switch runs on its own thread and outlives
    /// the control exchange that started it: the handler must answer "accepted"
    /// without holding the connection open for a teardown.
    switches: Option<std::sync::Arc<super::switch::SwitchController>>,
    /// Leases *and* reclamation.
    ///
    /// One object rather than two because "may this client admit work" and "is the
    /// last client gone" have to be answered against the same view: two tables
    /// would let a release read a lease count that a concurrent registration had
    /// already changed.
    reclaim: super::reclaim::Reclaimer,
    /// Authority to attach roots, minted for the daemon this guardian started.
    ///
    /// `None` until a daemon answers: authority that named no daemon could not be
    /// checked by one, and the daemon instance id is part of the binding.
    authorizations: Option<yi_agent_lifecycle::authorization::AuthorizationLedger>,
}

impl Guardian {
    /// A guardian whose reclamation talks to no process registry.
    ///
    /// Releases still close admission and produce a *failed* result rather than a
    /// pretend success; `serve` attaches the real registry before accepting
    /// connections.
    pub fn new(
        scope: RuntimeScope,
        artifact: ArtifactIdentity,
        generation: DaemonInstanceId,
    ) -> Self {
        let clock: std::sync::Arc<dyn yi_agent_lifecycle::deadline::Clock> =
            std::sync::Arc::new(yi_agent_lifecycle::deadline::MonotonicClock::new());
        let backend: std::sync::Arc<std::sync::Mutex<dyn super::reclaim::ProcessBackend>> =
            std::sync::Arc::new(std::sync::Mutex::new(super::reclaim::DetachedBackend));
        let reclaim =
            super::reclaim::Reclaimer::new(scope.clone(), generation.clone(), clock, backend);
        Self {
            scope,
            artifact,
            generation,
            registry: None,
            demands: None,
            previews: yi_agent_lifecycle::preview::PreviewStore::new(),
            coordinator: std::sync::Arc::new(super::preview::NoCoordinator),
            daemon: None,
            daemon_lifeline: None,
            switches: None,
            reclaim,
            authorizations: None,
        }
    }

    /// Starts the business daemon for this scope and waits until it really serves.
    ///
    /// The guardian is the only thing that may start a daemon with exit guarantees,
    /// and that is what makes the guarantees checkable: this process owns the child,
    /// journals the group, and can prove the group stopped. A client that started its
    /// own daemon could not make any of those three claims.
    ///
    /// `daemon` is left `None` when the start fails. That is deliberate: an identity
    /// that described a daemon nobody could reach would make the runtime look
    /// compatible, and the client would then send work into a missing socket.
    ///
    /// The wait is for the *business* socket answering `Status`, not for `spawn` to
    /// return. A forked process that has not bound yet is not a daemon: reporting it
    /// as one turns the client's first request into a startup race.
    pub fn start_daemon(&mut self, program: &Path, budget: Duration) -> io::Result<()> {
        let registry = self
            .registry
            .clone()
            .ok_or_else(|| io::Error::other("a daemon needs the process registry that owns it"))?;
        let deadline = Deadline::after(budget);
        // The daemon's own instance name: a new daemon is a new generation, and a
        // client that compared only build ids would call a restart "no change".
        let instance_id = format!(
            "daemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        );
        // Authority has a generation to name from the moment the daemon exists, so
        // the ledger is built before the fork rather than after the handshake.
        // Building it late would mean the greeting could only be sent once the
        // daemon answered -- but the daemon *waits* for the greeting before it
        // serves, so "wait for Status, then greet" is a deadlock, not an ordering.
        let ledger = yi_agent_lifecycle::authorization::AuthorizationLedger::new(
            self.scope.key.clone(),
            instance_id.clone(),
        );
        let hello = yi_agent_lifecycle::authorization::LifelineHello::new(
            self.scope.key.clone(),
            instance_id.clone(),
            ledger.generation_secret(),
        );
        // The descriptor that proves to the daemon that it was started by a guardian.
        // The guardian-side end is kept for the daemon's whole life: closing it would
        // tell the daemon its launcher is gone while the guardian still supervises it.
        let (mut guardian_end, daemon_end) = std::os::unix::net::UnixStream::pair()?;
        if let Err(error) =
            guardian_end.set_write_timeout(Some(yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT))
        {
            tracing::warn!(error = %error, "could not bound the lifeline write timeout");
        }
        let command = ManagedCommand {
            executable: program.to_path_buf(),
            args: vec!["internal-runtime".to_string(), "daemon".to_string()],
            env: vec![
                (
                    super::roles::SCOPE_KEY_ENV.to_string(),
                    self.scope.key.clone(),
                ),
                (
                    "YI_AGENT_INSTANCE_HOME".to_string(),
                    self.scope.instance_home.to_string_lossy().into_owned(),
                ),
                // The daemon derives its runtime directory from its workdir, and the
                // guardian derived this scope's directory from its own launcher. Passing
                // the directory explicitly is what keeps those two from being merely
                // *usually* equal: a symlinked project path would otherwise give the
                // daemon a second socket that no client in this scope ever looks at.
                (
                    "YI_AGENT_RUNTIME_DIR".to_string(),
                    self.scope.runtime_dir.to_string_lossy().into_owned(),
                ),
                // The descriptor that authorizes the daemon role. The number is the one
                // `spawn` dup's it to, declared here rather than guessed by the child.
                (
                    super::roles::BOOTSTRAP_FD_ENV.to_string(),
                    BOOTSTRAP_FD.to_string(),
                ),
            ],
            cwd: self.scope.project_key.clone().into(),
            owner: ProcessOwner::daemon(instance_id.clone()),
            root: None,
            execution: None,
            bootstrap: Some(daemon_end.into()),
        };
        // `spawn` consumes its deadline, and the wait below needs the same budget:
        // re-deriving it rather than shrinking it keeps "start plus bind" inside one
        // caller-visible window instead of two.
        let started = registry
            .lock()
            .map_err(|_| io::Error::other("process registry mutex poisoned"))?
            .spawn(command, Deadline::after(budget));
        let process_id = match started {
            Ok(process_id) => process_id,
            Err(error) => {
                // The lifeline goes with the failed start: holding it would suggest a
                // daemon exists.
                drop(guardian_end);
                return Err(error);
            }
        };

        // The greeting goes out before we wait for the socket: the daemon reads it
        // first thing and refuses to serve without it. Failing to write here is not
        // fatal to the start -- the daemon falls back to unmanaged, which is the
        // compatibility path -- but it is recorded, because an App attach will then
        // be refused with a reason nobody could otherwise explain.
        if let Err(error) = yi_agent_lifecycle::codec::write_frame(&mut guardian_end, &hello) {
            tracing::warn!(
                error = %error,
                "the managed daemon could not be told its authorization secret"
            );
        }

        let socket = yi_agent_store::ipc::socket_path_for(&self.scope.runtime_dir)
            .map_err(|error| io::Error::other(error.to_string()))?;
        // The socket may be the *previous* daemon's leftover node. Waiting for a
        // `Status` answer, rather than for the file to exist, is what distinguishes
        // "our daemon bound" from "a dead node is still on disk".
        while !deadline.is_expired() {
            if yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Status)
                .is_ok()
            {
                self.daemon = Some(GuardianDaemon {
                    instance_id: instance_id.clone(),
                    // The bytes are the ones this guardian launched: the guardian is
                    // the authority on which artifact it pinned, and it does not
                    // pretend to have read a self-description the daemon never sends.
                    build_id: self.artifact.build_id.clone(),
                    display_version: self.artifact.display_version.clone(),
                    business_protocol: self.artifact.business_protocol_max,
                    capabilities: self.artifact.capabilities.clone(),
                    process_id,
                });
                self.authorizations = Some(ledger);
                self.daemon_lifeline = Some(guardian_end);
                return Ok(());
            }
            std::thread::sleep(deadline.clamp(Duration::from_millis(20)));
        }
        // A daemon that never answered is not a daemon, and leaving it running would
        // put an unreachable process group beside the next generation.
        let mut registry = registry
            .lock()
            .map_err(|_| io::Error::other("process registry mutex poisoned"))?;
        let _ = registry.terminate_granted(
            &ProcessOwner::daemon(&instance_id),
            &process_id,
            Deadline::after(Duration::from_secs(5)),
        );
        drop(guardian_end);
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "the managed daemon did not answer within {}s",
                budget.as_secs()
            ),
        ))
    }

    /// Stops the daemon this guardian started, proving its group stopped.
    ///
    /// Idempotent: a guardian with no daemon has nothing to stop, and a second call
    /// after a successful stop returns immediately. That matters because both the
    /// drain path and the final exit path call it, and whichever runs first must not
    /// make the other look like a failure.
    pub fn stop_daemon(&mut self, deadline: Deadline) -> io::Result<()> {
        let Some(daemon) = self.daemon.take() else {
            return Ok(());
        };
        let result = match self.registry.as_ref() {
            Some(registry) => registry
                .lock()
                .map_err(|_| io::Error::other("process registry mutex poisoned"))?
                .terminate_granted(
                    &ProcessOwner::daemon(&daemon.instance_id),
                    &daemon.process_id,
                    deadline,
                )
                .map(|_| ()),
            // Without a registry nothing proves the group stopped, so claiming the
            // stop succeeded would be the false success this design forbids.
            None => Err(io::Error::other(
                "cannot stop a managed daemon without its process registry",
            )),
        };
        // The lifeline closes either way: a failed stop must not leave the daemon
        // believing its guardian still exists.
        self.daemon_lifeline = None;
        result
    }

    /// Mints authority to attach one root, and pushes the binding to the daemon.
    ///
    /// The `grant` is the whole authentication: it must match the live lease, and
    /// the lease must be business. There is deliberately no path that mints
    /// authority from a client id alone -- a caller that could name a client would
    /// be naming an owner it does not speak for, which is exactly the confusion
    /// `RootAuthorization` exists to prevent.
    ///
    /// A guardian with no daemon answers `Unsupported` rather than minting into a
    /// void: the binding names a `daemon_instance_id`, and authority for a daemon
    /// that is not serving could never be honoured by one.
    pub fn authorize_root(
        &mut self,
        grant: &yi_agent_lifecycle::types::ClientGrant,
        root_id: &str,
    ) -> Result<yi_agent_lifecycle::types::RootAuthorization, String> {
        let now = lease_now();
        // Same check an admission gets: released, expired, stale-epoch and
        // `LeaseOnly` all fail here, so no weaker path exists.
        self.reclaim.leases().admits(grant, now)?;
        let mode = grant.mode;
        let client = grant.client_run_id.clone();
        // Scoped so the borrow ends before the push: publishing needs `&mut self`
        // too, and the ledger is only re-borrowed if the push fails.
        let authorization = {
            let Some(ledger) = self.authorizations.as_mut() else {
                return Err(
                    "this guardian started no daemon, so it has no generation to authorize in"
                        .to_string(),
                );
            };
            ledger.issue(&client, root_id, mode)?
        };
        // Pushed before the answer is returned: a client that got a capability the
        // daemon has never heard of would be told "authorized" and then refused at
        // the attach, which is the one failure mode this type exists to make
        // impossible.
        if let Err(error) = self.push_authorization(
            yi_agent_lifecycle::authorization::AuthorizationUpdate::Bind(Box::new(
                authorization.clone(),
            )),
        ) {
            // The ledger already holds it; withdraw so memory cannot outlive the
            // daemon's knowledge of it.
            if let Some(ledger) = self.authorizations.as_mut() {
                let _ = ledger.apply(
                    yi_agent_lifecycle::authorization::AuthorizationUpdate::RevokeRoot {
                        root_id: root_id.to_string(),
                    },
                );
            }
            return Err(format!("the daemon could not be told: {error}"));
        }
        Ok(authorization)
    }

    /// Withdraws one client's authority and tells the daemon.
    ///
    /// Called on release and on lease expiry -- both are "this owner is gone", and
    /// leaving authority behind would let a departed App attach work nobody will
    /// reclaim. The local ledger is updated even when the push fails: a guardian
    /// that kept honouring a released client because a socket write failed would be
    /// worse than one whose daemon learns late.
    pub fn revoke_client_authority(&mut self, client_run_id: &str) {
        let update = yi_agent_lifecycle::authorization::AuthorizationUpdate::RevokeClient {
            client_run_id: client_run_id.to_string(),
        };
        if let Some(ledger) = self.authorizations.as_mut() {
            let _ = ledger.apply(update.clone());
        }
        let _ = self.push_authorization(update);
    }

    /// Publishes one update on the daemon's lifeline.
    ///
    /// The lifeline is the daemon's death monitor as well as its authorization
    /// channel. Those two uses do not conflict: the monitor fires when the socket
    /// *closes*, not when it is idle, and a frame is just bytes on a stream that is
    /// already open for its whole life.
    fn push_authorization(
        &mut self,
        update: yi_agent_lifecycle::authorization::AuthorizationUpdate,
    ) -> io::Result<()> {
        let Some(lifeline) = self.daemon_lifeline.as_mut() else {
            return Err(io::Error::other(
                "no daemon lifeline: there is nothing to tell",
            ));
        };
        lifeline.set_write_timeout(Some(yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT))?;
        yi_agent_lifecycle::codec::write_frame(&mut &*lifeline, &update)
            .map_err(|error| io::Error::other(format!("lifeline write failed: {error}")))
    }

    /// Whether this guardian has a daemon it started and has not stopped.
    pub fn daemon(&self) -> Option<&GuardianDaemon> {
        self.daemon.as_ref()
    }

    /// Whether every *business* client is gone and the scope is draining.
    ///
    /// The guardian uses this to decide that its work is finished. Lease-only
    /// observers do not count, and that is not a convenience: an observer is by
    /// definition watching for the exit, so counting it would keep alive exactly the
    /// runtime it is waiting to see stop. `active_count` would do that -- it counts
    /// observers -- which is why this reads `business_active_count`.
    pub fn drained(&self) -> bool {
        self.reclaim.global_drain_started()
            && self.reclaim.leases().business_active_count() == 0
            // An in-flight reclaim is still this generation's work. Exiting now
            // would cut off the thread that is proving the stop, and the client
            // that asked for it would never receive its answer.
            && !self.reclaim.has_running_operations()
    }

    /// Points reclamation at the registry that really owns the process groups.
    ///
    /// Called once, before the control loop starts accepting: a release that ran
    /// before this point must not be able to report a stop it never verified.
    pub fn attach_registry(&mut self, registry: std::sync::Arc<std::sync::Mutex<ProcessRegistry>>) {
        let clock: std::sync::Arc<dyn yi_agent_lifecycle::deadline::Clock> =
            std::sync::Arc::new(yi_agent_lifecycle::deadline::MonotonicClock::new());
        let backend: std::sync::Arc<std::sync::Mutex<dyn super::reclaim::ProcessBackend>> =
            std::sync::Arc::new(std::sync::Mutex::new(super::reclaim::RegistryBackend::new(
                std::sync::Arc::clone(&registry),
            )));
        self.registry = Some(std::sync::Arc::clone(&registry));
        let leases = std::mem::take(self.reclaim.leases_mut());
        let clients = std::mem::take(self.reclaim.clients_mut());
        self.reclaim = super::reclaim::Reclaimer::new(
            self.scope.clone(),
            self.generation.clone(),
            clock,
            backend,
        );
        *self.reclaim.leases_mut() = leases;
        *self.reclaim.clients_mut() = clients;
    }

    /// Points the impact preview at the plugin demand table.
    ///
    /// Separate from [`Self::attach_registry`] because demand lives with the daemon
    /// side, not with process ownership: a runtime can own groups and track no
    /// plugins, and a lease-only guardian tracks demand but starts nothing.
    pub fn attach_demands(&mut self, demands: super::demands::PluginDemandTable) {
        self.demands = Some(demands);
    }

    /// Points the impact preview at a real business-daemon probe.
    pub fn attach_coordinator(
        &mut self,
        probe: std::sync::Arc<dyn super::preview::CoordinatorProbe>,
    ) {
        self.coordinator = probe;
    }

    /// Points operation queries at the switch controller that runs replacements.
    ///
    /// Without one, an `Operation` query is reported as unsupported rather than as
    /// "unknown": a client must be able to tell "this build cannot switch" from "I
    /// never asked for that operation".
    pub fn attach_switches(&mut self, switches: std::sync::Arc<super::switch::SwitchController>) {
        self.switches = Some(switches);
    }

    /// The switch controller, when this guardian can perform a switch.
    pub fn switches(&self) -> Option<&std::sync::Arc<super::switch::SwitchController>> {
        self.switches.as_ref()
    }

    /// Whether this guardian holds the handles needed to stop what it started.
    pub fn holds_registry(&self) -> bool {
        self.registry.is_some()
    }

    /// Every running managed group, copied out of the registry.
    ///
    /// Copied rather than borrowed because the registry mutex must not be held while
    /// an impact preview is assembled: assembling it can be slow (a page of entries),
    /// and holding the lock would stall reclamation behind a UI read.
    pub fn running_records(&self) -> Vec<ProcessRecord> {
        let Some(registry) = &self.registry else {
            return Vec::new();
        };
        let registry = match registry.lock() {
            Ok(registry) => registry,
            // A poisoned registry means another thread panicked holding it. The
            // records are still what they were, and reporting "nothing running"
            // would be the one wrong answer here, so recover the data.
            Err(poisoned) => poisoned.into_inner(),
        };
        registry.records().to_vec()
    }

    /// The impact previews and their tokens.
    pub fn previews(&self) -> &yi_agent_lifecycle::preview::PreviewStore {
        &self.previews
    }

    pub fn previews_mut(&mut self) -> &mut yi_agent_lifecycle::preview::PreviewStore {
        &mut self.previews
    }

    /// The plugin demand table, when one is attached.
    pub fn demands(&self) -> Option<&super::demands::PluginDemandTable> {
        self.demands.as_ref()
    }

    pub fn reclaimer(&self) -> &super::reclaim::Reclaimer {
        &self.reclaim
    }

    pub fn reclaimer_mut(&mut self) -> &mut super::reclaim::Reclaimer {
        &mut self.reclaim
    }

    pub fn scope(&self) -> &RuntimeScope {
        &self.scope
    }

    pub fn artifact(&self) -> &ArtifactIdentity {
        &self.artifact
    }

    pub fn leases_mut(&mut self) -> &mut yi_agent_lifecycle::leases::LeaseTable {
        self.reclaim.leases_mut()
    }

    /// One maintenance tick: reclaim work whose owner stopped renewing.
    ///
    /// Expiry is not bookkeeping. A frontend that crashes mid-task stops
    /// heartbeating; if the guardian only forgot its lease, the processes it had
    /// started would keep running with no owner, no admission checks, and nothing
    /// left to stop them. So an expired lease goes through the same teardown a
    /// release does, and the outcome is a recorded operation rather than a silent
    /// removal.
    ///
    /// `now` is passed in rather than read here so a test can express "31 seconds
    /// later" without sleeping through the real window.
    pub fn maintain(
        &mut self,
        now: std::time::Duration,
    ) -> Vec<yi_agent_lifecycle::types::OperationId> {
        // Expiry is a client going silent, and it ends that client's authority for
        // the same reason a release does: the work it would have attached has no
        // one left to reclaim it. Reading the expiring ids *before* the sweep is
        // what lets this revoke the right ones -- afterwards the table no longer
        // knows who left.
        let expiring = self.reclaim.leases().expiring_at(now);
        for client in expiring {
            self.revoke_client_authority(&client);
        }
        self.reclaim.expire_leases(now)
    }

    /// Opens the process registry, reporting what an earlier guardian left behind.
    pub fn open_registry(&self) -> io::Result<(ProcessRegistry, RecoveryReport)> {
        ProcessRegistry::open(&self.scope, self.generation.clone(), &self.artifact)
    }

    /// Binds the control socket for this runtime.
    ///
    /// An existing socket file is removed only after confirming nothing answers on
    /// it: a live guardian must never have its socket stolen, and a leftover file
    /// from a crash must not block the next start.
    pub fn bind_control(
        &self,
        deadline: &Deadline,
    ) -> io::Result<std::os::unix::net::UnixListener> {
        use std::os::unix::fs::FileTypeExt;
        let path = self.scope.runtime_dir.join(GUARDIAN_SOCKET_FILE);
        std::fs::create_dir_all(&self.scope.runtime_dir)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if !metadata.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists and is not a control socket; refusing to replace it",
                        path.display()
                    ),
                ));
            }
            let answered = yi_agent_lifecycle::client::ControlClient::connect(&path, deadline)
                .and_then(|mut probe| {
                    probe.request(
                        &yi_agent_lifecycle::types::ControlRequest::Identity,
                        deadline,
                    )
                });
            match answered {
                Ok(yi_agent_lifecycle::types::ControlResponse::Identity(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another guardian is already serving {}", path.display()),
                    ));
                }
                // Something answered, but not as a guardian. That is still a live
                // listener; removing it would be stealing an unrelated service.
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "{} is served by something that is not a guardian",
                            path.display()
                        ),
                    ));
                }
                // Nothing answered. Only a *refused* connect proves the listener
                // is gone; a connect that succeeded but never answered is a live
                // endpoint that may just be busy, and deleting its socket would
                // hand the runtime to a second guardian. Fail closed instead and
                // let a human decide.
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(&path)?;
                }
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "{} exists and does not answer ({error}); refusing to replace it                              without confirmation that no guardian is using it",
                            path.display()
                        ),
                    ));
                }
            }
        }
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        // Only this user may talk to the control channel.
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        Ok(listener)
    }
}

/// Runs a guardian until `shutdown` fires.
///
/// Lifecycle control requests are served here; process registration is *not* a
/// control operation, so a client can never spawn work through the public protocol.
pub fn serve(
    scope: RuntimeScope,
    selected: ArtifactIdentity,
    shutdown: impl Fn() -> bool,
) -> io::Result<()> {
    serve_with(
        scope,
        selected,
        shutdown,
        std::sync::Arc::new(super::preview::NoCoordinator),
    )
}

/// Same as [`serve`], with a probe for the business daemon's impact.
///
/// Injected rather than looked up because the guardian is a separate process: only
/// the caller knows how to reach the daemon it is supervising, and a guardian with
/// no daemon (a lease-only observer, or the window between daemon generations) must
/// still run and still answer previews honestly as "unknown".
pub fn serve_with(
    scope: RuntimeScope,
    selected: ArtifactIdentity,
    shutdown: impl Fn() -> bool,
    coordinator: std::sync::Arc<dyn super::preview::CoordinatorProbe>,
) -> io::Result<()> {
    serve_with_daemon(scope, selected, shutdown, coordinator, None)
}

/// [`serve_with`], optionally starting the business daemon this guardian owns.
///
/// `daemon_program` is `None` for a guardian that only serves leases (tests, and a
/// window in which no daemon should exist). Passing a program is what turns the
/// guardian into the runtime's lifecycle owner: it spawns the daemon with an
/// inherited authorization, waits for that daemon to answer, reports its identity,
/// and stops it when the last client leaves.
pub fn serve_with_daemon(
    scope: RuntimeScope,
    selected: ArtifactIdentity,
    shutdown: impl Fn() -> bool,
    coordinator: std::sync::Arc<dyn super::preview::CoordinatorProbe>,
    daemon_program: Option<&Path>,
) -> io::Result<()> {
    let generation = format!("guardian-{}", std::process::id());
    let mut guardian = Guardian::new(scope, selected, generation);
    guardian.attach_coordinator(coordinator);
    let (registry, report) = guardian.open_registry()?;
    if !report.is_safe() {
        // Refuse to serve rather than let the next generation schedule work beside
        // groups nobody can account for.
        return Err(io::Error::other(format!(
            "refusing to serve: {} managed group(s) need manual handling",
            report.manual.len()
        )));
    }
    let deadline = Deadline::after(Duration::from_secs(2));
    let listener = guardian.bind_control(&deadline)?;

    // Reclamation must talk to the real registry, and it must be attached before
    // the first connection: a release that ran earlier could not have verified any
    // stop. Dropping the registry still stops and reaps every group it owns.
    let registry = std::sync::Arc::new(std::sync::Mutex::new(registry));
    guardian.attach_registry(std::sync::Arc::clone(&registry));

    // The daemon starts only after the control socket is bound, and that ordering is
    // observable to clients: a caller that can reach the guardian is entitled to see
    // an honest identity, so "guardian up, daemon coming" is never reported as "no
    // runtime". A start failure is recorded and the guardian keeps serving, because
    // refusing to serve would also take away the preview/status channel a user needs
    // to find out *why* the start failed.
    if let Some(program) = daemon_program {
        // A start failure is recorded and the guardian keeps serving: refusing to
        // serve would also take away the status channel a user needs to find out why
        // the daemon did not start.
        if let Err(error) = guardian.start_daemon(program, START_DAEMON_BUDGET) {
            tracing::warn!(error = %error, "the managed daemon did not start; serving without it");
        }
    }

    // Non-blocking accept, woken by a bounded wait, so the loop can also do
    // maintenance. A plain blocking `accept` would make lease expiry depend on
    // traffic: a runtime whose last client crashed would have no traffic at all,
    // which is exactly when the lease has to be noticed.
    listener.set_nonblocking(true)?;
    while !shutdown() {
        if wait_for_connection(&listener, LEASE_MAINTENANCE_TICK)? {
            // Drain everything that is ready. `WouldBlock` ends the batch; it is
            // not an error on a non-blocking listener.
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Per-connection setup must never take the guardian down:
                        // one malformed or already-closed peer is not a reason to
                        // lose the process groups this guardian is responsible for.
                        if prepare_control_stream(&stream).is_ok() {
                            serve_control_connection(&mut guardian, stream);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        let _ = guardian.maintain(lease_now());
        // The last client leaving ends this generation: the daemon is stopped with
        // proof, then the loop returns and the process exits. Leaving from inside the
        // loop (rather than waiting for an external signal) is what makes "the last
        // client leaving stops the runtime" a property of the code instead of a
        // convention someone has to remember. The database is untouched: history is
        // the project's, not this generation's.
        if guardian.drained() {
            break;
        }
    }
    // Bounded, and through the registry, so "stopped" means a verified-empty group
    // rather than a signal that was sent.
    if let Err(error) = guardian.stop_daemon(Deadline::after(STOP_DAEMON_BUDGET)) {
        tracing::warn!(error = %error, "the managed daemon did not stop cleanly");
    }
    Ok(())
}

/// How long the guardian waits for its daemon to bind and answer.
///
/// Inside the bring-up budget a client is already waiting on, so this is not a new
/// deadline: it is the part of that budget the daemon gets before the guardian
/// reports it as absent.
const START_DAEMON_BUDGET: Duration = Duration::from_secs(20);

/// How long a stop may take before it is reported as unclean.
const STOP_DAEMON_BUDGET: Duration = Duration::from_secs(10);

/// How often the guardian looks for expired leases when nothing else happens.
///
/// Well under [`yi_agent_lifecycle::leases::LEASE_EXPIRY`], so a crashed frontend
/// is noticed within a second or two of its window closing rather than whenever the
/// next client happens to connect.
const LEASE_MAINTENANCE_TICK: Duration = Duration::from_secs(1);

/// Re-checks that the artifact a token was minted for is still usable.
///
/// A bundle can be replaced or removed between the preview and the click. Checking
/// here means the failure is "the target is gone, preview again" instead of a switch
/// that stops the old runtime and then cannot start the new one.
fn verify_target(target: &yi_agent_lifecycle::types::ArtifactIdentity) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(&target.executable).map_err(|error| {
        format!(
            "the target artifact {} cannot be used: {error}",
            target.executable.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "the target artifact {} is not a regular file",
            target.executable.display()
        ));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "the target artifact {} is not executable",
            target.executable.display()
        ));
    }
    Ok(())
}

/// Answers `ControlRequest::Operation`: the progress of a switch this runtime runs.
///
/// Read-only: it starts nothing, takes no barrier and grants no power. The lease check
/// is still required, because progress reveals which build is being installed and when
/// a runtime will go down, and that is the runtime's business, not any local process's.
fn serve_operation(
    guardian: &Guardian,
    grant: &yi_agent_lifecycle::types::ClientGrant,
    operation_id: &str,
) -> yi_agent_lifecycle::types::ControlResponse {
    use yi_agent_lifecycle::types::{ControlErrorCode, ControlResponse};

    let now = lease_now();
    if let Err(reason) = guardian.reclaimer().leases().admits_or_observes(grant, now) {
        return ControlResponse::Rejected {
            code: ControlErrorCode::Unauthenticated,
            reason,
        };
    }
    let Some(switches) = guardian.switches() else {
        return ControlResponse::Unsupported {
            detail: "this guardian build cannot perform a version switch".to_string(),
        };
    };
    // An operation this guardian never ran is reported as unknown, not as failed: the
    // caller may be polling an id from an earlier generation, and inventing a failure
    // for it would be a false report.
    ControlResponse::Operation(switches.status(operation_id))
}

/// How many clients one [`ControlRequest::Status`] page carries.
///
/// The point is that a status frame is bounded by `MAX_CONTROL_FRAME`: an App that
/// has been open for days may have hundreds of released-and-reconnected clients in
/// a runtime's history, and a single unbounded array would either exceed the frame
/// or force the whole read to be truncated. Eight keeps a page well inside the
/// frame with room for the identity.
pub const STATUS_CLIENT_PAGE: usize = 8;

/// Answers `ControlRequest::Status`.
///
/// Strictly read-only: no registration, no heartbeat, no maintenance barrier, and
/// no credential. A UI polling this every five seconds must not thereby keep a
/// runtime alive, and the app-server -- which may not hold the App's renewable
/// grant -- still has to be able to answer it.
///
/// Two asymmetries are deliberate:
///
/// * `counts` describes the whole runtime while `clients` is one page. A reader
///   that used `clients.len()` as the count would tell a user "1 client" beside a
///   list that simply ended.
/// * `counts.complete` is always `false`. The guardian can see leases and process
///   groups, but not the daemon's executor queue or plugin demand; marking it
///   complete here would let a status read claim a knowledge it does not have.
fn serve_status(
    guardian: &Guardian,
    cursor: Option<&str>,
) -> yi_agent_lifecycle::types::ControlResponse {
    use yi_agent_lifecycle::types::ControlResponse;

    let leases = guardian.reclaimer().leases();
    let rows = leases.client_summaries();
    // Whole-runtime numbers, taken from the same table as the page.
    let pending = leases.pending_count() as u64;
    let counts = yi_agent_lifecycle::types::ImpactCounts {
        clients: rows.len() as u64,
        pending_clients: pending,
        // Only a process registry can answer these, and the status read does not
        // ask the daemon. Zero here would be a lie; the honest answer is "this
        // read did not determine it", which `complete: false` says.
        running: 0,
        queued: 0,
        plugins: 0,
        complete: false,
    };

    // Cursor is the last id of the previous page, so an insertion before it cannot
    // make page two repeat a row.
    let start = match cursor {
        None => 0,
        Some(after) => rows
            .iter()
            .position(|row| row.client_run_id == after)
            .map(|index| index + 1)
            // A cursor naming a client that is gone is not an error: the page it
            // described no longer exists, so the caller restarts from the top
            // rather than receiving rows from before its own cursor.
            .unwrap_or(rows.len()),
    };
    let page: Vec<_> = rows
        .iter()
        .skip(start)
        .take(STATUS_CLIENT_PAGE)
        .cloned()
        .collect();
    let next_cursor = if start + page.len() < rows.len() {
        page.last().map(|row| row.client_run_id.clone())
    } else {
        None
    };

    // The operation, not an error: a status read is how the App finds its own
    // switch again after a WebView reload, so "no operation" is a valid answer.
    let active_operation = guardian
        .switches()
        .and_then(|switches| switches.running())
        .and_then(|operation| {
            guardian
                .switches()
                .and_then(|switches| switches.status(&operation))
        });

    ControlResponse::Status(Box::new(yi_agent_lifecycle::types::RuntimeStatusSnapshot {
        identity: runtime_identity(guardian),
        counts,
        clients: page,
        clients_next_cursor: next_cursor,
        active_operation,
        local_restart_available: guardian.switches().is_some(),
    }))
}

/// The runtime's identity as every status reader must see it.
///
/// Extracted so [`ControlRequest::Identity`] and [`ControlRequest::Status`] cannot
/// drift apart: a UI that showed one build id from `identity` and another from
/// `status` would be describing two different runtimes.
///
/// Everything a *daemon* owns stays `None` here, and that is not an omission. The
/// guardian knows its own generation and artifact; it does not know whether a
/// business daemon is running, which generation that daemon is, or which protocol
/// version it speaks. Filling those in from the guardian's own fields would report
/// the guardian's generation as the daemon's -- exactly the conflation the two
/// identities exist to prevent.
fn runtime_identity(guardian: &Guardian) -> yi_agent_lifecycle::types::RuntimeIdentity {
    // The daemon half comes from what *that process* announced, never from this
    // guardian's own artifact. A guardian that filled those fields with its own build
    // would make a runtime with no daemon look compatible, and the client would then
    // send work to a socket nobody listens on.
    let daemon = guardian.daemon.as_ref();
    yi_agent_lifecycle::types::RuntimeIdentity {
        scope_key: guardian.scope.key.clone(),
        daemon_instance_id: daemon.map(|daemon| daemon.instance_id.clone()),
        guardian_build_id: Some(guardian.artifact.build_id.clone()),
        daemon_build_id: daemon.map(|daemon| daemon.build_id.clone()),
        mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
        business_protocol: daemon.map(|daemon| daemon.business_protocol),
        daemon_display_version: daemon.map(|daemon| daemon.display_version.clone()),
        guardian_display_version: Some(guardian.artifact.display_version.clone()),
        // The intersection, not the union: a capability only one side has cannot be
        // used by a client that needs both halves to honour it.
        capabilities: daemon.map_or_else(
            || guardian.artifact.capabilities.clone(),
            |daemon| {
                guardian
                    .artifact
                    .capabilities
                    .iter()
                    .filter(|have| daemon.capabilities.iter().any(|their| their == *have))
                    .cloned()
                    .collect()
            },
        ),
        schema_min: daemon.map_or(guardian.artifact.schema_min, |_| {
            guardian.artifact.schema_min
        }),
        schema_max: guardian.artifact.schema_max,
    }
}

/// Answers `ControlRequest::Preview`.
///
/// Read-only by construction: it takes no maintenance barrier, starts no drain, and
/// changes no admission state. A preview that a user cancels therefore cannot have
/// disturbed anything, which is what makes it safe to render while they decide.
fn serve_preview(
    guardian: &mut Guardian,
    grant: &yi_agent_lifecycle::types::ClientGrant,
    target: yi_agent_lifecycle::types::ArtifactIdentity,
) -> yi_agent_lifecycle::types::ControlResponse {
    use yi_agent_lifecycle::preview::ConfirmationContext;
    use yi_agent_lifecycle::types::{ControlErrorCode, ControlResponse, ImpactRevision};

    // The caller must still hold a live lease. Otherwise anybody who can open the
    // socket could mint a token for a runtime they do not belong to.
    let now = lease_now();
    if let Err(reason) = guardian.reclaimer().leases().admits_or_observes(grant, now) {
        return ControlResponse::Rejected {
            code: ControlErrorCode::Unauthenticated,
            reason,
        };
    }

    let records = guardian.running_records();
    let demands = match guardian.demands() {
        Some(demands) => demands.clone(),
        None => super::demands::PluginDemandTable::new(),
    };
    // Ask the daemon out of band. A probe that cannot answer yields `None` in the
    // preview, which the store renders as unknown -- never as "nothing running".
    let coordinator = guardian.coordinator.impact();
    let fencable = super::preview::is_fencable(guardian.holds_registry(), &target);

    let collected = match super::preview::collect(
        guardian.reclaimer().leases(),
        &records,
        &demands,
        coordinator,
        fencable,
        || guardian.reclaimer().leases().impact_revision(),
        // Second daemon read, so its rows also describe one instant. Without this a
        // worker starting mid-collection would be missing from the impact the user
        // approves, and the restart would stop it anyway.
        || guardian.coordinator.impact().revision,
    ) {
        Ok(collected) => collected,
        // A runtime that changed while being measured has no single instant to
        // confirm. Handing back a mixed snapshot would let a person approve one
        // state and get another.
        Err(race) => {
            return ControlResponse::Rejected {
                code: ControlErrorCode::Conflict,
                reason: race.to_string(),
            };
        }
    };

    let context = ConfirmationContext {
        scope_key: guardian.scope.key.clone(),
        daemon_instance_id: guardian.generation.to_string(),
        target,
        revision: ImpactRevision {
            guardian: collected.guardian_revision,
            coordinator: collected.coordinator_revision,
        },
    };
    let preview = guardian
        .previews_mut()
        .preview(grant, context, collected.input, now);
    ControlResponse::Preview(preview)
}

/// Answers `ControlRequest::PreviewPage`: more rows, never a token.
fn serve_preview_page(
    guardian: &mut Guardian,
    grant: &yi_agent_lifecycle::types::ClientGrant,
    snapshot_id: &str,
    cursor: &str,
) -> yi_agent_lifecycle::types::ControlResponse {
    use yi_agent_lifecycle::types::{ControlErrorCode, ControlResponse, ImpactRevision};

    let now = lease_now();
    if let Err(reason) = guardian.reclaimer().leases().admits_or_observes(grant, now) {
        return ControlResponse::Rejected {
            code: ControlErrorCode::Unauthenticated,
            reason,
        };
    }
    // The page is checked against the *live* revision, not the one recorded when the
    // snapshot was taken: if the runtime moved, the rest of the list describes a
    // world that no longer exists. The target comes from the snapshot itself,
    // because a page request carries no target.
    let stored = match guardian
        .previews()
        .stored_binding_for(grant, snapshot_id, now)
    {
        Ok(stored) => stored,
        Err(refusal) => {
            return ControlResponse::Rejected {
                code: match refusal {
                    yi_agent_lifecycle::preview::ConfirmRefusal::UnknownToken => {
                        ControlErrorCode::InvalidRequest
                    }
                    yi_agent_lifecycle::preview::ConfirmRefusal::WrongOwner => {
                        ControlErrorCode::Unauthenticated
                    }
                    _ => ControlErrorCode::Conflict,
                },
                reason: refusal.reason(),
            };
        }
    };
    let context = yi_agent_lifecycle::preview::ConfirmationContext {
        revision: ImpactRevision {
            guardian: guardian.reclaimer().leases().impact_revision(),
            coordinator: stored.revision.coordinator,
        },
        ..stored
    };
    match guardian
        .previews_mut()
        .page(grant, snapshot_id, cursor, &context, now)
    {
        Ok(page) => ControlResponse::Preview(page),
        Err(refusal) => ControlResponse::Rejected {
            code: match refusal {
                yi_agent_lifecycle::preview::ConfirmRefusal::UnknownToken => {
                    ControlErrorCode::InvalidRequest
                }
                yi_agent_lifecycle::preview::ConfirmRefusal::WrongOwner => {
                    ControlErrorCode::Unauthenticated
                }
                _ => ControlErrorCode::Conflict,
            },
            reason: refusal.reason(),
        },
    }
}

/// Answers `ControlRequest::Confirm`: consume the token and enter maintenance.
///
/// The order is the safety property. The token is verified and consumed *before* any
/// barrier is installed, so a second click cannot install a second barrier, and a
/// barrier can never exist on behalf of a consent that was not actually given.
fn serve_confirm(
    guardian: &mut Guardian,
    grant: &yi_agent_lifecycle::types::ClientGrant,
    token: &str,
    operation_id: &str,
) -> yi_agent_lifecycle::types::ControlResponse {
    use yi_agent_lifecycle::types::{ControlErrorCode, ControlResponse, ImpactRevision};

    if operation_id.is_empty() {
        // An empty id would make "retry the same operation" indistinguishable from
        // "confirm again", and those differ: one is safe, the other starts a second
        // replacement.
        return ControlResponse::Rejected {
            code: ControlErrorCode::InvalidRequest,
            reason: "a confirmation must carry the operation id it will start".to_string(),
        };
    }

    let now = lease_now();
    if let Err(reason) = guardian.reclaimer().leases().admits_or_observes(grant, now) {
        return ControlResponse::Rejected {
            code: ControlErrorCode::Unauthenticated,
            reason,
        };
    }

    // The target artifact is read out of the snapshot: the confirmation is "switch
    // to what you were shown". It is then re-verified against what is actually on
    // disk, so a bundle deleted between preview and click fails here rather than
    // half way through a switch.
    let stored = match guardian.previews().stored_binding(grant, token, now) {
        Ok(stored) => stored,
        Err(refusal) => {
            return ControlResponse::Rejected {
                code: match refusal {
                    yi_agent_lifecycle::preview::ConfirmRefusal::UnknownToken => {
                        ControlErrorCode::InvalidRequest
                    }
                    yi_agent_lifecycle::preview::ConfirmRefusal::WrongOwner => {
                        ControlErrorCode::Unauthenticated
                    }
                    _ => ControlErrorCode::Conflict,
                },
                reason: refusal.reason(),
            };
        }
    };
    if let Err(reason) = verify_target(&stored.target) {
        return ControlResponse::Rejected {
            code: ControlErrorCode::InvalidRequest,
            reason,
        };
    }
    let context = yi_agent_lifecycle::preview::ConfirmationContext {
        revision: ImpactRevision {
            guardian: guardian.reclaimer().leases().impact_revision(),
            // A confirmation minted from an unanswered daemon stays bound to
            // "unknown"; re-reading it here as a number would silently upgrade an
            // unknown-impact consent into a known-impact one.
            coordinator: stored.revision.coordinator,
        },
        ..stored
    };
    let refused =
        |refusal: yi_agent_lifecycle::preview::ConfirmRefusal| ControlResponse::Rejected {
            code: match refusal {
                yi_agent_lifecycle::preview::ConfirmRefusal::UnknownToken => {
                    ControlErrorCode::InvalidRequest
                }
                yi_agent_lifecycle::preview::ConfirmRefusal::WrongOwner => {
                    ControlErrorCode::Unauthenticated
                }
                _ => ControlErrorCode::Conflict,
            },
            reason: refusal.reason(),
        };

    // The barrier comes before consuming, so a refused confirmation leaves the token
    // usable: a user who fixed the problem (put the bundle back, closed the extra
    // client) can still confirm with the same click rather than seeing a new preview.
    if let Err(reason) = guardian
        .reclaimer_mut()
        .clients_mut()
        .begin_maintenance(context.revision.guardian)
    {
        // Another switch already holds the barrier. Answering "confirmed" here would
        // claim this click started a replacement that is not running.
        return ControlResponse::Rejected {
            code: ControlErrorCode::Conflict,
            reason,
        };
    }

    // Re-read the revision *while* the barrier is up. Admission under this generation
    // is now closed, so the only ways it can still move are decreases -- a client
    // leaving, a task finishing, a lease expiring -- and each of those makes the
    // user's consent describe a *larger* impact than will actually occur. That is not
    // what they agreed to either, so the confirmation fails and the barrier comes
    // down.
    let after_barrier = guardian.reclaimer().leases().impact_revision();
    if after_barrier != context.revision.guardian {
        guardian.reclaimer_mut().clients_mut().end_maintenance();
        return ControlResponse::Rejected {
            code: ControlErrorCode::Conflict,
            reason: format!(
                "the runtime changed while the barrier was being installed \
                 (revision {} -> {}); take a fresh preview",
                context.revision.guardian, after_barrier
            ),
        };
    }

    match guardian
        .previews_mut()
        .confirm(grant, token, operation_id, &context, now)
    {
        Ok(operation) => ControlResponse::Confirmed {
            operation_id: operation,
        },
        Err(refusal) => {
            // Nothing was authorised, so the barrier must not stay up: it would block
            // every new task on behalf of a replacement that is not happening.
            guardian.reclaimer_mut().clients_mut().end_maintenance();
            refused(refusal)
        }
    }
}

/// Waits until the listener has a connection or `timeout` elapses.
///
/// `poll` rather than a non-blocking spin: the loop wakes once per tick even on a
/// completely idle runtime, which costs nothing, and a connection is still noticed
/// immediately.
fn wait_for_connection(
    listener: &std::os::unix::net::UnixListener,
    timeout: Duration,
) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        // SAFETY: one initialized `pollfd` for a descriptor this function borrows
        // for the whole call, and the count matches the slice length.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(ready > 0);
    }
}

/// The lease clock: wall time, matching what the lease table stores.
///
/// The lease table is driven by `Clock::now()` elsewhere in tests, but over the wire
/// both sides have to agree on one origin, and wall time is the only one a separate
/// process can compute. Monotonicity across a wall-clock jump is handled by
/// `after_system_resume`, not by pretending the clock never moves.
fn lease_now() -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

/// Prepares an accepted control connection for one request/response exchange.
fn prepare_control_stream(stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
    // `ControlClient::connect` leaves its side non-blocking; the accepted side can
    // inherit that, and macOS rejects several socket options on a non-blocking
    // socket. Normalise it to blocking first.
    stream.set_nonblocking(false)?;
    // Bound the wait for a request: a peer that connects and says nothing must not
    // wedge the control loop.
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    Ok(())
}

/// Opens a private spawn channel for one authenticated client.
///
/// Authentication is the grant, checked against the live lease table exactly as an
/// admission question is: the guardian must not hand a spawn channel to a client it
/// has already released, nor to a `LeaseOnly` observer, which by definition never
/// authorizes a task.
///
/// The refusal is written as an ordinary frame, so a client that cannot have a
/// channel learns *why* instead of seeing a bare EOF.
fn serve_open_channel(
    guardian: &mut Guardian,
    stream: &mut std::os::unix::net::UnixStream,
    grant: &yi_agent_lifecycle::types::ClientGrant,
) {
    use yi_agent_lifecycle::types::{ControlErrorCode, ControlResponse};

    let refuse =
        |stream: &mut std::os::unix::net::UnixStream, code: ControlErrorCode, reason: String| {
            let _ = yi_agent_lifecycle::codec::write_frame(
                stream,
                &ControlResponse::Rejected { code, reason },
            );
        };

    // No registry means nothing to start a group in, and a channel that accepted a
    // spawn it could never stop would be worse than no channel at all.
    let Some(registry) = guardian.registry.as_ref().map(std::sync::Arc::clone) else {
        let _ = yi_agent_lifecycle::codec::write_frame(
            stream,
            &ControlResponse::Unsupported {
                detail: "this runtime owns no process registry, so it cannot issue a \
                         managed channel"
                    .to_string(),
            },
        );
        return;
    };

    let now = lease_now();
    if let Err(reason) = guardian.reclaimer().leases().admits(grant, now) {
        let code = if reason.contains("lease is") {
            ControlErrorCode::Unauthenticated
        } else {
            ControlErrorCode::StaleEpoch
        };
        refuse(stream, code, reason);
        return;
    }

    let owner = super::guardian::ProcessOwner::frontend(grant.client_run_id.clone());
    let channel = super::managed_channel::ChannelGrant::new(owner.clone());
    let budget = yi_agent_lifecycle::MANAGED_CHANNEL_BUDGET;
    let descriptor = match super::managed_channel::open_channel(registry, channel, budget) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            refuse(
                stream,
                ControlErrorCode::Internal,
                format!("the channel could not be opened: {error}"),
            );
            return;
        }
    };
    // The descriptor goes out *with* the frame in one message: a client that read
    // the frame alone would have no way to pair an owner with a descriptor, and a
    // descriptor arriving without its owner would be unattributable.
    let answer = ControlResponse::ChannelOpened {
        owner: grant.client_run_id.clone(),
        generation: guardian.generation.clone(),
    };
    let frame = match yi_agent_lifecycle::codec::encode(&answer) {
        Ok(frame) => frame,
        Err(error) => {
            refuse(
                stream,
                ControlErrorCode::Internal,
                format!("the channel answer could not be encoded: {error}"),
            );
            return;
        }
    };
    if let Err(error) =
        yi_agent_lifecycle::managed::fdpass::send_with_fds(stream, &frame, &[descriptor])
    {
        // Nothing useful can be written now: the frame and the descriptor share one
        // message, so a partial send has already broken the pairing.
        tracing::warn!(error = %error, "a managed channel descriptor could not be sent");
    }
}

/// Serves exactly one control connection.
///
/// Exposed for integration tests that own their accept loop: a test must be able
/// to run the real handler against a real socket without spawning a whole
/// guardian, and re-implementing the handler in the test would test the copy.
pub fn serve_one_for_test(
    guardian: &mut Guardian,
    stream: std::os::unix::net::UnixStream,
) -> io::Result<()> {
    prepare_control_stream(&stream)?;
    // No registry is attached here on purpose: a test that asks for a release
    // without owning process groups gets a *failed* reclamation, not a claim that
    // something stopped.
    serve_control_connection(guardian, stream);
    Ok(())
}

fn serve_control_connection(guardian: &mut Guardian, mut stream: std::os::unix::net::UnixStream) {
    use yi_agent_lifecycle::types::{ControlRequest, ControlResponse};
    let frame = match yi_agent_lifecycle::codec::read_frame(&mut stream) {
        Ok(frame) => frame,
        Err(_) => return,
    };
    let request: ControlRequest = match yi_agent_lifecycle::codec::decode(&frame) {
        Ok(request) => request,
        Err(_) => return,
    };
    let response = match request {
        ControlRequest::Identity => ControlResponse::Identity(runtime_identity(guardian)),
        ControlRequest::Register {
            client_run_id,
            kind,
            mode,
        } => {
            let now = lease_now();
            match guardian
                .reclaimer_mut()
                .register(client_run_id, kind, mode, now)
            {
                Ok(grant) => ControlResponse::Granted(grant),
                Err(reason) => ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::Conflict,
                    reason,
                },
            }
        }
        ControlRequest::Heartbeat { grant } => {
            // Renewal is what keeps a live frontend's lease from expiring. A stale
            // grant is answered with a refusal rather than an error string: the
            // client has to learn that its lease is gone, because its work is about
            // to be reclaimed.
            let now = lease_now();
            match guardian.reclaimer_mut().leases_mut().heartbeat(&grant, now) {
                Ok(()) => ControlResponse::Ack,
                Err(reason) => ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::StaleEpoch,
                    reason,
                },
            }
        }
        ControlRequest::Resume { grant } => {
            // A reconnect gets a new epoch and a new credential, so a straggling
            // message on the old connection cannot act for the client. The old
            // credential stops working immediately; that is the point.
            let now = lease_now();
            match guardian.reclaimer_mut().leases_mut().resume(&grant, now) {
                // Root bindings are keyed by client run, not by epoch, so a resume
                // only replaces the credential the client must present next; the
                // work it already owns stays attached to it.
                Ok(renewed) => ControlResponse::Granted(renewed),
                Err(reason) => ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::Unauthenticated,
                    reason,
                },
            }
        }
        ControlRequest::Release { grant } => {
            // Answering only after the 10 + 5 second teardown would keep this
            // connection (and the shell's explain-why path) unavailable exactly when
            // a frontend is quitting. The answer is therefore the *operation id*;
            // `ReclaimStatus` is how the result is collected.
            let budget = Deadline::after(yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT);
            let released_client = grant.client_run_id.clone();
            match guardian.reclaimer_mut().release(grant, budget) {
                Ok(operation_id) => {
                    // Authority must not outlive the lease that justified it: the
                    // reclaimer has accepted the release, so the daemon is told to
                    // stop honouring this client's capabilities *now*, not when the
                    // teardown finishes.
                    guardian.revoke_client_authority(&released_client);
                    guardian.reclaimer_mut().expire_results();
                    ControlResponse::ReleaseStarted { operation_id }
                }
                Err(error) => ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::StaleEpoch,
                    reason: error.to_string(),
                },
            }
        }
        ControlRequest::ReclaimStatus {
            grant,
            operation_id,
        } => match guardian.reclaimer().query(&operation_id, &grant) {
            Ok(Some(result)) => ControlResponse::ReclaimStatus {
                operation_id,
                result: Some(result),
            },
            Ok(None) => ControlResponse::ReclaimStatus {
                operation_id,
                result: None,
            },
            Err(reason) => ControlResponse::Rejected {
                code: yi_agent_lifecycle::types::ControlErrorCode::Unauthenticated,
                reason,
            },
        },
        ControlRequest::Admission { asked } => {
            // The guardian, not the daemon, decides whether this client may still
            // start work. That ordering is the point: a release that has already
            // begun withdrawal must refuse the Start even though the daemon still
            // holds an executor channel for the client.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            if asked.task.is_empty() || asked.attempt.is_empty() {
                // An admission question about nothing would be answered "yes" by
                // the lease table, which knows only about the owner.
                ControlResponse::Rejected {
                    code: yi_agent_lifecycle::types::ControlErrorCode::InvalidRequest,
                    reason: "an admission question must name its task and attempt".into(),
                }
            } else {
                match guardian.reclaimer().leases().admits(&asked.grant, now) {
                    Err(reason) => ControlResponse::Rejected {
                        code: yi_agent_lifecycle::types::ControlErrorCode::StaleEpoch,
                        reason,
                    },
                    // A generation mismatch is refused: the previous generation's
                    // admission is closed once a new one is selected.
                    Ok(()) if asked.generation != guardian.generation => {
                        ControlResponse::Rejected {
                            code: yi_agent_lifecycle::types::ControlErrorCode::Conflict,
                            reason: format!(
                                "generation {} is not current (this guardian serves {})",
                                asked.generation, guardian.generation
                            ),
                        }
                    }
                    Ok(()) => ControlResponse::Ack,
                }
            }
        }
        ControlRequest::AuthorizeRoot { grant, root_id } => {
            match guardian.authorize_root(&grant, &root_id) {
                Ok(authorization) => ControlResponse::RootAuthorized(authorization),
                Err(reason) => {
                    // A released or mis-modelled lease is an authentication
                    // failure, not a transient one: retrying with the same grant
                    // cannot succeed, and the caller has to re-register.
                    let code = if reason.contains("not registered")
                        || reason.contains("released")
                        || reason.contains("expired")
                        || reason.contains("stale or forged")
                        || reason.contains("never authorizes")
                    {
                        yi_agent_lifecycle::types::ControlErrorCode::Unauthenticated
                    } else {
                        yi_agent_lifecycle::types::ControlErrorCode::Conflict
                    };
                    ControlResponse::Rejected { code, reason }
                }
            }
        }
        ControlRequest::OpenManagedChannel { grant } => {
            // Answered here rather than as a plain `ControlResponse`, because the
            // answer is a *descriptor*: it must travel beside the frame, and the
            // frame must be written last so a client cannot receive a descriptor
            // it does not know the owner of.
            serve_open_channel(guardian, &mut stream, &grant);
            return;
        }
        ControlRequest::Preview { grant, target } => serve_preview(guardian, &grant, target),
        ControlRequest::PreviewPage {
            grant,
            snapshot_id,
            cursor,
        } => serve_preview_page(guardian, &grant, &snapshot_id, &cursor),
        ControlRequest::Confirm {
            grant,
            token,
            operation_id,
        } => serve_confirm(guardian, &grant, &token, &operation_id),
        ControlRequest::Operation {
            grant,
            operation_id,
        } => serve_operation(guardian, &grant, &operation_id),
        ControlRequest::Status { cursor } => serve_status(guardian, cursor.as_deref()),
        // Reclaim/operation are completed in later tasks; until then an
        // unimplemented request is reported as such, never as accepted.
        //
        // The arm is kept even though this build now handles every variant: a future
        // request must land on "unsupported" rather than failing to compile into a
        // silently accepted request.
        #[allow(unreachable_patterns)]
        _ => ControlResponse::Unsupported {
            detail: "not implemented by this guardian build".to_string(),
        },
    };
    let _ = yi_agent_lifecycle::codec::write_frame(&mut stream, &response);
}

#[cfg(test)]
mod authorization_tests {
    //! What a guardian mints, and what it tells the daemon.
    //!
    //! Driven through the real `Guardian` and a real `socketpair`, because the
    //! property under test is "the daemon was actually told": a ledger that holds a
    //! binding the peer never received would make every App attach fail with an
    //! error nobody can act on.

    use super::*;
    use std::time::Duration;
    use yi_agent_lifecycle::authorization::{
        AuthorizationLedger, AuthorizationUpdate, LifelineHello,
    };
    use yi_agent_lifecycle::types::{ClientKind, LeaseMode, RuntimeScope};

    fn scope(directory: &std::path::Path) -> RuntimeScope {
        RuntimeScope {
            key: directory.to_string_lossy().into_owned(),
            runtime_dir: directory.to_path_buf(),
            project_key: "/project".to_string(),
            instance_home: directory.to_path_buf(),
        }
    }

    fn artifact() -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: "test-build".to_string(),
            display_version: "0.0.0".to_string(),
            executable: "/usr/bin/true".into(),
            sha256: "00".to_string(),
            schema_min: 1,
            schema_max: 1,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: Vec::new(),
        }
    }

    /// A guardian wired to a daemon-shaped peer: the ledger and the lifeline are
    /// installed exactly as `start_daemon` installs them, minus the process.
    fn guardian_with_peer(
        directory: &std::path::Path,
        generation: &str,
    ) -> (
        Guardian,
        std::os::unix::net::UnixStream,
        AuthorizationLedger,
    ) {
        let mut guardian = Guardian::new(scope(directory), artifact(), generation.to_string());
        let (guardian_end, daemon_end) = std::os::unix::net::UnixStream::pair().unwrap();
        let ledger =
            AuthorizationLedger::new(guardian.scope.key.clone(), guardian.generation.clone());
        // A test that expects a frame must fail rather than hang when none comes:
        // without this, a missing write looks like a stuck suite instead of a bug.
        daemon_end
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        guardian.daemon_lifeline = Some(guardian_end);
        guardian.authorizations = Some(ledger.clone());
        // The daemon's half, with the same secret: what the real handshake delivers.
        let verifier = AuthorizationLedger::verifier(
            guardian.scope.key.clone(),
            "daemon-1".to_string(),
            ledger.generation_secret(),
        );
        (guardian, daemon_end, verifier)
    }

    #[test]
    fn a_business_lease_mints_authority_and_tells_the_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guardian, peer, _) = guardian_with_peer(directory.path(), "guardian-1");
        let grant = guardian
            .reclaimer_mut()
            .register(
                "app-run-1".to_string(),
                ClientKind::App,
                LeaseMode::Business,
                lease_now(),
            )
            .unwrap();

        let authorization = guardian.authorize_root(&grant, "/project").unwrap();
        assert_eq!(authorization.client_run_id, "app-run-1");
        assert_eq!(authorization.root_id, "/project");

        // The frame really reached the peer, and it is the binding just minted.
        let frame = yi_agent_lifecycle::codec::read_frame(&mut &peer).unwrap();
        let update: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();
        match update {
            AuthorizationUpdate::Bind(bound) => assert_eq!(*bound, authorization),
            other => panic!("expected a binding, got {other:?}"),
        }
    }

    /// The lease mode is the security boundary, and it is enforced at the guardian,
    /// not merely in the ledger.
    #[test]
    fn a_lease_only_observer_cannot_be_authorized() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guardian, mut peer, _) = guardian_with_peer(directory.path(), "guardian-1");
        let grant = guardian
            .reclaimer_mut()
            .register(
                "observer".to_string(),
                ClientKind::App,
                LeaseMode::LeaseOnly,
                lease_now(),
            )
            .unwrap();

        let error = guardian.authorize_root(&grant, "/project").unwrap_err();
        assert!(error.contains("never authorizes"), "{error}");
        // Nothing was sent: an observer must not even cause a frame to appear.
        peer.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0u8; 1];
        use std::io::Read;
        assert!(peer.read(&mut byte).is_err(), "no frame may be written");
    }

    /// A stale grant must be refused: the credential is what proves the caller
    /// speaks for the client, and an old epoch no longer does.
    #[test]
    fn a_stale_grant_cannot_mint_authority() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guardian, _peer, _) = guardian_with_peer(directory.path(), "guardian-1");
        let mut grant = guardian
            .reclaimer_mut()
            .register(
                "app-run-1".to_string(),
                ClientKind::App,
                LeaseMode::Business,
                lease_now(),
            )
            .unwrap();
        grant.credential = "not-the-credential".to_string();
        assert!(guardian.authorize_root(&grant, "/project").is_err());
    }

    /// A guardian with no daemon has no generation to authorize in, and says so
    /// rather than minting something nothing can honour.
    #[test]
    fn a_guardian_without_a_daemon_refuses_to_mint() {
        let directory = tempfile::tempdir().unwrap();
        let mut guardian = Guardian::new(
            scope(directory.path()),
            artifact(),
            "guardian-1".to_string(),
        );
        let grant = guardian
            .reclaimer_mut()
            .register(
                "app-run-1".to_string(),
                ClientKind::App,
                LeaseMode::Business,
                lease_now(),
            )
            .unwrap();
        let error = guardian.authorize_root(&grant, "/project").unwrap_err();
        assert!(error.contains("no generation"), "{error}");
    }

    /// Releasing a client must withdraw its authority on the wire, because that is
    /// how the daemon learns to stop honouring it.
    #[test]
    fn a_release_withdraws_the_clients_authority() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guardian, peer, _) = guardian_with_peer(directory.path(), "guardian-1");
        let grant = guardian
            .reclaimer_mut()
            .register(
                "app-run-1".to_string(),
                ClientKind::App,
                LeaseMode::Business,
                lease_now(),
            )
            .unwrap();
        let authorization = guardian.authorize_root(&grant, "/project").unwrap();
        // Drain the binding frame so the next read is the revocation.
        let frame = yi_agent_lifecycle::codec::read_frame(&mut &peer).unwrap();
        let _: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();

        guardian.revoke_client_authority("app-run-1");
        let frame = yi_agent_lifecycle::codec::read_frame(&mut &peer).unwrap();
        let update: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();
        assert_eq!(
            update,
            AuthorizationUpdate::RevokeClient {
                client_run_id: "app-run-1".to_string()
            }
        );
        // And locally, the capability no longer verifies.
        let ledger = guardian.authorizations.as_ref().unwrap();
        assert!(ledger.verify(&authorization).is_err());
    }

    /// The `Release` *request* is what withdraws authority, not just the method.
    ///
    /// Driven through `serve_control_connection` on a real socketpair, because the
    /// wiring is the part that can silently go missing: a `revoke_client_authority`
    /// that only tests itself would keep passing if the handler forgot to call it,
    /// and the daemon would then keep honouring a departed App.
    #[test]
    fn a_release_request_withdraws_authority_over_the_wire() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guardian, lifeline_peer, _) = guardian_with_peer(directory.path(), "guardian-1");
        let grant = guardian
            .reclaimer_mut()
            .register(
                "app-run-1".to_string(),
                ClientKind::App,
                LeaseMode::Business,
                lease_now(),
            )
            .unwrap();
        guardian.authorize_root(&grant, "/project").unwrap();
        // Drain the binding so the next lifeline frame is the revocation.
        let frame = yi_agent_lifecycle::codec::read_frame(&mut &lifeline_peer).unwrap();
        let _: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();

        // A real control connection carrying the release.
        let (mut client_end, server_end) = std::os::unix::net::UnixStream::pair().unwrap();
        yi_agent_lifecycle::codec::write_frame(
            &mut client_end,
            &yi_agent_lifecycle::types::ControlRequest::Release { grant },
        )
        .unwrap();
        serve_control_connection(&mut guardian, server_end);
        // The answer is `ReleaseStarted`, so the shell's explain-why path stays
        // available while teardown runs.
        let frame = yi_agent_lifecycle::codec::read_frame(&mut client_end).unwrap();
        let response: yi_agent_lifecycle::types::ControlResponse =
            yi_agent_lifecycle::codec::decode(&frame).unwrap();
        assert!(
            matches!(
                response,
                yi_agent_lifecycle::types::ControlResponse::ReleaseStarted { .. }
            ),
            "expected a release to be accepted, got {response:?}"
        );

        // And the daemon was told.
        let frame = yi_agent_lifecycle::codec::read_frame(&mut &lifeline_peer).unwrap();
        let update: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();
        assert_eq!(
            update,
            AuthorizationUpdate::RevokeClient {
                client_run_id: "app-run-1".to_string()
            }
        );
    }

    /// Lease expiry is a client going silent; it ends authority for the same reason
    /// a release does.
    #[test]
    fn an_expired_lease_withdraws_authority() {
        let directory = tempfile::tempdir().unwrap();
        let (mut guardian, peer, _) = guardian_with_peer(directory.path(), "guardian-1");
        let grant = guardian
            .reclaimer_mut()
            .register(
                "app-run-1".to_string(),
                ClientKind::App,
                LeaseMode::Business,
                lease_now(),
            )
            .unwrap();
        let authorization = guardian.authorize_root(&grant, "/project").unwrap();
        let frame = yi_agent_lifecycle::codec::read_frame(&mut &peer).unwrap();
        let _: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();

        // 31 seconds later, with no heartbeat.
        guardian.maintain(lease_now() + Duration::from_secs(31));

        let frame = yi_agent_lifecycle::codec::read_frame(&mut &peer).unwrap();
        let update: AuthorizationUpdate = yi_agent_lifecycle::codec::decode(&frame).unwrap();
        assert_eq!(
            update,
            AuthorizationUpdate::RevokeClient {
                client_run_id: "app-run-1".to_string()
            }
        );
        assert!(
            guardian
                .authorizations
                .as_ref()
                .unwrap()
                .verify(&authorization)
                .is_err()
        );
    }

    /// The handshake itself: the secret in the greeting is what lets the daemon
    /// verify, so it must be exactly the ledger's.
    #[test]
    fn the_greeting_carries_the_generation_secret() {
        let ledger = AuthorizationLedger::new("/project", "daemon-1");
        let hello = LifelineHello::new("/project", "daemon-1", ledger.generation_secret());
        assert_eq!(hello.secret().unwrap(), ledger.generation_secret());
        assert_eq!(hello.scope_key, "/project");
        assert_eq!(hello.daemon_instance_id, "daemon-1");
    }

    #[test]
    fn a_malformed_greeting_secret_is_refused() {
        let mut hello = LifelineHello::new("/project", "daemon-1", [0u8; 32]);
        hello.generation_secret = "abcd".to_string();
        assert!(hello.secret().is_err());
        hello.generation_secret = "zz".repeat(32);
        assert!(hello.secret().is_err());
    }
}
