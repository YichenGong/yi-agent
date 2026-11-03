//! Cross-process coordination for one runtime directory.
//!
//! Two locks exist per runtime and they mean different things:
//!
//! - **The coordination lock** (here) serialises *probe → start → switch*. It is
//!   held briefly by whoever is deciding what should run.
//! - **The daemon instance lock** (`yi-agent-store`'s `InstanceLock`) is held for
//!   the whole life of a running daemon.
//!
//! Keeping them separate is what lets a coordinator decide "a daemon is already
//! serving this runtime, leave it alone" without itself being mistaken for that
//! daemon.
//!
//! Both locks use a **fixed inode**: the lock file is created once and never
//! unlinked. Unlinking on release races a concurrent acquirer that already opened
//! that inode — the newcomer would hold a lock on an unlinked file while a third
//! process created a fresh one, so two holders would both believe they own the
//! runtime.

pub mod admission;
pub mod guardian;
pub mod processes;
pub mod scope;
pub mod selection;

use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::RuntimeScope;

/// Lock file name inside the runtime directory. Distinct from the daemon's
/// `runtime.lock` on purpose: see the module docs.
pub const COORDINATION_LOCK_FILE: &str = "coordination.lock";

/// How often a waiting acquirer retries while the budget has time left.
const RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// RAII ownership of one runtime's coordination lock.
///
/// Dropping it closes the descriptor, which releases the `flock`. There is no
/// explicit unlock: a process that dies without unwinding still releases it.
pub struct CoordinationGuard {
    file: std::fs::File,
    scope: RuntimeScope,
    path: PathBuf,
}

impl CoordinationGuard {
    /// Take the lock, waiting up to `deadline` for another coordinator to finish.
    ///
    /// Waiting is bounded by the caller's total budget rather than by a fixed
    /// per-attempt timeout: the spec forbids per-hop budgets that multiply, and an
    /// exhausted budget must fail fast instead of blocking a UI thread.
    pub fn acquire(scope: &RuntimeScope, deadline: Deadline) -> io::Result<Self> {
        std::fs::create_dir_all(&scope.runtime_dir)?;
        let path = scope.runtime_dir.join(COORDINATION_LOCK_FILE);

        loop {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if outcome == 0 {
                // Re-check that the descriptor still names the file at `path`. If
                // some other process unlinked and recreated it between our open and
                // our flock, we locked an orphaned inode and a later acquirer would
                // happily lock the new file alongside us.
                let held = file.metadata()?;
                let on_disk = match std::fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    // Someone removed the file after we locked it. Retry; the lock
                    // we hold is on an inode nobody else will open.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if held.ino() != on_disk.ino() || held.dev() != on_disk.dev() {
                    continue;
                }
                return Ok(Self {
                    file,
                    scope: scope.clone(),
                    path,
                });
            }

            let error = io::Error::last_os_error();
            let contended = matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            );
            if !contended {
                return Err(error);
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "another coordinator holds {} for {}",
                        COORDINATION_LOCK_FILE,
                        scope.runtime_dir.display()
                    ),
                ));
            }
            std::thread::sleep(deadline.clamp(RETRY_INTERVAL));
        }
    }

    /// The scope this guard owns.
    pub fn scope(&self) -> &RuntimeScope {
        &self.scope
    }

    /// The lock file's path. Exposed so tests can prove the inode is stable.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Metadata of the locked descriptor.
    pub fn file_metadata(&self) -> io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
}

impl std::fmt::Debug for CoordinationGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoordinationGuard")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_name_does_not_collide_with_the_daemon_lock() {
        // Reusing `runtime.lock` would make every coordinator look like a running
        // daemon and vice versa.
        assert_ne!(COORDINATION_LOCK_FILE, "runtime.lock");
    }
}
