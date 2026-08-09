//! Application-owned worker construction boundary for the runtime daemon.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::task::{AttemptId, RootSessionId, TaskId};

#[derive(Debug, Clone)]
pub struct WorkerStart {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub root_session_id: RootSessionId,
    pub cancellation: CancellationToken,
    /// Opaque daemon-issued capability required for worker IPC mutations.
    pub message_capability: String,
    /// User instructions queued before admission. Application factories must
    /// preload them before allowing the first Agent turn to start.
    pub initial_user_messages: Vec<String>,
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
            message_capability: String::new(),
            initial_user_messages: Vec::new(),
            objective: String::new(),
        }
    }

    pub fn with_objective(mut self, objective: impl Into<String>) -> Self {
        self.objective = objective.into();
        self
    }

    pub fn with_message_capability(mut self, capability: impl Into<String>) -> Self {
        self.message_capability = capability.into();
        self
    }

    pub fn with_initial_user_messages(mut self, messages: Vec<String>) -> Self {
        self.initial_user_messages = messages;
        self
    }
}

#[derive(Debug, Clone)]
pub struct WorkerHandle {
    cancellation: CancellationToken,
    pause_updates: watch::Sender<bool>,
    events: Arc<Mutex<Vec<WorkerEvent>>>,
    messages: Arc<Mutex<VecDeque<WorkerMessage>>>,
    message_updates: watch::Sender<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerMessage {
    pub body: String,
}

/// A single-worker inbox. Its update channel allows an idle worker to await a
/// message without polling while the queue preserves messages until consumed.
pub struct WorkerMailbox {
    messages: Arc<Mutex<VecDeque<WorkerMessage>>>,
    updates: watch::Receiver<u64>,
}

/// Receives a durable-in-process pause request. A worker acknowledges it by
/// reporting `WorkerEvent::Paused` only after reaching its safe checkpoint.
pub struct WorkerPauseSignal {
    updates: watch::Receiver<bool>,
}

impl WorkerPauseSignal {
    pub async fn requested(&mut self) -> bool {
        loop {
            if *self.updates.borrow() {
                return true;
            }
            if self.updates.changed().await.is_err() {
                return false;
            }
        }
    }
}

impl WorkerMailbox {
    pub async fn recv(&mut self) -> Option<WorkerMessage> {
        loop {
            if let Some(message) = self
                .messages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front()
            {
                return Some(message);
            }
            self.updates.changed().await.ok()?;
        }
    }
}

/// Facts reported by a worker. The supervisor remains the only component that
/// converts these into task-state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    CompletedWithoutDelivery,
    Paused,
    Cancelled,
    Failed(String),
}

impl WorkerHandle {
    pub fn new(cancellation: CancellationToken) -> Self {
        let (message_updates, _) = watch::channel(0_u64);
        let (pause_updates, _) = watch::channel(false);
        Self {
            cancellation,
            pause_updates,
            events: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(VecDeque::new())),
            message_updates,
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn request_pause(&self) {
        self.pause_updates.send_replace(true);
    }

    pub fn subscribe_pause(&self) -> WorkerPauseSignal {
        WorkerPauseSignal {
            updates: self.pause_updates.subscribe(),
        }
    }

    pub fn report_completed_without_delivery(&self) {
        self.report(WorkerEvent::CompletedWithoutDelivery);
    }

    pub fn report_paused(&self) {
        self.report(WorkerEvent::Paused);
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

    pub fn subscribe_messages(&self) -> WorkerMailbox {
        WorkerMailbox {
            messages: Arc::clone(&self.messages),
            updates: self.message_updates.subscribe(),
        }
    }

    pub fn deliver_message(&self, body: impl Into<String>) {
        self.messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(WorkerMessage { body: body.into() });
        self.message_updates
            .send_modify(|epoch| *epoch = epoch.saturating_add(1));
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
    /// Inspection-only daemon instances deliberately use an unavailable
    /// factory, so they must retain queued tasks without starting them.
    fn is_available(&self) -> bool {
        true
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>>;
}
