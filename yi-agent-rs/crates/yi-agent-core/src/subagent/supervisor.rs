use std::collections::HashMap;
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
    AgentTask, AttemptId, BlockReason, BudgetKind, CancelReason, PauseReason, PermissionDecision,
    PermissionRequestId, RecoveryEvidence, RootSessionId, TaskEvent, TaskFailure, TaskId,
    TaskState, TimeoutKind, WatchdogEvidence, WorkspaceLeaseId,
};
use super::worker::{
    AgentWorkerFactory, WorkerEvent, WorkerHandle, WorkerMessage, WorkerStart, WorkerWatchdogEvent,
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
    NeedsAttention,
    Completed(Vec<TaskId>),
}

pub struct AgentSupervisor {
    root_task_id: TaskId,
    tasks: HashMap<TaskId, AgentTask>,
    objectives: HashMap<TaskId, String>,
    children: HashMap<TaskId, Vec<TaskId>>,
    mailboxes: HashMap<TaskId, Mailbox>,
    workers: HashMap<TaskId, WorkerHandle>,
    worker_message_capabilities: HashMap<TaskId, String>,
    pending_user_override_acks: Vec<(TaskId, super::task::MessageId)>,
    events: Vec<SupervisorEvent>,
    updates: watch::Sender<u64>,
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
            children: HashMap::new(),
            mailboxes,
            workers: HashMap::new(),
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
            children: HashMap::new(),
            mailboxes,
            workers: HashMap::new(),
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

