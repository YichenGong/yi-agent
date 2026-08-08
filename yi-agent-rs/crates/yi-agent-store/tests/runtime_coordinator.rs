use std::sync::Arc;

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_store::runtime::RuntimeCoordinator;

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
