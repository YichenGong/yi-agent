use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::watch;
use uuid::Uuid;

use super::mailbox::{Mailbox, MailboxMessageDraft, MessageKind, MessagePriority, UserInstruction};
use super::scheduler::AdmissionPriority;
use super::task::{
    AgentTask, AttemptId, BlockReason, BudgetKind, CancelReason, ChildWriteMode, DeliveryId,
    InheritedSandbox, IntegrationValidation, MessageId, PauseReason, PermissionDecision,
    PermissionRequestId, RecoveryEvidence, RootSessionId, TaskAttempt, TaskEvent, TaskFailure,
    TaskId, TaskState, TimeoutKind, WatchdogEvidence, WorkspaceLeaseId,
};
use super::trace::TraceFact;
use super::worker::{
    AgentWorkerFactory, SpawnRequest, WorkerEvent, WorkerHandle, WorkerMessage, WorkerStart,
    WorkerWatchdogEvent,
};
use crate::agent::ProviderTurnGate;
use crate::tool::{Tool, ToolRegistry, ToolResult};

pub const MAX_DIRECT_CHILDREN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorEvent {
    TaskSpawned { parent_id: TaskId, task_id: TaskId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpawnError {
    #[error("parent task does not exist")]
    ParentNotFound,
    #[error("leaf tasks cannot spawn descendants")]
    MaximumDepthReached,
    #[error("an agent may have at most four direct children")]
    DirectChildLimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MessageDeliveryError {
    #[error("sender task does not exist")]
    SenderNotFound,
    #[error("terminal tasks cannot send mailbox messages")]
    SenderTerminal,
    #[error("recipient task does not exist")]
    RecipientNotFound,
    #[error("messages may only be sent to a direct parent or child")]
    RecipientNotAdjacent,
    #[error("terminal tasks do not accept mailbox messages")]
    RecipientTerminal,
}

#[derive(Clone, Copy)]
pub enum WaitMode {
    Any,
    All,
}

pub enum WaitOutcome {
    /// The caller has mailbox work at high priority or above, such as a
    /// permission request. Any child that has already finished still carries
    /// its report, so a caller that only checks `reports` never loses a
    /// delivery just because something else also needs attention.
    NeedsAttention { reports: Vec<CompletedChildReport> },
    Completed {
        children: Vec<TaskId>,
        reports: Vec<CompletedChildReport>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedChildReport {
    pub task_id: TaskId,
    pub state: String,
    pub report: Option<String>,
    /// The child's delivered commit, when the child produced a delivery.
    pub delivery: Option<String>,
}

pub struct AgentSupervisor {
    root_task_id: TaskId,
    tasks: HashMap<TaskId, AgentTask>,
    objectives: HashMap<TaskId, String>,
    workspace_modes: HashMap<TaskId, ChildWriteMode>,
    inherited_sandboxes: HashMap<TaskId, InheritedSandbox>,
    workdirs: HashMap<TaskId, Option<PathBuf>>,
    models: HashMap<TaskId, String>,
    children: HashMap<TaskId, Vec<TaskId>>,
    mailboxes: HashMap<TaskId, Mailbox>,
    workers: HashMap<TaskId, WorkerHandle>,
    completion_reports: HashMap<TaskId, String>,
    worker_message_capabilities: HashMap<TaskId, String>,
    pending_user_override_acks: Vec<(TaskId, super::task::MessageId)>,
    events: Vec<SupervisorEvent>,
    updates: watch::Sender<u64>,
}

#[derive(Debug)]
pub enum ReviewPersistenceError<E> {
    Supervisor(String),
    Persistence(E),
}

impl AgentSupervisor {
    pub fn new(root_session_id: RootSessionId) -> Self {
        Self::new_with_objective(
            root_session_id,
            "Root session objective not specified.".into(),
        )
    }

    pub fn new_with_objective(root_session_id: RootSessionId, objective: String) -> Self {
        let root = AgentTask::new_root(root_session_id);
        let root_task_id = root.id.clone();
        let mut tasks = HashMap::new();
        tasks.insert(root_task_id.clone(), root);
        let mut mailboxes = HashMap::new();
        mailboxes.insert(root_task_id.clone(), Mailbox::default());
        let mut objectives = HashMap::new();
        objectives.insert(root_task_id.clone(), objective);
        let (updates, _) = watch::channel(0_u64);
        Self {
            root_task_id,
            tasks,
            objectives,
            workspace_modes: HashMap::new(),
            inherited_sandboxes: HashMap::new(),
            workdirs: HashMap::new(),
            models: HashMap::new(),
            children: HashMap::new(),
            mailboxes,
            workers: HashMap::new(),
            completion_reports: HashMap::new(),
            worker_message_capabilities: HashMap::new(),
            pending_user_override_acks: Vec::new(),
            events: Vec::new(),
            updates,
        }
    }

    pub fn from_recovered_root(
        root_session_id: RootSessionId,
        root_task_id: TaskId,
        attempt_id: super::task::AttemptId,
        attempt_number: u32,
        objective: String,
    ) -> Self {
        let root = AgentTask::recovered_root(
            root_session_id,
            root_task_id.clone(),
            attempt_id,
            attempt_number,
        );
        let mut tasks = HashMap::new();
        tasks.insert(root_task_id.clone(), root);
        let mut mailboxes = HashMap::new();
        mailboxes.insert(root_task_id.clone(), Mailbox::default());
        let mut objectives = HashMap::new();
        objectives.insert(root_task_id.clone(), objective);
        let (updates, _) = watch::channel(0_u64);
        Self {
            root_task_id,
            tasks,
            objectives,
            workspace_modes: HashMap::new(),
            inherited_sandboxes: HashMap::new(),
            workdirs: HashMap::new(),
            models: HashMap::new(),
            children: HashMap::new(),
            mailboxes,
            workers: HashMap::new(),
            completion_reports: HashMap::new(),
            worker_message_capabilities: HashMap::new(),
            pending_user_override_acks: Vec::new(),
            events: Vec::new(),
            updates,
        }
    }

    pub fn from_recovered_gated_root(
        root_session_id: RootSessionId,
        root_task_id: TaskId,
        attempt_id: super::task::AttemptId,
        attempt_number: u32,
        objective: String,
    ) -> Self {
        let mut supervisor = Self::from_recovered_root(
            root_session_id.clone(),
            root_task_id.clone(),
            attempt_id.clone(),
            attempt_number,
            objective,
        );
        supervisor.tasks.insert(
            root_task_id.clone(),
            AgentTask::recovered_gated_root(
                root_session_id,
                root_task_id,
                attempt_id,
                attempt_number,
            ),
        );
        supervisor
    }

    pub fn from_hydrated_review_root(task: AgentTask, objective: String) -> Self {
        let root_task_id = task.id.clone();
        let mut tasks = HashMap::new();
        tasks.insert(root_task_id.clone(), task);
        let mut mailboxes = HashMap::new();
        mailboxes.insert(root_task_id.clone(), Mailbox::default());
        let mut objectives = HashMap::new();
        objectives.insert(root_task_id.clone(), objective);
        let (updates, _) = watch::channel(0_u64);
        Self {
            root_task_id,
            tasks,
            objectives,
            workspace_modes: HashMap::new(),
            inherited_sandboxes: HashMap::new(),
            workdirs: HashMap::new(),
            models: HashMap::new(),
            children: HashMap::new(),
            mailboxes,
            workers: HashMap::new(),
            completion_reports: HashMap::new(),
            worker_message_capabilities: HashMap::new(),
            pending_user_override_acks: Vec::new(),
            events: Vec::new(),
            updates,
        }
    }

    pub fn insert_hydrated_review_child(
        &mut self,
        task: AgentTask,
        objective: String,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
    ) -> Result<(), String> {
        let parent_id = task
            .parent_id
            .clone()
            .ok_or_else(|| "hydrated child has no parent".to_string())?;
        if !self.tasks.contains_key(&parent_id) {
            return Err("hydrated child parent is missing".into());
        }
        let task_id = task.id.clone();
        self.tasks.insert(task_id.clone(), task);
        self.mailboxes.insert(task_id.clone(), Mailbox::default());
        self.objectives.insert(task_id.clone(), objective);
        self.workspace_modes.insert(task_id.clone(), workspace_mode);
        self.inherited_sandboxes.remove(&task_id);
        if let Some(model) = model {
            self.models.insert(task_id.clone(), model);
        }
        self.children.entry(parent_id).or_default().push(task_id);
        Ok(())
    }

    pub fn hydrate_completion_report(
        &mut self,
        task_id: TaskId,
        report: String,
    ) -> Result<(), String> {
        if !self.tasks.contains_key(&task_id) {
            return Err("hydrated completion report references a missing task".into());
        }
        self.completion_reports.insert(task_id, report);
        Ok(())
    }

    pub fn completion_report(&self, task_id: &TaskId) -> Option<&str> {
        self.completion_reports.get(task_id).map(String::as_str)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_recovered_child(
        &mut self,
        task_id: TaskId,
        parent_id: TaskId,
        depth: super::task::TaskDepth,
        attempt_id: super::task::AttemptId,
        attempt_number: u32,
        recovery_gated: bool,
        objective: String,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
    ) -> Result<(), String> {
        if !self.tasks.contains_key(&parent_id) {
            return Err("recovered child parent is missing".into());
        }
        if matches!(depth, super::task::TaskDepth::Root) {
            return Err("recovered child has root depth".into());
        }
        let root_session_id = self
            .tasks
            .get(&parent_id)
            .expect("recovered parent was checked")
            .root_session_id
            .clone();
        let task = if recovery_gated {
            AgentTask::recovered_gated_child(
                root_session_id,
                task_id.clone(),
                parent_id.clone(),
                depth,
                attempt_id,
                attempt_number,
            )
        } else {
            AgentTask::recovered_child(
                root_session_id,
                task_id.clone(),
                parent_id.clone(),
                depth,
                attempt_id,
                attempt_number,
            )
        };
        self.tasks.insert(task_id.clone(), task);
        self.mailboxes.insert(task_id.clone(), Mailbox::default());
        self.objectives.insert(task_id.clone(), objective);
        self.workspace_modes.insert(task_id.clone(), workspace_mode);
        self.inherited_sandboxes.remove(&task_id);
        if let Some(model) = model {
            self.models.insert(task_id.clone(), model);
        }
        self.children.entry(parent_id).or_default().push(task_id);
        Ok(())
    }

    pub fn root_task_id(&self) -> &TaskId {
        &self.root_task_id
    }

    pub fn task(&self, task_id: &TaskId) -> Option<&AgentTask> {
        self.tasks.get(task_id)
    }

    pub fn objective(&self, task_id: &TaskId) -> Option<&str> {
        self.objectives.get(task_id).map(String::as_str)
    }

    /// Records the model the child should run with. The runtime seeds this
    /// from the persisted task row so it survives a restart.
    pub fn set_model(&mut self, task_id: &TaskId, model: String) {
        self.models.insert(task_id.clone(), model);
    }

    pub fn model(&self, task_id: &TaskId) -> Option<&str> {
        self.models.get(task_id).map(String::as_str)
    }

    pub fn set_workspace_mode(&mut self, task_id: &TaskId, mode: ChildWriteMode) {
        self.workspace_modes.insert(task_id.clone(), mode);
    }

    pub fn set_inherited_sandbox(&mut self, task_id: &TaskId, sandbox: InheritedSandbox) {
        self.inherited_sandboxes.insert(task_id.clone(), sandbox);
    }

    pub fn inherited_sandbox(&self, task_id: &TaskId) -> Option<InheritedSandbox> {
        self.inherited_sandboxes.get(task_id).copied()
    }

    /// Binds a directory to a task. A root has no workdir by default, so this is
    /// how an autonomous session is told to run in an isolated worktree instead of
    /// in the project directory. The directory must already exist: the runtime
    /// resolves a path, it never creates one.
    pub fn set_workdir(&mut self, task_id: &TaskId, workdir: PathBuf) -> Result<(), String> {
        if !self.tasks.contains_key(task_id) {
            return Err("task does not exist".into());
        }
        self.workdirs.insert(task_id.clone(), Some(workdir));
        self.notify_update();
        Ok(())
    }

    /// The task's workspace mode. A registered entry wins; otherwise the root
    /// defaults to `Coding` (it owns session isolation) and any other task to
    /// `ReadOnly`. The root's implicit `Coding` is a default pending persisted
    /// mode hydration.
    /// The directory this task's parent asked it to run in, if any.
    pub fn spawn_workdir(&self, task_id: &TaskId) -> Option<PathBuf> {
        self.workdirs.get(task_id).cloned().flatten()
    }

    pub fn workspace_mode(&self, task_id: &TaskId) -> ChildWriteMode {
        if let Some(mode) = self.workspace_modes.get(task_id) {
            return *mode;
        }
        if task_id == &self.root_task_id {
            ChildWriteMode::Coding
        } else {
            ChildWriteMode::ReadOnly
        }
    }

    pub fn set_objective(&mut self, task_id: &TaskId, objective: String) -> Result<(), String> {
        if objective.trim().is_empty() {
            return Err("task objective must be non-empty".into());
        }
        if !self.tasks.contains_key(task_id) {
            return Err("task does not exist".into());
        }
        self.objectives.insert(task_id.clone(), objective);
        self.notify_update();
        Ok(())
    }

    /// Whether `candidate` is `caller` or is reachable from `caller` by
    /// descending through `parent_id`.
    pub fn is_descendant_of(&self, caller: &TaskId, candidate: &TaskId) -> bool {
        let mut current = self.task(candidate).and_then(|task| task.parent_id.clone());
        while let Some(id) = current {
            if &id == caller {
                return true;
            }
            current = self.task(&id).and_then(|task| task.parent_id.clone());
        }
        false
    }

    pub fn children_of(&self, task_id: &TaskId) -> &[TaskId] {
        self.children.get(task_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Tasks currently waiting for their direct parent's review.
    pub fn tasks_awaiting_parent_review(&self) -> Vec<TaskId> {
        let mut ids = self
            .tasks
            .iter()
            .filter(|(_, task)| matches!(task.state(), TaskState::AwaitingParentReview(_)))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ids.sort_by_key(|id| id.to_string());
        ids
    }

    pub fn events(&self) -> &[SupervisorEvent] {
        &self.events
    }

    pub fn mailbox(&self, task_id: &TaskId) -> Option<&Mailbox> {
        self.mailboxes.get(task_id)
    }

    /// High-priority parent/root mailbox work must be able to consume the
    /// provider coordination reserve on its next model turn.
    pub fn provider_turn_admission_priority(&self, task_id: &TaskId) -> AdmissionPriority {
        let is_root_or_parent =
            task_id == &self.root_task_id || !self.children_of(task_id).is_empty();
        if is_root_or_parent
            && self.mailbox(task_id).is_some_and(|mailbox| {
                mailbox.messages().iter().any(|message| {
                    !message.consumed_by_worker && message.priority <= MessagePriority::High
                })
            })
        {
            AdmissionPriority::High
        } else {
            AdmissionPriority::Normal
        }
    }

    pub fn has_worker(&self, task_id: &TaskId) -> bool {
        self.workers.contains_key(task_id)
    }

    pub fn worker_task_ids(&self) -> impl Iterator<Item = &TaskId> {
        self.workers.keys()
    }

    pub fn worker_cancellation(
        &self,
        task_id: &TaskId,
    ) -> Option<tokio_util::sync::CancellationToken> {
        self.workers
            .get(task_id)
            .map(WorkerHandle::cancellation_token)
    }

    /// Requests a cooperative safe checkpoint from every active worker. The
    /// caller is responsible for durably recording the paired runtime events.
    pub fn request_safe_checkpoints(&mut self) -> Result<Vec<TaskId>, String> {
        let task_ids = self.workers.keys().cloned().collect::<Vec<_>>();
        for task_id in &task_ids {
            self.pause_task(task_id, PauseReason("daemon is draining".into()))?;
        }
        Ok(task_ids)
    }

    /// Records a recovery boundary for workers that missed the daemon's safe
    /// checkpoint deadline. The handles are removed so no provider/tool work
    /// can be replayed by this process after shutdown begins.
    pub fn interrupt_unacknowledged_workers(&mut self) -> Result<Vec<TaskId>, String> {
        let task_ids = self.workers.keys().cloned().collect::<Vec<_>>();
        let mut affected = Vec::new();
        for task_id in &task_ids {
            let worker = self
                .workers
                .get(task_id)
                .expect("worker key was collected from this map");
            worker.cancel();
            let attempt_id = self
                .tasks
                .get(task_id)
                .expect("worker task is retained by its supervisor")
                .active_attempt_id()
                .clone();
            affected.extend(self.reduce_task(
                task_id,
                TaskEvent::RuntimeInterrupted {
                    attempt_id,
                    evidence: RecoveryEvidence("safe checkpoint grace deadline elapsed".into()),
                },
            )?);
            self.workers.remove(task_id);
            self.worker_message_capabilities.remove(task_id);
        }
        if !task_ids.is_empty() {
            self.notify_update();
        }
        Ok(affected)
    }

    /// Quarantines one worker at an ambiguous durable-delivery boundary. The
    /// message may be offered again only through the explicit recovery gate.
    pub fn interrupt_worker_for_recovery(
        &mut self,
        task_id: &TaskId,
        message_ids: &[super::task::MessageId],
        evidence: impl Into<String>,
    ) -> Result<(), String> {
        self.workers
            .get(task_id)
            .ok_or_else(|| "worker does not exist".to_string())?
            .cancel();
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::RuntimeInterrupted {
                attempt_id,
                evidence: RecoveryEvidence(evidence.into()),
            },
        )?;
        let mailbox = self
            .mailboxes
            .get_mut(task_id)
            .expect("task mailbox is created with task");
        for message_id in message_ids {
            mailbox.mark_pending_for_worker(message_id);
        }
        self.workers.remove(task_id);
        self.worker_message_capabilities.remove(task_id);
        self.notify_update();
        Ok(())
    }

    /// Creates a worker only after the task has passed supervised admission.
    pub async fn start_worker(
        &mut self,
        factory: &dyn AgentWorkerFactory,
        task_id: &TaskId,
    ) -> Result<(), String> {
        self.start_worker_with_provider_turn_gate(factory, task_id, None)
            .await
    }

    /// Creates a worker with runtime-owned provider-turn admission attached.
    pub async fn start_worker_with_provider_turn_gate(
        &mut self,
        factory: &dyn AgentWorkerFactory,
        task_id: &TaskId,
        provider_turn_gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> Result<(), String> {
        if self.workers.contains_key(task_id) {
            return Err("task already owns a worker".into());
        }
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let objective = self
            .objective(task_id)
            .ok_or_else(|| "task objective does not exist".to_string())?;
        let initial_user_messages = self
            .mailboxes
            .get(task_id)
            .expect("task mailbox is created with task")
            .pending_worker_inputs();
        let start = WorkerStart::new(
            task.id.clone(),
            task.active_attempt_id().clone(),
            task.root_session_id.clone(),
        )
        .with_objective(objective)
        .with_workspace_mode(self.workspace_mode(task_id))
        .maybe_with_inherited_sandbox(self.inherited_sandbox(task_id))
        .with_model(self.model(task_id).unwrap_or_default().to_string())
        .with_message_capability(Uuid::new_v4().to_string())
        .with_initial_user_messages(
            initial_user_messages
                .iter()
                .map(|(id, body)| WorkerMessage {
                    id: id.clone(),
                    body: body.clone(),
                })
                .collect(),
        );
        let start = if let Some(workspace) = task.workspace.clone() {
            start.with_workspace_lease(workspace)
        } else {
            start
        };
        let message_capability = start.message_capability.clone();
        // Admission is visible before the application factory can create any
        // side effects. A factory failure is reduced to a terminal task state.
        self.start_task(task_id)?;
        let handle = match factory
            .start_with_provider_turn_gate(start, provider_turn_gate)
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                self.fail_task(task_id, error.to_string())?;
                return Err(error.to_string());
            }
        };
        self.worker_message_capabilities
            .insert(task_id.clone(), message_capability);
        for (id, _) in initial_user_messages {
            let mailbox = self
                .mailboxes
                .get_mut(task_id)
                .expect("task mailbox is created with task");
            mailbox.mark_delivered_to_worker(&id);
        }
        self.workers.insert(task_id.clone(), handle);
        Ok(())
    }

    pub fn cancel_worker(&mut self, task_id: &TaskId) -> Result<(), String> {
        self.workers
            .get(task_id)
            .ok_or_else(|| "worker does not exist".to_string())?
            .cancel();
        Ok(())
    }

    pub fn assign_workspace(
        &mut self,
        task_id: &TaskId,
        workspace: WorkspaceLeaseId,
    ) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        task.workspace = Some(workspace);
        self.notify_update();
        Ok(())
    }

    /// Applies worker facts through the task reducer and discards handles for
    /// terminal tasks. A normal model completion without a structured delivery
    /// is intentionally a failure: coding completion requires review evidence.
    pub fn reconcile_worker_events(&mut self) -> Result<Vec<TaskId>, String> {
        let events = self
            .workers
            .iter()
            .flat_map(|(task_id, handle)| {
                handle
                    .take_events()
                    .into_iter()
                    .map(|event| (task_id.clone(), event))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut changed = Vec::new();
        for (task_id, event) in events {
            if let WorkerEvent::MessageConsumed { message_id } = event {
                if self.mailboxes.get(&task_id).is_some_and(|mailbox| {
                    mailbox
                        .pending_user_overrides()
                        .iter()
                        .any(|(id, _)| id == &message_id)
                }) {
                    self.pending_user_override_acks
                        .push((task_id.clone(), message_id));
                    self.notify_update();
                }
                continue;
            }
            let task = self
                .tasks
                .get(&task_id)
                .ok_or_else(|| "worker task does not exist".to_string())?;
            if task.state().is_terminal() {
                continue;
            }
            let mut affected = Vec::new();
            match event {
                WorkerEvent::MessageConsumed { .. } => unreachable!("handled before task state"),
                WorkerEvent::Delivered(delivery) => {
                    let (attempt_id, parent_id) = {
                        let attempt_id = self
                            .tasks
                            .get(&task_id)
                            .expect("worker task was checked above")
                            .active_attempt_id()
                            .clone();
                        let parent_id = self
                            .tasks
                            .get(&task_id)
                            .expect("worker task was checked above")
                            .parent_id
                            .clone()
                            .ok_or_else(|| {
                                "root tasks cannot submit parent delivery".to_string()
                            })?;
                        affected.extend(
                            self.reduce_task(
                                &task_id,
                                TaskEvent::WorkerDelivered {
                                    attempt_id: attempt_id.clone(),
                                    delivery: delivery.clone(),
                                },
                            )
                            .map_err(|error| error.to_string())?,
                        );
                        (attempt_id, parent_id)
                    };
                    // The child's terminal outcome is normally the parent's next
                    // input. A parent that has already gone terminal — its root
                    // was recovered, cancelled, or otherwise finished — can never
                    // accept that message. That is not the child's fault: the
                    // delivery is already recorded above, so the completed task
                    // must still be reported instead of letting this notification
                    // failure abort the whole reconciliation pass (which would
                    // strand the child in `running` forever and starve every other
                    // task in the same pass).
                    let notification = self.send_message(
                        &task_id,
                        MailboxMessageDraft::new(
                            task_id.clone(),
                            parent_id,
                            MessageKind::Completed(delivery),
                            Some(attempt_id),
                        ),
                    );
                    if let Err(error) = notification {
                        if !matches!(error, MessageDeliveryError::RecipientTerminal) {
                            return Err(error.to_string());
                        }
                    }
                }
                WorkerEvent::Completed { report } => {
                    let attempt_id = self
                        .tasks
                        .get(&task_id)
                        .expect("worker task was checked above")
                        .active_attempt_id()
                        .clone();
                    affected = self
                        .reduce_task(&task_id, TaskEvent::WorkerCompletedNoChanges { attempt_id })
                        .map_err(|error| error.to_string())?;
                    self.completion_reports.insert(task_id.clone(), report);
                }
                WorkerEvent::BudgetExhausted { report } => {
                    let attempt_id = self
                        .tasks
                        .get(&task_id)
                        .expect("worker task was checked above")
                        .active_attempt_id()
                        .clone();
                    affected = self
                        .reduce_task(
                            &task_id,
                            TaskEvent::WorkerBudgetExhausted {
                                attempt_id,
                                kind: BudgetKind::Turns,
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    // Keep the partial transcript: a truncated report is still
                    // the parent's only window into what the child managed.
                    self.completion_reports.insert(task_id.clone(), report);
                }
                WorkerEvent::Paused => {
                    let attempt_id = self
                        .tasks
                        .get(&task_id)
                        .expect("worker task was checked above")
                        .active_attempt_id()
                        .clone();
                    affected = self
                        .reduce_task(&task_id, TaskEvent::PauseAcknowledged { attempt_id })
                        .map_err(|error| error.to_string())?;
                }
                WorkerEvent::Failed(message) => {
                    let attempt_id = self
                        .tasks
                        .get(&task_id)
                        .expect("worker task was checked above")
                        .active_attempt_id()
                        .clone();
                    affected = self
                        .reduce_task(
                            &task_id,
                            TaskEvent::WorkerFailed {
                                attempt_id,
                                failure: TaskFailure::new(message),
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
                WorkerEvent::RecoveryConflict(message) => {
                    let attempt_id = self
                        .tasks
                        .get(&task_id)
                        .expect("worker task was checked above")
                        .active_attempt_id()
                        .clone();
                    affected = self
                        .reduce_task(
                            &task_id,
                            TaskEvent::RecoveryConflict {
                                attempt_id,
                                reason: BlockReason(format!("recovery_conflict: {message}")),
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
                WorkerEvent::Cancelled => {
                    // Cancelling the task reduces it to `Cancelled`, a settled
                    // terminal, so `reduce_task` inside the tree cancel already
                    // cascades to its live descendants.
                    affected = self.cancel_task_tree(&task_id, false)?;
                }
                WorkerEvent::CompletedWithoutDelivery => {
                    let attempt_id = self
                        .tasks
                        .get(&task_id)
                        .expect("worker task was checked above")
                        .active_attempt_id()
                        .clone();
                    affected = self
                        .reduce_task(
                            &task_id,
                            TaskEvent::WorkerFailed {
                                attempt_id,
                                failure: TaskFailure::new(
                                    "worker completed without a structured delivery report",
                                ),
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
            }
            for id in std::iter::once(task_id.clone()).chain(affected) {
                if self.tasks.get(&id).is_some_and(|task| {
                    task.state().is_terminal()
                        || matches!(
                            task.state(),
                            TaskState::Paused(_) | TaskState::AwaitingParentReview(_)
                        )
                }) {
                    self.workers.remove(&id);
                    self.worker_message_capabilities.remove(&id);
                    if !changed.contains(&id) {
                        changed.push(id);
                    }
                    self.notify_update();
                }
            }
        }
        Ok(changed)
    }

    /// Drains non-terminal watchdog facts without applying task transitions.
    /// The coordinator persists these separately from reducer-owned events.
    pub fn take_worker_watchdog_events(&self) -> Vec<(TaskId, WorkerWatchdogEvent)> {
        self.workers
            .iter()
            .flat_map(|(task_id, handle)| {
                handle
                    .take_watchdog_events()
                    .into_iter()
                    .map(|event| (task_id.clone(), event))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Drains trace facts buffered by running workers, keyed by the task that
    /// produced them. Destructive: a second call yields nothing until a worker
    /// reports again. Persistence belongs to the coordinator.
    pub fn take_worker_trace_events(&self) -> Vec<(TaskId, TraceFact)> {
        self.workers
            .iter()
            .flat_map(|(task_id, handle)| {
                handle
                    .take_trace_events()
                    .into_iter()
                    .map(|fact| (task_id.clone(), fact))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Drains external override acknowledgements for RuntimeCoordinator to
    /// durably persist after its worker reconciliation pass.
    pub fn pending_user_override_acks(&self) -> &[(TaskId, super::task::MessageId)] {
        &self.pending_user_override_acks
    }

    /// Compatibility view for callers inspecting acknowledgements. Acks remain
    /// staged until the coordinator confirms their durable transaction.
    pub fn take_consumed_user_override_ids(&self) -> Vec<(TaskId, super::task::MessageId)> {
        self.pending_user_override_acks.clone()
    }

    pub fn confirm_user_override_consumed(
        &mut self,
        task_id: &TaskId,
        message_id: &super::task::MessageId,
    ) {
        if self
            .mailboxes
            .get_mut(task_id)
            .is_some_and(|mailbox| mailbox.mark_user_override_consumed(message_id))
        {
            self.pending_user_override_acks
                .retain(|ack| ack != &(task_id.clone(), message_id.clone()));
        }
    }

    /// Cancels a task and, when requested, every descendant owned by this
    /// supervisor. State reduction accompanies token cancellation so callers
    /// never see a running task after its worker was signalled.
    ///
    /// The returned ids are the union of every [`Self::reduce_task`] result,
    /// not merely the ids the traversal planned to visit. A reduction to a
    /// settled terminal cascades in memory to descendants the traversal did not
    /// enumerate (in particular when `recursive == false`), and those cascade
    /// victims must reach the caller so the coordinator persists them and
    /// releases their leases.
    pub fn cancel_task_tree(
        &mut self,
        task_id: &TaskId,
        recursive: bool,
    ) -> Result<Vec<TaskId>, String> {
        if !self.tasks.contains_key(task_id) {
            return Err("task does not exist".into());
        }
        let mut task_ids = Vec::new();
        self.collect_cancellation_targets(task_id, recursive, &mut task_ids);
        let mut affected = Vec::new();
        for id in &task_ids {
            if let Some(worker) = self.workers.get(id) {
                worker.cancel();
            }
            if self
                .tasks
                .get(id)
                .is_some_and(|task| !task.state().is_terminal())
            {
                let attempt_id = self
                    .tasks
                    .get(id)
                    .expect("collected task exists")
                    .active_attempt_id()
                    .clone();
                affected.extend(self.reduce_task(
                    id,
                    TaskEvent::CancelRequested {
                        attempt_id,
                        reason: CancelReason("cancelled by runtime coordinator".into()),
                    },
                )?);
            }
        }
        self.notify_update();
        Ok(affected)
    }

    pub fn pause_task(&mut self, task_id: &TaskId, reason: PauseReason) -> Result<(), String> {
        if !self.workers.contains_key(task_id) {
            return Err("worker does not exist".to_string());
        }
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        // A pause request is not a terminal transition, so no cascade can fire
        // and the handle can be re-fetched after the reduction.
        self.reduce_task(task_id, TaskEvent::PauseRequested { attempt_id, reason })?;
        if let Some(worker) = self.workers.get(task_id) {
            worker.request_pause();
        }
        self.notify_update();
        Ok(())
    }

    pub fn resume_task(&mut self, task_id: &TaskId) -> Result<(), String> {
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(task_id, TaskEvent::ResumeRequested { attempt_id })?;
        self.workers.remove(task_id);
        self.worker_message_capabilities.remove(task_id);
        self.notify_update();
        Ok(())
    }

    fn collect_cancellation_targets(
        &self,
        task_id: &TaskId,
        recursive: bool,
        targets: &mut Vec<TaskId>,
    ) {
        targets.push(task_id.clone());
        if recursive {
            for child_id in self.children_of(task_id) {
                self.collect_cancellation_targets(child_id, true, targets);
            }
        }
    }

    pub fn subscribe_updates(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
    }

    /// Computes the non-blocking part of a child join. Runtime callers own
    /// waiting on the update receiver so the supervisor mutex stays available.
    pub fn wait_outcome(&self, caller: &TaskId, mode: WaitMode) -> Option<WaitOutcome> {
        let children = self.children_of(caller);
        let needs_attention = self.mailbox(caller).is_some_and(|mailbox| {
            mailbox
                .messages()
                .iter()
                .any(|message| message.priority <= MessagePriority::High)
        });
        let completed_children: Vec<TaskId> = children
            .iter()
            .filter(|child| {
                self.task(child)
                    .is_some_and(|task| task.state().is_terminal())
            })
            .cloned()
            .collect();
        let selected_children = match mode {
            WaitMode::Any => completed_children,
            WaitMode::All => children.to_vec(),
        };
        let complete = match mode {
            WaitMode::Any => !selected_children.is_empty(),
            WaitMode::All => {
                !children.is_empty()
                    && children.iter().all(|child| {
                        self.task(child)
                            .is_some_and(|task| task.state().is_terminal())
                    })
            }
        };
        if needs_attention {
            return Some(WaitOutcome::NeedsAttention {
                reports: self.completed_child_reports(&self.children_needing_attention(caller)),
            });
        }
        complete.then(|| WaitOutcome::Completed {
            reports: self.completed_child_reports(&selected_children),
            children: selected_children,
        })
    }

    /// Children whose outcome the caller must act on: finished children, and
    /// children holding a delivery or report that is waiting for the caller's
    /// review. A delivery is not terminal yet, so terminality alone would hide
    /// exactly the child the caller has to merge.
    fn children_needing_attention(&self, caller: &TaskId) -> Vec<TaskId> {
        self.children_of(caller)
            .iter()
            .filter(|child| {
                self.task(child).is_some_and(|task| {
                    task.state().is_terminal()
                        || task.active_attempt().delivery.is_some()
                        || self.completion_reports.contains_key(*child)
                })
            })
            .cloned()
            .collect()
    }

    pub fn child_completion_snapshot(
        &self,
        caller: &TaskId,
    ) -> (Vec<TaskId>, Vec<CompletedChildReport>) {
        let children = self.children_of(caller).to_vec();
        let terminal_children = children
            .iter()
            .filter(|child| {
                self.task(child)
                    .is_some_and(|task| task.state().is_terminal())
            })
            .cloned()
            .collect::<Vec<_>>();
        let reports = self.completed_child_reports(&terminal_children);
        (children, reports)
    }

    fn completed_child_reports(&self, children: &[TaskId]) -> Vec<CompletedChildReport> {
        children
            .iter()
            .filter_map(|child| {
                let task = self.task(child)?;
                Some(CompletedChildReport {
                    task_id: child.clone(),
                    state: task_state_label(task.state()).to_string(),
                    report: self.completion_reports.get(child).cloned(),
                    delivery: task
                        .active_attempt()
                        .delivery
                        .as_ref()
                        .map(|delivery| delivery.commit.clone()),
                })
            })
            .collect()
    }

    fn notify_update(&self) {
        self.updates
            .send_modify(|epoch| *epoch = epoch.saturating_add(1));
    }

    pub fn spawn(&mut self, parent_id: TaskId) -> Result<TaskId, SpawnError> {
        self.spawn_with_objective(
            parent_id,
            SpawnRequest::new(
                "Complete the delegated task.".into(),
                ChildWriteMode::ReadOnly,
                None,
            ),
        )
    }

    pub fn spawn_with_objective(
        &mut self,
        parent_id: TaskId,
        request: SpawnRequest,
    ) -> Result<TaskId, SpawnError> {
        let parent = self
            .tasks
            .get(&parent_id)
            .ok_or(SpawnError::ParentNotFound)?;
        let child_depth = parent
            .depth
            .can_spawn_child()
            .map_err(|_| SpawnError::MaximumDepthReached)?;
        let active_direct_children = self
            .children_of(&parent_id)
            .iter()
            .filter(|child| {
                self.task(child)
                    .is_some_and(|task| !task.state().is_terminal())
            })
            .count();
        if active_direct_children >= MAX_DIRECT_CHILDREN {
            return Err(SpawnError::DirectChildLimitReached);
        }

        let mut child = AgentTask::new_child(parent.root_session_id.clone(), parent_id.clone())
            .with_workspace(WorkspaceLeaseId::new());
        child.depth = child_depth;
        let child_id = child.id.clone();
        self.tasks.insert(child_id.clone(), child);
        self.objectives
            .insert(child_id.clone(), request.objective.clone());
        self.workspace_modes.insert(child_id.clone(), request.mode);
        // A child starts with no recorded inheritance; the caller (coordinator)
        // seeds it from the persisted task row before the worker starts.
        self.inherited_sandboxes.remove(&child_id);
        self.workdirs.insert(child_id.clone(), request.workdir);
        self.mailboxes.insert(child_id.clone(), Mailbox::default());
        self.children
            .entry(parent_id.clone())
            .or_default()
            .push(child_id.clone());
        self.events.push(SupervisorEvent::TaskSpawned {
            parent_id,
            task_id: child_id.clone(),
        });
        self.notify_update();
        Ok(child_id)
    }

    pub fn send_message(
        &mut self,
        sender: &TaskId,
        draft: MailboxMessageDraft,
    ) -> Result<(), MessageDeliveryError> {
        let recipient = draft.recipient().clone();
        let worker_message = match draft.kind() {
            MessageKind::UserInstruction(UserInstruction(body)) => Some(body.clone()),
            MessageKind::Rework(instruction) => Some(instruction.0.clone()),
            // A child's terminal outcome is the parent's next input: the parent
            // acts on a delivery even when it never called wait_agent.
            MessageKind::Completed(delivery) => {
                Some(format!("Your child delivered commit {}.", delivery.commit))
            }
            MessageKind::Failed(failure) => Some(format!(
                "Your child's task failed: {}. Investigate before continuing.",
                failure.0
            )),
            MessageKind::Blocked(reason) => Some(format!(
                "Your child's task is blocked: {}. Resolve the blocker before continuing.",
                reason.0
            )),
            _ => None,
        };
        let sender_task = self
            .tasks
            .get(sender)
            .ok_or(MessageDeliveryError::SenderNotFound)?;
        if sender_task.state().is_terminal() {
            return Err(MessageDeliveryError::SenderTerminal);
        }
        let recipient_task = self
            .tasks
            .get(&recipient)
            .ok_or(MessageDeliveryError::RecipientNotFound)?;
        if recipient_task.state().is_terminal() {
            return Err(MessageDeliveryError::RecipientTerminal);
        }
        let is_parent = self
            .tasks
            .get(sender)
            .is_some_and(|sender_task| sender_task.parent_id.as_ref() == Some(&recipient));
        let is_child = self
            .children_of(sender)
            .iter()
            .any(|child| child == &recipient);
        if !is_parent && !is_child {
            return Err(MessageDeliveryError::RecipientNotAdjacent);
        }
        let receipt = self
            .mailboxes
            .get_mut(&recipient)
            .expect("task mailbox is created with task")
            .push(draft);
        if let (Some(body), Some(worker)) = (worker_message, self.workers.get(&recipient)) {
            worker.deliver_worker_message(WorkerMessage {
                id: receipt.message_id,
                body,
            });
        }
        self.notify_update();
        Ok(())
    }

    /// Restores or stages an already-authorized durable mailbox fact. This is
    /// reserved for runtime hydration/review transactions and deliberately
    /// does not weaken ordinary live sender, adjacency, or terminal checks.
    pub fn stage_persisted_message(
        &mut self,
        draft: MailboxMessageDraft,
    ) -> Result<(), MessageDeliveryError> {
        let recipient = draft.recipient().clone();
        let mailbox = self
            .mailboxes
            .get_mut(&recipient)
            .ok_or(MessageDeliveryError::RecipientNotFound)?;
        mailbox.push(draft);
        Ok(())
    }

    /// Bridges a committed external mailbox fact to an already-running worker.
    /// Callers must not invoke this until the matching repository transaction
    /// commits because a `WorkerHandle` delivery cannot be rolled back.
    pub fn deliver_committed_user_instruction(
        &mut self,
        recipient: &TaskId,
        message_id: &super::task::MessageId,
    ) -> Result<(), String> {
        let body = self
            .mailboxes
            .get(recipient)
            .ok_or_else(|| "recipient task does not exist".to_string())?
            .user_instruction(message_id)
            .ok_or_else(|| "committed user instruction does not exist".to_string())?;
        if let Some(worker) = self.workers.get(recipient) {
            worker.deliver_worker_message(WorkerMessage {
                id: message_id.clone(),
                body,
            });
            self.mailboxes
                .get_mut(recipient)
                .expect("recipient mailbox was checked")
                .mark_delivered_to_worker(message_id);
        }
        self.notify_update();
        Ok(())
    }

    pub fn pending_rework_message_ids(&self, task_id: &TaskId) -> Vec<super::task::MessageId> {
        let Some(mailbox) = self.mailboxes.get(task_id) else {
            return Vec::new();
        };
        mailbox
            .pending_worker_inputs()
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| mailbox.is_rework(id))
            .collect()
    }

    /// Delivers the daemon's text-only control message through the same
    /// adjacency and terminal-state checks as the model-facing tool.
    pub fn send_user_message(
        &mut self,
        sender: &TaskId,
        recipient: TaskId,
        message: String,
    ) -> Result<(), MessageDeliveryError> {
        self.send_message(
            sender,
            MailboxMessageDraft::new(
                sender.clone(),
                recipient.clone(),
                MessageKind::UserInstruction(UserInstruction(message)),
                None,
            ),
        )
    }

    /// Authenticates daemon-worker IPC before applying the ordinary adjacency
    /// rules. UI clients never receive this random per-worker capability.
    pub fn can_use_worker_capability(
        &self,
        task: &TaskId,
        capability: &str,
    ) -> Result<(), MessageDeliveryError> {
        if self
            .worker_message_capabilities
            .get(task)
            .is_none_or(|expected| expected != capability)
        {
            return Err(MessageDeliveryError::SenderNotFound);
        }
        let task = self
            .tasks
            .get(task)
            .ok_or(MessageDeliveryError::SenderNotFound)?;
        if task.state().is_terminal() {
            return Err(MessageDeliveryError::SenderTerminal);
        }
        Ok(())
    }

    pub fn pause_foreground_task(
        &mut self,
        task_id: &TaskId,
        reason: PauseReason,
    ) -> Result<(), String> {
        if self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .state()
            .is_terminal()
        {
            return Err("terminal task cannot be paused".into());
        }
        if matches!(
            self.tasks.get(task_id).map(|task| task.state()),
            Some(TaskState::Paused(_))
        ) {
            return Ok(());
        }
        let attempt_id = self
            .tasks
            .get(task_id)
            .expect("task existence was checked above")
            .active_attempt_id()
            .clone();
        self.reduce_task(task_id, TaskEvent::PauseRequested { attempt_id, reason })?;
        self.notify_update();
        Ok(())
    }
    pub fn send_worker_message(
        &mut self,
        sender: &TaskId,
        capability: &str,
        recipient: TaskId,
        message: String,
    ) -> Result<(), MessageDeliveryError> {
        self.can_send_worker_message(sender, capability, &recipient)?;
        self.send_user_message(sender, recipient, message)
    }

    pub fn can_send_worker_message(
        &self,
        sender: &TaskId,
        capability: &str,
        recipient: &TaskId,
    ) -> Result<(), MessageDeliveryError> {
        self.can_use_worker_capability(sender, capability)?;
        let sender_task = self
            .tasks
            .get(sender)
            .ok_or(MessageDeliveryError::SenderNotFound)?;
        let recipient_task = self
            .tasks
            .get(recipient)
            .ok_or(MessageDeliveryError::RecipientNotFound)?;
        if recipient_task.state().is_terminal() {
            return Err(MessageDeliveryError::RecipientTerminal);
        }
        let is_parent = sender_task.parent_id.as_ref() == Some(recipient);
        let is_child = self
            .children_of(sender)
            .iter()
            .any(|child| child == recipient);
        if !is_parent && !is_child {
            return Err(MessageDeliveryError::RecipientNotAdjacent);
        }
        Ok(())
    }

    /// User controls may cross the task tree, but may never target a terminal
    /// task. The persisted sender is `None`, preserving the external actor.
    pub fn send_user_override(
        &mut self,
        recipient: TaskId,
        message: String,
    ) -> Result<(), MessageDeliveryError> {
        self.send_user_override_with_id(super::task::MessageId::new(), recipient, message)
    }

    /// Delivers a persisted external override under its durable mailbox ID.
    pub fn send_user_override_with_id(
        &mut self,
        message_id: super::task::MessageId,
        recipient: TaskId,
        message: String,
    ) -> Result<(), MessageDeliveryError> {
        self.can_accept_user_override(&recipient)?;
        let receipt = self
            .mailboxes
            .get_mut(&recipient)
            .expect("task mailbox is created with task")
            .push(MailboxMessageDraft::user_override_with_id(
                message_id,
                recipient.clone(),
                message.clone(),
            ));
        if let Some(worker) = self.workers.get(&recipient) {
            worker.deliver_worker_message(WorkerMessage {
                id: receipt.message_id.clone(),
                body: message,
            });
            self.mailboxes
                .get_mut(&recipient)
                .expect("task mailbox is created with task")
                .mark_delivered_to_worker(&receipt.message_id);
        }
        self.notify_update();
        Ok(())
    }

    pub fn can_accept_user_override(&self, recipient: &TaskId) -> Result<(), MessageDeliveryError> {
        let recipient_task = self
            .tasks
            .get(recipient)
            .ok_or(MessageDeliveryError::RecipientNotFound)?;
        if recipient_task.state().is_terminal() {
            return Err(MessageDeliveryError::RecipientTerminal);
        }
        Ok(())
    }

    pub fn fail_task(
        &mut self,
        task_id: &TaskId,
        message: impl Into<String>,
    ) -> Result<(), String> {
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::WorkerFailed {
                attempt_id,
                failure: TaskFailure::new(message),
            },
        )?;
        self.notify_update();
        Ok(())
    }

    pub fn stall_task(
        &mut self,
        task_id: &TaskId,
        evidence: WatchdogEvidence,
    ) -> Result<(), String> {
        self.reduce_watchdog_event(task_id, |attempt_id| TaskEvent::WatchdogStalled {
            attempt_id,
            evidence,
        })
    }

    pub fn timeout_task(&mut self, task_id: &TaskId, kind: TimeoutKind) -> Result<(), String> {
        self.reduce_watchdog_event(task_id, |attempt_id| TaskEvent::WatchdogTimedOut {
            attempt_id,
            kind,
        })
    }

    pub fn exhaust_task_budget(
        &mut self,
        task_id: &TaskId,
        kind: BudgetKind,
    ) -> Result<(), String> {
        self.reduce_watchdog_event(task_id, |attempt_id| TaskEvent::WatchdogBudgetExhausted {
            attempt_id,
            kind,
        })
    }

    fn reduce_watchdog_event(
        &mut self,
        task_id: &TaskId,
        event: impl FnOnce(AttemptId) -> TaskEvent,
    ) -> Result<(), String> {
        if let Some(worker) = self.workers.get(task_id) {
            worker.cancel();
        }
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(task_id, event(attempt_id))?;
        self.notify_update();
        Ok(())
    }

    pub fn start_task(&mut self, task_id: &TaskId) -> Result<(), String> {
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(task_id, TaskEvent::AdmissionGranted { attempt_id })?;
        self.notify_update();
        Ok(())
    }

    /// Places a running task in its reducer-owned permission wait state.
    pub fn request_permission(
        &mut self,
        task_id: &TaskId,
        request: PermissionRequestId,
    ) -> Result<AttemptId, String> {
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::PermissionRequested {
                attempt_id: attempt_id.clone(),
                request,
            },
        )?;
        self.notify_update();
        Ok(attempt_id)
    }

    /// Applies an immutable permission decision through the same task reducer
    /// used by workers, so an external control surface cannot invent state.
    pub fn resolve_permission(
        &mut self,
        task_id: &TaskId,
        request: PermissionRequestId,
        decision: PermissionDecision,
    ) -> Result<AttemptId, String> {
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::PermissionResolved {
                attempt_id: attempt_id.clone(),
                request,
                decision,
            },
        )?;
        self.notify_update();
        Ok(attempt_id)
    }

    /// Applies an accepted delivery through the task reducer. The reducer
    /// verifies both direct-parent ownership and integration evidence.
    pub fn accept_review(
        &mut self,
        task_id: &TaskId,
        actor: &TaskId,
        delivery_id: DeliveryId,
        integration: IntegrationValidation,
    ) -> Result<AttemptId, String> {
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::ReviewAccepted {
                attempt_id: attempt_id.clone(),
                actor: actor.clone(),
                delivery_id,
                integration,
            },
        )?;
        self.notify_update();
        Ok(attempt_id)
    }

    /// Requests rework of the exact reviewed delivery and returns the newly
    /// created attempt for durable persistence by the runtime coordinator.
    pub fn rework_review(
        &mut self,
        task_id: &TaskId,
        actor: &TaskId,
        delivery_id: DeliveryId,
        feedback: MessageId,
    ) -> Result<TaskAttempt, String> {
        let successor = self.reduce_rework_review(task_id, actor, delivery_id, feedback)?;
        self.notify_update();
        Ok(successor)
    }

    /// Stages reducer/mailbox changes while the caller holds the supervisor
    /// lock, then commits the matching repository transaction. Failed staging
    /// or persistence restores both in-memory snapshots before returning.
    pub fn stage_review_with_persistence<S, T, E>(
        &mut self,
        stage: impl FnOnce(&mut Self) -> Result<S, String>,
        persist: impl FnOnce(&S) -> Result<T, E>,
    ) -> Result<(S, T), ReviewPersistenceError<E>> {
        let original_tasks = self.tasks.clone();
        let original_mailboxes = self.mailboxes.clone();
        let staged = match stage(self) {
            Ok(staged) => staged,
            Err(error) => {
                self.tasks = original_tasks;
                self.mailboxes = original_mailboxes;
                return Err(ReviewPersistenceError::Supervisor(error));
            }
        };
        let persisted = match persist(&staged) {
            Ok(persisted) => persisted,
            Err(error) => {
                self.tasks = original_tasks;
                self.mailboxes = original_mailboxes;
                return Err(ReviewPersistenceError::Persistence(error));
            }
        };
        self.notify_update();
        Ok((staged, persisted))
    }

    fn reduce_rework_review(
        &mut self,
        task_id: &TaskId,
        actor: &TaskId,
        delivery_id: DeliveryId,
        feedback: MessageId,
    ) -> Result<TaskAttempt, String> {
        if self
            .tasks
            .get(task_id)
            .and_then(|task| task.parent_id.as_ref())
            != Some(actor)
        {
            return Err("review actor does not match direct parent".into());
        }
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::ReviewRework {
                attempt_id,
                delivery_id,
                feedback,
            },
        )?;
        // `ReviewRework` always installs a successor attempt as the task's
        // active attempt, so that is the attempt this method reports.
        let successor = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt()
            .clone();
        Ok(successor)
    }

    /// Rejects the exact reviewed delivery through reducer-owned state.
    pub fn reject_review(
        &mut self,
        task_id: &TaskId,
        actor: &TaskId,
        delivery_id: DeliveryId,
        reason: MessageId,
    ) -> Result<AttemptId, String> {
        if self
            .tasks
            .get(task_id)
            .and_then(|task| task.parent_id.as_ref())
            != Some(actor)
        {
            return Err("review actor does not match direct parent".into());
        }
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::ReviewRejected {
                attempt_id: attempt_id.clone(),
                delivery_id,
                reason: reason.clone(),
            },
        )?;
        self.stage_persisted_message(MailboxMessageDraft::new_with_id(
            reason.clone(),
            actor.clone(),
            task_id.clone(),
            MessageKind::ReviewRejected(reason),
            Some(attempt_id.clone()),
        ))
        .map_err(|error| error.to_string())?;
        self.notify_update();
        Ok(attempt_id)
    }

    pub fn record_recovery_conflict(
        &mut self,
        task_id: &TaskId,
        message: impl Into<String>,
    ) -> Result<(), String> {
        if matches!(
            self.task(task_id).map(AgentTask::state),
            Some(TaskState::Queued)
        ) {
            self.start_task(task_id)?;
        }
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(
            task_id,
            TaskEvent::RecoveryConflict {
                attempt_id,
                reason: BlockReason(format!("recovery_conflict: {}", message.into())),
            },
        )?;
        self.notify_update();
        Ok(())
    }

    /// Creates a fresh attempt only after a terminal outcome, preserving the
    /// prior attempt's evidence for later user inspection.
    pub fn retry_task(&mut self, task_id: &TaskId) -> Result<super::task::TaskAttempt, String> {
        if !self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .state()
            .is_terminal()
        {
            return Err("retry requires a terminal task state".into());
        }
        // A cancelled worker may still have a handle until its asynchronous
        // event is reconciled; a successor attempt must not inherit it.
        self.workers.remove(task_id);
        self.worker_message_capabilities.remove(task_id);
        let attempt_id = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt_id()
            .clone();
        self.reduce_task(task_id, TaskEvent::RetryRequested { attempt_id })?;
        // `RetryRequested` installs the successor as the active attempt.
        let next = self
            .tasks
            .get(task_id)
            .ok_or_else(|| "task does not exist".to_string())?
            .active_attempt()
            .clone();
        self.notify_update();
        Ok(next)
    }

    pub fn resume_recovery_task(
        &mut self,
        task_id: &TaskId,
    ) -> Result<super::task::TaskAttempt, String> {
        if !matches!(
            self.task(task_id).map(|task| task.state()),
            Some(TaskState::RecoveryRequired(_))
        ) {
            return Err("recovery resume requires a recovery-required task".into());
        }
        self.retry_task(task_id)
    }

    /// After `task_id` has been reduced, enforce the tree invariant: a task in a
    /// settled terminal state must not leave live descendants behind. Returns only
    /// ids whose state actually changed, so callers persist exactly what moved.
    fn settle_terminal(&mut self, task_id: &TaskId) -> Vec<TaskId> {
        let Some(state) = self.tasks.get(task_id).map(|task| task.state().clone()) else {
            return Vec::new();
        };
        if classify_terminal(&state) != TerminalKind::Settled {
            return Vec::new();
        }
        if matches!(state, TaskState::Completed | TaskState::CompletedNoChanges) {
            let live: Vec<TaskId> = self
                .collect_subtree(task_id)
                .into_iter()
                .filter(|id| {
                    self.tasks.get(id).is_some_and(|task| {
                        classify_terminal(task.state()) == TerminalKind::NonTerminal
                    })
                })
                .collect();
            if !live.is_empty() {
                tracing::warn!(
                    ?task_id,
                    live_children = live.len(),
                    "task reached a successful terminal state while live descendants remained; cascading cancellation"
                );
            }
        }
        let mut changed = Vec::new();
        for id in self.collect_subtree(task_id) {
            if id == *task_id {
                continue;
            }
            let Some(task) = self.tasks.get_mut(&id) else {
                continue;
            };
            if classify_terminal(task.state()) != TerminalKind::NonTerminal {
                continue;
            }
            if let Some(worker) = self.workers.get(&id) {
                worker.cancel();
            }
            let attempt_id = task.active_attempt_id().clone();
            if task
                .reduce(
                    TaskEvent::CancelRequested {
                        attempt_id,
                        reason: CancelReason("parent reached a terminal state".into()),
                    },
                    chrono::Utc::now(),
                )
                .is_ok()
            {
                changed.push(id);
            }
        }
        if !changed.is_empty() {
            self.notify_update();
        }
        changed
    }

    fn collect_subtree(&self, task_id: &TaskId) -> Vec<TaskId> {
        let mut out = Vec::new();
        self.collect_subtree_into(task_id, &mut out);
        out
    }

    fn collect_subtree_into(&self, task_id: &TaskId, out: &mut Vec<TaskId>) {
        out.push(task_id.clone());
        for child in self.children_of(task_id) {
            self.collect_subtree_into(child, out);
        }
    }

    /// The single entry point every state-changing reduce must go through. It
    /// performs the reduction, then enforces the parent-terminal cascade. Returns
    /// every task id whose state changed (the reduced task plus any cascade
    /// victims), so the runtime coordinator can persist and release each one.
    ///
    /// `RecoveryRequired` is deliberately excluded from the cascade: it is
    /// terminal for bookkeeping but *parked and resumable*, so cancelling its
    /// descendants would destroy children a resumed attempt still needs.
    pub fn reduce_task(
        &mut self,
        task_id: &TaskId,
        event: TaskEvent,
    ) -> Result<Vec<TaskId>, String> {
        {
            let task = self
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| "task does not exist".to_string())?;
            task.reduce(event, chrono::Utc::now())
                .map_err(|error| error.to_string())?;
        }
        let mut changed = vec![task_id.clone()];
        changed.extend(self.settle_terminal(task_id));
        self.notify_update();
        Ok(changed)
    }
}

/// Whether a state should trigger the parent-terminal cascade. Deliberately
/// narrower than `TaskState::is_terminal`: `RecoveryRequired` is terminal for
/// bookkeeping but is a *parked, resumable* state, so cascading on it would
/// kill children the parent could still need after a resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalKind {
    NonTerminal,
    Settled,
    RecoveryRequired,
}

fn classify_terminal(state: &TaskState) -> TerminalKind {
    match state {
        TaskState::Completed
        | TaskState::CompletedNoChanges
        | TaskState::Blocked(_)
        | TaskState::Stalled(_)
        | TaskState::TimedOut(_)
        | TaskState::BudgetExhausted(_)
        | TaskState::Failed(_)
        | TaskState::Cancelled(_) => TerminalKind::Settled,
        TaskState::RecoveryRequired(_) => TerminalKind::RecoveryRequired,
        _ => TerminalKind::NonTerminal,
    }
}

fn task_state_label(state: &TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Running => "running",
        TaskState::WaitingForResource(_) => "waiting_for_resource",
        TaskState::WaitingForPermission(_) => "waiting_for_permission",
        TaskState::WaitingForChildren(_) => "waiting_for_children",
        TaskState::Paused(_) => "paused",
        TaskState::AwaitingParentReview(_) => "awaiting_parent_review",
        TaskState::Completed => "completed",
        TaskState::CompletedNoChanges => "completed_no_changes",
        TaskState::Blocked(_) => "blocked",
        TaskState::Stalled(_) => "stalled",
        TaskState::TimedOut(_) => "timed_out",
        TaskState::BudgetExhausted(_) => "budget_exhausted",
        TaskState::Failed(_) => "failed",
        TaskState::Cancelled(_) => "cancelled",
        TaskState::RecoveryRequired(_) => "recovery_required",
    }
}

/// The per-task built-in tool set. Tool calls delegate to the Supervisor; they
/// never create worker state independently.
#[derive(Clone)]
pub struct SupervisorTools {
    supervisor: Arc<Mutex<AgentSupervisor>>,
    caller: TaskId,
    updates: watch::Receiver<u64>,
}

impl SupervisorTools {
    pub fn new(supervisor: Arc<Mutex<AgentSupervisor>>, caller: TaskId) -> Self {
        let updates = supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .subscribe_updates();
        Self {
            supervisor,
            caller,
            updates,
        }
    }

    pub fn spawn_agent(&self) -> Arc<dyn Tool> {
        Arc::new(SpawnAgentTool {
            tools: self.clone(),
        })
    }

    pub fn wait_agent(&self) -> Arc<dyn Tool> {
        Arc::new(WaitAgentTool {
            tools: self.clone(),
        })
    }

    pub fn send_message(&self) -> Arc<dyn Tool> {
        Arc::new(SendMessageTool {
            tools: self.clone(),
        })
    }

    pub fn register_into(&self, registry: &mut ToolRegistry) {
        registry.register(self.spawn_agent());
        registry.register(self.wait_agent());
        registry.register(self.send_message());
    }
}

struct SpawnAgentTool {
    tools: SupervisorTools,
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Create an asynchronously scheduled direct child agent."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "Delegated objective." },
                "mode": {
                    "type": "string",
                    "enum": ["coding", "read_only"],
                    "description": "Use 'coding' only when the child must change files. Defaults to 'read_only'."
                },
                "workdir": {
                    "type": "string",
                    "description": "Directory the child works in. Required for 'coding': create it yourself with `git worktree add <path> -b <branch>` first."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task) = args.get("task").and_then(Value::as_str) else {
            return ToolResult::error("task is required");
        };
        if task.trim().is_empty() {
            return ToolResult::error("task must not be empty");
        }
        let mode = match args.get("mode") {
            None => ChildWriteMode::ReadOnly,
            Some(Value::String(value)) => match ChildWriteMode::parse(value) {
                Some(mode) => mode,
                None => return ToolResult::error("mode must be 'coding' or 'read_only'"),
            },
            Some(_) => return ToolResult::error("mode must be a string"),
        };
        let workdir = match args.get("workdir") {
            None => None,
            Some(Value::String(value)) if !value.trim().is_empty() => Some(PathBuf::from(value)),
            Some(Value::String(_)) => return ToolResult::error("workdir must not be blank"),
            Some(_) => return ToolResult::error("workdir must be a string"),
        };
        let mut supervisor = self
            .tools
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let request = SpawnRequest::new(task.to_string(), mode, workdir);
        match supervisor.spawn_with_objective(self.tools.caller.clone(), request) {
            Ok(task_id) => ToolResult::text(
                json!({ "task_id": task_id.to_string(), "status": "queued" }).to_string(),
            ),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}

struct WaitAgentTool {
    tools: SupervisorTools,
}

#[async_trait]
impl Tool for WaitAgentTool {
    fn name(&self) -> &str {
        "wait_agent"
    }

    fn description(&self) -> &str {
        "Wait for one, all, or any direct child to report a terminal result."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "mode": { "type": "string", "enum": ["one", "all", "any"] } },
            "required": ["mode"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let mode = match args.get("mode").and_then(Value::as_str) {
            Some("one" | "any") => WaitMode::Any,
            Some("all") => WaitMode::All,
            _ => return ToolResult::error("mode must be one, any, or all"),
        };
        let mut updates = self.tools.updates.clone();
        loop {
            let status = {
                let supervisor = self
                    .tools
                    .supervisor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                supervisor
                    .wait_outcome(&self.tools.caller, mode)
                    .map(|outcome| match outcome {
                        WaitOutcome::NeedsAttention { reports } => (
                            "needs_attention",
                            Vec::new(),
                            reports
                                .into_iter()
                                .map(|report| {
                                    json!({
                                        "task_id": report.task_id.to_string(),
                                        "state": report.state,
                                        "report": report.report,
                                        "delivery": report.delivery,
                                    })
                                })
                                .collect::<Vec<_>>(),
                        ),
                        WaitOutcome::Completed { children, reports } => (
                            "completed",
                            children
                                .into_iter()
                                .map(|child| child.to_string())
                                .collect(),
                            reports
                                .into_iter()
                                .map(|report| {
                                    json!({
                                        "task_id": report.task_id.to_string(),
                                        "state": report.state,
                                        "report": report.report,
                                        "delivery": report.delivery,
                                    })
                                })
                                .collect(),
                        ),
                    })
            };
            if let Some((status, children, reports)) = status {
                return ToolResult::text(
                    json!({ "status": status, "children": children, "reports": reports })
                        .to_string(),
                );
            }
            if updates.changed().await.is_err() {
                return ToolResult::error("supervisor is no longer available");
            }
        }
    }
}

struct SendMessageTool {
    tools: SupervisorTools,
}

// This focused test intentionally sits beside the provider-turn routing API.
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod provider_turn_priority_tests {
    use super::*;
    use crate::subagent::mailbox::ReworkInstruction;
    use crate::subagent::task::{
        DeliveryReport, IntegrationValidation, MessageId, PermissionDecision, PermissionRequestId,
        WorkspaceLeaseId,
    };

    #[test]
    fn spawn_request_carries_the_parents_workdir() {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let root = supervisor.root_task_id().clone();
        let child = supervisor
            .spawn_with_objective(
                root,
                SpawnRequest::new(
                    "implement".into(),
                    ChildWriteMode::Coding,
                    Some(PathBuf::from("/tmp/yi-agent-impl")),
                ),
            )
            .unwrap();

        assert_eq!(
            supervisor.spawn_workdir(&child),
            Some(PathBuf::from("/tmp/yi-agent-impl"))
        );
    }

    #[test]
    fn parent_routes_pending_high_priority_mail_to_the_coordination_reserve() {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let root = supervisor.root_task_id().clone();
        let child = supervisor.spawn(root.clone()).unwrap();

        assert_eq!(
            supervisor.provider_turn_admission_priority(&root),
            AdmissionPriority::Normal
        );
        supervisor
            .send_message(
                &child,
                MailboxMessageDraft::new(
                    child.clone(),
                    root.clone(),
                    MessageKind::Rework(ReworkInstruction("review this result".into())),
                    None,
                ),
            )
            .unwrap();

        assert_eq!(
            supervisor.provider_turn_admission_priority(&root),
            AdmissionPriority::High
        );
    }

    #[test]
    fn permission_resolution_is_reduced_by_the_owning_supervisor() {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let root = supervisor.root_task_id().clone();
        let request = PermissionRequestId::new();
        supervisor.start_task(&root).unwrap();

        supervisor
            .request_permission(&root, request.clone())
            .unwrap();
        assert_eq!(
            supervisor.task(&root).unwrap().state(),
            &TaskState::WaitingForPermission(request.clone())
        );

        supervisor
            .resolve_permission(&root, request, PermissionDecision::Allow)
            .unwrap();
        assert_eq!(supervisor.task(&root).unwrap().state(), &TaskState::Queued);
    }

    #[test]
    fn worker_delivery_transitions_a_child_to_direct_parent_review() {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let root = supervisor.root_task_id().clone();
        let child = supervisor.spawn(root).unwrap();
        let workspace = WorkspaceLeaseId::new();
        supervisor.tasks.get_mut(&child).unwrap().workspace = Some(workspace.clone());
        supervisor.start_task(&child).unwrap();
        let handle = WorkerHandle::new(tokio_util::sync::CancellationToken::new());
        supervisor.workers.insert(child.clone(), handle.clone());
        let delivery = DeliveryReport::coding("deadbeef", "main", workspace, "checks passed");

        handle.report_delivery(delivery.clone());
        supervisor.reconcile_worker_events().unwrap();

        assert_eq!(
            supervisor.task(&child).unwrap().state(),
            &TaskState::AwaitingParentReview(delivery.id)
        );
    }

    #[test]
    fn tasks_awaiting_parent_review_lists_only_review_waiters() {
        let (supervisor, _parent, child, _delivery) = delivered_child();

        assert_eq!(supervisor.tasks_awaiting_parent_review(), vec![child]);
    }

    #[test]
    fn tasks_awaiting_parent_review_is_empty_without_review_waiters() {
        let supervisor = AgentSupervisor::new(RootSessionId::new());
        assert!(supervisor.tasks_awaiting_parent_review().is_empty());
    }

    #[test]
    fn direct_parent_accepts_a_validated_child_delivery() {
        let (mut supervisor, parent, child, delivery) = delivered_child();

        supervisor
            .accept_review(
                &child,
                &parent,
                delivery.id.clone(),
                IntegrationValidation::passed("parent integration passed"),
            )
            .unwrap();

        assert_eq!(
            supervisor.task(&child).unwrap().state(),
            &TaskState::Completed
        );
    }

    #[test]
    fn direct_parent_rework_creates_a_successor_attempt() {
        let (mut supervisor, parent, child, delivery) = delivered_child();
        let previous_attempt = supervisor.task(&child).unwrap().active_attempt_id().clone();

        let successor = supervisor
            .rework_review(&child, &parent, delivery.id, MessageId::new())
            .unwrap();

        assert_ne!(successor.id, previous_attempt);
        assert_eq!(successor.number, 2);
        assert_eq!(supervisor.task(&child).unwrap().state(), &TaskState::Queued);
        assert_eq!(
            supervisor.task(&child).unwrap().active_attempt_id(),
            &successor.id
        );
    }

    #[test]
    fn direct_parent_reject_blocks_the_reviewed_child() {
        let (mut supervisor, parent, child, delivery) = delivered_child();
        let reason = MessageId::new();

        supervisor
            .reject_review(&child, &parent, delivery.id, reason.clone())
            .unwrap();

        assert!(matches!(
            supervisor.task(&child).unwrap().state(),
            TaskState::Blocked(_)
        ));
        assert_eq!(supervisor.mailbox(&child).unwrap().messages()[0].id, reason);
    }

    #[test]
    fn rework_rejects_an_actor_other_than_the_direct_parent() {
        let (mut supervisor, _parent, child, delivery) = delivered_child();

        let error = supervisor
            .rework_review(&child, &TaskId::new(), delivery.id, MessageId::new())
            .unwrap_err();

        assert_eq!(error, "review actor does not match direct parent");
        assert!(matches!(
            supervisor.task(&child).unwrap().state(),
            TaskState::AwaitingParentReview(_)
        ));
    }

    #[test]
    fn rejection_rejects_an_actor_other_than_the_direct_parent() {
        let (mut supervisor, _parent, child, delivery) = delivered_child();

        let error = supervisor
            .reject_review(&child, &TaskId::new(), delivery.id, MessageId::new())
            .unwrap_err();

        assert_eq!(error, "review actor does not match direct parent");
        assert!(matches!(
            supervisor.task(&child).unwrap().state(),
            TaskState::AwaitingParentReview(_)
        ));
    }

    #[test]
    fn failed_review_persistence_rolls_back_staged_state_and_mailboxes() {
        let (mut supervisor, parent, child, delivery) = delivered_child();
        let before_child = supervisor.task(&child).unwrap().clone();
        let before_parent_mailbox = supervisor.mailbox(&parent).unwrap().messages().to_vec();
        let notification = MessageId::new();

        let result = supervisor.stage_review_with_persistence(
            |supervisor| {
                supervisor.accept_review(
                    &child,
                    &parent,
                    delivery.id.clone(),
                    IntegrationValidation::passed("trusted integration passed"),
                )?;
                supervisor
                    .stage_persisted_message(MailboxMessageDraft::user_override_with_id(
                        notification,
                        parent.clone(),
                        "review accepted",
                    ))
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
            |_| Err::<(), _>("injected repository failure"),
        );

        assert!(matches!(
            result,
            Err(ReviewPersistenceError::Persistence(_))
        ));
        assert_eq!(supervisor.task(&child).unwrap(), &before_child);
        assert_eq!(
            supervisor.mailbox(&parent).unwrap().messages(),
            before_parent_mailbox
        );
    }

    fn delivered_child() -> (AgentSupervisor, TaskId, TaskId, DeliveryReport) {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let parent = supervisor.root_task_id().clone();
        let child = supervisor.spawn(parent.clone()).unwrap();
        let workspace = WorkspaceLeaseId::new();
        supervisor.tasks.get_mut(&child).unwrap().workspace = Some(workspace.clone());
        supervisor.start_task(&child).unwrap();
        let handle = WorkerHandle::new(tokio_util::sync::CancellationToken::new());
        supervisor.workers.insert(child.clone(), handle.clone());
        let delivery = DeliveryReport::coding("deadbeef", "main", workspace, "checks passed");
        handle.report_delivery(delivery.clone());
        supervisor.reconcile_worker_events().unwrap();
        (supervisor, parent, child, delivery)
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a structured message to a direct parent or child task."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": { "type": "string" },
                "message": { "type": "string" }
            },
            "required": ["recipient", "message"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let recipient = match args
            .get("recipient")
            .and_then(Value::as_str)
            .and_then(|id| TaskId::from_str(id).ok())
        {
            Some(recipient) => recipient,
            None => return ToolResult::error("recipient must be a task UUID"),
        };
        let message = match args.get("message").and_then(Value::as_str) {
            Some(message) if !message.trim().is_empty() => message,
            _ => return ToolResult::error("message must be a non-empty string"),
        };
        let draft = MailboxMessageDraft::new(
            self.tools.caller.clone(),
            recipient,
            MessageKind::UserInstruction(UserInstruction(message.to_owned())),
            None,
        );
        let mut supervisor = self
            .tools
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match supervisor.send_message(&self.tools.caller, draft) {
            Ok(()) => ToolResult::text("message delivered"),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn set_workdir_binds_a_directory_to_the_root() {
        let session = RootSessionId::new();
        let mut supervisor = AgentSupervisor::new_with_objective(session, "objective".into());
        let root = supervisor.root_task_id().clone();
        assert_eq!(
            supervisor.spawn_workdir(&root),
            None,
            "a root starts with no workdir"
        );

        supervisor
            .set_workdir(&root, PathBuf::from("/tmp/example-worktree"))
            .unwrap();

        assert_eq!(
            supervisor.spawn_workdir(&root),
            Some(PathBuf::from("/tmp/example-worktree"))
        );
    }

    #[test]
    fn set_workdir_rejects_an_unknown_task() {
        let session = RootSessionId::new();
        let mut supervisor = AgentSupervisor::new_with_objective(session, "objective".into());
        let unknown = TaskId::new();
        let error = supervisor
            .set_workdir(&unknown, PathBuf::from("/tmp/example-worktree"))
            .unwrap_err();
        assert_eq!(error, "task does not exist");
    }
}
