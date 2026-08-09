use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_store::repository::{RuntimeEvent, RuntimeRepository};
use yi_agent_store::runtime::{RuntimeCoordinator, RuntimeStopOptions};

#[derive(Default)]
struct RecordingFactory;

impl AgentWorkerFactory for RecordingFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

struct FailingFactory;

impl AgentWorkerFactory for FailingFactory {
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

impl AgentWorkerFactory for PauseRecordingFactory {
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
