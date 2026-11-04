//! A fixture for the foreground root lifecycle tests.
//!
//! It runs the *real* coordinator paths: `spawn_child_with_objective`,
//! `close_client_admission`, `release_owner_and_stop`, the commit gate and the worker
//! factory wrapper. Nothing here re-implements an admission rule. A helper that
//! re-checked admission itself would pass its own tests while the production path
//! stayed broken, which is the failure this whole task exists to prevent.
//!
//! This module is compiled into several test binaries and each uses a different
//! subset, so helpers that one binary does not touch are allowed to be dead there.
//!
//! The two barriers are the fixture's reason to exist. A race cannot be produced by
//! sleeping, so the tests name the exact window they mean to land in and the
//! production code reports when it is there:
//!
//! * `hold_before_admission_commit` -- the child row is not written yet, so a release
//!   must make the spawn fail.
//! * `hold_after_admission_commit` -- the child exists and the factory has not been
//!   called, so a release must cancel it and leave nothing running.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use yi_agent_core::subagent::task::AttemptId;

use futures::future::BoxFuture;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yi_agent_core::RootSessionId;
use yi_agent_core::subagent::task::TaskId;
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, StopWitness, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_store::foreground::TestBarrier;
use yi_agent_store::runtime::RuntimeCoordinator;
use yi_agent_store::runtime::RuntimeCoordinatorError;

/// How long any single cross-task wait may take before the test fails instead of
/// hanging the suite.
const WAIT: Duration = Duration::from_secs(5);

/// A factory that records every start and can be made to park inside one.
///
/// It is deliberately the *outer* factory: the fixture's barrier sits in the
/// production admission path, not here. This one only answers "did a worker ever
/// start", which is what `assert_no_started_workers` asks.
#[derive(Default)]
pub struct RecordingFactory {
    started: Mutex<Vec<TaskId>>,
}

impl RecordingFactory {
    pub fn started(&self) -> Vec<TaskId> {
        self.started.lock().unwrap().clone()
    }
}

fn joined_handle(cancellation: CancellationToken) -> WorkerHandle {
    // The factory runs inline, so there is no thread to join. Saying so explicitly
    // matters: the production default is "no evidence", so a fake worker that never
    // proves a stop would make every release look unproven.
    let (witness, confirmation) = StopWitness::pair();
    witness.mark_joined();
    WorkerHandle::new(cancellation).with_confirmation(confirmation)
}

/// The recovery evidence the fixture's workers report.
///
/// The runtime refuses a `running` transition without it, and `default()` is a
/// refusal, not an empty-but-valid answer. The fixture's children are read-only,
/// which is the checkpoint kind that needs no worktree lease.
fn read_only_recovery_context() -> WorkerRecoveryContext {
    WorkerRecoveryContext {
        workspace_lease_id: None,
        worktree_lease: None,
        checkpoint_json: r#"{"kind":"read_only_checkpoint"}"#.into(),
        tool_state_json: "{}".into(),
    }
}

/// The context for one concrete start.
///
/// The runtime reads the lease from this answer, so a context that ignored the
/// request would be refused as "workspace lease is missing" even though the position
/// was resolved. The honest context names the workspace this start was given.
fn recovery_context_for_start(request: &WorkerStart) -> WorkerRecoveryContext {
    let mut context = read_only_recovery_context();
    context.workspace_lease_id = request
        .workspace
        .as_ref()
        .map(|workspace| format!("workspace:{}", workspace.lease_id));
    context
}

impl AgentWorkerFactory for RecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        read_only_recovery_context()
    }

    fn recovery_context_for(&self, request: &WorkerStart) -> WorkerRecoveryContext {
        recovery_context_for_start(request)
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.started.lock().unwrap().push(request.task_id.clone());
        Box::pin(async move { Ok(joined_handle(request.cancellation)) })
    }
}

/// The workspace service the runtime consults before it asks a factory for a worker.
///
/// The fixture spawns read-only children, and the runtime refuses a read-only task
/// when the provider cannot resolve a position -- so "no workspace" here would fail
/// before the commit gate, and the tests would assert about the wrong layer. A
/// read-only child runs in place, so one fixed directory is the honest answer.
struct InPlaceWorkspaces {
    path: PathBuf,
}

