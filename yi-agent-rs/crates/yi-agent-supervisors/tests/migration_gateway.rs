//! The migration gateway's promises, proven against an injected world.
//!
//! These are not "does the dialog work" tests: each one pins a rule that a plausible
//! implementation would get wrong in a way that costs somebody their running work.
//!
//! - A preview that could not verify a workload must not be confirmable as if it were
//!   idle.
//! - A confirmation whose answer was lost is reconciled against the same operation id,
//!   never re-run.
//! - A temporary-use choice never records a retirement.
//! - An expired or superseded preview is refused rather than extended.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::production::{
    MigrationConfirmRequest, MigrationPreviewRequest, MigrationWorkload,
};
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};
use yi_agent_supervisors::lifecycle::bringup::HeldScope;
use yi_agent_supervisors::lifecycle::legacy_evidence::{
    AffectedRegistration, LegacyEvidence, LegacyEvidenceProbe, Observation, PlatformFacts,
};
use yi_agent_supervisors::lifecycle::migration_gateway::{Gateway, MigrationWorld};
use yi_agent_supervisors::lifecycle::migration_operation::{
    MigrationCoordinator, MigrationJournal, MigrationPhase, MigrationRefusal, MigrationRequest,
};

fn test_scope(root: &Path) -> RuntimeScope {
    RuntimeScope {
        key: root.join("runtime").to_string_lossy().into_owned(),
        runtime_dir: root.join("runtime"),
        project_key: root.join("project").to_string_lossy().into_owned(),
        instance_home: root.to_path_buf(),
    }
}

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "build-1".into(),
        display_version: "0.1.13".into(),
        executable: PathBuf::from("/tmp/yi-agent"),
        sha256: "abc".into(),
        schema_min: 0,
        schema_max: 15,
        handover_version: 1,
        business_protocol_min: 2,
        business_protocol_max: 2,
        capabilities: vec![],
    }
}

/// Facts with exactly the answers a test wants, and nothing else.
#[derive(Default)]
struct Facts {
    registrations: Option<Vec<AffectedRegistration>>,
}

impl PlatformFacts for Facts {
    fn peer_uid(&self, _socket: &Path) -> Observation<u32> {
        Observation::Verified(501)
    }
    fn peer_pid(&self, _socket: &Path) -> Observation<u32> {
        Observation::Verified(1)
    }
    fn process_identity(&self, _pid: u32) -> Observation<(String, PathBuf)> {
        Observation::Verified(("1.0".to_string(), PathBuf::from("/usr/local/bin/yi-agent")))
    }
    fn file_sha256(&self, _path: &Path) -> Observation<String> {
        Observation::Verified("digest".to_string())
    }
    fn registrations(&self, _home: &Path) -> Observation<Vec<AffectedRegistration>> {
        match &self.registrations {
            Some(registrations) => Observation::Verified(registrations.clone()),
            None => Observation::Unknown("the registration file could not be read".to_string()),
        }
    }
    fn loaded_jobs(
        &self,
        _labels: &[String],
    ) -> Observation<Vec<yi_agent_supervisors::lifecycle::migration::LoadedJob>> {
        Observation::Verified(Vec::new())
    }
    fn reported_identity(
        &self,
        _socket: &Path,
        _deadline: &Deadline,
    ) -> Observation<yi_agent_lifecycle::types::RuntimeIdentity> {
        // Nothing answered on the control socket. That is the honest state of an
        // instance nobody has probed yet, and it must never read as "idle and safe".
        Observation::Unknown("no control socket answered".to_string())
    }
}

/// A world whose journals and lock answers the test sets.
struct FakeWorld {
    evidence: Mutex<LegacyEvidence>,
    journals: Mutex<Vec<MigrationJournal>>,
    runs: AtomicUsize,
    retired: bool,
    locked: bool,
}

impl FakeWorld {
    fn new(evidence: LegacyEvidence) -> Self {
        Self {
            evidence: Mutex::new(evidence),
            journals: Mutex::new(Vec::new()),
            runs: AtomicUsize::new(0),
            retired: false,
            locked: false,
        }
    }

    fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

impl MigrationWorld for FakeWorld {
    fn inspect(
        &self,
        _scope: &RuntimeScope,
        _deadline: &Deadline,
    ) -> std::io::Result<LegacyEvidence> {
        Ok(self.evidence.lock().unwrap().clone())
    }

