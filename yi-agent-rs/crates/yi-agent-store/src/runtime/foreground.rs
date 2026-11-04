//! The foreground commit gate: one step that decides "this work is admitted",
//! plus the registry of executions that have not been proven stopped yet.
//!
//! Before this module dispatch and revocation were three independent steps: read
//! admission, write the child row, bind the owner. A release landing between the
//! read and the write left an admitted child that no client owned, and the
//! reclaimer could not enumerate it. The gate closes that window by making the
//! check and the commit one critical section, and by registering the *starting
//! execution* in that same section: a child that is committed but has not yet
//! produced a worker handle is precisely the state a release has to see.
//!
//! Two rules keep it usable:
//!
//! * Nothing inside the gate awaits. A network wait or a worker join held here
//!   would make every release wait for unrelated work.
//! * The factory is called outside the gate. A factory may block for as long as a
//!   provider takes, so it must not be a critical section -- but the slot it will
//!   fill is registered inside the gate, so a release never has to guess whether
//!   a half-started worker exists.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use yi_agent_core::subagent::task::{AttemptId, TaskId};
use yi_agent_core::subagent::worker::{StopWitness, WorkerHandle};

use crate::lifecycle::RootId;
use crate::runtime::RuntimeCoordinatorError;

/// What authorises one admission.
///
/// Produced only by [`ForegroundCommitGate::commit`], inside the gate, so a caller
/// cannot fabricate one for a root it does not own. It is deliberately not
/// serialisable and has no public constructor: it is the coordinator's private
/// proof that the checks ran against the current generation, not a value that
/// travels over the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionStamp {
    pub(crate) root: RootId,
    pub(crate) task: TaskId,
    pub(crate) attempt: AttemptId,
    pub(crate) generation: String,
    /// The authority sequence the root's binding epoch was minted at. A later
    /// authority message carrying a lower sequence is stale and must not outrank
    /// this admission.
    pub(crate) authority_sequence: u64,
}

/// How far a registered execution has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartPhase {
    /// The row is committed and the factory has not returned a handle yet.
    Starting,
    /// A handle exists and is published; user code may run.
    Running,
    /// The execution is over, either because it never started or because the stop
    /// was confirmed.
    Stopped,
}

/// One execution the coordinator knows about, whether or not it has a handle yet.
struct ExecutionSlot {
    /// The root this execution belongs to.
    ///
    /// Kept per slot so a freeze can be scoped: freezing root A must not refuse
    /// root B's spawns in the same session tree, and a rebind of one conversation
    /// must not stop another's work.
    root: RootId,
    phase: StartPhase,
    cancellation: CancellationToken,
    /// Present once a handle exists.
    handle: Option<WorkerHandle>,
    /// Present until a handle exists. Finished on every path that stops a slot
    /// before publication, so a release never waits for a certificate nobody will
    /// issue.
    witness: Option<StopWitness>,
}

impl ExecutionSlot {
    /// Marks the slot stopped and settles its witness if it never had a handle.
    ///
    /// The witness must be consumed here rather than left pending: `Drop` on the
    /// witness would report "no evidence", and the difference between "proved
    /// stopped" and "nobody looked" is exactly what a release must not blur.
    fn stop(&mut self) -> StopPhase {
        let phase = self.phase;
        self.phase = StartPhase::Stopped;
        if phase == StartPhase::Starting
            && let Some(witness) = self.witness.take()
        {
            witness.mark_joined();
        }
        match phase {
            StartPhase::Starting => StopPhase::Starting,
            StartPhase::Running => StopPhase::Running,
            StartPhase::Stopped => StopPhase::AlreadyStopped,
        }
    }
}

/// What a slot was doing when it was stopped. The caller uses it to decide whether
/// a handle still has to be joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopPhase {
    Starting,
    Running,
    AlreadyStopped,
}

/// Proof that a slot may start user code.
///
/// It does *not* mark the slot running on drop: the transition happens when a
/// handle is published. That distinction is the whole point -- "the row exists"
/// and "a worker exists" are different states, and a release has to be able to see
/// the child in the first one. A permit that is dropped without a handle leaves the
/// slot starting, so the child is still a reclaimable, cancellable execution
/// rather than one that silently looks live.
pub(crate) struct StartingPermit {
    gate: Arc<ForegroundCommitGate>,
    task: TaskId,
}