    pub fn children_of(&self, task_id: &TaskId) -> &[TaskId] {
        self.children.get(task_id).map(Vec::as_slice).unwrap_or(&[])
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
        for task_id in &task_ids {
            let worker = self
                .workers
                .get(task_id)
                .expect("worker key was collected from this map");
            worker.cancel();
            let task = self
                .tasks
                .get_mut(task_id)
                .expect("worker task is retained by its supervisor");
            let attempt_id = task.active_attempt_id().clone();
            task.reduce(
                TaskEvent::RuntimeInterrupted {
                    attempt_id,
                    evidence: RecoveryEvidence("safe checkpoint grace deadline elapsed".into()),
                },
                chrono::Utc::now(),
            )
            .map_err(|error| error.to_string())?;
            self.workers.remove(task_id);
            self.worker_message_capabilities.remove(task_id);
        }
        if !task_ids.is_empty() {
            self.notify_update();
        }
        Ok(task_ids)
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
            .pending_user_overrides();
        let start = WorkerStart::new(
            task.id.clone(),
            task.active_attempt_id().clone(),
            task.root_session_id.clone(),
        )
        .with_objective(objective)
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
            self.mailboxes
                .get_mut(task_id)
                .expect("task mailbox is created with task")
                .mark_delivered_to_worker(&id);
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
            match event {
                WorkerEvent::MessageConsumed { .. } => unreachable!("handled before task state"),
                WorkerEvent::Delivered(delivery) => {
                    let (attempt_id, parent_id) = {
                        let task = self
                            .tasks
                            .get_mut(&task_id)
                            .expect("worker task was checked above");
                        let attempt_id = task.active_attempt_id().clone();
                        let parent_id = task.parent_id.clone().ok_or_else(|| {
                            "root tasks cannot submit parent delivery".to_string()
                        })?;
                        task.reduce(
                            TaskEvent::WorkerDelivered {
                                attempt_id: attempt_id.clone(),
                                delivery: delivery.clone(),
                            },
                            chrono::Utc::now(),
                        )
                        .map_err(|error| error.to_string())?;
                        (attempt_id, parent_id)
                    };
                    self.send_message(
                        &task_id,
                        MailboxMessageDraft::new(
                            task_id.clone(),
                            parent_id,
                            MessageKind::Completed(delivery),
                            Some(attempt_id),
                        ),
                    )
                    .map_err(|error| error.to_string())?;
                }
                WorkerEvent::Paused => {
                    let task = self
                        .tasks
                        .get_mut(&task_id)
                        .expect("worker task was checked above");
                    let attempt_id = task.active_attempt_id().clone();
                    task.reduce(
                        TaskEvent::PauseAcknowledged { attempt_id },
                        chrono::Utc::now(),
                    )
                    .map_err(|error| error.to_string())?;
                }
                WorkerEvent::Failed(message) => self.fail_task(&task_id, message)?,
                WorkerEvent::RecoveryConflict(message) => {
                    let task = self
                        .tasks
                        .get_mut(&task_id)
                        .expect("worker task was checked above");
                    let attempt_id = task.active_attempt_id().clone();
                    task.reduce(
                        TaskEvent::RecoveryConflict {
                            attempt_id,
                            reason: BlockReason(format!("recovery_conflict: {message}")),
                        },
                        chrono::Utc::now(),
                    )
                    .map_err(|error| error.to_string())?;
                }
                WorkerEvent::Cancelled => {
                    self.cancel_task_tree(&task_id, false)?;
                }
                WorkerEvent::CompletedWithoutDelivery => {
                    self.fail_task(
                        &task_id,
                        "worker completed without a structured delivery report",
                    )?;
                }
            }
            if self.tasks.get(&task_id).is_some_and(|task| {
                task.state().is_terminal()
                    || matches!(
                        task.state(),
                        TaskState::Paused(_) | TaskState::AwaitingParentReview(_)
                    )
            }) {
                self.workers.remove(&task_id);
                self.worker_message_capabilities.remove(&task_id);
                changed.push(task_id);
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
        for id in &task_ids {
            if let Some(worker) = self.workers.get(id) {
                worker.cancel();
            }
            let task = self.tasks.get_mut(id).expect("collected task exists");
            if !task.state().is_terminal() {
                let attempt_id = task.active_attempt_id().clone();
                task.reduce(
                    TaskEvent::CancelRequested {
                        attempt_id,
                        reason: CancelReason("cancelled by runtime coordinator".into()),
                    },
                    chrono::Utc::now(),
                )
                .map_err(|error| error.to_string())?;
            }
        }
        self.notify_update();
        Ok(task_ids)
    }

    pub fn pause_task(&mut self, task_id: &TaskId, reason: PauseReason) -> Result<(), String> {
        let worker = self
            .workers
            .get(task_id)
            .ok_or_else(|| "worker does not exist".to_string())?;
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::PauseRequested { attempt_id, reason },
            chrono::Utc::now(),
        )
        .map_err(|error| error.to_string())?;
        worker.request_pause();
        self.notify_update();
        Ok(())
    }