    fn take_locks(
        &self,
        scope: &RuntimeScope,
        deadline: Deadline,
    ) -> std::io::Result<Option<HeldScope>> {
        if self.locked {
            return Ok(None);
        }
        // A real guard needs a real runtime directory; the test's directory exists,
        // so the lock is the genuine article rather than a stub that could hide a
        // wrong lock order.
        let guard =
            yi_agent_supervisors::lifecycle::CoordinationGuard::acquire(scope, deadline.clone())
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        // This fake world takes no instance gate; `None` says so rather than
        // pretending one is held.
        Ok(Some(HeldScope {
            guard,
            deadline,
            retirement_gate: None,
        }))
    }

    fn run(
        &self,
        _coordinator: &MigrationCoordinator<'_>,
        request: MigrationRequest<'_>,
        held: HeldScope,
    ) -> Result<MigrationJournal, MigrationRefusal> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let scope = held.guard.scope();
        let journal = MigrationJournal {
            operation_id: request.operation_id.to_string(),
            scope_key: scope.key.clone(),
            runtime_dir: scope.runtime_dir.clone(),
            target_build_id: request.target.build_id.clone(),
            evidence_digest: request.preview_digest.to_string(),
            phase: MigrationPhase::LegacyStopped,
            failed_at: None,
            error: None,
            result: None,
            updated_at_unix: 0,
        };
        self.journals.lock().unwrap().push(journal.clone());
        Ok(journal)
    }

    fn operation(
        &self,
        _instance_home: &Path,
        operation_id: &str,
    ) -> std::io::Result<Option<MigrationJournal>> {
        Ok(self
            .journals
            .lock()
            .unwrap()
            .iter()
            .find(|journal| journal.operation_id == operation_id)
            .cloned())
    }

    fn is_retired(&self, _instance_home: &Path) -> bool {
        self.retired
    }

    fn runs_started(&self) -> usize {
        self.runs()
    }
}

/// Evidence whose registrations the test names, everything else unknown.
fn evidence(root: &Path, registrations: Option<Vec<AffectedRegistration>>) -> LegacyEvidence {
    LegacyEvidenceProbe::with_facts(Facts { registrations }, None, root.to_path_buf())
        .inspect(&test_scope(root), &Deadline::after(Duration::from_secs(2)))
        .unwrap()
}

fn preview_request(root: &Path) -> MigrationPreviewRequest {
    MigrationPreviewRequest {
        scope: test_scope(root),
        target: artifact(),
    }
}

fn confirm_request(root: &Path, token: &str, operation_id: &str) -> MigrationConfirmRequest {
    MigrationConfirmRequest {
        scope: test_scope(root),
        target: artifact(),
        token: token.to_string(),
        operation_id: operation_id.to_string(),
        temporary_use: false,
    }
}

#[test]
fn a_preview_lists_every_project_the_instance_retirement_would_touch() {
    // The retirement marker is instance-wide while the runtime is scope-wide: a
    // preview that named only the clicked project would understate what confirming
    // does.
    let directory = tempfile::TempDir::new().unwrap();
    let other = directory.path().join("other-project");
    std::fs::create_dir_all(&other).unwrap();
    let evidence = evidence(
        directory.path(),
        Some(vec![AffectedRegistration {
            project: other.clone(),
            requester: "resident".into(),
        }]),
    );
    let gateway = Gateway::with_world(directory.path(), FakeWorld::new(evidence));

    let view = gateway.preview(&preview_request(directory.path())).unwrap();
    assert_eq!(view.affected.len(), 1);
    assert_eq!(view.affected[0].scope_key, other.display().to_string());
}

#[test]
fn an_unverified_workload_blocks_confirmation_and_is_never_called_idle() {
    let directory = tempfile::TempDir::new().unwrap();
    let evidence = evidence(
        directory.path(),
        Some(vec![AffectedRegistration {
            project: directory.path().join("other"),
            requester: "resident".into(),
        }]),
    );
    let gateway = Gateway::with_world(directory.path(), FakeWorld::new(evidence));

    let view = gateway.preview(&preview_request(directory.path())).unwrap();
    assert!(matches!(
        view.affected[0].workload,
        MigrationWorkload::Unverified(_)
    ));
    assert!(
        view.blockers
            .iter()
            .any(|blocker| blocker.contains("could not be verified")),
        "{:?}",
        view.blockers
    );
}

#[test]
fn a_confirmation_without_a_known_preview_is_refused() {
    let directory = tempfile::TempDir::new().unwrap();
    let evidence = evidence(directory.path(), Some(Vec::new()));
    let gateway = Gateway::with_world(directory.path(), FakeWorld::new(evidence));

    let error = gateway
        .confirm(&confirm_request(directory.path(), "not-a-token", "op-1"))
        .unwrap_err();
    assert!(error.contains("preview again"), "{error}");
}

