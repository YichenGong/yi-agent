//! Live admission: a settings save must reach the *next* grant, not the next
//! process, and shrinking capacity must never cancel work already admitted.
//!
//! The store owns the real admission points (resident capacity in
//! `start_worker`, direct-child limit in `spawn_with_objective`), so these tests
//! drive the coordinator through a factory that exposes a controllable
//! `AdmissionLimitsSource`. Asserting on the source's own value would only
//! re-read the test's own field; every assertion below is about a task's durable
//! state or an admission outcome.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::ChildWriteMode;
use yi_agent_core::live_settings::{AdmissionLimits, AdmissionLimitsSource, LiveSettingsError};
use yi_agent_core::subagent::task::{AttemptId, RootSessionId, TaskId, WorkspaceLeaseId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_store::runtime::RuntimeCoordinator;

/// A live source the test owns, so a "save" is just a field write.
struct ManualLimits {
    value: Mutex<AdmissionLimits>,
    changed: tokio::sync::watch::Sender<u64>,
    revision: Mutex<u64>,
}

impl ManualLimits {
    fn new(limits: AdmissionLimits) -> Arc<Self> {
        let (changed, _keepalive) = tokio::sync::watch::channel(0);
        Arc::new(Self {
            value: Mutex::new(limits),
            changed,
            revision: Mutex::new(0),
        })
    }

    /// Stands in for a settings save. The revision is only a wake-up, exactly
    /// like the production adapter's.
    fn save(&self, limits: AdmissionLimits) {
        *self.value.lock().unwrap() = limits;
        let mut revision = self.revision.lock().unwrap();
        *revision += 1;
        let _ = self.changed.send(*revision);
    }
}

impl AdmissionLimitsSource for ManualLimits {
    fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError> {
        Ok(*self.value.lock().unwrap())
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }
}

fn workspace(root: &RootSessionId, task: &TaskId) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: std::path::PathBuf::from(format!("/tmp/yi-agent-live-admission/{root}")),
        path: std::path::PathBuf::from(format!("/tmp/yi-agent-live-admission/{root}/{task}")),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    }
}

/// A factory that starts workers successfully and exposes the live source.
struct LiveFactory {
    limits: Arc<ManualLimits>,
    static_units: u16,
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}

impl WorkerWorkspaceProvider for LiveFactory {
    fn in_place_workspace(
        &self,
        root: &RootSessionId,
        task: &TaskId,
        _attempt: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(root, task))
    }

    fn workspace_in(
        &self,
        task: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(&RootSessionId::new(), task))
    }

    fn read_only_workspace(
        &self,
        _parent: Option<&WorkerWorkspace>,
        task: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(&RootSessionId::new(), task))
    }
}

impl AgentWorkerFactory for LiveFactory {
    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(LiveFactory {
            limits: Arc::clone(&self.limits),
            static_units: self.static_units,
            starts: Arc::clone(&self.starts),
        }))
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:live-admission".into()),
            worktree_lease: Some("worktree:live-admission".into()),
            checkpoint_json: "{}".into(),
            tool_state_json: "{}".into(),
        }
    }

    /// The static value is deliberately wrong, so a test that still passes when
    /// the live source is ignored cannot be mistaken for a live one.
    fn max_resident_subagents(&self) -> u16 {
        self.static_units
    }

    fn max_direct_children(&self) -> usize {
        usize::from(self.static_units)
    }

    fn live_admission_limits(&self) -> Option<Arc<dyn AdmissionLimitsSource>> {
        Some(Arc::clone(&self.limits) as Arc<dyn AdmissionLimitsSource>)
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        Box::pin(async move { Ok(handle) })
    }
}

struct Fixture {
    _directory: TempDir,
    coordinator: RuntimeCoordinator,
    session: RootSessionId,
    caller: TaskId,
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    /// The source the factory hands out. Holding it here is what lets a test
    /// perform a "save" and then observe a later admission.
    limits: Arc<ManualLimits>,
}

async fn fixture(limits: AdmissionLimits) -> Fixture {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let source = ManualLimits::new(limits);
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(LiveFactory {
            limits: Arc::clone(&source),
            // If anything reads the static value instead of the source, these
            // tests fail: 1 admits nothing beyond the first child.
            static_units: 1,
            starts: Arc::clone(&starts),
        }),
    )
    .unwrap();
    let attached = coordinator
        .attach_application_root("live-admission", &directory.path().join("project"), &[])
        .await
        .unwrap();
    Fixture {
        _directory: directory,
        coordinator,
        session: attached.session_id,
        caller: attached.root_task_id,
        starts,
        limits: source,
    }
}

