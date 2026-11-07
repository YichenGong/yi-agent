//! The daemon's side of execution admission: ask the guardian before starting.
//!
//! The guardian owns the lease table, and the lease table is what knows whether a
//! client has been released. The daemon owns the executor channels. Those two facts
//! are in different processes, which is why a Start has to be admitted *before* it
//! is framed: once an executor channel exists, a stale daemon could otherwise keep
//! pushing work into a client the guardian has already withdrawn.
//!
//! The question is asked over the control socket, with the client's own grant. That
//! makes it owner-bound in both directions: the guardian learns *which* owner is
//! asking, and a caller cannot ask about an owner it does not hold a grant for.

use std::path::{Path, PathBuf};
use std::time::Duration;

use yi_agent_lifecycle::client::ControlClient;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{ClientGrant, ControlRequest, ControlResponse, ExecutionAdmission};

/// Per-ask budget. Admission sits in the path of a Start, so it must be short: a
/// wedged guardian should refuse work, not stall the coordinator.
pub const ADMISSION_BUDGET: Duration = Duration::from_secs(2);

/// Asks the guardian's control socket for permission to run one execution.
pub struct GuardianAdmission {
    socket: PathBuf,
    grant: ClientGrant,
}

impl GuardianAdmission {
    /// Binds one client's grant to the guardian socket of `runtime_dir`.
    ///
    /// The instance home is taken from the environment. A caller that already holds
    /// it — a fixture, or anything carrying a `RuntimeScope` — must use
    /// [`Self::with_instance_home`] instead: the guardian derives its socket from the
    /// scope it was handed, and only the caller knows whether that matches the
    /// environment.
    pub fn new(runtime_dir: &Path, grant: ClientGrant) -> Self {
        Self {
            // Through the lifecycle resolver: the control socket can move out of the
            // runtime directory when its path overflows `sun_path`.
            socket: yi_agent_lifecycle::scope::control_path(runtime_dir),
            grant,
        }
    }

    /// [`Self::new`], but against an explicitly known instance home.
    ///
    /// Same socket the guardian computed from `instance_home`, without re-reading
    /// the environment. Tests need this: they build a scope with a chosen home, and
    /// resolving from the environment instead would look for the socket in the
    /// developer's real `~/.yi-agent`.
    pub fn with_instance_home(
        runtime_dir: &Path,
        instance_home: &Path,
        grant: ClientGrant,
    ) -> Self {
        Self {
            socket: yi_agent_lifecycle::scope::guardian_socket(runtime_dir, instance_home),
            grant,
        }
    }

    /// The exact question: `generation/owner/task/attempt`.
    ///
    /// Each ask opens a fresh connection. Reusing one would let a long-lived
    /// daemon hold an authorised channel open across a guardian restart, which is
    /// the bypass this exists to close.
    pub fn admit(&self, generation: &str, task: &str, attempt: &str) -> Result<(), String> {
        let asked = ExecutionAdmission {
            grant: self.grant.clone(),
            generation: generation.to_string(),
            task: task.to_string(),
            attempt: attempt.to_string(),
        };
        let deadline = Deadline::after(ADMISSION_BUDGET);
        let mut client = ControlClient::connect(&self.socket, &deadline)
            .map_err(|error| format!("cannot reach the guardian: {error}"))?;
        // No retry: a Start is admitted once, and a retried ask could succeed after
        // the first was refused by a release that landed in between.
        match client.request(&ControlRequest::Admission { asked }, &deadline) {
            Ok(ControlResponse::Ack) => Ok(()),
            Ok(ControlResponse::Rejected { reason, .. }) => Err(reason),
            Ok(other) => Err(format!("unexpected admission answer: {other:?}")),
            Err(error) => Err(format!("the admission question failed: {error}")),
        }
    }
}