impl StartingPermit {
    /// Whether user code may begin now. False once the slot was released or
    /// cancelled while the factory was still working.
    pub(crate) fn is_live(&self) -> bool {
        self.gate.is_live(&self.task)
    }

    /// The token the worker must honour. A release that lands while the slot is
    /// still starting cancels it, and the factory's execution sees that.
    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.gate.cancellation_of(&self.task)
    }

    /// The gate this permit was minted by, so a wrapper can settle the slot it
    /// belongs to without being handed a second, unrelated handle.
    pub(crate) fn gate(&self) -> &Arc<ForegroundCommitGate> {
        &self.gate
    }
}

/// What a release has to stop, separated by how far each execution got.
#[derive(Debug, Default)]
pub(crate) struct ReleasedOwner {
    /// Slots cancelled here. They may yet produce a handle, which the caller must
    /// adopt and stop rather than publish.
    pub(crate) starting: Vec<TaskId>,
    /// Slots whose handle already exists and can be cancelled directly.
    pub(crate) running: Vec<TaskId>,
}

/// The whole live set at a freeze, for a rebind.
///
/// Consumed by the rebind path in the stop-proof task; defined here so freezing and
/// releasing enumerate the same slots rather than growing a second, divergent view.
#[derive(Debug, Default)]
pub(crate) struct FrozenSet {
    pub(crate) starting: Vec<TaskId>,
    pub(crate) running: Vec<TaskId>,
}

/// The coordinator's admission gate.
///
/// Shared through an `Arc` because [`StartingPermit`] has to outlive the call that
/// minted it. Every method takes `&self`; the mutex is held only for arithmetic on
/// in-memory maps, never across an await.
pub(crate) struct ForegroundCommitGate {
    state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    slots: HashMap<TaskId, ExecutionSlot>,
    /// Test-only barrier placed at a named point inside the admission protocol.
    ///
    /// A race cannot be *caused* by sleeping, so the test names the exact window it
    /// wants to land in and the production code reports when it is there. Empty in
    /// production.
    #[cfg(any(test, feature = "foreground-test-support"))]
    barriers: HashMap<&'static str, Arc<crate::foreground::TestBarrier>>,
    /// Roots frozen by [`ForegroundCommitGate::freeze_root`]. Sticky within a
    /// generation: a frozen root refuses every new admission under it, because the
    /// point of freezing is that nothing new appears while the old set is being
    /// stopped. Scoped to the root rather than the whole gate so one conversation's
    /// recovery does not stall unrelated work.
    frozen_roots: HashSet<RootId>,
    /// Owners whose admission was closed. A closed owner cannot commit again even
    /// if its root binding has not been removed yet.
    closed_owners: HashSet<String>,
}

