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

/// How long opening one private spawn channel may take.
///
/// Short for the same reason a control exchange is short: the guardian serves it
/// from its own loop, so a slow answer means the loop is busy, not that the
/// channel is expensive to make.
const CHANNEL_BUDGET: Duration = Duration::from_secs(2);

/// One frontend process's lease, or nothing when embedded mode is in use.
///
/// Shared with the parties that must act *as* this frontend. On the managed path the
/// TUI attaches its conversation root inside the bring-up (before any tool exists) and
/// the root's authority broker needs the very lease that was registered then; handing
/// out clones would register a second client for one process and make "the last
/// frontend left" wrong, which is the one question this lease exists to answer.
#[derive(Clone)]
pub struct FrontendLeaseHandle {
    lease: std::sync::Arc<FrontendLease>,
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
        let Some(lease) =
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
        Ok(Some(Self {
            lease: std::sync::Arc::new(lease),
        }))
    }

    /// Registers a lease a *managed* frontend cannot do without.
    ///
    /// [`Self::register`] answers `Ok(None)` when no guardian serves the runtime,
    /// which is the honest answer for embedded mode. On the managed path that same
    /// `None` means the bring-up reported a runtime whose guardian is not there, so
    /// the two are opposite facts and must not share a return value: a managed
    /// frontend that proceeded would attach a root nobody can reclaim and open a
    /// tool channel nobody authorizes.
    ///
    /// Errors are returned rather than swallowed, including a heartbeat that would
    /// not start: a lease that cannot be renewed looks alive for one window and then
    /// expires under running work, and that failure is exactly what the lease exists
    /// to prevent.
    pub fn register_required(
        runtime_dir: &Path,
        kind: ClientKind,
        client_run_id: String,
    ) -> std::io::Result<Self> {
        let control_path = control_path(runtime_dir);
        let deadline = Deadline::after(Duration::from_secs(2));
        let mode = mode_for(kind);
        let lease = FrontendLease::register(&control_path, client_run_id, kind, mode, &deadline)?;
        Ok(Self {
            lease: std::sync::Arc::new(lease),
        })
    }

    /// Starts renewing this lease, or says why it could not.
    ///
    /// A separate step from registration on the managed path so "the runtime admitted
    /// this process" and "the runtime will keep believing that" are two observable
    /// facts. Collapsing them would let a launch report a joined frontend whose lease
    /// quietly expires one window later, under work the user is watching.
    pub fn require_heartbeat(&self) -> std::io::Result<()> {
        self.lease.start_heartbeat()
    }

    /// The same lease as a shared handle, for a party that must outlive this one.
    ///
    /// Still one lease: the `Arc` is the same allocation, so releasing through either
    /// handle releases this process once. Used by the root-authority broker, which is
    /// held by a conversation for as long as its tools live.
    pub fn shared(&self) -> std::sync::Arc<FrontendLease> {
        std::sync::Arc::clone(&self.lease)
    }

    /// The client run this frontend registered under.
    pub fn client_run_id(&self) -> &str {
        self.lease.client_run_id()
    }

    /// Builds the owner a managed tool spawn needs, opening one channel per spawn.
    ///
    /// The factory opens a *new* channel each time it is called, and that is not an
    /// optimisation to revisit: a channel's owner is fixed when the guardian creates
    /// it, so reusing one would let a second owner's processes answer on the first
    /// owner's connection. Opening one per child is also what keeps a tool from
    /// holding a channel across a guardian restart.
    ///
    /// `None` when the guardian will not issue a channel: a lease-only observer does
    /// not authorize work, and a runtime that owns no registry cannot reclaim what it
    /// starts. In both cases the tool layer must keep its direct spawn rather than
    /// inventing an owner -- an unmanaged child is at least honestly unmanaged.
    pub fn managed_owner(
        &self,
    ) -> Option<(
        yi_agent_tools::ManagedProcessPolicy,
        yi_agent_tools::ManagedSpawner,
    )> {
        self.managed_owner_result().ok()
    }

    /// [`Self::managed_owner`] without the "just keep spawning directly" fallback.
    ///
    /// A managed frontend has no direct spawn to fall back to: a child it started
    /// itself belongs to no client, so the guardian can neither journal it nor reclaim
    /// it, and the user's "stop that run" would stop nothing. Refusing here is what
    /// turns a refused channel into a refused turn instead of an orphaned process.
    pub fn managed_owner_required(
        &self,
    ) -> Result<
        (
            yi_agent_tools::ManagedProcessPolicy,
            yi_agent_tools::ManagedSpawner,
        ),
        String,
    > {
        self.managed_owner_result().map_err(|error| {
            format!("the managed runtime did not open a private spawn channel: {error}")
        })
    }

    fn managed_owner_result(
        &self,
    ) -> Result<
        (
            yi_agent_tools::ManagedProcessPolicy,
            yi_agent_tools::ManagedSpawner,
        ),
        std::io::Error,
    > {
        use std::sync::Arc;
        // Probe once, now: deciding per spawn would turn a refused channel into a
        // per-tool failure long after the session already looked healthy.
        let probe = self
            .lease
            .open_managed_channel(&Deadline::after(CHANNEL_BUDGET))?;
        let owner = probe.owner().to_string();
        // The probe's channel is spent on the first real spawn, so it is handed to
        // the factory rather than closed and reopened.
        let first = Arc::new(std::sync::Mutex::new(Some(probe)));
        let control_path = self.lease.control_path().to_path_buf();
        // The grant is this process's own credential. It is captured here so later
        // channels are opened without re-registering: a second registration would
        // mint a second lease for one process and make "the last frontend" wrong.
        let grant = self.lease.grant().clone();
        let factory: yi_agent_tools::ChannelFactory = Arc::new(move || {
            if let Some(channel) = first.lock().ok().and_then(|mut slot| slot.take()) {
                return Ok(channel.into_parts().0);
            }
            let channel = yi_agent_lifecycle::client::FrontendLease::open_channel_with(
                &control_path,
                &grant,
                &Deadline::after(CHANNEL_BUDGET),
            )?;
            Ok(channel.into_parts().0)
        });
        Ok((
            yi_agent_tools::ManagedProcessPolicy::Frontend {
                client_run_id: owner,
            },
            yi_agent_tools::ManagedSpawner::new(factory),
        ))
    }

    /// Asks the guardian for one conversation's root authority.
    ///
    /// Thin passthrough so the CLI does not need the lifecycle client type threaded
    /// through every call site. A refusal (the guardian's policy answer) and a
    /// transport failure are both surfaced as errors: unlike the desktop path, a
    /// managed CLI cannot run the turn with legacy semantics, because the daemon it
    /// would spawn against refuses an unattributed root.
    pub fn authorize_root(
        &self,
        intent: &yi_agent_lifecycle::foreground::RootBindIntent,
        deadline: &Deadline,
    ) -> std::io::Result<yi_agent_lifecycle::types::RootAuthorization> {
        self.lease.authorize_root(intent, deadline)
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
    pub fn release(&self) -> Option<yi_agent_lifecycle::types::ReclaimResult> {
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
