//! Application-owned worker construction boundary for the runtime daemon.

use std::sync::{Arc, Mutex};

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
    /// Narrow task instruction supplied by the parent supervisor.
    pub objective: String,
}

impl WorkerStart {
    pub fn new(task_id: TaskId, attempt_id: AttemptId, root_session_id: RootSessionId) -> Self {
        Self {
            task_id,
            attempt_id,
            root_session_id,
            cancellation: CancellationToken::new(),
            objective: String::new(),
        }
    }

    pub fn with_objective(mut self, objective: impl Into<String>) -> Self {
        self.objective = objective.into();
        self
    }
}

#[derive(Debug, Clone)]
pub struct WorkerHandle {
    cancellation: CancellationToken,
    events: Arc<Mutex<Vec<WorkerEvent>>>,
}

/// Facts reported by a worker. The supervisor remains the only component that
/// converts these into task-state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    CompletedWithoutDelivery,
    Cancelled,
    Failed(String),
}

impl WorkerHandle {
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn report_completed_without_delivery(&self) {
        self.report(WorkerEvent::CompletedWithoutDelivery);
    }

    pub fn report_cancelled(&self) {
        self.report(WorkerEvent::Cancelled);
    }

    pub fn report_failure(&self, message: impl Into<String>) {
        self.report(WorkerEvent::Failed(message.into()));
    }

    pub fn take_events(&self) -> Vec<WorkerEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn report(&self, event: WorkerEvent) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
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
