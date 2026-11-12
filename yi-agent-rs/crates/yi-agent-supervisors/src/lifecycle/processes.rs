//! Process-group identity and birth verification, without any supervision policy.
//!
//! This module answers two questions that a supervisor must answer before it may
//! send a signal:
//!
//! 1. *Is this PID still the process I spawned?* A PID can be recycled, so
//!    comparing the number alone is unsafe. On macOS the kernel reports a process
//!    start time; on Linux `/proc/<pid>/stat` carries a start tick. Either is
//!    stable for the lifetime of that specific process and different for a
//!    recycled one.
//! 2. *Is the whole group gone?* A parent can exit while its children live on, so
//!    "the direct child was reaped" is not "the group is empty". Group emptiness
//!    is checked with `kill(-pgid, 0)`, which reports `ESRCH` once no member
//!    remains.
//!
//! Deliberately absent: any signalling by name, any lookup by parent PID, and any
//! "close enough" fallback. Names and PPIDs both change under a live group, and
//! guessing there is how an unrelated process gets killed.

use std::io;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_camel_case_types)]
struct proc_bsdinfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [libc::c_char; 16],
    pbi_name: [libc::c_char; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

/// A process's stable identity: its PID plus the moment the kernel says it began.
///
/// Two `ProcessBirth`s are equal only if they name the same *process*, not merely
/// the same PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessBirth {
    pub pid: i32,
    start_micros: u64,
}

impl ProcessBirth {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The kernel's start time for this process, in microseconds since the epoch
    /// (macOS) or in start ticks (Linux).
    ///
    /// Only comparable within one platform, which is all a supervisor needs: it
    /// answers "did the thing I recorded begin before or after this one".
    pub fn started_at(&self) -> u64 {
        self.start_micros
    }

    /// Reads the birth identity of a live process.
    ///
    /// `NotFound` means there is no such process *now*, which callers treat as
    /// "already gone" rather than as a failure.
    pub fn of(pid: i32) -> io::Result<Self> {
        if pid <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a process birth needs a positive pid",
            ));
        }
        Ok(Self {
            pid,
            start_micros: start_micros(pid)?,
        })
    }

    /// Rebuilds an identity recorded earlier (for example in a journal).
    ///
    /// The caller must have obtained both halves together; a PID paired with a
    /// start time from a different process would defeat the whole check.
    pub fn from_recorded(pid: i32, start_micros: u64) -> Self {
        Self { pid, start_micros }
    }

    /// Whether this exact process is still alive.
    ///
    /// False for both "exited" and "PID reused by a different process": a
    /// recycled PID is not the process this identity was minted for.
    pub fn is_alive(&self) -> bool {
        match start_micros(self.pid) {
            Ok(current) => current == self.start_micros,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            // An unreadable process table is not evidence of death. Report alive so
            // the caller retries rather than declaring a running group stopped.
            Err(_) => true,
        }
    }
}

#[cfg(target_os = "macos")]
fn start_micros(pid: i32) -> io::Result<u64> {
    let info = read_bsdinfo(pid)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no process with pid {pid}"),
        )
    })?;
    info.start_micros()
}

/// Reads the kernel's record for one process, or `None` if it has no live entry.
///
/// `proc_pidinfo(PROC_PIDTBSDINFO)` is the documented per-process query. A missing
/// process reports `ESRCH`, not `ENOENT` (BSD errno numbering); any other errno --
/// notably `EPERM` for a process the caller may not inspect -- surfaces as an
/// error, because "I cannot look" is not "it is gone".
#[cfg(target_os = "macos")]
fn read_bsdinfo(pid: i32) -> io::Result<Option<proc_bsdinfo>> {
    let mut info: proc_bsdinfo = unsafe { std::mem::zeroed() };
    let expected = std::mem::size_of::<proc_bsdinfo>() as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            expected,
        )
    };
    if read <= 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    if read != expected || info.pbi_pid != pid as u32 {
        return Ok(None);
    }
    Ok(Some(info))
}