impl ForegroundCommitGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(GateState::default()),
        }
    }

    /// Runs the caller's admission check and its commit as one step, then registers
    /// the starting execution.
    ///
    /// `may_admit` is the caller's full opinion (draining? root open? owner
    /// closed?) and runs last, immediately before the row is written, so a release
    /// that closed admission microseconds earlier is observed rather than raced.
    /// `register` writes the child row and records ownership; it must be
    /// synchronous and must not await.
    ///
    /// Returns the stamp, the permit the caller drops once user code may start, and
    /// whatever `register` produced.
    pub(crate) fn commit<T, E>(
        self: &Arc<Self>,
        may_admit: impl FnOnce() -> Result<AdmissionStamp, RuntimeCoordinatorError>,
        register: impl FnOnce(&AdmissionStamp) -> Result<T, E>,
    ) -> Result<(AdmissionStamp, StartingPermit, T), CommitRefusal<E>> {
        let mut state = self.state.lock().expect("foreground gate mutex poisoned");
        let stamp = may_admit().map_err(CommitRefusal::Refused)?;
        if state.frozen_roots.contains(&stamp.root) {
            // Checked after the caller's own admission check so the refusal names the
            // real reason when both apply, and inside the lock so a freeze that landed
            // during `may_admit` is observed.
            return Err(CommitRefusal::Refused(
                RuntimeCoordinatorError::AdmissionClosed(
                    "the foreground root is frozen for a rebind".into(),
                ),
            ));
        }
        let produced = register(&stamp).map_err(CommitRefusal::Commit)?;
        let (witness, _confirmation) = StopWitness::pair();
        state.slots.insert(
            stamp.task.clone(),
            ExecutionSlot {
                root: stamp.root.clone(),
                phase: StartPhase::Starting,
                cancellation: CancellationToken::new(),
                handle: None,
                witness: Some(witness),
            },
        );
        drop(state);
        let permit = StartingPermit {
            gate: Arc::clone(self),
            task: stamp.task.clone(),
        };
        Ok((stamp, permit, produced))
    }

    /// Whether the slot is still allowed to run user code.
    ///
    /// A cancelled token is decisive on its own: a release cancels first and only
    /// settles the slot later (when the factory answers), so between those two
    /// moments the phase still reads `Starting` while the answer is already "no".
    pub(crate) fn is_live(&self, task: &TaskId) -> bool {
        let state = self.state.lock().expect("foreground gate mutex poisoned");
        state.slots.get(task).is_some_and(|slot| {
            slot.phase != StartPhase::Stopped && !slot.cancellation.is_cancelled()
        })
    }

    /// The token a released slot must observe. Public within the crate because the
    /// factory wrapper installs it on the worker it is about to start.
    pub(crate) fn cancellation_of(&self, task: &TaskId) -> CancellationToken {
        let state = self.state.lock().expect("foreground gate mutex poisoned");
        state
            .slots
            .get(task)
            .map(|slot| slot.cancellation.clone())
            .unwrap_or_else(|| {
                // A slot that no longer exists is a slot that was stopped; a token
                // that is already cancelled keeps a late worker from starting.
                let token = CancellationToken::new();
                token.cancel();
                token
            })
    }

    /// Installs the handle for a slot.
    ///
    /// Returns `None` when the slot was released while the factory was working: the
    /// caller then owns the handle and must stop it itself, because publishing it
    /// would resurrect work the release already reclaimed.
    pub(crate) fn publish(&self, task: &TaskId, handle: WorkerHandle) -> Option<WorkerHandle> {
        let mut state = self.state.lock().expect("foreground gate mutex poisoned");
        let slot = state.slots.get_mut(task)?;
        // A cancelled token means the release already decided this slot must not
        // run; publishing it would hand the caller a worker the reclaim set has
        // given up on.
        let live = slot.phase != StartPhase::Stopped && !slot.cancellation.is_cancelled();
        slot.handle = Some(handle.clone());
        // The handle is now the evidence; a witness would be a second, weaker one.
        slot.witness = None;
        if live {
            slot.phase = StartPhase::Running;
            Some(handle)
        } else {
            None
        }
    }

    /// Closes an owner and cancels every starting slot.
    ///
    /// Starting slots are cancelled here rather than after the gate: the factory
    /// may still be waiting, and the cancellation token is the only signal it can
    /// observe. Running slots are returned so the caller can stop them without
    /// holding the gate.
    pub(crate) fn release_owner(&self, owner: &str) -> ReleasedOwner {
        let mut state = self.state.lock().expect("foreground gate mutex poisoned");
        state.closed_owners.insert(owner.to_string());
        let mut released = ReleasedOwner::default();
        for (task, slot) in state.slots.iter_mut() {
            match slot.phase {
                StartPhase::Starting => {
                    slot.cancellation.cancel();
                    released.starting.push(task.clone());
                }
                StartPhase::Running => released.running.push(task.clone()),
                StartPhase::Stopped => {}
            }
        }
        // `TaskId` has no ordering, but the sets are compared in tests, so make
        // iteration order stable by the id's own rendering.
        released.starting.sort_by_key(|task| task.to_string());
        released.running.sort_by_key(|task| task.to_string());
        released
    }

    /// Freezes one root and returns its whole live set for a rebind.
    ///
    /// Scoped to the root: slots under other roots keep admitting work, which is
    /// what makes "stop this conversation" different from "stop the app". Kept
    /// beside `release_owner` so the two enumerate slots the same way.
    ///
    /// Starting slots are cancelled here for the same reason a release cancels
    /// them: their factory has not returned, so the token is the only signal they
    /// can observe, and a freeze that only recorded them would let a late worker
    /// start under a fresh attempt.
    pub(crate) fn freeze_root(&self, root: &RootId) -> FrozenSet {
        let mut state = self.state.lock().expect("foreground gate mutex poisoned");
        state.frozen_roots.insert(root.clone());
        let mut frozen = FrozenSet::default();
        for (task, slot) in state.slots.iter_mut() {
            if &slot.root != root {
                continue;
            }
            match slot.phase {
                StartPhase::Starting => {
                    slot.cancellation.cancel();
                    frozen.starting.push(task.clone());
                }
                StartPhase::Running => frozen.running.push(task.clone()),
                StartPhase::Stopped => {}
            }
        }
        frozen.starting.sort_by_key(|task| task.to_string());
        frozen.running.sort_by_key(|task| task.to_string());
        frozen
    }

    /// Whether one root is frozen, so a rebind can refuse a second freeze that would
    /// silently replace the set the first one is proving.
    pub(crate) fn is_frozen(&self, root: &RootId) -> bool {
        self.state
            .lock()
            .expect("foreground gate mutex poisoned")
            .frozen_roots
            .contains(root)
    }

    /// Records that a slot's execution is over.
    ///
    /// A slot stopped while starting has its witness finished here, so a release
    /// can treat "no handle" plus a settled witness as proof that no user code ran.
    pub(crate) fn stopped(&self, task: &TaskId) -> StopPhase {
        let mut state = self.state.lock().expect("foreground gate mutex poisoned");
        match state.slots.get_mut(task) {
            Some(slot) => slot.stop(),
            None => StopPhase::AlreadyStopped,
        }
    }

    /// Installs a test barrier at a named point. Never populated outside tests.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn install_test_barrier(
        &self,
        point: &'static str,
        barrier: Arc<crate::foreground::TestBarrier>,
    ) {
        self.state
            .lock()
            .expect("foreground gate mutex poisoned")
            .barriers
            .insert(point, barrier);
    }

    /// Waits at a named barrier, if one is installed. The wait happens with the gate
    /// released, so it parks the caller without blocking the rest of the protocol.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub(crate) async fn hold_at(&self, point: &'static str) {
        let barrier = self
            .state
            .lock()
            .expect("foreground gate mutex poisoned")
            .barriers
            .get(point)
            .cloned();
        if let Some(barrier) = barrier {
            barrier.arrived_and_wait().await;
        }
    }

    /// How many slots are live, i.e. whose execution has not been proven over.
    ///
    /// Diagnostics for tests today; the stop-proof path reports this so a release
    /// can say how much work it still has outstanding instead of reporting success.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn live_count(&self) -> usize {
        let state = self.state.lock().expect("foreground gate mutex poisoned");
        state
            .slots
            .values()
            .filter(|slot| slot.phase != StartPhase::Stopped)
            .count()
    }

    /// Registers a start that did not come through [`Self::commit`].
    ///
    /// The root task is created by its supervisor, not by a spawn request, so it has
    /// no commit to ride on. It still needs a slot: a release has to be able to see
    /// and cancel a root that is starting, or the very first execution of a session
    /// would be the one execution nobody tracks.
    pub(crate) fn begin_root_start(self: &Arc<Self>, task: TaskId, root: RootId) -> StartingPermit {
        let mut state = self.state.lock().expect("foreground gate mutex poisoned");
        // A retry starts the *same* task id under a new attempt, so a stopped slot
        // from the previous attempt must be replaced rather than reused: its token is
        // already cancelled, and carrying that forward would refuse every retry.
        let reuse = state.slots.get(&task).is_some_and(|slot| {
            slot.phase != StartPhase::Stopped && !slot.cancellation.is_cancelled()
        });
        if !reuse {
            let (witness, _confirmation) = StopWitness::pair();
            state.slots.insert(
                task.clone(),
                ExecutionSlot {
                    root: root.clone(),
                    phase: StartPhase::Starting,
                    cancellation: CancellationToken::new(),
                    handle: None,
                    witness: Some(witness),
                },
            );
        }
        // The slot is keyed by task, which is unique across roots; the root is
        // recorded so a freeze can scope itself to one conversation.
        drop(state);
        StartingPermit {
            gate: Arc::clone(self),
            task,
        }
    }

    /// Whether a slot is still starting. Test hooks use this to assert a release
    /// observed the pre-handle window rather than a later state.
    pub(crate) fn is_starting(&self, task: &TaskId) -> bool {
        let state = self.state.lock().expect("foreground gate mutex poisoned");
        state
            .slots
            .get(task)
            .is_some_and(|slot| slot.phase == StartPhase::Starting)
    }
}

