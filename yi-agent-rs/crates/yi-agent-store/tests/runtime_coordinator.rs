use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use rusqlite::Connection;
use tempfile::TempDir;
use yi_agent_core::ProviderTurnGate;
use yi_agent_core::RootSessionId;
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryAttestation,
    WorkerRecoveryContext, WorkerRecoveryPreflight, WorkerRecoveryPreflightResult, WorkerStart,
};
use yi_agent_store::repository::{RuntimeEvent, RuntimeRepository};
use yi_agent_store::runtime::{RuntimeCoordinator, RuntimeStopOptions};

#[derive(Default)]
struct RecordingFactory;

impl AgentWorkerFactory for RecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

struct FailingFactory;

#[derive(Clone, Default)]
struct ProviderGateRecordingFactory {
    received_gate: Arc<Mutex<bool>>,
}

impl AgentWorkerFactory for ProviderGateRecordingFactory {
    fn provider_profile_id(&self) -> Option<String> {
        Some("test-profile".into())
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.received_gate.lock().unwrap() = gate.is_some();
        self.start(request)
    }
}

impl AgentWorkerFactory for FailingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move {
            let handle = WorkerHandle::new(request.cancellation);
            handle.report_failure("provider disconnected");
            Ok(handle)
        })
    }
}

#[derive(Clone, Default)]
struct MessageRecordingFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

impl AgentWorkerFactory for MessageRecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn preflight_recovery(
        &self,
        _request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        WorkerRecoveryPreflightResult::Attested(durable_attestation())
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[derive(Clone, Default)]
struct PauseRecordingFactory {
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

struct AdmissionObservingFactory {
    database: std::path::PathBuf,
}

struct RecoveryStartObservingFactory {
    database: std::path::PathBuf,
    starts: Arc<Mutex<usize>>,
}

#[derive(Default)]
struct ConflictReportingFactory {
    starts: Arc<Mutex<usize>>,
}

struct StartupErrorFactory;

fn durable_context() -> WorkerRecoveryContext {
    WorkerRecoveryContext {
        workspace_lease_id: Some("workspace:test".into()),
        worktree_lease: Some("worktree:test".into()),
        checkpoint_json: r#"{"git_head":"test"}"#.into(),
        tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
    }
}

fn durable_attestation() -> WorkerRecoveryAttestation {
    WorkerRecoveryAttestation {
        checkpoint_json: r#"{"kind":"recovery_attestation","git_head":"test","git_status":""}"#
            .into(),
        tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
        evidence_json: r#"{"kind":"deterministic_recovery_preflight","result":"attested"}"#.into(),
    }
}

impl AgentWorkerFactory for AdmissionObservingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let repository =
            RuntimeRepository::open(&self.database).expect("factory can inspect runtime store");
        assert_eq!(repository.task_state(&request.task_id).unwrap(), "running");
        assert!(
            repository
                .has_active_lease_prefix(&request.task_id, "workspace:")
                .unwrap()
        );
        assert!(
            repository
                .has_active_lease_prefix(&request.task_id, "worktree:")
                .unwrap()
        );
        let connection = Connection::open(&self.database).unwrap();
        let (checkpoint, tool_state): (String, String) = connection
            .query_row(
                "SELECT checkpoint_json, usage_json FROM attempts WHERE id = ?1",
                [request.attempt_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(checkpoint, durable_context().checkpoint_json);
        assert_eq!(tool_state, durable_context().tool_state_json);
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

impl AgentWorkerFactory for RecoveryStartObservingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn preflight_recovery(
        &self,
        _request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        WorkerRecoveryPreflightResult::Attested(durable_attestation())
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.starts.lock().unwrap() += 1;
        let connection = Connection::open(&self.database).unwrap();
        let attested: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND kind = 'task_recovery_attested'",
                [request.task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attested, 1, "recovery attestation must precede start");
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

impl AgentWorkerFactory for ConflictReportingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn preflight_recovery(
        &self,
        _request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        WorkerRecoveryPreflightResult::Conflict(
            "recorded Git HEAD no longer matches checkpoint".into(),
        )
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.starts.lock().unwrap() += 1;
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

impl AgentWorkerFactory for StartupErrorFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(
        &self,
        _request: WorkerStart,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async { Err(WorkerError::Startup("provider bootstrap failed".into())) })
    }
}

impl AgentWorkerFactory for PauseRecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[tokio::test]
async fn coordinator_starts_root_worker_and_cancels_its_tree() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();
    let root_cancellation = coordinator
        .worker_cancellation(&session, &root)
        .await
        .unwrap();
    let child_cancellation = coordinator
        .worker_cancellation(&session, &child)
        .await
        .unwrap();

    coordinator
        .cancel_task(&session, &root, true)
        .await
        .unwrap();

    assert!(root_cancellation.is_cancelled());
    assert!(child_cancellation.is_cancelled());
    assert_eq!(coordinator.task_state(&root).unwrap(), "cancelled");
    assert_eq!(coordinator.task_state(&child).unwrap(), "cancelled");
}

#[tokio::test]
async fn runtime_passes_an_llm_gate_to_a_profile_aware_worker_factory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ProviderGateRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();

    assert!(*factory.received_gate.lock().unwrap());
}

#[tokio::test]
async fn runtime_persists_initial_attempts_for_root_and_child_workers() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = factory.starts.lock().unwrap();
    let root_attempt = starts
        .iter()
        .find(|start| start.task_id == root)
        .unwrap()
        .attempt_id
        .clone();
    let child_attempt = starts
        .iter()
        .find(|start| start.task_id == child)
        .unwrap()
        .attempt_id
        .clone();
    drop(starts);
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.attempt_state(&root_attempt).unwrap(), "running");
    assert_eq!(repository.attempt_state(&child_attempt).unwrap(), "running");
}

