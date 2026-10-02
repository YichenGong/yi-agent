//! A global concurrency lease shared across every project's board.
//!
//! The provider rate-limits us, and several boards may run at once — one per
//! project — so a per-project cap cannot bound the total. Each held slot is an
//! advisory `flock` in a **global** directory
//! (`$HOME/.yi-agent/superpowers-kanban/leases`), so every plugin process on the
//! machine draws from the same pool of `limit` slots.
//!
//! `flock` rather than a PID file, for the same reason the host's `InstanceLock`
//! uses it: the kernel releases the lock when the holding process exits,
//! *including* on `SIGKILL`. A PID file cannot — PIDs are recycled, so a slot
//! left by a dead plugin would look owned by an unrelated process and the quota
//! would be permanently short by one.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// One held concurrency slot. Releasing is dropping it: closing the descriptor
/// releases the `flock`, and a process that dies without unwinding releases it
/// just the same.
pub struct Lease {
    /// Closing this file's descriptor is the entire release protocol, so it is
    /// never read — only kept alive for as long as the lease is held.
    _file: File,
    path: PathBuf,
}

impl Lease {
    /// The slot file backing this lease. Exposed for tests and diagnostics.
    pub fn slot_path(&self) -> &Path {
        &self.path
    }
}

/// The directory every project's plugin leases from:
/// `$HOME/.yi-agent/superpowers-kanban/leases`.
///
/// `None` when `HOME` is unset. A caller that cannot resolve it must fall back
/// to per-project limiting rather than inventing a directory: two different
/// guesses would hand out two disjoint pools and silently overrun the provider.
pub fn global_leases_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".yi-agent")
                .join("superpowers-kanban")
                .join("leases")
        })
}

/// Takes one of `limit` slots in `dir`, or returns `None` when all are held.
///
/// Tries the slots in order, so a small `limit` reuses `slot-0`, `slot-1`, …
/// rather than scattering across a directory that has seen larger limits.
pub fn acquire_in(dir: &Path, limit: usize) -> Option<Lease> {
    if limit == 0 {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    for slot in 0..limit {
        let path = dir.join(format!("slot-{slot}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            return Some(Lease { _file: file, path });
        }
        let error = std::io::Error::last_os_error();
        // `EWOULDBLOCK`/`EAGAIN` is contention: this slot is taken, try the next.
        // Anything else (a bad descriptor, an unsupported filesystem) is a real
        // failure — stop rather than loop, so the error is visible instead of
        // being reported as a full pool.
        let contended = matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        );
        if !contended {
            return None;
        }
    }
    None
}

/// Convenience: lease from the global pool. `None` means either "no slot free"
/// or "no global pool resolvable" — callers use [`global_leases_dir`] to tell
/// those apart when the distinction matters.
pub fn acquire(limit: usize) -> Option<Lease> {
    acquire_in(&global_leases_dir()?, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_first_n_holders_get_slots_and_the_next_one_waits() {
        let dir = tempfile::tempdir().unwrap();
        let a = acquire_in(dir.path(), 2).unwrap();
        let _b = acquire_in(dir.path(), 2).unwrap();
        assert!(
            acquire_in(dir.path(), 2).is_none(),
            "第 3 个必须等：只有 2 个名额"
        );
        drop(a);
        assert!(
            acquire_in(dir.path(), 2).is_some(),
            "释放一个名额后必须能再领到"
        );
    }

    #[test]
    fn a_zero_limit_grants_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(acquire_in(dir.path(), 0).is_none());
    }

    #[test]
    fn slots_are_reused_in_order_instead_of_scattering() {
        let dir = tempfile::tempdir().unwrap();
        let lease = acquire_in(dir.path(), 2).unwrap();
        assert_eq!(lease.slot_path().file_name().unwrap(), "slot-0.lock");
    }

    #[test]
    fn a_limit_of_one_admits_exactly_one_holder() {
        let dir = tempfile::tempdir().unwrap();
        let _only = acquire_in(dir.path(), 1).unwrap();
        assert!(acquire_in(dir.path(), 1).is_none());
    }

    /// A slot held by a process that is `SIGKILL`ed must come back.
    ///
    /// The holder is this very test binary re-invoked with `LEASE_HOLD_DIR` set
    /// to run only [`lease_holder_child`]; that child takes one slot and sleeps.
    /// Killing it with `-9` skips any unwinding, so the only thing that can free
    /// the slot is the kernel dropping the `flock` on process exit — which is
    /// exactly the property that keeps the quota from leaking slots.
    #[test]
    fn a_slot_held_by_a_dead_process_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lease::tests::lease_holder_child",
                "--nocapture",
            ])
            .env("LEASE_HOLD_DIR", dir.path())
            .spawn()
            .expect("re-invoke the test binary as a slot holder");

        // Wait until the child actually holds the only slot.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut held = false;
        while std::time::Instant::now() < deadline {
            if acquire_in(dir.path(), 1).is_none() {
                held = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(held, "子进程必须在时限内占住唯一的名额");

        let status = std::process::Command::new("kill")
            .args(["-9", &holder.id().to_string()])
            .status()
            .expect("kill -9 the holder");
        assert!(status.success(), "kill -9 必须成功");
        // 收尸：`kill -9` 之后必须 `wait()`，否则子进程成为僵尸，clippy 也会报
        // 「spawned process is never waited on」。
        let _ = holder.wait();

        // Kernel releases the flock asynchronously with process teardown.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut reclaimed = false;
        while std::time::Instant::now() < deadline {
            if acquire_in(dir.path(), 1).is_some() {
                reclaimed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reclaimed, "SIGKILL 之后名额必须可回收，否则额度会被永久占死");
    }

    /// The slot holder, run only when re-invoked with `LEASE_HOLD_DIR` set.
    ///
    /// In a normal `cargo test` run the variable is absent and this returns
    /// immediately, so it never interferes with the suite.
    #[test]
    fn lease_holder_child() {
        let Ok(dir) = std::env::var("LEASE_HOLD_DIR") else {
            return;
        };
        let _lease = acquire_in(Path::new(&dir), 1).expect("holder must get the only slot");
        println!("LEASE-HELD");
        std::thread::sleep(Duration::from_secs(30));
    }
}
