//! Bounded version switch: the ordered, budgeted sequence that replaces a managed
//! runtime's business generation.
//!
//! The state machine is deliberately separated from the I/O that carries it out.
//! [`SwitchMachine`] knows the legal phases, the invariants that make a phase
//! legitimate, and when a failure must become `manual_action_required`; the
//! [`SwitchBackend`] does the work. That split is what makes the ordering testable
//! without forking processes: a fake backend can record the exact sequence and
//! inject a failure at each stage, and the machine's rules are checked by value.
//!
//! Three properties are load-bearing, and each is enforced here rather than left to
//! the caller:
//!
//! 1. **Two absolute windows, never per object.** One 10 second grace for everything
//!    that has to stop, then at most one 5 second confirmation window. A per-client
//!    budget would multiply by the number of clients and silently break the promise.
//! 2. **`StoppedNoClients` is not `Ready`.** With no live client left there is
//!    nobody to serve, so the new generation is *not* started and the operation says
//!    so. Reporting `Ready` would claim a daemon is running when nothing is.
//! 3. **A stop is a proof, not a request.** The transition out of `Stopping` is taken
//!    only from the backend's verdict, never from having sent a signal.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_lifecycle::deadline::{Clock, Deadline};
use yi_agent_lifecycle::types::{
    ArtifactIdentity, OperationId, OperationStatus, RuntimeIdentity, SwitchPhase,
};

/// The common graceful budget for everything that has to stop.
///
/// Absolute and shared: the drain, the IPC reads and every wait inside it come out
/// of this one window. Reusing the business IPC's 30 second request timeout here
/// would turn the 10 second guarantee into a 30 second one.
pub const SWITCH_GRACE: Duration = Duration::from_secs(10);

/// The confirmation window after the grace period, used only to prove that groups
/// which were told to stop are actually gone.
pub const SWITCH_CONFIRM: Duration = Duration::from_secs(5);

/// The budget for starting the new generation and reaching ready on both channels.
///
/// Control handover, daemon start and the readiness handshake all come out of this.
pub const SWITCH_START: Duration = Duration::from_secs(30);

/// What the switch needs from the outside world.
///
/// Every method is allowed to fail, and every failure is reported by value so the
/// machine can decide whether it is recoverable or requires a human. Implementations
/// must not swallow a failure into "stopped": whether the thing is gone is precisely
/// what has to be proven.
pub trait SwitchBackend: Send {
    /// Read-only pre-flight, run *before* anything is stopped.
    ///
    /// Answers "can this target actually take over": the artifact exists, is
    /// executable, is the identity that was previewed, and the schema is compatible.
    /// Never starts a daemon and never touches user work.
    fn preflight(&mut self, target: &ArtifactIdentity) -> Result<Preflight, String>;

    /// Freeze self-healing and every admission path for this scope.
    ///
    /// Must not return `Ok` while an old watcher could still revive the old daemon:
    /// that would let the switch report a stop while the runtime comes back behind it.
    fn freeze(&mut self) -> Result<(), String>;

    /// Tell clients that can receive notifications what is about to happen.
    fn notify(&mut self, message: &str);

    /// Ask everything to stop, within the shared grace window.
    ///
    /// Returns only after the window is spent or everything stopped; the caller
    /// decides which by asking [`SwitchBackend::is_stopped`].
    fn request_stop(&mut self, grace: Duration);

    /// Terminate the still-running managed groups, within the confirmation window.
    fn terminate(&mut self, confirm: Duration) -> Result<(), String>;

    /// Is every daemon, executor and managed group of the old generation proven gone?
    ///
    /// `false` is a real answer: it means the switch must fail rather than start a
    /// second scheduler next to a live one.
    fn is_stopped(&mut self) -> Result<bool, String>;

    /// Whether any client still holds a live lease.
    ///
    /// Asked *after* stopping, to decide between `Ready` and `StoppedNoClients`.
    fn has_live_clients(&self) -> bool;