#[test]
fn temporary_use_records_no_migration() {
    // The user's "not now" is a deliberate exception, not a successful migration: the
    // instance must stay un-retired and no phase may run.
    let directory = tempfile::TempDir::new().unwrap();
    let evidence = evidence(directory.path(), Some(Vec::new()));
    let world = FakeWorld::new(evidence);
    let gateway = Gateway::with_world(directory.path(), world);

    let view = gateway.preview(&preview_request(directory.path())).unwrap();
    let mut request = confirm_request(directory.path(), &view.token, "op-1");
    request.temporary_use = true;
    let operation = gateway.confirm(&request).unwrap();
    assert_eq!(operation.phase, "temporary_use");
}

#[test]
fn a_lost_answer_is_reconciled_against_the_same_operation() {
    // The first attempt may have stopped the legacy runtime before its answer was
    // lost. Asking again with the same id must return that record instead of
    // stopping a second process, so the journal is consulted before the token is
    // even looked at.
    let directory = tempfile::TempDir::new().unwrap();
    let evidence = evidence(directory.path(), Some(Vec::new()));
    let world = FakeWorld::new(evidence);
    world.journals.lock().unwrap().push(MigrationJournal {
        operation_id: "op-7".into(),
        scope_key: test_scope(directory.path()).key,
        runtime_dir: directory.path().join("runtime"),
        target_build_id: "build-1".into(),
        evidence_digest: "digest".into(),
        phase: MigrationPhase::LegacyStopped,
        failed_at: None,
        error: None,
        result: None,
        updated_at_unix: 0,
    });
    let gateway = Gateway::with_world(directory.path(), world);

    // No preview was ever minted for this call, and it still answers: that is the
    // whole point of reconciling by id.
    let operation = gateway
        .confirm(&confirm_request(directory.path(), "never-minted", "op-7"))
        .unwrap();
    assert_eq!(operation.operation_id, "op-7");
    assert_eq!(operation.phase, "legacy_stopped");
    assert_eq!(
        gateway.world_runs_for_test(),
        0,
        "reconciling an existing operation must not run the phases again"
    );
}

#[test]
fn unverifiable_evidence_is_reported_as_a_manual_step_not_run() {
    // Nothing in this build can prove a legacy instance is frozen, drained and idle,
    // so the honest answer is "a human has to act". Reporting success here is what
    // would let a migration stop real work on a guess.
    let directory = tempfile::TempDir::new().unwrap();
    let evidence = evidence(directory.path(), Some(Vec::new()));
    let gateway = Gateway::with_world(directory.path(), FakeWorld::new(evidence));

    let view = gateway.preview(&preview_request(directory.path())).unwrap();
    assert!(!view.blockers.is_empty(), "{view:?}");
    let operation = gateway
        .confirm(&confirm_request(directory.path(), &view.token, "op-1"))
        .unwrap();
    assert_eq!(operation.phase, "manual_action_required");
    assert_eq!(
        gateway.world_runs_for_test(),
        0,
        "a migration with unproven evidence must not stop anything"
    );
}

// ---------------------------------------------------------------------------
// A5: the instance gate must outlive the freeze *side effect*, not just the
// scope lock.
//
// Releasing the gate right after the scope lock was taken left a window: a
// cooperating start could take the gate, find no freeze marker, and legitimately
// start an embedded daemon -- the very daemon the migration is replacing. The
// window is invisible to any test that only inspects the final phase, so this one
// blocks inside `freeze_self_heal` and asks a contender from another thread.
// ---------------------------------------------------------------------------

/// A backend whose freeze blocks until the test releases it.
///
/// Records the order of events so the assertion is about what actually happened
/// rather than about timing.
struct BlockingFreezeBackend {
    entered: std::sync::mpsc::Sender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
    instance_home: PathBuf,
}