impl InPlaceWorkspaces {
    fn position(&self) -> WorkerWorkspace {
        WorkerWorkspace {
            lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
            repository_root: self.path.clone(),
            path: self.path.clone(),
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        }
    }
}

impl WorkerWorkspaceProvider for InPlaceWorkspaces {
    fn in_place_workspace(
        &self,
        _session: &RootSessionId,
        _task_id: &TaskId,
        _attempt: &yi_agent_core::subagent::task::AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.position())
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup(
            "this fixture spawns read-only children only".into(),
        ))
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        // A read-only child inherits its parent's position; a root has none, so it
        // gets the fixture's single in-place directory.
        Ok(parent.cloned().unwrap_or_else(|| self.position()))
    }
}

impl AgentWorkerFactory for RecordingFactoryWithWorkspaces {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        read_only_recovery_context()
    }

    fn recovery_context_for(&self, request: &WorkerStart) -> WorkerRecoveryContext {
        // The coordinator asks *this* factory, not the inner one, so the wrapper must
        // answer with the same request-aware context or the lease read is lost.
        recovery_context_for_start(request)
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::clone(&self.workspaces))
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.inner
            .started
            .lock()
            .unwrap()
            .push(request.task_id.clone());
        Box::pin(async move { Ok(joined_handle(request.cancellation)) })
    }
}

struct RecordingFactoryWithWorkspaces {
    inner: Arc<RecordingFactory>,
    workspaces: Arc<dyn WorkerWorkspaceProvider>,
}

/// A live coordinator plus the session and root a child can be spawned under.
pub struct ForegroundFixture {
    /// Must outlive the coordinator's threads: dropping it first would delete the
    /// database under them.
    _directory: TempDir,
    coordinator: Arc<RuntimeCoordinator>,
    session: RootSessionId,
    root: TaskId,
    factory: Arc<RecordingFactory>,
}

