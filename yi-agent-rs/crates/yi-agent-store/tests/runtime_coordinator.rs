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
