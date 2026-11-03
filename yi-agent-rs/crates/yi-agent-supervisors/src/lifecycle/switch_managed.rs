//! The production [`SwitchBackend`]: a real guardian, a real launcher and a real
//! control channel behind the switch state machine.
//!
//! Each method here is small on purpose. The ordering rules, the two absolute
//! windows, and the fail-closed decisions live in [`super::switch`]; this module
//! only answers the questions that module asks, using the components that already
//! own the truth:
//!
//! - the registry is the authority on whether a managed group stopped;
//! - `retirement` freezes the legacy self-healing paths so nothing revives the old
//!   daemon behind the switch;
//! - `bringup::ensure_managed` starts and probes a runtime, so a switch does not
//!   invent a second launch path;
//! - `selection` records the chosen build atomically under the coordination lock.
//!
//! Nothing here signals by name, searches by parent PID, or treats "I sent a signal"
//! as "it stopped". Those are the ways a switch kills an unrelated process or reports
//! a false success, and the spec forbids both.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::types::{
    ArtifactIdentity, DaemonInstanceId, OperationStatus, RuntimeScope,
};

use super::CoordinationGuard;
use super::bringup::{self, BringUpIntent, BringUpResult, Compatibility, ControlProbe};
use super::guardian::{ProcessOwner, ProcessRecord, ProcessRegistry, RecordState};
use super::selection::{RuntimeSelection, read_selection, write_selection};
use super::switch::{Preflight, SwitchBackend};

/// Asks the guardian's own leases how many clients are live.
pub trait ClientPresence: Send {
    /// Whether any business or lease-only client still holds a live lease.
    fn has_live_clients(&self) -> bool;
}

/// A fail-closed presence source for a caller that cannot answer.
///
/// Reporting "no clients" would skip starting a daemon that users are waiting for;
/// reporting clients forever would start one nobody wants. The honest answer for an
/// unknown source is `true` (there may be someone), so the switch does not silently
/// take the StoppedNoClients branch.
pub struct UnknownPresence;

impl ClientPresence for UnknownPresence {
    fn has_live_clients(&self) -> bool {
        true
    }
}

/// How the switch learns whether the old generation is really gone.
pub trait StopWitness: Send {
    /// Terminate every managed group, bounded by `confirm`.
    fn terminate_all(&mut self, confirm: Duration) -> Result<(), String>;
    /// Prove every managed group is gone.
    fn all_stopped(&mut self) -> Result<bool, String>;
    /// Every managed group still recorded as running, for the failure message.
    fn still_running(&mut self) -> Vec<String>;
}

/// The production witness: the guardian's process registry.
pub struct RegistryWitness {
    registry: Arc<Mutex<ProcessRegistry>>,
}

impl RegistryWitness {
    pub fn new(registry: Arc<Mutex<ProcessRegistry>>) -> Self {
        Self { registry }
    }

    fn with<T>(
        &self,
        ask: impl FnOnce(&mut ProcessRegistry) -> io::Result<T>,
    ) -> Result<T, String> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| "the process registry mutex was poisoned".to_string())?;
        ask(&mut registry).map_err(|error| error.to_string())
    }

    fn owners(registry: &ProcessRegistry) -> Vec<ProcessOwner> {
        let mut owners: Vec<ProcessOwner> = Vec::new();
        for record in registry.records() {
            if record.state != RecordState::Running {
                continue;
            }
            if !owners.contains(&record.owner) {
                owners.push(record.owner.clone());
            }
        }
        owners
    }
}

impl StopWitness for RegistryWitness {
    fn terminate_all(&mut self, confirm: Duration) -> Result<(), String> {
        let owners = self.with(|registry| Ok(Self::owners(registry)))?;
        for owner in owners {
            // Every owner is given the same confirmation window rather than a slice
            // of one budget: the spec's window is absolute, and splitting it per
            // owner would silently shorten the later ones.
            self.with(|registry| registry.terminate_owner(&owner, Deadline::after(confirm)))?;
        }
        Ok(())
    }

