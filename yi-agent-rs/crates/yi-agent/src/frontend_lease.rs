//! This frontend's lifecycle lease with the runtime.
//!
//! A shared runtime has to answer one question about every process using it: is
//! this frontend still here? The daemon cannot tell from the business socket --
//! a long tool call looks exactly like a dead client -- so the guardian keeps a
//! lease per frontend, renewed over the control channel and expired when the
//! renewals stop.
//!
//! Two rules follow from what the lease is *for*:
//!
//! * **A lease is per process, not per session or per root.** A TUI that opens
//!   three threads is one frontend; three leases would make "the last client left"
//!   impossible to compute and would keep the runtime alive after the window closed.
//! * **Embedded mode holds no lease.** In embedded mode this process started the
//!   daemon, so its own lifetime *is* the runtime's lifetime; registering a lease
//!   against a guardian that does not exist would either fail every launch or, worse,
//!   quietly invent a second notion of who owns the runtime.

use std::path::{Path, PathBuf};
use std::time::Duration;

use yi_agent_lifecycle::client::FrontendLease;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ClientKind, LeaseMode};

/// How long a quitting frontend waits to learn how its teardown went.
///
/// Short on purpose: the runtime's own windows are 10 s of grace plus 5 s to
/// confirm, but a frontend is leaving and must not be the thing that keeps a
/// terminal window open. The result is also written to a file under the scope, so
/// a shell that gives up waiting can still read it.
pub const RELEASE_WAIT: Duration = Duration::from_secs(5);

/// One frontend process's lease, or nothing when embedded mode is in use.
pub struct FrontendLeaseHandle {
    lease: FrontendLease,
}

impl FrontendLeaseHandle {
    /// Registers this process as a frontend of `runtime_dir`'s runtime.
    ///
    /// `Ok(None)` means no guardian is serving there: embedded mode, or a runtime
    /// that has not been started yet. Both are normal, and neither is an error a
    /// launch should fail on -- delegation simply does not depend on a lease.
    pub fn register(
        runtime_dir: &Path,
        kind: ClientKind,
        client_run_id: String,
    ) -> std::io::Result<Option<Self>> {
        let control_path = control_path(runtime_dir);
        let deadline = Deadline::after(Duration::from_secs(2));
        let mode = mode_for(kind);
        let Some(mut lease) =
            FrontendLease::register_if_served(&control_path, client_run_id, kind, mode, &deadline)?
        else {
            return Ok(None);
        };
        // Best effort: a lease without renewals still lives for its window, which
        // is longer than any single call a frontend makes. Failing the launch over
        // a missing heartbeat thread would trade a whole session for a lease.
        if let Err(error) = lease.start_heartbeat() {
            tracing::warn!(error = %error, "frontend lease heartbeat unavailable");
        }
        Ok(Some(Self { lease }))
    }

    /// The client run this frontend registered under.
    pub fn client_run_id(&self) -> &str {
        self.lease.client_run_id()
    }

    /// Whether the runtime has stopped counting this frontend as present.
    pub fn is_lost(&self) -> bool {
        self.lease.is_lost()
    }

    /// Leaves the runtime, bounded by [`RELEASE_WAIT`].
    ///
    /// Returns the reclaim result when it arrived inside the budget. A `None`
    /// result is *not* success: it means the answer did not arrive, and the caller
    /// must not print "your work stopped".
    pub fn release(mut self) -> Option<yi_agent_lifecycle::types::ReclaimResult> {
        let deadline = Deadline::after(Duration::from_secs(2));
        let operation = match self.lease.release(&deadline) {
            Ok(operation) => operation,
            Err(error) => {
                // The guardian is gone or refused. Nothing to wait for; the scope
                // will expire this lease on its own.
                tracing::debug!(error = %error, "frontend lease release did not complete");
                return None;
            }
        };
        match self.lease.await_result(&operation, RELEASE_WAIT) {
            Ok(result) => result,
            Err(error) => {
                tracing::debug!(error = %error, "frontend lease result unavailable");
                None
            }
        }
    }
}

/// The lease mode a frontend kind asks for.
///
/// A lease is only ever business or observability. `WaitingNextGeneration` is
/// assigned by the guardian during maintenance, never requested by a client.
pub fn mode_for(kind: ClientKind) -> LeaseMode {
    match kind {
        // A TUI or a headless run admits work; that is the authority a business
        // lease carries, and without it the guardian would refuse every Start.
        ClientKind::Tui | ClientKind::Headless => LeaseMode::Business,
        // The desktop shell observes and controls upgrades. It runs no tasks, so
        // asking for business authority would invent work to reclaim on quit.
        ClientKind::App => LeaseMode::LeaseOnly,
    }
}

/// Where the guardian serves `runtime_dir`.
///
/// One function rather than a literal at each call site: a frontend that computed
/// its own path would talk to a socket nobody is listening on, and the symptom
/// (lease silently absent) is indistinguishable from embedded mode.
pub fn control_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(yi_agent_supervisors::lifecycle::guardian::GUARDIAN_SOCKET_FILE)
}

/// A client run id unique to this process and launch.
///
/// The process id alone is reused after a reboot, and the guardian refuses a run id
/// whose work it already reclaimed; the suffix is what makes "this launch" distinct
/// from "a previous launch by a recycled pid".
pub fn new_client_run_id(prefix: &str) -> String {
    format!("{prefix}:{}:{}", std::process::id(), uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_run_id_is_unique_per_launch() {
        let first = new_client_run_id("tui");
        let second = new_client_run_id("tui");
        assert_ne!(
            first, second,
            "a reused id would be refused by the guardian"
        );
        assert!(first.starts_with("tui:"));
        assert!(first.len() > "tui:".len());
    }

    #[test]
    fn only_the_kinds_that_run_work_get_a_business_lease() {
        assert_eq!(mode_for(ClientKind::Tui), LeaseMode::Business);
        assert_eq!(mode_for(ClientKind::Headless), LeaseMode::Business);
        // The observer must not hold the scope open: a desktop shell that asked for
        // business authority would make "the last client left" unreachable.
        assert_eq!(mode_for(ClientKind::App), LeaseMode::LeaseOnly);
    }

    #[test]
    fn the_control_path_is_the_guardians_socket_in_the_runtime_directory() {
        let path = control_path(Path::new("/tmp/project/runtime"));
        assert_eq!(
            path,
            Path::new("/tmp/project/runtime")
                .join(yi_agent_supervisors::lifecycle::guardian::GUARDIAN_SOCKET_FILE)
        );
    }
}