    pub fn resume_task(&mut self, task_id: &TaskId) -> Result<(), String> {
        {
            let task = self
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| "task does not exist".to_string())?;
            let attempt_id = task.active_attempt_id().clone();
            task.reduce(
                TaskEvent::ResumeRequested { attempt_id },
                chrono::Utc::now(),
            )
            .map_err(|error| error.to_string())?;
        }
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
        if self.mailbox(caller).is_some_and(|mailbox| {
            mailbox
                .messages()
                .iter()
                .any(|message| message.priority <= MessagePriority::High)
        }) {
            return Some(WaitOutcome::NeedsAttention);
        }
        let children = self.children_of(caller);
        let complete = match mode {
            WaitMode::Any => children.iter().any(|child| {
                self.task(child)
                    .is_some_and(|task| task.state().is_terminal())
            }),
            WaitMode::All => {
                !children.is_empty()
                    && children.iter().all(|child| {
                        self.task(child)
                            .is_some_and(|task| task.state().is_terminal())
                    })
            }
        };
        complete.then(|| WaitOutcome::Completed(children.to_vec()))
    }

    fn notify_update(&self) {
        self.updates
            .send_modify(|epoch| *epoch = epoch.saturating_add(1));
    }

    pub fn spawn(&mut self, parent_id: TaskId) -> Result<TaskId, SpawnError> {
        self.spawn_with_objective(parent_id, "Complete the delegated task.".into())
    }

    pub fn spawn_with_objective(
        &mut self,
        parent_id: TaskId,
        objective: String,
    ) -> Result<TaskId, SpawnError> {
        let parent = self
            .tasks
            .get(&parent_id)
            .ok_or(SpawnError::ParentNotFound)?;
        let child_depth = parent
            .depth
            .can_spawn_child()
            .map_err(|_| SpawnError::MaximumDepthReached)?;
        if self.children_of(&parent_id).len() >= MAX_DIRECT_CHILDREN {
            return Err(SpawnError::DirectChildLimitReached);
        }

        let mut child = AgentTask::new_child(parent.root_session_id.clone(), parent_id.clone())
            .with_workspace(WorkspaceLeaseId::new());
        child.depth = child_depth;
        let child_id = child.id.clone();
        self.tasks.insert(child_id.clone(), child);
        self.objectives.insert(child_id.clone(), objective);
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
        if self
            .worker_message_capabilities
            .get(sender)
            .is_none_or(|expected| expected != capability)
        {
            return Err(MessageDeliveryError::SenderNotFound);
        }
        let sender_task = self
            .tasks
            .get(sender)
            .ok_or(MessageDeliveryError::SenderNotFound)?;
        if sender_task.state().is_terminal() {
            return Err(MessageDeliveryError::SenderTerminal);
        }
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
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::WorkerFailed {
                attempt_id,
                failure: TaskFailure::new(message),
            },
            chrono::Utc::now(),
        )
        .map_err(|error| error.to_string())?;
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
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(event(attempt_id), chrono::Utc::now())
            .map_err(|error| error.to_string())?;
        self.notify_update();
        Ok(())
    }

    pub fn start_task(&mut self, task_id: &TaskId) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted { attempt_id },
            chrono::Utc::now(),
        )
        .map_err(|error| error.to_string())?;
        self.notify_update();
        Ok(())
    }

    /// Places a running task in its reducer-owned permission wait state.
    pub fn request_permission(
        &mut self,
        task_id: &TaskId,
        request: PermissionRequestId,
    ) -> Result<AttemptId, String> {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::PermissionRequested {
                attempt_id: attempt_id.clone(),
                request,
            },
            chrono::Utc::now(),
        )
        .map_err(|error| error.to_string())?;
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
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::PermissionResolved {
                attempt_id: attempt_id.clone(),
                request,
                decision,
            },
            chrono::Utc::now(),
        )
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
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::RecoveryConflict {
                attempt_id,
                reason: BlockReason(format!("recovery_conflict: {}", message.into())),
            },
            chrono::Utc::now(),
        )
        .map_err(|error| error.to_string())?;
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
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        let attempt_id = task.active_attempt_id().clone();
        let next = task
            .reduce(TaskEvent::RetryRequested { attempt_id }, chrono::Utc::now())
            .map_err(|error| error.to_string())?
            .new_attempt
            .expect("retry creates a successor attempt");
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
            "properties": { "task": { "type": "string", "description": "Delegated objective." } },
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
        let mut supervisor = self
            .tools
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match supervisor.spawn_with_objective(self.tools.caller.clone(), task.to_string()) {
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
                        WaitOutcome::NeedsAttention => ("needs_attention", Vec::new()),
                        WaitOutcome::Completed(children) => (
                            "completed",
                            children
                                .into_iter()
                                .map(|child| child.to_string())
                                .collect(),
                        ),
                    })
            };
            if let Some((status, children)) = status {
                return ToolResult::text(
                    json!({ "status": status, "children": children }).to_string(),
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
        DeliveryReport, PermissionDecision, PermissionRequestId, WorkspaceLeaseId,
    };

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