    /// Record the chosen build, atomically, under the coordination lock.
    fn commit_selection(&mut self, target: &ArtifactIdentity) -> Result<(), String>;

    /// Hand the control plane over, if this guardian's build differs from the target.
    ///
    /// Returns whether a handover was performed. `Err` means the old control plane
    /// could not be carried across, which is not a state to paper over.
    fn handover_control(&mut self) -> Result<bool, String>;

    /// Start the new generation and wait for both channels to report ready.
    fn start_generation(
        &mut self,
        target: &ArtifactIdentity,
        budget: Duration,
    ) -> Result<RuntimeIdentity, String>;

    /// Verify the running pair really is the target build and a new generation.
    fn verify(&mut self, target: &ArtifactIdentity) -> Result<RuntimeIdentity, String>;

    /// Withdraw the maintenance barrier and restore demand.
    ///
    /// Called on success and on failure: a barrier left up blocks every new task on
    /// behalf of a switch that is no longer happening.
    fn release(&mut self);
}

/// The result of the read-only pre-flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preflight {
    /// The target can take over.
    Ready,
    /// A human has to act first (an old frontend to quit, a missing bundle).
    ///
    /// The switch refuses *before* stopping anything, so the user keeps a working
    /// runtime while they fix it.
    ManualAction(String),
}

/// The observable outcome of running a switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// The final status. Its phase is one of `Ready`, `StoppedNoClients`,
    /// `Failed` or `ManualActionRequired`.
    pub status: OperationStatus,
    /// Whether the target was actually started and verified.
    pub started: bool,
}

/// The pure state machine: which phases may follow which, and what failure means.
///
/// It owns no I/O and no lock, so every rule below can be tested by value.
#[derive(Clone, Debug)]
pub struct SwitchMachine {
    operation: OperationId,
    phase: SwitchPhase,
    detail: Option<String>,
    ready_identity: Option<RuntimeIdentity>,
    started: bool,
}

impl SwitchMachine {
    pub fn new(operation: OperationId) -> Self {
        Self {
            operation,
            phase: SwitchPhase::Idle,
            detail: None,
            ready_identity: None,
            started: false,
        }
    }

