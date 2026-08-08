//! Application-owned worker construction boundary for the runtime daemon.

use futures::future::BoxFuture;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::task::{AttemptId, RootSessionId, TaskId};

#[derive(Debug, Clone)]
pub struct WorkerStart {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub root_session_id: RootSessionId,
    pub cancellation: CancellationToken,
}

impl WorkerStart {
    pub fn new(task_id: TaskId, attempt_id: AttemptId, root_session_id: RootSessionId) -> Self {
        Self {
            task_id,
            attempt_id,
            root_session_id,
            cancellation: CancellationToken::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerHandle {
    cancellation: CancellationToken,
}

impl WorkerHandle {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self { cancellation }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("worker startup failed: {0}")]
    Startup(String),
}

/// Constructs an application-specific `Agent` worker without making core
/// depend on the CLI, provider bootstrap, or tool registry construction.
pub trait AgentWorkerFactory: Send + Sync {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>>;
}