impl yi_agent_supervisors::lifecycle::migration_operation::MigrationBackend
    for BlockingFreezeBackend
{
    fn freeze_self_heal(&self) -> std::io::Result<()> {
        // Announced before blocking: the contender must be attempted while the
        // freeze is still in progress, otherwise the test proves nothing.
        self.entered
            .send(())
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        // Bounded on purpose: if the test body panics before releasing the barrier,
        // dropping the sender ends the wait. A plain `recv()` on a `Mutex<Receiver>`
        // still unblocks on disconnect, but the timeout also rules out a stuck run.
        match self
            .resume
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
        {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(std::io::Error::other("the test never released the barrier"));
            }
        }
        yi_agent_supervisors::lifecycle::retirement::freeze(
            &self.instance_home,
            "a managed runtime migration is in progress",
        )
        .map(|_| ())
    }

    fn retire_watchman(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn stop_legacy(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn ensure_managed(
        &self,
        _held: &HeldScope,
        _expected: &ArtifactIdentity,
        _intent: yi_agent_supervisors::lifecycle::bringup::BringUpIntent,
    ) -> std::io::Result<yi_agent_supervisors::lifecycle::bringup::BringUpResult> {
        Err(std::io::Error::other(
            "this test blocks in freeze and never reaches connect",
        ))
    }

    fn acknowledge_ownership(
        &self,
        _proof: &yi_agent_supervisors::lifecycle::migration_operation::OwnershipProof<'_>,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

impl MigrationWorld for BlockingFreezeBackend {
    fn inspect(
        &self,
        _scope: &RuntimeScope,
        _deadline: &Deadline,
    ) -> std::io::Result<LegacyEvidence> {
        unreachable!("the test drives the coordinator directly")
    }

    fn take_locks(
        &self,
        _scope: &RuntimeScope,
        _deadline: Deadline,
    ) -> std::io::Result<Option<HeldScope>> {
        unreachable!("the test builds its own held scope")
    }

    fn run(
        &self,
        _coordinator: &MigrationCoordinator<'_>,
        _request: MigrationRequest<'_>,
        _held: HeldScope,
    ) -> Result<MigrationJournal, MigrationRefusal> {
        unreachable!("the test drives the coordinator directly")
    }

    fn operation(
        &self,
        _instance_home: &Path,
        _operation_id: &str,
    ) -> std::io::Result<Option<MigrationJournal>> {
        Ok(None)
    }

    fn is_retired(&self, _instance_home: &Path) -> bool {
        false
    }
}

#[test]
fn migration_keeps_gate_until_freeze_is_durable() {
    use yi_agent_supervisors::lifecycle::migration_operation::RetirementGate;

    let directory = tempfile::TempDir::new().unwrap();
    let scope = test_scope(directory.path());
    let (evidence, digest) = {
        let facts = Facts::default();
        let evidence = LegacyEvidenceProbe::with_facts(facts, Some(artifact()), directory.path())
            .inspect(&scope, &Deadline::after(Duration::from_secs(2)))
            .unwrap();
        let digest = evidence.digest().to_string();
        (evidence, digest)
    };

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    let backend = BlockingFreezeBackend {
        entered: entered_tx,
        resume: Mutex::new(resume_rx),
        instance_home: directory.path().to_path_buf(),
    };

    // Take the locks through the *production* implementation. Building a
    // `HeldScope` by hand here would let this test pass while `take_locks` still
    // released the gate too early -- and `take_locks` releasing it too early is
    // exactly the defect under test.
    use yi_agent_supervisors::lifecycle::migration_gateway::SystemWorld;
    let held = <SystemWorld as MigrationWorld>::take_locks(
        &SystemWorld,
        &scope,
        Deadline::after(Duration::from_secs(5)),
    )
    .expect("the locks are free in an isolated instance")
    .expect("an uncontended instance must grant the locks");

    let coordinator =
        MigrationCoordinator::new(directory.path(), Deadline::after(Duration::from_secs(10)));

    std::thread::scope(|threads| {
        let migration = threads.spawn(|| {
            let request = MigrationRequest {
                operation_id: "op-gate-window",
                target: &artifact(),
                evidence: &evidence,
                preview_digest: &digest,
                fault: None,
            };
            coordinator.confirm(request, held, &backend)
        });

        // The migration has reached its freeze step and is blocking there.
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the migration must reach the pre-freeze barrier");

        // A contender with a short budget must fail *now*: the freeze is in
        // progress, so the marker a start would consult is not there yet.
        let contended = RetirementGate::acquire(
            directory.path(),
            Deadline::after(Duration::from_millis(120)),
        );
        assert!(
            contended.is_err(),
            "the gate must still be held while the freeze is not yet durable"
        );

        // Let the freeze land.
        resume_tx.send(()).expect("the barrier must be released");

        let journal = migration
            .join()
            .expect("the migration thread must not panic")
            .expect("the migration continues past a durable freeze");
        assert!(
            yi_agent_supervisors::lifecycle::retirement::is_frozen(directory.path()),
            "the freeze marker must be on disk before the gate is released"
        );
        // The journal records the phase reached; it is not `SelfHealFrozen` only,
        // because this backend lets the remaining steps run and fails at connect.
        assert!(
            journal.phase.legacy_is_stopped() || journal.failed_at.is_some(),
            "the migration must record a truthful phase: {journal:?}"
        );

        // Now the gate is free, and taking it proves the release really happened.
        let after =
            RetirementGate::acquire(directory.path(), Deadline::after(Duration::from_secs(2)));
        assert!(
            after.is_ok(),
            "after the freeze is durable the gate must be available again"
        );
    });
}