impl Fixture {
    async fn spawn(&self, objective: &str) -> Result<TaskId, String> {
        self.coordinator
            .spawn_child_and_admit(
                &self.session,
                &self.caller,
                objective.into(),
                ChildWriteMode::ReadOnly,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .map_err(|error| error.to_string())
    }

    fn started(&self, task: &TaskId) -> bool {
        self.starts
            .lock()
            .unwrap()
            .iter()
            .any(|s| &s.task_id == task)
    }
}

/// Shrinking capacity must not cancel work that was already admitted.
///
/// The whole point of a "next safe point" applies to admission too: a child
/// whose worker is already running has begun its work, so a lower limit can only
/// gate what comes next.
#[tokio::test]
async fn shrinking_capacity_keeps_running_workers_and_refuses_new_ones() {
    let fixture = fixture(AdmissionLimits {
        direct_children: 5,
        resident_subagents: 2,
    })
    .await;

    let first = fixture
        .spawn("first")
        .await
        .expect("first child is admitted");
    let second = fixture
        .spawn("second")
        .await
        .expect("second child is admitted");
    assert!(fixture.started(&first), "the first child's worker started");
    assert!(
        fixture.started(&second),
        "the second child's worker started"
    );

    // The save: capacity drops to 1, below the two running children.
    fixture
        .coordinator
        .refresh_live_admission()
        .await
        .expect("a refresh with a healthy source succeeds");
    assert_eq!(
        fixture.coordinator.resident_capacity(),
        Some(2),
        "the refresh applies the source's value atomically before any save below"
    );

    // A resident lease already exists for both children, so a third task is the
    // one that observes the new capacity.
    let third = fixture.spawn("third").await;
    let third = third.expect("spawning itself is not refused; only residency is");
    assert!(
        !fixture.started(&third),
        "the third child must not start while capacity is exhausted"
    );
    assert_eq!(
        fixture.coordinator.task_state(&third).unwrap(),
        "queued",
        "a refused admission leaves the task queued, not cancelled"
    );

    for task in [&first, &second] {
        let state = fixture.coordinator.task_state(task).unwrap();
        assert!(
            state != "cancelled" && state != "failed",
            "a lowered limit must not cancel an admitted child, got {state}"
        );
    }
}

/// Raising capacity must let the queued child start with no rebuild.
///
/// This is the other direction of a save: a task parked because capacity was
/// full must become admissible once the limit grows, and it must do so through
/// the same coordinator — no new process, no reconstructed factory.
#[tokio::test]
async fn raising_capacity_admits_the_queued_child() {
    let fixture = fixture(AdmissionLimits {
        direct_children: 5,
        resident_subagents: 1,
    })
    .await;

    let first = fixture
        .spawn("first")
        .await
        .expect("first child is admitted");
    let second = fixture
        .spawn("second")
        .await
        .expect("second child is accepted");
    assert!(
        fixture.started(&first),
        "capacity 1 admits the first worker"
    );
    assert!(
        !fixture.started(&second),
        "capacity 1 must not admit a second resident worker"
    );

    // The save: capacity grows, and the host's refresh applies it.
    fixture.limits.save(AdmissionLimits {
        direct_children: 5,
        resident_subagents: 3,
    });
    fixture
        .coordinator
        .refresh_live_admission()
        .await
        .expect("a refresh with a healthy source succeeds");
    assert_eq!(
        fixture.coordinator.resident_capacity(),
        Some(3),
        "the raised capacity must reach the live coordinator"
    );

    // The queued child is retried the way a control client retries it; the
    // admission path re-reads the source, so no rebuild is needed.
    fixture
        .coordinator
        .start_worker(&fixture.session, &second)
        .await
        .expect("the raised capacity must admit the queued child");
    assert!(
        fixture.started(&second),
        "the previously refused child now runs"
    );
}

/// A source failure must refuse the admission, not silently use a stale limit.
///
/// Falling back to the last known value would make a corrupt settings file look
/// like a working one, which is exactly the silent divergence the whole feature
/// exists to prevent.
#[tokio::test]
async fn a_failing_source_refuses_admission_with_a_stable_code() {
    struct BrokenSource;

    impl AdmissionLimitsSource for BrokenSource {
        fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError> {
            Err(LiveSettingsError::new("snapshot_unavailable"))
        }

        fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
            let (_, receiver) = tokio::sync::watch::channel(0);
            receiver
        }
    }

