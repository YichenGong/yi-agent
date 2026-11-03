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

use super::guardian::GUARDIAN_SOCKET_FILE;

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
    pub fn new(runtime_dir: &Path, grant: ClientGrant) -> Self {
        Self {
            socket: runtime_dir.join(GUARDIAN_SOCKET_FILE),
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