#[tokio::test]
async fn admission_persists_recovery_boundary_before_worker_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(AdmissionObservingFactory {
            database: database.clone(),
        }),
    )
    .unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
}

#[tokio::test]
async fn recovered_root_resumes_in_a_fresh_attempt_after_runtime_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:project",
            "worktree:feature/recovery",
            r#"{"checkpoint":"before restart"}"#,
            r#"{"tool":"git","status":"clean"}"#,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);

    coordinator.resume_task(&session, &task).await.unwrap();
    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 1);
    assert_ne!(starts[0].attempt_id, attempt);
    drop(starts);
    let events = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    let attested = events
        .iter()
        .position(|event| event.event == RuntimeEvent::TaskRecoveryAttested)
        .unwrap();
    let started = events
        .iter()
        .rposition(|event| event.event == RuntimeEvent::TaskStarted)
        .unwrap();
    assert!(attested < started);
}

#[test]
fn recovered_successor_remains_gated_after_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let interrupted_attempt = yi_agent_core::AttemptId::new();
    let successor_attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &interrupted_attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &interrupted_attempt,
            "workspace:test",
            "worktree:test",
            &durable_context().checkpoint_json,
            &durable_context().tool_state_json,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    repository
        .activate_successor_attempt(
            &task,
            &successor_attempt,
            2,
            "recovery_gated",
            RuntimeEvent::TaskQueued,
        )
        .unwrap();
    drop(repository);

    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);
    assert_eq!(coordinator.task_state(&task).unwrap(), "recovery_gated");
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .attempt_state(&successor_attempt)
            .unwrap(),
        "recovery_gated"
    );
}

#[tokio::test]
async fn attested_successor_remains_resumable_after_restart_before_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "recovery_gated")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:test",
            "worktree:test",
            &durable_context().checkpoint_json,
            &durable_context().tool_state_json,
        )
        .unwrap();
    repository
        .attest_recovery_gate(&task, &attempt, &durable_attestation())
        .unwrap();
    drop(repository);

    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();

    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);
    assert_eq!(coordinator.task_state(&task).unwrap(), "recovery_attested");
    coordinator.resume_task(&session, &task).await.unwrap();
    assert_eq!(coordinator.task_state(&task).unwrap(), "running");
}

#[tokio::test]
async fn recovery_attestation_is_durable_before_worker_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .record_recovery_context(
            &task,
            &attempt,
            "workspace:test",
            "worktree:test",
            &durable_context().checkpoint_json,
            &durable_context().tool_state_json,
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let starts = Arc::new(Mutex::new(0));
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(RecoveryStartObservingFactory {
            database: database.clone(),
            starts: starts.clone(),
        }),
    )
    .unwrap();

    coordinator.resume_task(&session, &task).await.unwrap();

    assert_eq!(*starts.lock().unwrap(), 1);
}