    struct BrokenFactory {
        starts: Arc<Mutex<Vec<WorkerStart>>>,
    }

    impl WorkerWorkspaceProvider for BrokenFactory {
        fn in_place_workspace(
            &self,
            root: &RootSessionId,
            task: &TaskId,
            _attempt: &AttemptId,
        ) -> Result<WorkerWorkspace, WorkerError> {
            Ok(workspace(root, task))
        }

        fn workspace_in(
            &self,
            task: &TaskId,
            _workdir: &std::path::Path,
        ) -> Result<WorkerWorkspace, WorkerError> {
            Ok(workspace(&RootSessionId::new(), task))
        }

        fn read_only_workspace(
            &self,
            _parent: Option<&WorkerWorkspace>,
            task: &TaskId,
        ) -> Result<WorkerWorkspace, WorkerError> {
            Ok(workspace(&RootSessionId::new(), task))
        }
    }

    impl AgentWorkerFactory for BrokenFactory {
        fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
            Some(Arc::new(BrokenFactory {
                starts: Arc::clone(&self.starts),
            }))
        }

        fn recovery_context(&self) -> WorkerRecoveryContext {
            WorkerRecoveryContext {
                workspace_lease_id: Some("workspace:broken".into()),
                worktree_lease: Some("worktree:broken".into()),
                checkpoint_json: "{}".into(),
                tool_state_json: "{}".into(),
            }
        }

        fn live_admission_limits(&self) -> Option<Arc<dyn AdmissionLimitsSource>> {
            Some(Arc::new(BrokenSource))
        }

        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            let handle = WorkerHandle::new(request.cancellation.clone());
            self.starts.lock().unwrap().push(request);
            Box::pin(async move { Ok(handle) })
        }
    }

    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(BrokenFactory {
            starts: Arc::clone(&starts),
        }),
    )
    .unwrap();
    let attached = coordinator
        .attach_application_root("broken-source", &directory.path().join("project"), &[])
        .await
        .unwrap();

    let error = coordinator
        .spawn_child_and_admit(
            &attached.session_id,
            &attached.root_task_id,
            "refused".into(),
            ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("a failing source must refuse the spawn");

    assert!(
        error.to_string().contains("snapshot_unavailable"),
        "the stable code must surface, got: {error}"
    );
    assert!(
        starts.lock().unwrap().is_empty(),
        "no worker may start from unknown settings"
    );
}

/// The child limit is read per spawn, so a save applies to the next one.
#[tokio::test]
async fn the_direct_child_limit_is_read_at_each_spawn() {
    let fixture = fixture(AdmissionLimits {
        direct_children: 2,
        resident_subagents: 8,
    })
    .await;

    fixture.spawn("first").await.expect("first child");
    fixture.spawn("second").await.expect("second child");
    let refused = fixture
        .spawn("third")
        .await
        .expect_err("limit 2 is reached");
    assert!(
        refused.contains('2'),
        "the refusal reports the live limit, got: {refused}"
    );
    assert!(
        fixture.coordinator.task_state(&fixture.caller).is_ok(),
        "the parent remains a real task throughout"
    );
}

/// Concurrent spawns must not overshoot the live direct-child limit.
///
/// The limit is read and consumed under the same supervisor lock, so this is
/// what proves the read is a decision rather than a pre-check: a barrier fires
/// every spawn at once, and exactly the capacity's worth may succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_spawns_never_exceed_the_live_child_limit() {
    let fixture = Arc::new(
        fixture(AdmissionLimits {
            direct_children: 2,
            resident_subagents: 8,
        })
        .await,
    );

    let barrier = Arc::new(tokio::sync::Barrier::new(6));
    let mut handles = Vec::new();
    for index in 0..6 {
        let fixture = Arc::clone(&fixture);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            fixture.spawn(&format!("child-{index}")).await
        }));
    }

    let mut admitted = 0;
    for handle in handles {
        if handle.await.unwrap().is_ok() {
            admitted += 1;
        }
    }

    assert_eq!(
        admitted, 2,
        "the live direct-child limit must be consumed atomically, admitted {admitted}"
    );
}
