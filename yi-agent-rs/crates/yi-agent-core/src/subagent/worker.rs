//! Application-owned worker construction boundary for the runtime daemon.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent::ProviderTurnGate;

use super::task::{AttemptId, DeliveryReport, MessageId, RootSessionId, TaskId, WorkspaceLeaseId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerWorkspace {
    pub lease_id: WorkspaceLeaseId,
    pub repository_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub parent_branch: String,
    pub base_commit: String,
}

#[derive(Debug, Clone)]
pub struct WorkerStart {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub root_session_id: RootSessionId,
    /// Runtime-owned workspace identity required in a coding delivery report.
    pub workspace_lease_id: Option<WorkspaceLeaseId>,
    /// Full application-owned workspace assignment for this task, when known.
    pub workspace: Option<WorkerWorkspace>,
    pub cancellation: CancellationToken,
    /// Opaque daemon-issued capability required for worker IPC mutations.
    pub message_capability: String,
    /// User instructions queued before admission. Application factories must
    /// preload them before allowing the first Agent turn to start.
    pub initial_user_messages: Vec<WorkerMessage>,
    /// Narrow task instruction supplied by the parent supervisor.
    pub objective: String,
}

/// Non-secret evidence recorded when a worker is admitted. The runtime keeps
/// it with the attempt so a restart can establish a safe recovery boundary.
#[derive(Debug, Clone)]
pub struct WorkerRecoveryContext {
    pub workspace_lease_id: Option<String>,
    pub worktree_lease: Option<String>,
    pub checkpoint_json: String,
    pub tool_state_json: String,
}

#[derive(Debug, Clone)]
pub struct WorkerRecoveryPreflight {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub context: WorkerRecoveryContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecoveryAttestation {
    pub checkpoint_json: String,
    pub tool_state_json: String,
    pub evidence_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerRecoveryPreflightResult {
    Attested(WorkerRecoveryAttestation),
    Conflict(String),
}

impl Default for WorkerRecoveryContext {
    fn default() -> Self {
        Self {
            workspace_lease_id: None,
            worktree_lease: None,
            checkpoint_json: r#"{"state":"unavailable","reason":"worker factory did not report a safe checkpoint"}"#.into(),
            tool_state_json: r#"{"state":"unavailable","reason":"worker factory did not report tool state"}"#.into(),
        }
    }
}

impl WorkerStart {
    pub fn new(task_id: TaskId, attempt_id: AttemptId, root_session_id: RootSessionId) -> Self {
        Self {
            task_id,
            attempt_id,
            root_session_id,
            workspace_lease_id: None,
            workspace: None,
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

    pub fn with_workspace_lease(mut self, workspace_lease_id: WorkspaceLeaseId) -> Self {
        self.workspace_lease_id = Some(workspace_lease_id);
        self
    }

    pub fn with_workspace(mut self, workspace: WorkerWorkspace) -> Self {
        self.workspace_lease_id = Some(workspace.lease_id.clone());
        self.workspace = Some(workspace);
        self
    }

    pub fn with_message_capability(mut self, capability: impl Into<String>) -> Self {
        self.message_capability = capability.into();
        self
    }

    pub fn with_initial_user_messages(mut self, messages: Vec<WorkerMessage>) -> Self {
        self.initial_user_messages = messages;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::task::WorkspaceLeaseId;

    #[test]
    fn worker_start_retains_the_task_workspace_lease() {
        let workspace = WorkspaceLeaseId::new();
        let start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_workspace_lease(workspace.clone());
        assert_eq!(start.workspace_lease_id, Some(workspace));
    }

    #[test]
    fn worker_start_workspace_assignment_keeps_legacy_lease_coherent() {
        let lease = WorkspaceLeaseId::new();
        let workspace = WorkerWorkspace {
            lease_id: lease.clone(),
            repository_root: "/repo".into(),
            path: "/repo/.worktrees/task".into(),
            branch: "feat/task".into(),
            parent_branch: "main".into(),
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        };

        let start = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
            .with_workspace(workspace.clone());

        assert_eq!(start.workspace_lease_id, Some(lease));
        assert_eq!(start.workspace, Some(workspace));
    }
}

#[derive(Debug, Clone)]
pub struct WorkerHandle {
    cancellation: CancellationToken,
    pause_updates: watch::Sender<bool>,
    events: Arc<Mutex<Vec<WorkerEvent>>>,
    watchdog_events: Arc<Mutex<Vec<WorkerWatchdogEvent>>>,
    messages: Arc<Mutex<VecDeque<WorkerMessage>>>,
    message_updates: watch::Sender<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerMessage {
    pub id: MessageId,
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
    /// An inbox item was committed to the next Agent prompt.
    MessageConsumed {
        message_id: MessageId,
    },
    /// A validated coding result awaits inspection by the direct parent.
    Delivered(DeliveryReport),
    /// A normal text-only task completed without a coding delivery.
    Completed {
        report: String,
    },
    CompletedWithoutDelivery,
    Paused,
    Cancelled,
    Failed(String),
    RecoveryConflict(String),
}

/// Non-terminal facts used by the runtime watchdog. Generated model text is
/// deliberately absent: it must not reset the idle-progress deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerWatchdogEvent {
    ProviderUsage {
        input_tokens: u64,
        output_tokens: u64,
    },
    ProviderRetry,
    ToolRetry,
    MeaningfulProgress,
}

impl WorkerHandle {
    pub fn new(cancellation: CancellationToken) -> Self {
        let (message_updates, _) = watch::channel(0_u64);
        let (pause_updates, _) = watch::channel(false);
        Self {
            cancellation,
            pause_updates,
            events: Arc::new(Mutex::new(Vec::new())),
            watchdog_events: Arc::new(Mutex::new(Vec::new())),
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

    pub fn report_delivery(&self, delivery: DeliveryReport) {
        self.report(WorkerEvent::Delivered(delivery));
    }

    pub fn report_completed(&self, report: impl Into<String>) {
        self.report(WorkerEvent::Completed {
            report: report.into(),
        });
    }

    pub fn report_message_consumed(&self, message_id: MessageId) {
        self.report(WorkerEvent::MessageConsumed { message_id });
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

    pub fn report_recovery_conflict(&self, message: impl Into<String>) {
        self.report(WorkerEvent::RecoveryConflict(message.into()));
    }

    /// Records one completed provider turn. The runtime persists this as
    /// budget usage without treating generated model text as progress.
    pub fn report_provider_usage(&self, input_tokens: u64, output_tokens: u64) {
        self.report_watchdog(WorkerWatchdogEvent::ProviderUsage {
            input_tokens,
            output_tokens,
        });
    }

    /// Records an automatic retry after a classified transient provider failure.
    pub fn report_provider_retry(&self) {
        self.report_watchdog(WorkerWatchdogEvent::ProviderRetry);
    }

    /// Records an automatic retry of a tool that explicitly opted in.
    pub fn report_tool_retry(&self) {
        self.report_watchdog(WorkerWatchdogEvent::ToolRetry);
    }

    /// Records a durable progress point such as a successful tool result.
    pub fn report_meaningful_progress(&self) {
        self.report_watchdog(WorkerWatchdogEvent::MeaningfulProgress);
    }

    pub fn take_events(&self) -> Vec<WorkerEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    pub fn take_watchdog_events(&self) -> Vec<WorkerWatchdogEvent> {
        std::mem::take(
            &mut *self
                .watchdog_events
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
        self.deliver_worker_message(WorkerMessage {
            id: MessageId::new(),
            body: body.into(),
        });
    }

    pub fn deliver_worker_message(&self, message: WorkerMessage) {
        self.messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(message);
        self.message_updates
            .send_modify(|epoch| *epoch = epoch.saturating_add(1));
    }

    fn report(&self, event: WorkerEvent) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
    }

    fn report_watchdog(&self, event: WorkerWatchdogEvent) {
        self.watchdog_events
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

pub trait AgentWorkspaceService: Send + Sync {
    fn prepare_root(
        &self,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError>;

    fn prepare_child(
        &self,
        parent: &WorkerWorkspace,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError>;

    fn inspect_delivery(
        &self,
        _workspace: &WorkerWorkspace,
    ) -> Result<DeliveryReport, WorkerError> {
        Err(WorkerError::Startup(
            "coding workspace service does not support delivery inspection".into(),
        ))
    }

    fn cleanup_prepared(&self, _workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct UnavailableWorkspaceService;

impl AgentWorkspaceService for UnavailableWorkspaceService {
    fn prepare_root(
        &self,
        _root_session_id: &RootSessionId,
        _task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup(
            "coding workspace service is unavailable".into(),
        ))
    }

    fn prepare_child(
        &self,
        _parent: &WorkerWorkspace,
        _root_session_id: &RootSessionId,
        _task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup(
            "coding workspace service is unavailable".into(),
        ))
    }
}

/// Constructs an application-specific `Agent` worker without making core
/// depend on the CLI, provider bootstrap, or tool registry construction.
pub trait AgentWorkerFactory: Send + Sync {
    /// Inspection-only daemon instances deliberately use an unavailable
    /// factory, so they must retain queued tasks without starting them.
    fn is_available(&self) -> bool {
        true
    }

    /// A stable, non-secret provider/API-key profile identifier used for
    /// daemon-wide LLM admission. Factories without one opt out of LLM gates.
    fn provider_profile_id(&self) -> Option<String> {
        None
    }

    fn workspace_service(&self) -> Option<Arc<dyn AgentWorkspaceService>> {
        None
    }

    /// Supplies the facts that must survive an interrupted worker attempt.
    /// Factories without a workspace deliberately return explicit absence,
    /// which turns a later recovery attempt into a conflict instead of a
    /// replay from an unknown base.
    fn recovery_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext::default()
    }

    fn recovery_context_for(&self, _request: &WorkerStart) -> WorkerRecoveryContext {
        self.recovery_context()
    }

    /// Inspects durable recovery evidence without constructing an Agent,
    /// provider turn, or ordinary worker tool set.
    fn preflight_recovery(
        &self,
        _request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        WorkerRecoveryPreflightResult::Conflict(
            "worker factory cannot attest deterministic recovery state".into(),
        )
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>>;

    /// Lets the runtime attach per-provider-turn admission without making
    /// ordinary factories or core task ownership depend on a store type.
    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        _gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.start(request)
    }
}
