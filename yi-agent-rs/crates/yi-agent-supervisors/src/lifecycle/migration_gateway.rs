//! The production-facing half of legacy migration: preview, confirm, reconcile.
//!
//! Everything destructive lives in [`super::migration_operation`], which journals each
//! phase before performing it. This module is the seam the app-server calls, and it
//! exists to make three misreadings impossible:
//!
//! 1. **A preview is not a plan.** It re-probes the instance every time it is asked,
//!    and the token it returns is bound to that probe's digest. Reusing the token after
//!    anything on the machine changed is refused, not "close enough".
//! 2. **A confirmation is not a retry.** The caller supplies the operation id, and an
//!    id that already has a journal returns that journal. That is what makes a lost
//!    answer a reconciliation instead of a second stop.
//! 3. **A temporary-use choice never completes a migration.** It records the user's
//!    deliberate exception and leaves the instance un-retired, so the UI keeps showing
//!    the unmanaged runtime instead of claiming success.
//!
//! The retirement marker is instance-wide while a runtime is scope-wide. That is why
//! [`Gateway::preview`] reports every registration in the instance, not just the one
//! the user clicked: confirming this button changes the whole instance's self-heal
//! policy, and the preview has to say so before it happens.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::production::{
    MigrationAffected, MigrationConfirmRequest, MigrationOperationView, MigrationPreviewRequest,
    MigrationPreviewView, MigrationWorkload,
};
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};

use super::CoordinationGuard;
use super::bringup::{
    ControlChannelProbe, ControlProbe, GuardianLauncher, HeldScope, ProcessGuardian,
};
use super::legacy_evidence::{LegacyEvidence, LegacyEvidenceProbe, SystemFacts};
use super::migration::SystemLaunchctl;
use super::migration_operation::{
    MigrationCoordinator, MigrationPhase, MigrationProbes, MigrationRequest, SystemMigrationBackend,
};
use super::retirement;

/// Budget for one migration's destructive phases.
///
/// One total budget for stop *and* start, as the spec requires: per-hop budgets
/// multiply, and a migration that quietly took four minutes would look hung.
pub const MIGRATION_BUDGET: Duration = Duration::from_secs(90);

/// A preview handed out by this process.
///
/// Kept in memory only. A token on disk would outlive the evidence it describes, and
/// the whole point of the digest is that the machine cannot have changed underneath.
#[derive(Clone, Debug)]
pub struct MintedPreview {
    pub token: String,
    pub evidence_digest: String,
    pub scope_key: String,
    pub expires_at: Instant,
}

impl MintedPreview {
    pub fn still_authorises(&self, evidence: &LegacyEvidence, now: Instant) -> bool {
        now < self.expires_at
            && self.scope_key == evidence.scope_key
            && self.evidence_digest == evidence.digest()
    }
}

/// What the gateway needs from the world, injectable so tests can be deterministic.
pub trait MigrationWorld: Send + Sync {
    /// Read the instance, read-only.
    fn inspect(&self, scope: &RuntimeScope, deadline: &Deadline)
    -> std::io::Result<LegacyEvidence>;

    /// Take the instance gate and the scope lock, in that order.
    ///
    /// Returns `None` when either is contended: a migration that waited would hold the
    /// instance gate while another one finishes, and the user's click would look hung.
    fn take_locks(
        &self,
        scope: &RuntimeScope,
        deadline: Deadline,
    ) -> std::io::Result<Option<HeldScope>>;

    /// Run the journaled phases.
    fn run(
        &self,
        coordinator: &MigrationCoordinator<'_>,
        request: MigrationRequest<'_>,
        held: HeldScope,
    ) -> Result<
        super::migration_operation::MigrationJournal,
        super::migration_operation::MigrationRefusal,
    >;

    /// How many migration runs this world started. Tests use it to prove an id is
    /// idempotent; production has no reason to count.
    fn runs_started(&self) -> usize {
        0
    }

    /// Read one journal by id.
    fn operation(
        &self,
        instance_home: &Path,
        operation_id: &str,
    ) -> std::io::Result<Option<super::migration_operation::MigrationJournal>>;