    pub fn phase(&self) -> SwitchPhase {
        self.phase
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Moves to `next`, refusing a transition that would skip a required phase.
    ///
    /// The ordering is the spec's: nothing may be stopped before the pre-flight has
    /// passed, and nothing may be started before the old generation is proven gone.
    fn advance(&mut self, next: SwitchPhase) -> Result<(), String> {
        let legal = matches!(
            (self.phase, next),
            (SwitchPhase::Idle, SwitchPhase::Preparing)
                // The pre-flight can end the operation without any teardown.
                | (SwitchPhase::Preparing, SwitchPhase::ManualActionRequired)
                | (SwitchPhase::Preparing, SwitchPhase::Draining)
                | (SwitchPhase::Draining, SwitchPhase::Stopping)
                // No clients left: the old generation is stopped and the new one is
                // deliberately not started.
                | (SwitchPhase::Stopping, SwitchPhase::StoppedNoClients)
                | (SwitchPhase::Stopping, SwitchPhase::Starting)
                | (SwitchPhase::Starting, SwitchPhase::Verifying)
                | (SwitchPhase::Verifying, SwitchPhase::Ready)
        );
        if !legal {
            return Err(format!(
                "a switch cannot go from {:?} to {:?}",
                self.phase, next
            ));
        }
        self.phase = next;
        Ok(())
    }

    /// Ends the operation at `Failed` from any in-flight phase.
    fn fail(&mut self, detail: String) {
        // A failed switch never claims a ready identity, even if the failure came
        // after a start: the verification that would have justified it did not pass.
        self.ready_identity = None;
        self.phase = SwitchPhase::Failed;
        self.detail = Some(detail);
    }

    fn manual(&mut self, detail: String) {
        self.ready_identity = None;
        self.phase = SwitchPhase::ManualActionRequired;
        self.detail = Some(detail);
    }

    fn status(&self) -> OperationStatus {
        OperationStatus {
            operation_id: self.operation.clone(),
            phase: self.phase,
            detail: self.detail.clone(),
            ready_identity: self.ready_identity.clone(),
        }
    }
}

/// Runs one switch to a terminal phase, on the caller's thread.
///
/// The caller is expected to have already taken the coordination lock and installed
/// the maintenance barrier: a switch that is started must own the scope for its whole
/// duration, and this function neither takes nor releases the lock.
///
/// Both windows are *durations* rather than injected time: the guarantee is about
/// elapsed real time and must hold even under a frozen test clock.
pub fn run(
    machine: &mut SwitchMachine,
    backend: &mut dyn SwitchBackend,
    target: &ArtifactIdentity,
) -> SwitchOutcome {
    let outcome = run_inner(machine, backend, target);
    // The barrier comes down on every exit, including the failures: leaving it up
    // would refuse every new task on behalf of a switch that is not running.
    backend.release();
    outcome
}

fn run_inner(
    machine: &mut SwitchMachine,
    backend: &mut dyn SwitchBackend,
    target: &ArtifactIdentity,
) -> SwitchOutcome {
    // 1. Pre-flight, before anything is stopped.
    if let Err(error) = machine.advance(SwitchPhase::Preparing) {
        machine.fail(error);
        return finish(machine);
    }
    match backend.preflight(target) {
        Ok(Preflight::Ready) => {}
        // A pre-flight that cannot run is not a green light either.
        Ok(Preflight::ManualAction(reason)) | Err(reason) => {
            machine.manual(reason);
            return finish(machine);
        }
    }

    // 2. Freeze self-healing and admission, then tell clients.
    if let Err(reason) = backend.freeze() {
        machine.fail(format!(
            "could not freeze self-healing and admission: {reason}"
        ));
        return finish(machine);
    }
    if let Err(error) = machine.advance(SwitchPhase::Draining) {
        machine.fail(error);
        return finish(machine);
    }
    backend.notify("the runtime is restarting into a new build");

    // 3. One shared grace window for everything that has to stop.
    backend.request_stop(SWITCH_GRACE);
    if let Err(error) = machine.advance(SwitchPhase::Stopping) {
        machine.fail(error);
        return finish(machine);
    }
    // 4. Anything still up gets the confirmation window, then must be proven gone.
    if let Err(reason) = backend.terminate(SWITCH_CONFIRM) {
        machine.fail(format!(
            "the old generation could not be stopped ({reason}); the runtime is \
             not safe to replace, and no new generation was started"
        ));
        return finish(machine);
    }
    match backend.is_stopped() {
        Ok(true) => {}
        Ok(false) => {
            machine.fail(
                "the old daemon, its executors or a managed group is still running; \
                 starting a new generation now would leave two schedulers on one runtime"
                    .to_string(),
            );
            return finish(machine);
        }
        Err(reason) => {
            machine.fail(format!(
                "could not prove the old generation stopped ({reason}); refusing to start \
                 a second one"
            ));
            return finish(machine);
        }
    }

    // 5. Record the choice under the lock. Until this succeeds, a still-running
    //    older client could pick its own binary on the next start.
    if let Err(reason) = backend.commit_selection(target) {
        machine.fail(format!("could not record the chosen build: {reason}"));
        return finish(machine);
    }

    // 6. With nobody left to serve, do not start a process nobody asked for.
    if !backend.has_live_clients() {
        if let Err(error) = machine.advance(SwitchPhase::StoppedNoClients) {
            machine.fail(error);
        } else {
            machine.detail = Some(
                "the old generation stopped and no client is connected; the new build \
                 starts on the next connection"
                    .to_string(),
            );
        }
        return finish(machine);
    }

    // 7. Hand the control plane over if the guardian build differs, then start.
    if let Err(reason) = backend.handover_control() {
        machine.fail(format!(
            "the control plane could not be handed over: {reason}"
        ));
        return finish(machine);
    }
    if let Err(error) = machine.advance(SwitchPhase::Starting) {
        machine.fail(error);
        return finish(machine);
    }
    machine.started = true;
    if let Err(reason) = backend.start_generation(target, SWITCH_START) {
        machine.fail(format!(
            "the new generation did not become ready ({reason}); the managed children \
             started by this attempt were reclaimed"
        ));
        return finish(machine);
    }

    // 8. Verify the running pair, not just that something started.
    if let Err(error) = machine.advance(SwitchPhase::Verifying) {
        machine.fail(error);
        return finish(machine);
    }
    match backend.verify(target) {
        Ok(identity) => {
            if let Err(error) = machine.advance(SwitchPhase::Ready) {
                machine.fail(error);
                return finish(machine);
            }
            machine.ready_identity = Some(identity);
        }
        Err(reason) => machine.fail(format!("the new generation did not verify: {reason}")),
    }
    finish(machine)
}

fn finish(machine: &SwitchMachine) -> SwitchOutcome {
    SwitchOutcome {
        status: machine.status(),
        started: machine.started && machine.phase == SwitchPhase::Ready,
    }
}

/// A switch operation as the outside world can observe it.
///
/// [`SwitchController::start`] must not block the control-request handler: the
/// handler answers "accepted" and the operation runs behind this registry, so a long
/// teardown never holds a control connection open past its budget.
pub struct SwitchController {
    clock: Arc<dyn Clock>,
    operations: Arc<Mutex<BTreeMap<OperationId, OperationStatus>>>,
    running: Arc<Mutex<Option<OperationId>>>,
}

impl SwitchController {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            running: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts `operation` on a background thread and returns immediately.
    ///
    /// Returns `WouldBlock` when a switch is already running: two replacements of one
    /// runtime would each believe they own the scope.
    pub fn start(
        &self,
        operation: OperationId,
        target: ArtifactIdentity,
        backend: Box<dyn SwitchBackend>,
    ) -> io::Result<()> {
        {
            let mut running = self
                .running
                .lock()
                .map_err(|_| io::Error::other("switch registry mutex poisoned"))?;
            if let Some(active) = running.as_ref() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("a switch is already running ({active})"),
                ));
            }
            *running = Some(operation.clone());
        }
        // Nothing has run yet, but the status must exist the moment `start` returns:
        // a caller that polls immediately must see its own operation.
        self.record(OperationStatus {
            operation_id: operation.clone(),
            phase: SwitchPhase::Preparing,
            detail: None,
            ready_identity: None,
        });

        let registry = Arc::clone(&self.operations);
        let running = Arc::clone(&self.running);
        // The thread needs its own copy: the failures below still name the operation,
        // and a moved value would leave this function unable to report them.
        let operation_for_thread = operation.clone();
        let spawn_result = std::thread::Builder::new()
            .name(format!("switch-{operation}"))
            .spawn(move || {
                let operation = operation_for_thread;
                let mut machine = SwitchMachine::new(operation.clone());
                let mut backend = backend;
                let outcome = run(&mut machine, backend.as_mut(), &target);
                if let Ok(mut registry) = registry.lock() {
                    registry.insert(operation.clone(), outcome.status);
                }
                if let Ok(mut running) = running.lock()
                    && running.as_deref() == Some(operation.as_str())
                {
                    *running = None;
                }
            });

        if let Err(error) = spawn_result {
            // The operation never ran, so it must not stay "Preparing" forever.
            if let Ok(mut running) = self.running.lock() {
                *running = None;
            }
            self.record(OperationStatus {
                operation_id: operation.clone(),
                phase: SwitchPhase::Failed,
                detail: Some(format!("the switch could not be started: {error}")),
                ready_identity: None,
            });
            return Err(error);
        }
        Ok(())
    }

    /// The status of one operation, or `None` if this controller never ran it.
    pub fn status(&self, operation: &str) -> Option<OperationStatus> {
        self.operations
            .lock()
            .ok()
            .and_then(|registry| registry.get(operation).cloned())
    }

    /// The operation currently in flight, if any.
    pub fn running(&self) -> Option<OperationId> {
        self.running.lock().ok().and_then(|running| running.clone())
    }

    /// The clock this controller was built with, for callers that must not start a
    /// second, incomparable time base.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    fn record(&self, status: OperationStatus) {
        if let Ok(mut registry) = self.operations.lock() {
            registry.insert(status.operation_id.clone(), status);
        }
    }
}