#[tokio::test]
async fn recovery_conflict_is_durable_without_factory_start() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let factory = Arc::new(ConflictReportingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    coordinator.resume_task(&session, &task).await.unwrap();
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(*factory.starts.lock().unwrap(), 0);
    assert_eq!(coordinator.task_state(&task).unwrap(), "blocked");
    let terminal = RuntimeRepository::open(&database)
        .unwrap()
        .attempt_terminal_json_for_task(&task)
        .unwrap()
        .unwrap();
    let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
    assert_eq!(terminal["reason"], "recovery_conflict");
    assert!(terminal["evidence"].as_str().unwrap().contains("Git HEAD"));
}

#[tokio::test]
async fn recovery_required_task_rejects_direct_start_without_factory_action() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());

    assert!(factory.starts.lock().unwrap().is_empty());
    assert_eq!(coordinator.task_state(&task).unwrap(), "recovery_required");
}

#[tokio::test]
async fn startup_failure_closes_attempt_and_releases_admission_leases() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(StartupErrorFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&task).unwrap(), "failed");
    assert!(
        !repository
            .has_active_lease_prefix(&task, "workspace:")
            .unwrap()
    );
    assert!(
        !repository
            .has_active_lease_prefix(&task, "worktree:")
            .unwrap()
    );
}

#[tokio::test]
async fn resident_lease_is_released_after_child_startup_failure() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(StartupErrorFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    assert!(coordinator.start_worker(&session, &child).await.is_err());

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&child).unwrap(), "failed");
    assert!(
        !repository
            .has_active_lease_prefix(&child, "resident:")
            .unwrap()
    );
}

#[tokio::test]
async fn recovered_legacy_empty_attempt_task_resumes_with_a_successor_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository.create_task(&task, &session, "running").unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    assert_eq!(coordinator.root_task_id(&session).unwrap(), task);
    coordinator.resume_task(&session, &task).await.unwrap();
    assert_eq!(factory.starts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn recovered_child_resumes_after_runtime_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let root = yi_agent_core::TaskId::new();
    let root_attempt = yi_agent_core::AttemptId::new();
    let child = yi_agent_core::TaskId::new();
    let child_attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&root, &session, &root_attempt, 1, "running")
        .unwrap();
    repository
        .create_child_task_with_attempt_and_objective(
            &child,
            &session,
            &root,
            1,
            &child_attempt,
            1,
            "running",
            "Preserve this recovered child objective.",
        )
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    coordinator.resume_task(&session, &child).await.unwrap();
    assert_eq!(factory.starts.lock().unwrap()[0].task_id, child);
    assert_eq!(
        factory.starts.lock().unwrap()[0].objective,
        "Preserve this recovered child objective."
    );
}

#[tokio::test]
async fn unsafe_recovery_inspection_blocks_the_task_with_recovery_conflict() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = yi_agent_core::TaskId::new();
    let attempt = yi_agent_core::AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);

    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    coordinator.resume_task(&session, &task).await.unwrap();
    factory.handles.lock().unwrap()[0]
        .report_recovery_conflict("dirty worktree cannot prove a safe base");
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(coordinator.task_state(&task).unwrap(), "blocked");
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|event| event.event == RuntimeEvent::TaskBlocked)
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .attempt_terminal_json_for_task(&task)
            .unwrap()
            .as_deref(),
        Some(r#"{"reason":"recovery_conflict"}"#)
    );
}

#[tokio::test]
async fn cancellation_closes_the_active_attempt_before_a_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &task).await.unwrap();
    let attempt = factory.starts.lock().unwrap()[0].attempt_id.clone();

    coordinator
        .cancel_task(&session, &task, false)
        .await
        .unwrap();
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.attempt_state(&attempt).unwrap(), "cancelled");
    assert!(repository.attempt_ended_at(&attempt).unwrap().is_some());
    drop(repository);

    let mut repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.recover_inflight_tasks().unwrap(), 0);
    assert_eq!(repository.attempt_state(&attempt).unwrap(), "cancelled");
}