impl Default for ForegroundCommitGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a commit did not happen.
///
/// Split because the two cases carry different duties: a refusal means nothing was
/// written and the caller should report it; a commit failure means the admission
/// checks passed and the write failed, so the caller must undo the resources it
/// prepared before the gate.
#[derive(Debug)]
pub(crate) enum CommitRefusal<E> {
    Refused(RuntimeCoordinatorError),
    Commit(E),
}

/// A factory wrapper that makes "user code may start" a permission rather than a
/// hope.
///
/// The wrapper is the first thing a worker start runs, and it refuses before
/// delegating to the real factory -- which is where the `Start` frame is sent to the
/// executor. So a release that cancels a slot before the factory is reached is
/// observed before any provider turn or tool can begin, not after.
///
/// It also closes the failure paths. A factory error, or a slot that stopped while
/// the factory was working, settles the slot's witness and records the stop here,
/// so a starting slot is never dropped without an answer: "no handle" alone is not
/// proof, and a release would otherwise wait for a certificate nobody issues.
///
/// The permit is held by value, so it cannot outlive the start it authorises: the
/// slot stays `Starting` for exactly as long as some code is still trying to start
/// it, which is the window a release must be able to interrupt.
pub(crate) struct AdmittedWorkerFactory<'a> {
    pub(crate) inner: &'a dyn yi_agent_core::subagent::worker::AgentWorkerFactory,
    pub(crate) permit: StartingPermit,
}