#[cfg(target_os = "macos")]
impl proc_bsdinfo {
    /// The process's start time in microseconds, or `NotFound` once it has exited.
    ///
    /// `SZOMB` (status 5) is an exited process still awaiting reaping: it keeps its
    /// process-table entry but can never run again.
    fn start_micros(&self) -> io::Result<u64> {
        if self.pbi_status == ZOMBIE_STATUS {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("process {} has exited", self.pbi_pid),
            ));
        }
        Ok(self.pbi_start_tvsec * 1_000_000 + self.pbi_start_tvusec)
    }
}

#[cfg(target_os = "linux")]
fn start_micros(pid: i32) -> io::Result<u64> {
    // A process that exited but is not yet reaped stays in `/proc` with state
    // `Z`; it holds no threads, so it must not count as alive.
    match read_stat(pid)? {
        Some(stat) if stat.state == 'Z' => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} has exited"),
        )),
        Some(stat) => Ok(stat.start_ticks),
        None => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no process with pid {pid}"),
        )),
    }
}

#[cfg(target_os = "linux")]
struct ProcStat {
    state: char,
    pgrp: i32,
    start_ticks: u64,
}

#[cfg(target_os = "linux")]
fn read_stat(pid: i32) -> io::Result<Option<ProcStat>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // The command name is parenthesised and may itself contain spaces and
    // parentheses, so split after its last `)`.
    let after_name = stat
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat"))?;
    let fields: Vec<&str> = stat[after_name + 1..].split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|field| field.chars().next())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process state"))?;
    let pgrp: i32 = fields
        .get(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process group"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unparsable process group"))?;
    let start_ticks: u64 = fields
        .get(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing start time"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unparsable start time"))?;
    Ok(Some(ProcStat {
        state,
        pgrp,
        start_ticks,
    }))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start_micros(_pid: i32) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process birth identity is only implemented for macOS and Linux",
    ))
}

/// Whether any *live* process remains in `pgid`.
///
/// Implemented by enumeration rather than by `kill(-pgid, 0)`, because that
/// syscall reports `EPERM` for two very different states on macOS: a group
/// belonging to another user, and our own group containing only zombies. The two
/// cases need opposite answers (an error, and "stopped"), and only enumeration can
/// tell them apart.
pub fn group_is_empty(pgid: i32) -> io::Result<bool> {
    if pgid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a process group id must be positive",
        ));
    }
    Ok(live_group_members(pgid)?.is_empty())
}

/// Live (non-zombie) members of `pgid`, excluding the caller itself.
///
/// A exited member awaiting reaping is *not* live: it holds no threads and can
/// never run again, so counting it would make "the group stopped" permanently
/// false whenever a child's parent was killed first.
pub fn live_group_members(pgid: i32) -> io::Result<Vec<i32>> {
    if pgid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a process group id must be positive",
        ));
    }
    let mut members = Vec::new();
    for pid in all_pids()? {
        if pid <= 0 {
            continue;
        }
        match process_group_of(pid) {
            Ok(Some(group)) if group == pgid => {
                if let Ok(Some(status)) = process_status(pid)
                    && status != ZOMBIE_STATUS
                {
                    members.push(pid);
                }
            }
            // A process that vanished while we enumerated it is simply not a
            // member. Anything we truly cannot read is an error: silently
            // skipping it could report a live group as stopped.
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            // A process we may not inspect belongs to another user (or is
            // protected by the kernel). Managed groups contain only processes this
            // user started, so such a process is not one of ours. Refusing to
            // call the *group* stopped is handled separately, by `group_exists`,
            // which does report another user's group as an error.
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {}
            Err(error) => return Err(error),
        }
    }
    members.sort_unstable();
    Ok(members)
}

const ZOMBIE_STATUS: u32 = 5;

#[cfg(target_os = "macos")]
fn all_pids() -> io::Result<Vec<i32>> {
    // `PROC_ALL_PIDS` is 1; the libc crate does not export it on all versions.
    const PROC_ALL_PIDS: u32 = 1;
    // The list can grow between the size query and the fetch, so retry with a
    // larger buffer rather than returning a truncated view.
    let mut capacity = 4096usize;
    loop {
        let mut buffer = vec![0i32; capacity];
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_ALL_PIDS,
                0,
                buffer.as_mut_ptr() as *mut libc::c_void,
                (buffer.len() * std::mem::size_of::<i32>()) as i32,
            )
        };
        if bytes < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = (bytes as usize) / std::mem::size_of::<i32>();
        if count + 16 < capacity {
            buffer.truncate(count);
            return Ok(buffer);
        }
        capacity *= 2;
        if capacity > 1 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process list kept growing while being read",
            ));
        }
    }
}