    /// Whether this instance has already retired its self-heal.
    fn is_retired(&self, instance_home: &Path) -> bool;
}

/// The real machine.
pub struct SystemWorld;

impl MigrationWorld for SystemWorld {
    fn inspect(
        &self,
        scope: &RuntimeScope,
        deadline: &Deadline,
    ) -> std::io::Result<LegacyEvidence> {
        // No `expected` artifact: evidence must describe what *is* running, and
        // supplying our own build would make every mismatch look like a difference
        // this migration chose.
        LegacyEvidenceProbe::system(None, &scope.instance_home).inspect(scope, deadline)
    }

    fn take_locks(
        &self,
        scope: &RuntimeScope,
        deadline: Deadline,
    ) -> std::io::Result<Option<HeldScope>> {
        use super::migration_operation::RetirementGate;
        let gate = match RetirementGate::acquire(&scope.instance_home, deadline.clone()) {
            Ok(gate) => gate,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        let guard = match CoordinationGuard::acquire(scope, deadline.clone()) {
            Ok(guard) => guard,
            // The gate is released by dropping it, which is the correct answer: a
            // contended scope means another coordinator is already replacing this
            // runtime, and holding the instance gate would only serialise the wait.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        drop(gate);
        Ok(Some(HeldScope { guard, deadline }))
    }

    fn run(
        &self,
        coordinator: &MigrationCoordinator<'_>,
        request: MigrationRequest<'_>,
        held: HeldScope,
    ) -> Result<
        super::migration_operation::MigrationJournal,
        super::migration_operation::MigrationRefusal,
    > {
        let launchctl = SystemLaunchctl;
        let probe = ControlChannelProbe;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        // Cloned before `held` moves: the backend borrows the scope, and the request
        // is consumed by `confirm`. Reborrowing through `held` after the move would
        // not compile, and silently rebuilding the scope would be a second guess at
        // which directory is being migrated.
        let scope = held.guard.scope().clone();
        let launcher = ProcessGuardian {
            instance_home: scope.instance_home.clone(),
        };
        let backend = SystemMigrationBackend {
            scope: &scope,
            evidence: request.evidence,
            home: &home,
            launchctl: &launchctl,
            probes: MigrationProbes {
                probe: &probe,
                launcher: &launcher,
            },
            required_capabilities: &[],
        };
        coordinator.confirm(request, held, &backend)
    }

    fn operation(
        &self,
        instance_home: &Path,
        operation_id: &str,
    ) -> std::io::Result<Option<super::migration_operation::MigrationJournal>> {
        MigrationCoordinator::new(instance_home, Deadline::after(Duration::from_secs(2)))
            .operation(operation_id)
    }

    fn is_retired(&self, instance_home: &Path) -> bool {
        retirement::is_retired(instance_home)
    }
}

/// Previews and runs migrations for one instance home.
pub struct Gateway<W: MigrationWorld = SystemWorld> {
    instance_home: PathBuf,
    world: W,
    previews: std::sync::Mutex<std::collections::HashMap<String, MintedPreview>>,
}

impl Gateway<SystemWorld> {
    pub fn system(instance_home: impl Into<PathBuf>) -> Self {
        Self::with_world(instance_home, SystemWorld)
    }
}

impl<W: MigrationWorld> Gateway<W> {
    pub fn with_world(instance_home: impl Into<PathBuf>, world: W) -> Self {
        Self {
            instance_home: instance_home.into(),
            world,
            previews: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Read-only. Mints a token bound to the evidence it just read.
    pub fn preview(
        &self,
        request: &MigrationPreviewRequest,
    ) -> Result<MigrationPreviewView, String> {
        if request.scope.instance_home != self.instance_home {
            return Err("this sidecar does not manage that instance".to_string());
        }
        let deadline = Deadline::after(Duration::from_secs(10));
        let evidence = self
            .world
            .inspect(&request.scope, &deadline)
            .map_err(|error| format!("the instance could not be inspected: {error}"))?;

        // The managed token is minted over the evidence digest. The evidence module's
        // own token is bound to the same digest; taking it from there would tie this
        // gateway's lifetime rules to another struct's TTL, so the token is minted
        // here and the digest is what the two share.
        let digest = evidence.digest();
        let token = mint_token(&digest, &evidence.scope_key);
        let minted = MintedPreview {
            token: token.clone(),
            evidence_digest: digest.clone(),
            scope_key: evidence.scope_key.clone(),
            expires_at: Instant::now() + super::legacy_evidence::MigrationPreview::TTL,
        };

        let affected = affected_projects(&evidence);
        let mut blockers = evidence.blockers();
        if self.world.is_retired(&self.instance_home) {
            // Already retired: a second migration would be a no-op that reports
            // success, which is worse than saying so.
            blockers.push("this instance has already retired its self-heal".to_string());
        }
        if affected
            .iter()
            .any(|project| matches!(project.workload, MigrationWorkload::Unverified(_)))
        {
            blockers
                .push("at least one affected project's workload could not be verified".to_string());
        }

        let view = MigrationPreviewView {
            token: token.clone(),
            evidence_digest: digest,
            scope_key: evidence.scope_key.clone(),
            affected,
            blockers,
            expires_at_unix: now_unix() + super::legacy_evidence::MigrationPreview::TTL.as_secs(),
        };

        if let Ok(mut previews) = self.previews.lock() {
            // Drop anything already expired so the table cannot grow without bound.
            let now = Instant::now();
            previews.retain(|_, preview| now < preview.expires_at);
            previews.insert(token, minted);
        }
        Ok(view)
    }

    /// Runs the migration, or reports the operation that already exists for this id.
    pub fn confirm(
        &self,
        request: &MigrationConfirmRequest,
    ) -> Result<MigrationOperationView, String> {
        if request.scope.instance_home != self.instance_home {
            return Err("this sidecar does not manage that instance".to_string());
        }
        let coordinator =
            MigrationCoordinator::new(&self.instance_home, Deadline::after(MIGRATION_BUDGET));

        // An id that already has a journal returns it. This is the reconciliation
        // path, and it is deliberately checked before anything else so a repeat of a
        // lost confirm cannot stop anything twice.
        if let Ok(Some(journal)) = self
            .world
            .operation(&self.instance_home, &request.operation_id)
        {
            return Ok(operation_view(&journal));
        }

        let minted = {
            let mut previews = self
                .previews
                .lock()
                .map_err(|_| "the preview table is poisoned".to_string())?;
            previews.remove(&request.token)
        };
        let Some(minted) = minted else {
            return Err(
                "this preview is not known here; preview again before confirming".to_string(),
            );
        };
        if minted.scope_key != request.scope.key {
            return Err("the confirmation names a different scope than the preview".to_string());
        }
        if Instant::now() >= minted.expires_at {
            return Err("the preview expired; preview again before confirming".to_string());
        }

        // Re-probe immediately before doing anything: a preview is a photograph, and
        // the machine may have changed since.
        let deadline = Deadline::after(Duration::from_secs(10));
        let evidence = self
            .world
            .inspect(&request.scope, &deadline)
            .map_err(|error| format!("the instance could not be re-inspected: {error}"))?;
        if !minted.still_authorises(&evidence, Instant::now()) {
            return Err(
                "the instance changed since the preview; preview again before confirming"
                    .to_string(),
            );
        }

        if request.temporary_use {
            // The user's answer, not an inference. Nothing is stopped and the
            // instance stays un-retired, so the exception keeps being visible.
            return Ok(MigrationOperationView {
                operation_id: request.operation_id.clone(),
                phase: "temporary_use".to_string(),
                detail: Some(
                    "the old runtime is kept for now; this instance has not been migrated"
                        .to_string(),
                ),
            });
        }
        if !evidence.blockers().is_empty() {
            return Ok(MigrationOperationView {
                operation_id: request.operation_id.clone(),
                phase: "manual_action_required".to_string(),
                detail: Some(evidence.blockers().join("; ")),
            });
        }

        let Some(held) = self
            .world
            .take_locks(&request.scope, Deadline::after(MIGRATION_BUDGET))
            .map_err(|error| format!("the migration locks could not be taken: {error}"))?
        else {
            return Err(
                "another migration holds this instance's gate; try again when it finishes"
                    .to_string(),
            );
        };

        let migration = MigrationRequest {
            operation_id: &request.operation_id,
            target: &request.target,
            evidence: &evidence,
            preview_digest: &minted.evidence_digest,
            fault: None,
        };
        match self.world.run(&coordinator, migration, held) {
            Ok(journal) => Ok(operation_view(&journal)),
            Err(refusal) => Err(refusal.to_string()),
        }
    }

    /// Test seam: how many migrations this gateway started.
    #[doc(hidden)]
    pub fn world_runs_for_test(&self) -> usize {
        self.world.runs_started()
    }

    /// One read of an operation. Never changes anything.
    pub fn operation(&self, operation_id: &str) -> Result<Option<MigrationOperationView>, String> {
        self.world
            .operation(&self.instance_home, operation_id)
            .map(|journal| journal.as_ref().map(operation_view))
            .map_err(|error| error.to_string())
    }
}

/// Every project this instance's retirement would touch.
///
/// One project's runtime is one scope, but the retirement marker is instance-wide, so
/// the list comes from the instance's registrations. A registration whose workload
/// cannot be read is `unverified`, never idle: an empty list must not read as "nothing
/// is running anywhere".
fn affected_projects(evidence: &LegacyEvidence) -> Vec<MigrationAffected> {
    match evidence.verified_registrations() {
        Some(registrations) => registrations
            .iter()
            .map(|registration| MigrationAffected {
                scope_key: registration.project.display().to_string(),
                // The registration file says which project, not what it is doing. Saying
                // "idle" here would be inventing the one fact that matters most.
                workload: MigrationWorkload::Unverified(
                    "the registration file does not record its workload".to_string(),
                ),
            })
            .collect(),
        None => Vec::new(),
    }
}

fn operation_view(
    journal: &super::migration_operation::MigrationJournal,
) -> MigrationOperationView {
    MigrationOperationView {
        operation_id: journal.operation_id.clone(),
        phase: journal.phase.label().to_string(),
        detail: journal.error.clone().or_else(|| {
            journal
                .failed_at
                .map(|phase| format!("stopped at {}", phase.label()))
        }),
    }
}

fn mint_token(digest: &str, scope_key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"yi-agent/shell-migration-preview");
    hasher.update(digest.as_bytes());
    hasher.update(scope_key.as_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(now_unix().to_le_bytes());
    let bytes = hasher.finalize();
    bytes[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Whether a phase is terminal, so the UI stops polling.
pub fn is_terminal(phase: &str) -> bool {
    matches!(
        phase,
        "completed" | "failed" | "temporary_use" | "manual_action_required"
    )
}

/// Re-exported for callers that need the phase spelling without the journal type.
pub fn phase_label(phase: MigrationPhase) -> &'static str {
    phase.label()
}

/// A [`ControlProbe`] that never answers. Present so tests can build a world without
/// pretending a socket exists.
pub struct SilentProbe;

impl ControlProbe for SilentProbe {
    fn probe(&self, _path: &Path, _deadline: &Deadline) -> super::bringup::ProbeOutcome {
        super::bringup::ProbeOutcome::Absent
    }
}

/// A [`GuardianLauncher`] that refuses. Present for the same reason as [`SilentProbe`].
pub struct NoLaunch;

impl GuardianLauncher for NoLaunch {
    fn launch(&self, _scope: &RuntimeScope, _expected: &ArtifactIdentity) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "this world does not launch guardians",
        ))
    }
}

/// Kept so a caller can name the evidence source without importing `SystemFacts`.
pub fn system_probe(instance_home: &Path) -> LegacyEvidenceProbe<SystemFacts> {
    LegacyEvidenceProbe::system(None, instance_home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_phase_stops_polling() {
        assert!(is_terminal("completed"));
        assert!(is_terminal("failed"));
        assert!(!is_terminal("legacy_stopped"));
    }

    #[test]
    fn a_silent_world_reports_absence_rather_than_a_guess() {
        let outcome = SilentProbe.probe(
            Path::new("/nonexistent/sock"),
            &Deadline::after(Duration::from_secs(1)),
        );
        assert!(matches!(
            outcome,
            super::super::bringup::ProbeOutcome::Absent
        ));
    }
}
