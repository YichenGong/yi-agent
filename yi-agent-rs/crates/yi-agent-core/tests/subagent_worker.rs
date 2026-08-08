use futures::future::BoxFuture;
use yi_agent_core::subagent::task::{AttemptId, RootSessionId, TaskId};
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerHandle, WorkerStart};

struct ImmediateFactory;

impl AgentWorkerFactory for ImmediateFactory {
    fn start(
        &self,
        request: WorkerStart,
    ) -> BoxFuture<'static, Result<WorkerHandle, yi_agent_core::subagent::worker::WorkerError>>
    {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

#[tokio::test]
async fn worker_factory_receives_task_identity_and_cancellation_handle() {
    let request = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
    let handle = ImmediateFactory.start(request.clone()).await.unwrap();

    assert!(!handle.cancellation_token().is_cancelled());
    handle.cancel();
    assert!(request.cancellation.is_cancelled());
}