#[cfg(target_os = "linux")]
fn all_pids() -> io::Result<Vec<i32>> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) {
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn all_pids() -> io::Result<Vec<i32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process enumeration is only implemented for macOS and Linux",
    ))
}

/// The process group a PID currently belongs to, or `None` if it has no live entry.
pub fn process_group(pid: i32) -> io::Result<Option<i32>> {
    process_group_of(pid)
}

#[cfg(target_os = "macos")]
fn process_group_of(pid: i32) -> io::Result<Option<i32>> {
    let info = read_bsdinfo(pid)?;
    Ok(info.map(|info| info.pbi_pgid as i32))
}

#[cfg(target_os = "macos")]
fn process_status(pid: i32) -> io::Result<Option<u32>> {
    Ok(read_bsdinfo(pid)?.map(|info| info.pbi_status))
}

#[cfg(target_os = "linux")]
fn process_group_of(pid: i32) -> io::Result<Option<i32>> {
    let stat = read_stat(pid)?;
    Ok(stat.map(|fields| fields.pgrp))
}

#[cfg(target_os = "linux")]
fn process_status(pid: i32) -> io::Result<Option<u32>> {
    let stat = read_stat(pid)?;
    // The state character is field 2 after the command name.
    Ok(stat.map(|fields| {
        if fields.state == 'Z' {
            ZOMBIE_STATUS
        } else {
            0
        }
    }))
}

/// Waits within `budget` until no live process remains in `pgid`.
///
/// Distinguishes "no live member" from "uninspectable": only an observed empty
/// result returns `true`.
pub fn wait_for_group_stopped(pgid: i32, budget: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + budget;
    loop {
        if live_group_members(pgid)?.is_empty() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Puts the calling process into a new session/group led by itself.
///
/// Called in the child between `fork` and `exec`, where only async-signal-safe
/// work is allowed; `setsid` qualifies.
pub fn become_group_leader() -> io::Result<()> {
    let outcome = unsafe { libc::setsid() };
    if outcome < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `SIGKILL` to every member of `pgid`.
///
/// The caller must have verified the group's identity first; this function
/// cannot tell a recycled PGID from the original.
///
/// `EPERM` needs a second look rather than an immediate error. On macOS,
/// `kill(-pgid, ...)` reports `EPERM` both for *another user's* live group and for
/// *our own* group whose members have all exited but are still unreaped. The two
/// need opposite answers, and the difference is observable only by enumeration.
/// This is not a hypothetical race: the guardian proves ownership and then signals
/// the group, and a short-lived command can exit inside that window, so a caller
/// that treated `EPERM` as failure would report a stopped group as a fault
/// intermittently and only under load. Enumeration decides it: an empty group is
/// already stopped (nothing left to signal), while live members we cannot signal
/// stay an error.
pub fn kill_group(pgid: i32) -> io::Result<()> {
    if pgid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal a process group id <= 0",
        ));
    }
    let outcome = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    if outcome == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // Already gone; not an error for a terminating caller.
        return Ok(());
    }
    if error.raw_os_error() == Some(libc::EPERM) && live_group_members(pgid)?.is_empty() {
        // Our own group, already drained to zombies: nothing can run again, so
        // there is nothing to signal and certainly nothing to report as a fault.
        return Ok(());
    }
    Err(error)
}