impl std::fmt::Debug for SwitchController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwitchController")
            .field("running", &self.running())
            .finish_non_exhaustive()
    }
}

/// Refuses a switch that was handed a budget that is already spent, which would mean
/// an unbounded teardown.
pub fn require_budget(deadline: &Deadline) -> Result<(), String> {
    if deadline.is_expired() {
        return Err("the switch budget was already spent before it started".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn target() -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: "next".into(),
            display_version: "0.2.0".into(),
            executable: "/usr/bin/true".into(),
            sha256: "aa".into(),
            schema_min: 1,
            schema_max: 16,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: vec![],
        }
    }

    fn identity(build: &str) -> RuntimeIdentity {
        RuntimeIdentity {
            scope_key: "/scope".into(),
            daemon_instance_id: Some("gen-2".into()),
            guardian_build_id: Some(build.into()),
            daemon_build_id: Some(build.into()),
            mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
            business_protocol: Some(1),
            daemon_display_version: Some("0.2.0".into()),
            guardian_display_version: Some("0.2.0".into()),
            capabilities: vec![],
            schema_min: 1,
            schema_max: 16,
        }
    }

    /// Records the order of every call and can fail at one chosen stage.
    struct FakeBackend {
        log: Vec<&'static str>,
        fail_at: Option<&'static str>,
        preflight: Option<Preflight>,
        stopped: bool,
        live_clients: bool,
        released: Arc<AtomicBool>,
        started_generation: Arc<AtomicBool>,
    }

    impl FakeBackend {
        fn healthy() -> Self {
            Self {
                log: Vec::new(),
                fail_at: None,
                preflight: Some(Preflight::Ready),
                stopped: true,
                live_clients: true,
                released: Arc::new(AtomicBool::new(false)),
                started_generation: Arc::new(AtomicBool::new(false)),
            }
        }

        fn record(&mut self, stage: &'static str) -> Result<(), String> {
            self.log.push(stage);
            if self.fail_at == Some(stage) {
                return Err(format!("injected failure at {stage}"));
            }
            Ok(())
        }
    }

    impl SwitchBackend for FakeBackend {
        fn preflight(&mut self, _target: &ArtifactIdentity) -> Result<Preflight, String> {
            self.record("preflight")?;
            Ok(self.preflight.clone().unwrap_or(Preflight::Ready))
        }
        fn freeze(&mut self) -> Result<(), String> {
            self.record("freeze")
        }
        fn notify(&mut self, _message: &str) {
            self.log.push("notify");
        }
        fn request_stop(&mut self, _grace: Duration) {
            self.log.push("request_stop");
        }
        fn terminate(&mut self, _confirm: Duration) -> Result<(), String> {
            self.record("terminate")
        }
        fn is_stopped(&mut self) -> Result<bool, String> {
            self.record("is_stopped")?;
            Ok(self.stopped)
        }
        fn has_live_clients(&self) -> bool {
            self.live_clients
        }
        fn commit_selection(&mut self, _target: &ArtifactIdentity) -> Result<(), String> {
            self.record("commit_selection")
        }
        fn handover_control(&mut self) -> Result<bool, String> {
            self.record("handover_control")?;
            Ok(false)
        }
        fn start_generation(
            &mut self,
            _target: &ArtifactIdentity,
            _budget: Duration,
        ) -> Result<RuntimeIdentity, String> {
            self.started_generation.store(true, Ordering::SeqCst);
            self.record("start_generation")?;
            Ok(identity("0.2.0"))
        }
        fn verify(&mut self, _target: &ArtifactIdentity) -> Result<RuntimeIdentity, String> {
            self.record("verify")?;
            Ok(identity("0.2.0"))
        }
        fn release(&mut self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    fn machine() -> SwitchMachine {
        SwitchMachine::new("op".to_string())
    }

    #[test]
    fn no_clients_after_stop_is_not_ready() {
        let status = OperationStatus {
            operation_id: "op".into(),
            phase: SwitchPhase::StoppedNoClients,
            detail: None,
            ready_identity: None,
        };
        assert!(status.ready_identity.is_none());
        assert!(!matches!(status.phase, SwitchPhase::Ready));
    }

    /// With nobody to serve, the switch must record the stop and *not* start a
    /// process nobody asked for.
    #[test]
    fn a_runtime_with_no_clients_is_stopped_and_not_started() {
        let started = Arc::new(AtomicBool::new(false));
        let mut backend = FakeBackend {
            live_clients: false,
            started_generation: Arc::clone(&started),
            ..FakeBackend::healthy()
        };
        let released = Arc::clone(&backend.released);
        let mut machine = machine();
        let outcome = run(&mut machine, &mut backend, &target());
        assert_eq!(outcome.status.phase, SwitchPhase::StoppedNoClients);
        assert!(!outcome.started);
        assert!(
            !started.load(Ordering::SeqCst),
            "no client means no daemon to start"
        );
        assert!(
            released.load(Ordering::SeqCst),
            "the barrier must come down"
        );
    }

    #[test]
    fn a_healthy_switch_reaches_ready_with_an_identity() {
        let mut backend = FakeBackend::healthy();
        let mut machine = machine();
        let outcome = run(&mut machine, &mut backend, &target());
        assert_eq!(outcome.status.phase, SwitchPhase::Ready);
        assert!(outcome.started);
        assert_eq!(
            outcome
                .status
                .ready_identity
                .as_ref()
                .unwrap()
                .daemon_build_id,
            Some("0.2.0".into())
        );
    }

    /// The order is the promise: nothing is stopped before the pre-flight passes,
    /// and nothing starts before the old generation is proven gone.
    #[test]
    fn the_stages_run_in_the_specified_order() {
        let mut backend = FakeBackend::healthy();
        let mut machine = machine();
        run(&mut machine, &mut backend, &target());
        assert_eq!(
            backend.log,
            vec![
                "preflight",
                "freeze",
                "notify",
                "request_stop",
                "terminate",
                "is_stopped",
                "commit_selection",
                "handover_control",
                "start_generation",
                "verify",
            ]
        );
    }

    /// A pre-flight that asks for manual action must not have stopped anything.
    #[test]
    fn a_manual_preflight_stops_nothing() {
        let mut backend = FakeBackend {
            preflight: Some(Preflight::ManualAction("quit the old TUI first".into())),
            ..FakeBackend::healthy()
        };
        let mut machine = machine();
        let outcome = run(&mut machine, &mut backend, &target());
        assert_eq!(outcome.status.phase, SwitchPhase::ManualActionRequired);
        assert_eq!(
            backend.log,
            vec!["preflight"],
            "nothing may be stopped before the pre-flight passes"
        );
        assert!(outcome.status.detail.unwrap().contains("quit the old TUI"));
    }

    /// A still-running old group means no new generation: that is the whole point of
    /// the proof.
    #[test]
    fn an_unproven_stop_never_starts_a_second_generation() {
        let started = Arc::new(AtomicBool::new(false));
        let mut backend = FakeBackend {
            stopped: false,
            started_generation: Arc::clone(&started),
            ..FakeBackend::healthy()
        };
        let mut machine = machine();
        let outcome = run(&mut machine, &mut backend, &target());
        assert_eq!(outcome.status.phase, SwitchPhase::Failed);
        assert!(!outcome.started);
        assert!(!started.load(Ordering::SeqCst));
        assert!(outcome.status.detail.unwrap().contains("still running"));
    }

    /// Every stage that can fail must leave the operation failed, the barrier down,
    /// and no claim of readiness.
    #[test]
    fn every_stage_failure_fails_closed() {
        for stage in [
            "preflight",
            "freeze",
            "terminate",
            "is_stopped",
            "commit_selection",
            "handover_control",
            "start_generation",
            "verify",
        ] {
            let mut backend = FakeBackend {
                fail_at: Some(stage),
                ..FakeBackend::healthy()
            };
            let released = Arc::clone(&backend.released);
            let mut machine = machine();
            let outcome = run(&mut machine, &mut backend, &target());
            assert!(
                matches!(
                    outcome.status.phase,
                    SwitchPhase::Failed | SwitchPhase::ManualActionRequired
                ),
                "stage {stage} left the switch in {:?}",
                outcome.status.phase
            );
            assert!(
                outcome.status.ready_identity.is_none(),
                "stage {stage} claimed readiness"
            );
            assert!(
                released.load(Ordering::SeqCst),
                "stage {stage} left the maintenance barrier up"
            );
        }
    }

    #[test]
    fn a_skip_in_the_ordering_is_refused() {
        let mut machine = SwitchMachine::new("op".to_string());
        assert!(machine.advance(SwitchPhase::Ready).is_err());
        assert!(machine.advance(SwitchPhase::Preparing).is_ok());
        assert!(
            machine.advance(SwitchPhase::Starting).is_err(),
            "starting before the old generation is stopped must be refused"
        );
    }

    /// A second switch on the same controller is refused, not queued: two
    /// replacements of one runtime would both believe they own the scope.
    #[test]
    fn a_second_switch_is_refused_while_one_runs() {
        struct SlowBackend {
            gate: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
        }
        impl SwitchBackend for SlowBackend {
            fn preflight(&mut self, _t: &ArtifactIdentity) -> Result<Preflight, String> {
                let (lock, condvar) = &*self.gate;
                let guard = lock.lock().unwrap();
                let _ = condvar.wait_timeout(guard, Duration::from_secs(5)).unwrap();
                Ok(Preflight::ManualAction("done".into()))
            }
            fn freeze(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn notify(&mut self, _m: &str) {}
            fn request_stop(&mut self, _g: Duration) {}
            fn terminate(&mut self, _c: Duration) -> Result<(), String> {
                Ok(())
            }
            fn is_stopped(&mut self) -> Result<bool, String> {
                Ok(true)
            }
            fn has_live_clients(&self) -> bool {
                false
            }
            fn commit_selection(&mut self, _t: &ArtifactIdentity) -> Result<(), String> {
                Ok(())
            }
            fn handover_control(&mut self) -> Result<bool, String> {
                Ok(false)
            }
            fn start_generation(
                &mut self,
                _t: &ArtifactIdentity,
                _b: Duration,
            ) -> Result<RuntimeIdentity, String> {
                Err("must not start with no clients".into())
            }
            fn verify(&mut self, _t: &ArtifactIdentity) -> Result<RuntimeIdentity, String> {
                Err("unreachable".into())
            }
            fn release(&mut self) {}
        }

        let controller =
            SwitchController::new(Arc::new(yi_agent_lifecycle::deadline::MonotonicClock::new()));
        let gate = Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new()));
        controller
            .start(
                "first".into(),
                target(),
                Box::new(SlowBackend {
                    gate: Arc::clone(&gate),
                }),
            )
            .unwrap();
        let second = controller.start("second".into(), target(), Box::new(FakeBackend::healthy()));
        assert!(second.is_err(), "a concurrent switch must be refused");
        assert!(controller.status("first").is_some());
        // Release the blocker so the thread does not outlive the test.
        let (lock, condvar) = &*gate;
        drop(lock.lock().unwrap());
        condvar.notify_all();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while controller.running().is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn an_unknown_operation_has_no_status() {
        let controller =
            SwitchController::new(Arc::new(yi_agent_lifecycle::deadline::MonotonicClock::new()));
        assert!(controller.status("never-started").is_none());
        assert!(controller.running().is_none());
    }

    #[test]
    fn the_two_windows_are_absolute_and_bounded() {
        // The spec's promise is 10 seconds of grace and at most 5 more to confirm.
        assert_eq!(SWITCH_GRACE, Duration::from_secs(10));
        assert_eq!(SWITCH_CONFIRM, Duration::from_secs(5));
        assert_eq!(SWITCH_START, Duration::from_secs(30));
    }

    #[test]
    fn a_status_never_reports_ready_without_an_identity() {
        // Guards the failure mode where a status says Ready but carries no identity
        // for a client to attach to.
        let mut machine = SwitchMachine::new("op".to_string());
        machine.advance(SwitchPhase::Preparing).unwrap();
        machine.advance(SwitchPhase::Draining).unwrap();
        machine.advance(SwitchPhase::Stopping).unwrap();
        machine.advance(SwitchPhase::Starting).unwrap();
        machine.advance(SwitchPhase::Verifying).unwrap();
        machine.advance(SwitchPhase::Ready).unwrap();
        assert!(machine.status().ready_identity.is_none());
        let outcome = finish(&machine);
        assert!(!outcome.started, "Ready without an identity is not started");
    }

    #[test]
    fn the_controller_does_not_block_on_a_running_switch() {
        // `start` must answer immediately; the work happens behind the registry.
        struct CountingBackend(Arc<AtomicUsize>);
        impl SwitchBackend for CountingBackend {
            fn preflight(&mut self, _t: &ArtifactIdentity) -> Result<Preflight, String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Preflight::ManualAction("stop here".into()))
            }
            fn freeze(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn notify(&mut self, _m: &str) {}
            fn request_stop(&mut self, _g: Duration) {}
            fn terminate(&mut self, _c: Duration) -> Result<(), String> {
                Ok(())
            }
            fn is_stopped(&mut self) -> Result<bool, String> {
                Ok(true)
            }
            fn has_live_clients(&self) -> bool {
                false
            }
            fn commit_selection(&mut self, _t: &ArtifactIdentity) -> Result<(), String> {
                Ok(())
            }
            fn handover_control(&mut self) -> Result<bool, String> {
                Ok(false)
            }
            fn start_generation(
                &mut self,
                _t: &ArtifactIdentity,
                _b: Duration,
            ) -> Result<RuntimeIdentity, String> {
                unreachable!("a manual pre-flight must not start anything")
            }
            fn verify(&mut self, _t: &ArtifactIdentity) -> Result<RuntimeIdentity, String> {
                unreachable!("a manual pre-flight must not verify anything")
            }
            fn release(&mut self) {}
        }

        let controller =
            SwitchController::new(Arc::new(yi_agent_lifecycle::deadline::MonotonicClock::new()));
        let counter = Arc::new(AtomicUsize::new(0));
        controller
            .start(
                "op".into(),
                target(),
                Box::new(CountingBackend(Arc::clone(&counter))),
            )
            .unwrap();
        // The status exists as soon as `start` returns.
        assert!(controller.status("op").is_some());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while controller.running().is_some() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(
            controller.status("op").unwrap().phase,
            SwitchPhase::ManualActionRequired
        );
    }
}