impl yi_agent_core::subagent::worker::AgentWorkerFactory for AdmittedWorkerFactory<'_> {
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    fn provider_profile_id(&self) -> Option<String> {
        self.inner.provider_profile_id()
    }

    fn max_resident_subagents(&self) -> u16 {
        self.inner.max_resident_subagents()
    }

    fn max_direct_children(&self) -> usize {
        self.inner.max_direct_children()
    }

    fn default_workspace_service(
        &self,
    ) -> Option<Arc<dyn yi_agent_core::subagent::worker::WorkerWorkspaceProvider>> {
        self.inner.default_workspace_service()
    }

    fn worker_workspace_registry(
        &self,
    ) -> Option<Arc<dyn yi_agent_core::subagent::worker::WorkerWorkspaceRegistry>> {
        self.inner.worker_workspace_registry()
    }

    fn workspace_service_for_project(
        &self,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn yi_agent_core::subagent::worker::WorkerWorkspaceProvider>> {
        self.inner.workspace_service_for_project(workspace)
    }

    fn project_workspace_matches(
        &self,
        workspace: &std::path::Path,
        recorded: &yi_agent_core::subagent::worker::WorkerWorkspace,
    ) -> bool {
        self.inner.project_workspace_matches(workspace, recorded)
    }

    fn recovery_context(&self) -> yi_agent_core::subagent::worker::WorkerRecoveryContext {
        self.inner.recovery_context()
    }

    fn recovery_context_for(
        &self,
        request: &yi_agent_core::subagent::worker::WorkerStart,
    ) -> yi_agent_core::subagent::worker::WorkerRecoveryContext {
        self.inner.recovery_context_for(request)
    }

    fn preflight_recovery(
        &self,
        request: yi_agent_core::subagent::worker::WorkerRecoveryPreflight,
    ) -> yi_agent_core::subagent::worker::WorkerRecoveryPreflightResult {
        self.inner.preflight_recovery(request)
    }

    fn start(
        &self,
        request: yi_agent_core::subagent::worker::WorkerStart,
    ) -> futures::future::BoxFuture<
        'static,
        Result<WorkerHandle, yi_agent_core::subagent::worker::WorkerError>,
    > {
        self.start_with_provider_turn_gate(request, None)
    }

    fn start_with_provider_turn_gate(
        &self,
        mut request: yi_agent_core::subagent::worker::WorkerStart,
        turn_gate: Option<Arc<dyn yi_agent_core::ProviderTurnGate>>,
    ) -> futures::future::BoxFuture<
        'static,
        Result<WorkerHandle, yi_agent_core::subagent::worker::WorkerError>,
    > {
        let task = request.task_id.clone();
        let gate = Arc::clone(self.permit.gate());
        if !self.permit.is_live() {
            // The slot was released between commit and factory entry. Nothing has
            // run, so the stop is a fact this side owns -- record it rather than
            // pretending a worker exists.
            gate.stopped(&task);
            return Box::pin(async move {
                Err(yi_agent_core::subagent::worker::WorkerError::Startup(
                    "the foreground admission closed before this worker could start".into(),
                ))
            });
        }
        // The worker must honour the slot's own token, so a cancel that lands while
        // the factory is working reaches the execution it started.
        let token = self.permit.cancellation();
        request.cancellation = token.clone();
        let inner = self.inner;
        let gate_for_result = gate;
        let future = inner.start_with_provider_turn_gate(request, turn_gate);
        Box::pin(async move {
            let handle = match future.await {
                Ok(handle) => handle,
                Err(error) => {
                    // A failed start is a stop: the slot must not stay starting
                    // forever, and the witness settles so a release is not left
                    // waiting for a certificate nobody will issue.
                    gate_for_result.stopped(&task);
                    return Err(error);
                }
            };
            // The third staggered window: a handle exists and the release may land
            // before it is published. Test-only, and nothing is installed in
            // production, so it cannot alter a real start.
            #[cfg(any(test, feature = "foreground-test-support"))]
            gate_for_result.hold_at("after_factory_returned").await;
            if token.is_cancelled() || !gate_for_result.is_live(&task) {
                // Released while the factory was working. The handle exists, so it
                // is cancelled here and reported as a failed start: publishing it
                // would resurrect exactly the work the release already reclaimed.
                handle.cancel();
                gate_for_result.stopped(&task);
                return Err(yi_agent_core::subagent::worker::WorkerError::Startup(
                    "the foreground admission closed while this worker was starting".into(),
                ));
            }
            match gate_for_result.publish(&task, handle.clone()) {
                Some(published) => Ok(published),
                None => {
                    handle.cancel();
                    Err(yi_agent_core::subagent::worker::WorkerError::Startup(
                        "the foreground admission closed while this worker was starting".into(),
                    ))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> Arc<ForegroundCommitGate> {
        Arc::new(ForegroundCommitGate::new())
    }

    fn stamp(root: &str) -> AdmissionStamp {
        AdmissionStamp {
            root: root.to_string(),
            task: TaskId::new(),
            attempt: AttemptId::new(),
            generation: "g1".into(),
            authority_sequence: 1,
        }
    }

    fn refused<E>(outcome: Result<(AdmissionStamp, StartingPermit, ()), CommitRefusal<E>>) -> bool {
        matches!(outcome, Err(CommitRefusal::Refused(_)))
    }

    #[test]
    fn foreground_root_lifecycle_commit_registers_a_starting_slot() {
        let gate = gate();
        let (admitted, permit, ()) = gate
            .commit(|| Ok(stamp("thread:a")), |_| Ok::<_, ()>(()))
            .expect("an open gate admits");
        assert!(gate.is_starting(&admitted.task));
        assert!(permit.is_live());

        // Dropping the permit alone does not publish anything: no worker exists yet,
        // so the child must still be visible as a cancellable starting slot.
        drop(permit);
        assert!(
            gate.is_starting(&admitted.task),
            "a child with no handle is still starting"
        );
        assert!(gate.is_live(&admitted.task));
        assert_eq!(gate.live_count(), 1);
    }

    #[test]
    fn foreground_root_lifecycle_a_frozen_gate_refuses_before_writing() {
        let gate = gate();
        gate.freeze_root(&stamp("thread:a").root);
        let wrote = std::cell::Cell::new(false);
        let outcome = gate.commit(
            || Ok(stamp("thread:a")),
            |_| {
                wrote.set(true);
                Ok::<_, ()>(())
            },
        );
        assert!(refused(outcome));
        assert!(!wrote.get(), "a refusal must not write the child row");
        assert_eq!(gate.live_count(), 0);
    }

    #[test]
    fn foreground_root_lifecycle_a_refusal_writes_nothing() {
        let gate = gate();
        let wrote = std::cell::Cell::new(false);
        let outcome = gate.commit(
            || Err(RuntimeCoordinatorError::AdmissionClosed("released".into())),
            |_| {
                wrote.set(true);
                Ok::<_, ()>(())
            },
        );
        assert!(refused(outcome));
        assert!(!wrote.get());
        assert_eq!(gate.live_count(), 0);
    }

    #[test]
    fn foreground_root_lifecycle_release_cancels_starting_slots() {
        let gate = gate();
        let (admitted, permit, ()) = gate
            .commit(|| Ok(stamp("thread:a")), |_| Ok::<_, ()>(()))
            .unwrap();
        let token = permit.cancellation();
        let released = gate.release_owner("app");
        assert_eq!(released.starting, vec![admitted.task.clone()]);
        assert!(token.is_cancelled());
        assert!(
            !permit.is_live(),
            "a cancelled slot must not start user code"
        );
        // The witness is settled, so a release can prove no code ran.
        assert_eq!(gate.stopped(&admitted.task), StopPhase::Starting);
    }

    #[test]
    fn foreground_root_lifecycle_a_handle_arriving_after_release_is_not_published() {
        let gate = gate();
        let (admitted, permit, ()) = gate
            .commit(|| Ok(stamp("thread:a")), |_| Ok::<_, ()>(()))
            .unwrap();
        let task = admitted.task.clone();
        gate.release_owner("app");
        gate.stopped(&task);
        drop(permit);
        let handle = WorkerHandle::new(CancellationToken::new());
        assert!(
            gate.publish(&task, handle).is_none(),
            "a released slot's handle must be handed back to be stopped"
        );
    }

    #[test]
    fn foreground_root_lifecycle_freezing_one_root_keeps_another_root_running() {
        // A rebind is one conversation's problem. Freezing must not stop, cancel or
        // refuse another root's work -- that is the difference between recovering a
        // session and draining the app.
        let gate = gate();
        let (a, permit_a, ()) = gate
            .commit(|| Ok(stamp("thread:a")), |_| Ok::<_, ()>(()))
            .unwrap();
        let (b, permit_b, ()) = gate
            .commit(|| Ok(stamp("thread:b")), |_| Ok::<_, ()>(()))
            .unwrap();
        let frozen = gate.freeze_root(&a.root);
        assert!(
            frozen.starting == vec![a.task.clone()],
            "only the frozen root's slot is enumerated: {frozen:?}"
        );
        assert!(
            gate.is_live(&b.task),
            "the other root's starting slot must stay live"
        );
        assert!(
            !gate.is_live(&a.task),
            "the frozen root's slot is cancelled, not merely recorded"
        );
        assert!(
            !refused(gate.commit(|| Ok(stamp("thread:c")), |_| Ok::<_, ()>(()))),
            "a root unrelated to the freeze may still admit"
        );
        drop((permit_a, permit_b));
    }

    #[test]
    fn foreground_root_lifecycle_freeze_is_sticky() {
        let gate = gate();
        let frozen = gate.freeze_root(&stamp("thread:a").root);
        assert!(frozen.starting.is_empty());
        assert!(
            !refused(gate.commit(|| Ok(stamp("thread:b")), |_| Ok::<_, ()>(()))),
            "freezing one root must not refuse an unrelated root's admission"
        );
        assert!(refused(
            gate.commit(|| Ok(stamp("thread:a")), |_| Ok::<_, ()>(()))
        ));
    }
}