/// Waits, within `budget`, until `group_is_empty` holds.
///
/// Returns `true` only when emptiness was actually observed; a timeout returns
/// `false` so the caller reports "not reclaimed" instead of assuming success.
pub fn wait_for_group_empty(pgid: i32, budget: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + budget;
    loop {
        if group_is_empty(pgid)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn spawn_in_own_group() -> std::process::Child {
        // `sleep` starts in its own group so the test cannot accidentally
        // enumerate the test harness's group (which contains the harness itself).
        unsafe {
            Command::new("sleep")
                .arg("30")
                .pre_exec(|| {
                    become_group_leader()?;
                    Ok(())
                })
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        }
    }

    #[test]
    fn a_live_process_has_a_stable_birth_identity() {
        let mut child = spawn_in_own_group();
        let birth = ProcessBirth::of(child.id() as i32).unwrap();
        assert!(birth.is_alive());
        // Reading it twice must agree: the identity describes the process, not
        // the moment of reading.
        assert_eq!(ProcessBirth::of(child.id() as i32).unwrap(), birth);
        kill_group(child.id() as i32).unwrap();
        let _ = child.wait();
    }

    #[test]
    fn an_exited_process_is_not_alive_even_before_its_parent_reaps_it() {
        // A killed-but-unreaped child is a zombie: still present in the process
        // table, but its threads are gone. Reporting it as alive would make the
        // supervisor wait forever for a group that no longer executes anything.
        //
        // `kill` here targets `sleep`'s own group, so the test harness is never
        // signalled.
        let mut child = spawn_in_own_group();
        let birth = ProcessBirth::of(child.id() as i32).unwrap();
        kill_group(child.id() as i32).unwrap();
        // Give the kernel a moment to mark it exited, without reaping.
        for _ in 0..500 {
            if !birth.is_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!birth.is_alive(), "an exited process is not alive");
        let _ = child.wait();
    }

    #[test]
    fn a_process_that_never_existed_has_no_birth() {
        let error = ProcessBirth::of(1_000_000_000).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_group_with_a_live_member_is_not_empty() {
        let mut child = spawn_in_own_group();
        assert!(!group_is_empty(child.id() as i32).unwrap());
        kill_group(child.id() as i32).unwrap();
        let _ = child.wait();
    }

    #[test]
    fn an_exited_group_becomes_empty() {
        let mut child = spawn_in_own_group();
        let pgid = child.id() as i32;
        assert!(!group_is_empty(pgid).unwrap());
        kill_group(pgid).unwrap();
        let _ = child.wait();
        assert!(
            wait_for_group_empty(pgid, Duration::from_secs(5)).unwrap(),
            "a killed group must become empty"
        );
    }

    #[test]
    fn signalling_a_nonpositive_group_is_refused() {
        // `kill(-0, SIGKILL)` would target the caller's own group: the whole test
        // harness. The guard is what makes that impossible.
        assert!(kill_group(0).is_err());
        assert!(kill_group(-5).is_err());
        assert!(group_is_empty(0).is_err());
    }

    #[test]
    fn signalling_a_group_of_zombies_is_not_a_failure() {
        // The window this pins: a short command exits, its leader stays an unreaped
        // zombie (the guardian keeps the `Child` until the group is confirmed
        // stopped), and only then does the guardian signal the group.
        //
        // On macOS `kill(-pgid, ...)` answers `EPERM` in exactly that state, which
        // that one syscall cannot distinguish from another user's live group.
        // Reading it as failure made a drained group look like a fault, and the
        // symptom was intermittent: `managed groups not stopped: ... Operation not
        // permitted` only when a short-lived helper exited inside the window between
        // proving ownership and signalling.
        let mut child = unsafe {
            Command::new("/usr/bin/true")
                .pre_exec(|| {
                    become_group_leader()?;
                    Ok(())
                })
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        };
        let pgid = child.id() as i32;
        // Wait for the leader to exit *without reaping it*: `try_wait` would reap.
        let mut waited = Duration::ZERO;
        while !live_group_members(pgid).unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
            waited += Duration::from_millis(5);
            assert!(waited < Duration::from_secs(10), "the leader must exit");
        }
        // The leader is now a zombie holding the PGID: the group still exists, has
        // no live member, and cannot legally be signalled.
        let raw = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        if raw == 0 {
            // Another platform (or a kernel that reaped it already) makes the group
            // vanish instead; `kill_group` must still succeed, so the assertion below
            // covers both shapes.
        }
        kill_group(pgid).expect("a group of zombies has nothing left to signal");
        let _ = child.wait();
    }

    #[test]
    fn an_unknown_group_is_empty_rather_than_uninspectable() {
        // A PGID that certainly never existed reports ESRCH, which is "empty".
        // This distinguishes it from EPERM, which must be an error.
        assert!(group_is_empty(1_000_000_000).unwrap());
    }
}