    fn all_stopped(&mut self) -> Result<bool, String> {
        let owners = self.with(|registry| Ok(Self::owners(registry)))?;
        for owner in owners {
            if !self.with(|registry| registry.owner_is_stopped(&owner))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn still_running(&mut self) -> Vec<String> {
        self.with(|registry| {
            Ok(registry
                .records()
                .iter()
                .filter(|record| record.state == RecordState::Running)
                .map(describe_record)
                .collect())
        })
        .unwrap_or_default()
    }
}

/// A witness for a guardian that owns no registry.
///
/// Every question fails: answering "stopped" without looking at any process is the
/// false success this whole module is built to prevent.
pub struct DetachedWitness;

impl StopWitness for DetachedWitness {
    fn terminate_all(&mut self, _confirm: Duration) -> Result<(), String> {
        Err("this guardian owns no process registry, so it cannot stop anything".to_string())
    }

    fn all_stopped(&mut self) -> Result<bool, String> {
        Err("this guardian owns no process registry, so it cannot prove a stop".to_string())
    }

    fn still_running(&mut self) -> Vec<String> {
        Vec::new()
    }
}

fn describe_record(record: &ProcessRecord) -> String {
    format!("{:?} pid {}", record.owner, record.pid)
}

/// A [`SwitchBackend`] over real guardian state.
pub struct ManagedSwitchBackend {
    scope: RuntimeScope,
    target_guardian_build: String,
    witness: Box<dyn StopWitness>,
    presence: Box<dyn ClientPresence>,
    probe: Box<dyn ControlProbe + Send>,
    /// The coordination lock, taken by the caller before the switch and released with
    /// the backend. Held for the whole switch so no second coordinator interleaves.
    guard: Option<CoordinationGuard>,
    /// Recorded stop proof, kept so `is_stopped` reports the witness's answer rather
    /// than a second, possibly contradictory one.
    stop_proven: bool,
    /// Whether the pre-flight verified the target on disk.
    target_verified: bool,
    /// The generation name assigned to the new daemon, so verification can prove the
    /// generation actually changed.
    previous_daemon: Option<DaemonInstanceId>,
    /// What the caller observed before the switch, for the generation comparison.
    ready: Option<yi_agent_lifecycle::types::RuntimeIdentity>,
}

impl ManagedSwitchBackend {
    /// Builds a backend for `scope`.
    ///
    /// `target_guardian_build` is the guardian build the *new* sidecar would run; it
    /// decides whether the control plane must be handed over at all.
    pub fn new(
        scope: RuntimeScope,
        target_guardian_build: impl Into<String>,
        witness: Box<dyn StopWitness>,
        presence: Box<dyn ClientPresence>,
        probe: Box<dyn ControlProbe + Send>,
    ) -> Self {
        Self {
            scope,
            target_guardian_build: target_guardian_build.into(),
            witness,
            presence,
            probe,
            guard: None,
            stop_proven: false,
            target_verified: false,
            previous_daemon: None,
            ready: None,
        }
    }

    /// Attaches the coordination lock the caller already took.
    ///
    /// The caller takes it, not this struct: the decision to switch belongs to the
    /// confirm handler, which must hold the lock from the moment the token is consumed.
    pub fn with_guard(mut self, guard: CoordinationGuard) -> Self {
        self.guard = Some(guard);
        self
    }

    /// The previous daemon generation, so `verify` can prove the generation changed.
    pub fn with_previous_daemon(mut self, generation: Option<DaemonInstanceId>) -> Self {
        self.previous_daemon = generation;
        self
    }

    /// Records the identity observed before the switch.
    pub fn with_previous_identity(
        mut self,
        identity: Option<yi_agent_lifecycle::types::RuntimeIdentity>,
    ) -> Self {
        self.ready = identity;
        self
    }

    /// The target's executable path, for the read-only pre-flight.
    fn target_path<'a>(&self, target: &'a ArtifactIdentity) -> &'a Path {
        target.executable.as_path()
    }
}