#[tokio::test]
async fn coordinator_persists_worker_failure_reported_by_the_factory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(FailingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "failed");
}

#[tokio::test]
async fn resident_admission_persists_and_restores_the_fairness_cursor() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator.spawn_child(&session, &root).await.unwrap();

    coordinator.start_worker(&session, &child).await.unwrap();
    let in_memory_cursor = coordinator.resident_admission_cursor();
    let cursor = RuntimeRepository::open(&database)
        .unwrap()
        .admission_cursor("resident:global")
        .unwrap();
    assert!(cursor.is_some());
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .has_active_lease_prefix(&child, "resident:")
            .unwrap()
    );
    assert_eq!(cursor.as_ref().unwrap().sequence, in_memory_cursor.sequence);
    drop(coordinator);

    let reopened = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    assert_eq!(reopened.resident_admission_cursor(), in_memory_cursor);
    drop(reopened);
}

#[tokio::test]
async fn global_resident_capacity_leaves_excess_child_queued() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mut children = Vec::new();
    for _ in 0..17 {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        let child = coordinator.spawn_child(&session, &root).await.unwrap();
        children.push((session, child));
    }

    for (session, child) in children.iter().take(16) {
        coordinator.start_worker(session, child).await.unwrap();
    }
    let (session, child) = &children[16];
    assert!(coordinator.start_worker(session, child).await.is_err());
    assert_eq!(coordinator.task_state(child).unwrap(), "queued");
}

#[tokio::test]
async fn fair_resident_grant_is_retained_for_the_selected_queued_child() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let mut children = Vec::new();
    for _ in 0..17 {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        let child = coordinator.spawn_child(&session, &root).await.unwrap();
        children.push((session, child));
    }
    for (session, child) in children.iter().take(16) {
        coordinator.start_worker(session, child).await.unwrap();
    }
    let (waiting_session, waiting_child) = &children[16];
    assert!(
        coordinator
            .start_worker(waiting_session, waiting_child)
            .await
            .is_err()
    );

    let (released_session, released_child) = &children[0];
    coordinator
        .cancel_task(released_session, released_child, false)
        .await
        .unwrap();
    let later_session = coordinator.create_session().unwrap();
    let later_root = coordinator.root_task_id(&later_session).unwrap();
    let later_child = coordinator
        .spawn_child(&later_session, &later_root)
        .await
        .unwrap();

    assert!(
        coordinator
            .start_worker(&later_session, &later_child)
            .await
            .is_err()
    );
    coordinator
        .start_worker(waiting_session, waiting_child)
        .await
        .unwrap();
    assert_eq!(coordinator.task_state(waiting_child).unwrap(), "running");
    assert_eq!(coordinator.task_state(&later_child).unwrap(), "queued");
}

#[tokio::test]
async fn queued_capacity_rejects_without_persisting_a_child_task() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    for _ in 0..RuntimeCoordinator::DEFAULT_GLOBAL_QUEUED_SUBAGENTS {
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        coordinator.spawn_child(&session, &root).await.unwrap();
    }
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    assert!(matches!(
        coordinator.spawn_child(&session, &root).await,
        Err(yi_agent_store::runtime::RuntimeCoordinatorError::QueueCapacityExceeded)
    ));
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .queued_task_count()
            .unwrap(),
        64
    );
}

#[tokio::test]
async fn coordinator_retries_a_terminal_task_as_a_new_running_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator
        .cancel_task(&session, &root, false)
        .await
        .unwrap();

    coordinator.retry_task(&session, &root).await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "running");
}

#[tokio::test]
async fn draining_rejects_all_new_runtime_admissions() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.begin_draining().await.unwrap();

    assert!(coordinator.create_session().is_err());
    assert!(coordinator.spawn_child(&session, &root).await.is_err());
    assert!(coordinator.start_worker(&session, &root).await.is_err());
    assert!(coordinator.retry_task(&session, &root).await.is_err());
    assert!(coordinator.resume_task(&session, &root).await.is_err());
}

