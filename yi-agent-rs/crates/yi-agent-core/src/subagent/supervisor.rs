use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::watch;

use super::mailbox::{Mailbox, MailboxMessageDraft, MessageKind, MessagePriority, UserInstruction};
use super::task::{AgentTask, CancelReason, RootSessionId, TaskEvent, TaskFailure, TaskId};
use super::worker::{AgentWorkerFactory, WorkerEvent, WorkerHandle, WorkerStart};
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
    #[error("recipient task does not exist")]
    RecipientNotFound,
    #[error("messages may only be sent to a direct parent or child")]
    RecipientNotAdjacent,
    #[error("terminal tasks do not accept mailbox messages")]
    RecipientTerminal,
}

pub struct AgentSupervisor {
    root_task_id: TaskId,
    tasks: HashMap<TaskId, AgentTask>,
    objectives: HashMap<TaskId, String>,
    children: HashMap<TaskId, Vec<TaskId>>,
    mailboxes: HashMap<TaskId, Mailbox>,
    workers: HashMap<TaskId, WorkerHandle>,
    events: Vec<SupervisorEvent>,
    updates: watch::Sender<u64>,
}

impl AgentSupervisor {
    pub fn new(root_session_id: RootSessionId) -> Self {
        let root = AgentTask::new_root(root_session_id);
        let root_task_id = root.id.clone();
        let mut tasks = HashMap::new();
        tasks.insert(root_task_id.clone(), root);
        let mut mailboxes = HashMap::new();
        mailboxes.insert(root_task_id.clone(), Mailbox::default());
        let mut objectives = HashMap::new();
        objectives.insert(
            root_task_id.clone(),
            "Root session objective not specified.".into(),
        );
        let (updates, _) = watch::channel(0_u64);
        Self {
            root_task_id,
            tasks,
            objectives,
            children: HashMap::new(),
            mailboxes,
            workers: HashMap::new(),
            events: Vec::new(),
            updates,
        }
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

    pub fn has_worker(&self, task_id: &TaskId) -> bool {
        self.workers.contains_key(task_id)
    }

    pub fn worker_cancellation(
        &self,
        task_id: &TaskId,
    ) -> Option<tokio_util::sync::CancellationToken> {
        self.workers
            .get(task_id)
            .map(WorkerHandle::cancellation_token)
    }

    /// Creates a worker only after the task has passed supervised admission.
    pub async fn start_worker(
        &mut self,
        factory: &dyn AgentWorkerFactory,
        task_id: &TaskId,
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
        let start = WorkerStart::new(
            task.id.clone(),
            task.active_attempt_id().clone(),
            task.root_session_id.clone(),
        )
        .with_objective(objective);
        // Admission is visible before the application factory can create any
        // side effects. A factory failure is reduced to a terminal task state.
        self.start_task(task_id)?;
        let handle = match factory.start(start).await {
            Ok(handle) => handle,
            Err(error) => {
                self.fail_task(task_id, error.to_string())?;
                return Err(error.to_string());
            }
        };
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
            let task = self
                .tasks
                .get(&task_id)
                .ok_or_else(|| "worker task does not exist".to_string())?;
            if task.state().is_terminal() {
                continue;
            }
            match event {
                WorkerEvent::Failed(message) => self.fail_task(&task_id, message)?,
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
            if self
                .tasks
                .get(&task_id)
                .is_some_and(|task| task.state().is_terminal())
            {
                self.workers.remove(&task_id);
                changed.push(task_id);
            }
        }
        Ok(changed)
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

    fn subscribe_updates(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
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

        let mut child = AgentTask::new_child(parent.root_session_id.clone(), parent_id.clone());
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
        self.mailboxes
            .get_mut(&recipient)
            .expect("task mailbox is created with task")
            .push(draft);
        self.notify_update();
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
                if supervisor
                    .mailbox(&self.tools.caller)
                    .is_some_and(|mailbox| {
                        mailbox
                            .messages()
                            .iter()
                            .any(|message| message.priority <= MessagePriority::High)
                    })
                {
                    Some(("needs_attention", Vec::new()))
                } else {
                    let children = supervisor.children_of(&self.tools.caller);
                    let complete = match mode {
                        WaitMode::Any => children.iter().any(|child| {
                            supervisor
                                .task(child)
                                .is_some_and(|task| task.state().is_terminal())
                        }),
                        WaitMode::All => {
                            !children.is_empty()
                                && children.iter().all(|child| {
                                    supervisor
                                        .task(child)
                                        .is_some_and(|task| task.state().is_terminal())
                                })
                        }
                    };
                    complete.then(|| {
                        (
                            "completed",
                            children.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        )
                    })
                }
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

#[derive(Clone, Copy)]
enum WaitMode {
    Any,
    All,
}

struct SendMessageTool {
    tools: SupervisorTools,
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