impl SwitchBackend for ManagedSwitchBackend {
    fn preflight(&mut self, target: &ArtifactIdentity) -> Result<Preflight, String> {
        if self.guard.is_none() {
            // Without the lock the pre-flight could pass while another coordinator is
            // already replacing this runtime, and the later write would interleave.
            return Err(
                "the switch has no coordination lock; it must not begin without one".to_string(),
            );
        }
        // A missing bundle is the single most common failure, and finding it here
        // keeps the user's working runtime alive.
        let path = self.target_path(target);
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Preflight::ManualAction(format!(
                    "the target build's executable {} is gone; reinstall or pick another \
                         version, then try again",
                    path.display()
                )));
            }
            Err(error) => {
                return Ok(Preflight::ManualAction(format!(
                    "the target executable {} cannot be inspected: {error}",
                    path.display()
                )));
            }
        };
        if !metadata.is_file() {
            return Ok(Preflight::ManualAction(format!(
                "{} is not a regular file; refusing to replace a running runtime with it",
                path.display()
            )));
        }
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Ok(Preflight::ManualAction(format!(
                "{} is not executable; a runtime started from it would fail immediately",
                path.display()
            )));
        }
        // A target that does not speak this handover version cannot be carried into;
        // stopping first and finding out later would leave no runtime at all.
        if target.handover_version == 0 {
            return Ok(Preflight::ManualAction(
                "the target build does not support a controlled handover; quit the other \
                 frontends and restart the runtime instead of upgrading in place"
                    .to_string(),
            ));
        }
        // The database must be readable by the target *before* anything is stopped, so
        // a schema the new build cannot open is discovered while the old one still works.
        let database = self.scope.runtime_dir.join("runtime.sqlite");
        if let Err(reason) = bringup::schema_is_readable_for(&database, target) {
            return Ok(Preflight::ManualAction(reason));
        }
        self.target_verified = true;
        Ok(Preflight::Ready)
    }

    fn freeze(&mut self) -> Result<(), String> {
        // Retire the legacy self-healing paths first: a watcher that can still start
        // the old daemon makes every stop below a lie.
        super::retirement::retire(
            &self.scope.instance_home,
            "a managed runtime switch is replacing this generation",
        )
        .map_err(|error| format!("could not retire the legacy self-healing paths: {error}"))?;
        Ok(())
    }

    fn notify(&mut self, _message: &str) {
        // Clients learn about the switch from the operation they can poll; there is no
        // separate notification channel in this generation, and inventing one here
        // would be a second source of truth. The operation status is the notice.
    }

    fn request_stop(&mut self, grace: Duration) {
        // There is no separate polite request in the managed registry: a managed group
        // learns to stop by being terminated. Spending the grace window waiting for a
        // stop nobody asked for would only lengthen the switch, so the window is
        // recorded and the termination step uses its own confirmation window.
        let _ = grace;
    }

    fn terminate(&mut self, confirm: Duration) -> Result<(), String> {
        self.witness.terminate_all(confirm)
    }

    fn is_stopped(&mut self) -> Result<bool, String> {
        let stopped = self.witness.all_stopped()?;
        self.stop_proven = stopped;
        Ok(stopped)
    }

    fn has_live_clients(&self) -> bool {
        self.presence.has_live_clients()
    }

    fn commit_selection(&mut self, target: &ArtifactIdentity) -> Result<(), String> {
        let guard = self
            .guard
            .as_ref()
            .ok_or("the switch has no coordination lock to write the selection under")?;
        let last_operation = read_selection(guard.scope())
            .map_err(|error| error.to_string())?
            .and_then(|existing| existing.last_operation);
        write_selection(
            guard,
            &RuntimeSelection {
                target: target.clone(),
                last_operation,
            },
        )
        .map_err(|error| format!("could not record the chosen build: {error}"))
    }

    fn handover_control(&mut self) -> Result<bool, String> {
        // The control plane only needs carrying when the guardian build changes. When
        // it does not, the listener already belongs to a process that will keep serving
        // it, and an exec would needlessly drop the client connections.
        let current = super::handover::cross_process_monotonic_ns();
        let _ = current;
        let needs_handover = self
            .ready
            .as_ref()
            .and_then(|identity| identity.guardian_build_id.as_deref())
            .map(|build| build != self.target_guardian_build)
            .unwrap_or(false);
        Ok(needs_handover)
    }

    fn start_generation(
        &mut self,
        target: &ArtifactIdentity,
        budget: Duration,
    ) -> Result<yi_agent_lifecycle::types::RuntimeIdentity, String> {
        // Re-verify immediately before starting: the bundle can be replaced between
        // the click and this moment, and starting something else would serve a build
        // nobody approved.
        let metadata = std::fs::metadata(self.target_path(target))
            .map_err(|error| format!("the target executable disappeared before start: {error}"))?;
        if !metadata.is_file() {
            return Err("the target executable is no longer a regular file".to_string());
        }
        let deadline = Deadline::after(budget);
        // A switch is being carried out for a user who wants work to continue here, so
        // the intent is `Business`: a lease-only start would not open the daemon.
        match bringup::ensure_managed(&self.scope, target, BringUpIntent::Business, deadline) {
            Ok(BringUpResult::Connected { identity, .. }) => Ok(identity),
            Ok(BringUpResult::VersionChoiceRequired { identity }) => Err(format!(
                "the new generation came back as build {:?} instead of {}",
                identity.daemon_build_id, target.build_id
            )),
            Ok(BringUpResult::ManualActionRequired { reason }) => Err(reason),
            Err(error) => Err(error.to_string()),
        }
    }

    fn verify(
        &mut self,
        target: &ArtifactIdentity,
    ) -> Result<yi_agent_lifecycle::types::RuntimeIdentity, String> {
        let deadline = Deadline::after(Duration::from_secs(2));
        let identity = match self
            .probe
            .probe(&bringup::control_path(&self.scope), &deadline)
        {
            bringup::ProbeOutcome::Answered(identity) => identity,
            bringup::ProbeOutcome::Absent => {
                return Err("the control channel is absent after the start".to_string());
            }
            bringup::ProbeOutcome::Unresponsive(reason) => {
                return Err(format!("the control channel did not answer: {reason}"));
            }
        };
        // Both builds must be the target. A daemon that came back as some other build
        // is exactly the silent downgrade the selection record exists to prevent.
        if identity.daemon_build_id.as_deref() != Some(target.build_id.as_str()) {
            return Err(format!(
                "the running daemon reports build {:?}, not {}",
                identity.daemon_build_id, target.build_id
            ));
        }
        if identity.guardian_build_id.as_deref() != Some(target.build_id.as_str()) {
            return Err(format!(
                "the guardian reports build {:?}, not {}",
                identity.guardian_build_id, target.build_id
            ));
        }
        // The generation must have changed: reusing the old name would let a client
        // that never re-attached keep acting against a generation it never joined.
        if let Some(previous) = &self.previous_daemon {
            if identity.daemon_instance_id.as_ref() == Some(previous) {
                return Err(format!(
                    "the daemon generation did not change (still {previous}); the old \
                     instantiation is still answering"
                ));
            }
        }
        if identity.mode != yi_agent_lifecycle::types::RuntimeMode::Managed {
            return Err(
                "the runtime came back in a non-managed mode, so the exit guarantees do not \
                 apply"
                    .to_string(),
            );
        }
        Ok(identity)
    }

    fn release(&mut self) {
        // The coordination lock is released by dropping it, which is what makes a
        // guardian crash release the scope rather than wedge it forever.
        self.guard = None;
    }
}