#[tokio::test]
async fn draining_is_persisted_before_safe_checkpoint_requests() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(PauseRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    coordinator.begin_draining().await.unwrap();
    coordinator.request_safe_checkpoints().await.unwrap();

    let events = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    let draining = events
        .iter()
        .position(|event| event.event == RuntimeEvent::RuntimeDraining)
        .expect("draining must be persisted before checkpoint requests");
    let pause_requested = events
        .iter()
        .position(|event| event.event == RuntimeEvent::TaskPauseRequested)
        .expect("safe checkpoint request must be persisted");
    assert!(draining < pause_requested);

    factory.handles.lock().unwrap()[0].report_paused();
    coordinator.reconcile_worker_events().await.unwrap();
    assert_eq!(coordinator.task_state(&root).unwrap(), "paused");
}

#[tokio::test]
async fn draining_interrupts_unacknowledged_workers_after_the_grace_deadline() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let cancellation = coordinator
        .worker_cancellation(&session, &root)
        .await
        .unwrap();

    coordinator.begin_draining().await.unwrap();
    coordinator.request_safe_checkpoints().await.unwrap();
    coordinator
        .interrupt_unacknowledged_workers()
        .await
        .unwrap();

    assert!(cancellation.is_cancelled());
    assert_eq!(coordinator.task_state(&root).unwrap(), "recovery_required");
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|event| event.event == RuntimeEvent::TaskRecoveryRequired)
    );
}

#[tokio::test]
async fn graceful_stop_waits_for_a_cooperative_safe_checkpoint() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(PauseRecordingFactory::default());
    let coordinator = Arc::new(RuntimeCoordinator::open(&database, factory.clone()).unwrap());
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let reporter = factory.handles.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        reporter.lock().unwrap()[0].report_paused();
    });
    let summary = coordinator
        .graceful_stop(RuntimeStopOptions {
            grace: Duration::from_millis(100),
        })
        .await
        .unwrap();

    assert_eq!(summary.paused, 1);
    assert_eq!(summary.recovery_required, 0);
    assert_eq!(coordinator.task_state(&root).unwrap(), "paused");
}

#[tokio::test]
async fn coordinator_persists_pause_only_after_worker_safe_checkpoint_acknowledgement() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(PauseRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator.pause_task(&session, &root).await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "running");
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&root).unwrap(), "running");
    assert!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|record| record.event == RuntimeEvent::TaskPauseRequested)
    );
    drop(repository);

    assert!(coordinator.resume_task(&session, &root).await.is_err());
    assert_eq!(coordinator.task_state(&root).unwrap(), "running");

    factory.handles.lock().unwrap()[0].report_paused();
    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(coordinator.task_state(&root).unwrap(), "paused");
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&root).unwrap(), "paused");
    assert!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|record| record.event == RuntimeEvent::TaskPaused)
    );

    coordinator.resume_task(&session, &root).await.unwrap();
    assert_eq!(coordinator.task_state(&root).unwrap(), "running");
}

#[tokio::test]
async fn coordinator_persists_an_external_override_only_after_worker_consumes_it() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator
        .send_user_override(&root, "continue with the fix".into())
        .await
        .unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let initial = factory.starts.lock().unwrap()[0]
        .initial_user_messages
        .clone();
    assert_eq!(initial.len(), 1);
    let message_id = initial[0].id.clone();
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&message_id)
            .unwrap(),
        None
    );

    factory.handles.lock().unwrap()[0].report_message_consumed(message_id.clone());
    coordinator.reconcile_worker_events().await.unwrap();

    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .mailbox_message_delivered_at(&message_id)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn retry_replays_an_unconsumed_external_override_but_not_an_acknowledged_one() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator
        .send_user_override(&root, "keep the existing scope".into())
        .await
        .unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    coordinator
        .cancel_task(&session, &root, false)
        .await
        .unwrap();
    coordinator.retry_task(&session, &root).await.unwrap();

    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1].initial_user_messages.len(), 1);
    let message_id = starts[1].initial_user_messages[0].id.clone();
    drop(starts);

    factory.handles.lock().unwrap()[1].report_message_consumed(message_id);
    coordinator.reconcile_worker_events().await.unwrap();
    coordinator
        .cancel_task(&session, &root, false)
        .await
        .unwrap();
    coordinator.retry_task(&session, &root).await.unwrap();

    assert!(
        factory.starts.lock().unwrap()[2]
            .initial_user_messages
            .is_empty()
    );
}