impl ForegroundFixture {
    /// Builds a database-backed coordinator with owner `app` bound to its root.
    pub async fn managed() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let factory = Arc::new(RecordingFactory::default());
        let coordinator = RuntimeCoordinator::open(
            directory.path().join("runtime.sqlite"),
            Arc::new(RecordingFactoryWithWorkspaces {
                inner: Arc::clone(&factory),
                workspaces: Arc::new(InPlaceWorkspaces {
                    path: directory.path().to_path_buf(),
                }),
            }),
        )
        .expect("the fixture's database opens");
        let session = coordinator
            .create_session()
            .expect("a session can be created");
        let root = coordinator
            .root_task_id(&session)
            .expect("a session has a root task");
        coordinator
            .bind_root("app", &root.to_string())
            .expect("the fixture's owner binds");
        Self {
            _directory: directory,
            coordinator: Arc::new(coordinator),
            session,
            root,
            factory,
        }
    }

    pub fn coordinator(&self) -> &Arc<RuntimeCoordinator> {
        &self.coordinator
    }

    pub fn root(&self) -> &TaskId {
        &self.root
    }

    pub fn session(&self) -> RootSessionId {
        self.session.clone()
    }

    /// Spawns through the production path, including the commit gate and the worker
    /// start. A refusal is returned rather than unwrapped, because the refusals are
    /// what half these tests assert.
    pub async fn spawn(&self, objective: &str) -> Result<TaskId, RuntimeCoordinatorError> {
        self.coordinator
            .spawn_child_and_admit(
                &self.session,
                &self.root,
                objective.to_string(),
                yi_agent_core::subagent::task::ChildWriteMode::ReadOnly,
                None,
                None,
                None,
                None,
                None,
            )
            .await
    }

    /// Releases the owner the way a frontend going away does: admission closes, then
    /// the registered work is stopped.
    pub async fn release_owner(&self) -> Result<(), RuntimeCoordinatorError> {
        self.coordinator
            .release_owner_and_stop("app")
            .await
            .map(|_| ())
    }

    /// Waits until the coordinator reports the owner's admission closed.
    ///
    /// It polls the real state rather than assuming the call returned: a release may
    /// still be waiting on a starting slot, and reporting "closed" early is exactly
    /// the mistake the plan calls out.
    pub async fn wait_owner_admission_closed(&self) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while tokio::time::Instant::now() < deadline {
            if !self
                .coordinator
                .authorize_work("app", &self.root.to_string())
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("the owner's admission never closed");
    }

    /// Parks the production path before the child row is written.
    pub fn hold_before_admission_commit(&self) -> BarrierPair {
        self.install("before_admission_commit")
    }

    /// Parks the production path after the child row and its slot exist, before a
    /// factory is asked for a handle.
    pub fn hold_after_admission_commit(&self) -> BarrierPair {
        self.install("after_admission_commit")
    }

    /// Parks the production path inside the factory wrapper, after a handle exists
    /// but before it is published.
    ///
    /// This is the third staggered window the plan names: the release lands when the
    /// worker is real, so the cancel must reach the handle rather than a token nobody
    /// observes.
    pub fn hold_after_factory_returned(&self) -> BarrierPair {
        self.install("after_factory_returned")
    }

    fn install(&self, point: &'static str) -> BarrierPair {
        let pair = BarrierPair::new();
        self.coordinator
            .install_test_barrier(point, Arc::clone(&pair.code_side));
        pair
    }

    /// Every child the coordinator persisted for this root.
    pub fn child_ids(&self) -> Vec<TaskId> {
        self.coordinator
            .children_of_root(&self.session)
            .into_iter()
            .filter(|task| task != &self.root)
            .collect()
    }

    /// Asserts every child is attributable to the owner, so reclamation can find it.
    pub fn assert_all_owned(&self) {
        let owned: HashSet<String> = self.coordinator.owned_roots_of("app").into_iter().collect();
        for child in self.child_ids() {
            assert!(
                owned.contains(&child.to_string()),
                "child {child} is not attributable to the owner"
            );
        }
    }

    /// Makes this fixture's root `pending`: authenticated-in-progress, so it may not
    /// dispatch yet. Written through the same repository method the prepare path uses,
    /// so the gate reads a real row rather than a stubbed answer.
    pub fn mark_root_pending(&self) -> Result<(), RuntimeCoordinatorError> {
        self.coordinator.record_foreground_class_for_test(
            &self.root,
            yi_agent_store::repository::foreground::ForegroundClassification::Pending,
        )
    }

    /// Makes this fixture's root `corrupt` by declaring it legacy without the
    /// attachment the declaration requires, then removing that attachment.
    ///
    /// The contradiction is built the way the production reader would meet it, so the
    /// test asserts the reader's verdict rather than a hand-set flag.
    pub fn mark_root_corrupt(&self) -> Result<(), RuntimeCoordinatorError> {
        self.coordinator.record_foreground_class_for_test(
            &self.root,
            yi_agent_store::repository::foreground::ForegroundClassification::Legacy,
        )?;
        let repository = self
            .coordinator
            .repository_for_test()
            .expect("the fixture's repository lock is free");
        repository
            .execute_test_statement(
                "DELETE FROM application_root_attachments WHERE root_task_id = ?1",
                &self.root.to_string(),
            )
            .expect("the fixture's attachment row is removable");
        Ok(())
    }

    /// The verdict the spawn commit's guard reaches for this fixture's session.
    pub fn root_dispatch_refusal(&self) -> Result<(), RuntimeCoordinatorError> {
        self.coordinator
            .check_root_dispatchable_for_test(&self.session)
    }

    /// Binds a second owner to a separate session's root, so a test can prove one
    /// owner's release leaves the other's tree alone.
    pub fn bind_second_owner(
        &self,
        owner: &str,
    ) -> Result<(RootSessionId, TaskId), RuntimeCoordinatorError> {
        let session = self.coordinator.create_session()?;
        let root = self.coordinator.root_task_id(&session)?;
        self.coordinator.bind_root(owner, &root.to_string())?;
        Ok((session, root))
    }

    /// Spawns under a root the caller names, for the second-owner tests.
    pub async fn spawn_under(
        &self,
        session: &RootSessionId,
        root: &TaskId,
        objective: &str,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        self.coordinator
            .spawn_child_and_admit(
                session,
                root,
                objective.to_string(),
                yi_agent_core::subagent::task::ChildWriteMode::ReadOnly,
                None,
                None,
                None,
                None,
                None,
            )
            .await
    }

    /// The root's TASK id, which is what ownership and watchdogs are keyed by.
    pub fn root_task(&self) -> TaskId {
        self.root.clone()
    }

    /// The active attempt of a task, read from the live supervisor.
    pub fn active_attempt(&self, task: &TaskId) -> AttemptId {
        self.coordinator
            .active_attempt_for_test(task)
            .expect("the task has an active attempt")
    }

    /// Gives a task the default watchdog budgets and no meaningful progress yet.
    ///
    /// The clock is the caller's: `advance_watchdog` moves the *observation* the
    /// watchdog compares against, which is the only way to test a 2700-second budget
    /// without waiting 45 minutes. Real leases and heartbeats are untouched, because a
    /// test that froze the lease would prove nothing about an owner staying alive.
    pub fn seed_watchdog(&self, task: &TaskId, last_meaningful_at: DateTime<Utc>) {
        let attempt = self.active_attempt(task);
        self.coordinator
            .repository_for_test()
            .expect("the fixture's repository lock is free")
            .save_attempt_watchdog_snapshot(
                task,
                &attempt,
                &yi_agent_store::schedule::WatchdogLimits::default(),
                &yi_agent_store::schedule::WatchdogUsage::default(),
                None,
                last_meaningful_at,
                None,
            )
            .expect("the fixture can seed a watchdog snapshot");
    }

    /// Runs one watchdog scan as of `now`.
    pub async fn advance_watchdog_to(&self, now: DateTime<Utc>) -> usize {
        self.coordinator
            .evaluate_watchdogs(now)
            .await
            .expect("a watchdog scan does not fail")
    }

    /// The root's state before any watchdog scan, so a test can assert "unchanged"
    /// without assuming which non-terminal state it happened to be in.
    pub fn initial_root_state(&self) -> String {
        self.root_state()
    }

    /// The root's durable task state.
    pub fn root_state(&self) -> String {
        self.coordinator
            .task_state(&self.root)
            .expect("the root has a state")
    }

    /// Whether the root's active attempt carries a watchdog terminal.
    pub fn root_has_watchdog_terminal(&self) -> bool {
        let attempt = self.active_attempt(&self.root);
        self.coordinator
            .repository_for_test()
            .expect("the fixture's repository lock is free")
            .attempt_has_terminal(&attempt)
            .expect("the attempt row is readable")
    }

    /// How many meaningful-progress events the root's attempt recorded.
    pub fn root_meaningful_progress_count(&self) -> usize {
        let attempt = self.active_attempt(&self.root);
        self.coordinator
            .repository_for_test()
            .expect("the fixture's repository lock is free")
            .meaningful_progress_count(&attempt)
            .expect("the attempt row is readable")
    }

    /// Every child the coordinator registered for this root, root excluded.
    pub async fn wait_children(&self) -> Vec<TaskId> {
        self.child_ids()
    }

    /// Every task the factory was asked to start, in order.
    pub fn started_workers(&self) -> Vec<TaskId> {
        self.factory.started()
    }

    /// Whether every registered execution is proven over.
    pub fn no_live_executions(&self) -> bool {
        self.coordinator.live_execution_count() == 0
    }

    /// Asserts the factory was never asked for a worker.
    pub fn assert_no_started_workers(&self) {
        assert!(
            self.factory.started().is_empty(),
            "no worker may start for work the release cancelled, started: {:?}",
            self.factory.started()
        );
    }
}

/// The two ends of one barrier: the coordinator parks on `code_side`, the test
/// waits and releases through `test_side`.
///
/// Both are the same object, which is why `release` consumes the test side by value:
/// releasing twice would be a second wakeup with no waiter, and a test that did it
/// would be asserting against a barrier it already opened.
pub struct BarrierPair {
    pub code_side: Arc<TestBarrier>,
    pub test_side: Arc<TestBarrier>,
}

impl BarrierPair {
    fn new() -> Self {
        let barrier = Arc::new(TestBarrier::new());
        Self {
            code_side: Arc::clone(&barrier),
            test_side: barrier,
        }
    }

    /// Waits for the production path to reach the barrier's point.
    pub async fn arrived(&mut self) {
        // `arrived` needs `&mut self` on the underlying barrier, which an `Arc`
        // cannot give. The barrier's own arrival channel is one-shot, so a shared
        // handle is enough: take it once through `Arc::get_mut` is not possible here,
        // so the barrier exposes a shared wait instead.
        self.test_side.wait_arrival().await;
    }

    /// Lets the parked production path continue.
    pub fn release(self) {
        self.test_side.release_shared();
    }
}