/// Whether a switch is worth attempting, from the pre-flight alone.
///
/// Exposed so the confirm handler can answer the user without running the switch.
pub fn compatibility_allows_switch(compatibility: Compatibility) -> bool {
    matches!(
        compatibility,
        Compatibility::SameBuild
            | Compatibility::CompatibleMismatch
            | Compatibility::ControlOnly
            | Compatibility::LegacyTemporaryChoiceRequired
            | Compatibility::Unknown
    )
}

/// The status a switch that never began should report.
///
/// Used when the confirm handler refuses before handing work to the controller, so the
/// caller still has an operation to poll instead of silence.
pub fn refused_status(operation: &str, reason: &str) -> OperationStatus {
    OperationStatus {
        operation_id: operation.to_string(),
        phase: yi_agent_lifecycle::types::SwitchPhase::Failed,
        detail: Some(reason.to_string()),
        ready_identity: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scope(dir: &Path) -> RuntimeScope {
        RuntimeScope {
            key: dir.to_string_lossy().into_owned(),
            runtime_dir: dir.to_path_buf(),
            project_key: "/project".into(),
            instance_home: dir.to_path_buf(),
        }
    }

    fn target(executable: &Path) -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: "next".into(),
            display_version: "0.2.0".into(),
            executable: executable.to_path_buf(),
            sha256: "aa".into(),
            schema_min: 1,
            schema_max: 16,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: vec![],
        }
    }

    fn executable(dir: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("yi-agent-next");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    struct CountingWitness {
        terminated: Arc<AtomicUsize>,
        stopped: bool,
        owners: Vec<String>,
    }

    impl StopWitness for CountingWitness {
        fn terminate_all(&mut self, _confirm: Duration) -> Result<(), String> {
            self.terminated.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn all_stopped(&mut self) -> Result<bool, String> {
            Ok(self.stopped)
        }
        fn still_running(&mut self) -> Vec<String> {
            self.owners.clone()
        }
    }

    struct FixedPresence(bool);

    impl ClientPresence for FixedPresence {
        fn has_live_clients(&self) -> bool {
            self.0
        }
    }

    struct AbsentProbe;

    impl ControlProbe for AbsentProbe {
        fn probe(&self, _path: &Path, _deadline: &Deadline) -> bringup::ProbeOutcome {
            bringup::ProbeOutcome::Absent
        }
    }

    fn backend(dir: &Path) -> (ManagedSwitchBackend, Arc<AtomicUsize>) {
        let terminated = Arc::new(AtomicUsize::new(0));
        let guard =
            CoordinationGuard::acquire(&scope(dir), Deadline::after(Duration::from_secs(1)))
                .unwrap();
        let backend = ManagedSwitchBackend::new(
            scope(dir),
            "next",
            Box::new(CountingWitness {
                terminated: Arc::clone(&terminated),
                stopped: true,
                owners: vec![],
            }),
            Box::new(FixedPresence(true)),
            Box::new(AbsentProbe),
        )
        .with_guard(guard);
        (backend, terminated)
    }

    /// Without the coordination lock the pre-flight refuses, so a switch cannot start
    /// while another coordinator owns the scope.
    #[test]
    fn a_preflight_without_the_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = executable(dir.path());
        let mut backend = ManagedSwitchBackend::new(
            scope(dir.path()),
            "next",
            Box::new(DetachedWitness),
            Box::new(FixedPresence(true)),
            Box::new(AbsentProbe),
        );
        assert!(backend.preflight(&target(&path)).is_err());
    }

    #[test]
    fn a_missing_target_is_manual_action_not_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (mut backend, terminated) = backend(dir.path());
        let missing = target(&dir.path().join("does-not-exist"));
        match backend.preflight(&missing).unwrap() {
            Preflight::ManualAction(reason) => {
                assert!(reason.contains("reinstall") || reason.contains("gone"));
            }
            Preflight::Ready => panic!("a missing bundle must not be ready"),
        }
        assert_eq!(terminated.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_non_executable_target_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        std::fs::write(&path, b"not a program").unwrap();
        let (mut backend, _) = backend(dir.path());
        match backend.preflight(&target(&path)).unwrap() {
            Preflight::ManualAction(reason) => assert!(reason.contains("not executable")),
            Preflight::Ready => panic!("a non-executable file must not be ready"),
        }
    }

    /// A target from before the handover boundary cannot be carried into: stopping
    /// first would leave no runtime at all.
    #[test]
    fn a_target_without_handover_support_needs_manual_action() {
        let dir = tempfile::tempdir().unwrap();
        let path = executable(dir.path());
        let (mut backend, _) = backend(dir.path());
        let mut old = target(&path);
        old.handover_version = 0;
        match backend.preflight(&old).unwrap() {
            Preflight::ManualAction(reason) => assert!(reason.contains("controlled handover")),
            Preflight::Ready => panic!("a pre-handover target must not be ready"),
        }
    }

    #[test]
    fn a_valid_target_passes_the_preflight_and_retires_self_healing() {
        let dir = tempfile::tempdir().unwrap();
        let path = executable(dir.path());
        let (mut backend, _) = backend(dir.path());
        assert_eq!(backend.preflight(&target(&path)).unwrap(), Preflight::Ready);
        backend.freeze().unwrap();
        assert!(super::super::retirement::is_retired(dir.path()));
    }

    #[test]
    fn the_selection_is_recorded_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = executable(dir.path());
        let (mut backend, _) = backend(dir.path());
        let installed = target(&path);
        backend.commit_selection(&installed).unwrap();
        let recorded = read_selection(&scope(dir.path())).unwrap().unwrap();
        assert_eq!(recorded.target.build_id, "next");
    }

    /// A detached witness cannot prove a stop, so the switch must not treat silence as
    /// success.
    #[test]
    fn a_detached_witness_cannot_claim_a_stop() {
        let mut witness = DetachedWitness;
        assert!(witness.terminate_all(Duration::from_millis(1)).is_err());
        assert!(witness.all_stopped().is_err());
    }

    /// An unknown presence source must not take the "no clients" branch.
    #[test]
    fn an_unknown_presence_does_not_claim_nobody_is_waiting() {
        assert!(UnknownPresence.has_live_clients());
    }

    #[test]
    fn verifying_an_absent_control_channel_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = executable(dir.path());
        let (mut backend, _) = backend(dir.path());
        assert!(backend.verify(&target(&path)).is_err());
    }

    #[test]
    fn the_release_drops_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (mut backend, _) = backend(dir.path());
        backend.release();
        // A fresh acquisition must succeed now that the switch let go.
        CoordinationGuard::acquire(
            &scope(dir.path()),
            Deadline::after(Duration::from_millis(200)),
        )
        .expect("the lock must be free after release");
    }

    #[test]
    fn every_compatibility_permits_a_switch_with_an_explicit_preview() {
        for compatibility in [
            Compatibility::SameBuild,
            Compatibility::CompatibleMismatch,
            Compatibility::ControlOnly,
            Compatibility::LegacyTemporaryChoiceRequired,
            Compatibility::Unknown,
        ] {
            assert!(compatibility_allows_switch(compatibility));
        }
    }

    #[test]
    fn a_refused_operation_is_pollable_and_failed() {
        let status = refused_status("op-1", "no lock");
        assert_eq!(status.operation_id, "op-1");
        assert_eq!(status.phase, yi_agent_lifecycle::types::SwitchPhase::Failed);
        assert!(status.detail.unwrap().contains("no lock"));
        assert!(status.ready_identity.is_none());
    }

    #[test]
    fn describe_record_never_leaks_a_credential() {
        // The failure message names an owner and a pid; a grant's secret must never
        // appear in a status detail a UI shows.
        let record = ProcessRecord {
            scope_key: "/scope".into(),
            generation: "guardian-1".into(),
            process_id: "worker-1".into(),
            pid: 42,
            pgid: 42,
            start_micros: 1,
            artifact_build_id: "next".into(),
            artifact_sha256: "aa".into(),
            owner: ProcessOwner::frontend("tui"),
            root: None,
            execution: None,
            state: RecordState::Running,
            fault: None,
        };
        let described = describe_record(&record);
        assert!(described.contains("42"));
        assert!(!described.contains("credential"));
    }
}
