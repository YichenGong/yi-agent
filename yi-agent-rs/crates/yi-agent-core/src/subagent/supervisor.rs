use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;

use super::mailbox::{Mailbox, MailboxMessageDraft, MessageKind, UserInstruction};
use super::task::{AgentTask, RootSessionId, TaskId};
use crate::tool::{Tool, ToolResult};

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
    children: HashMap<TaskId, Vec<TaskId>>,
    mailboxes: HashMap<TaskId, Mailbox>,
    events: Vec<SupervisorEvent>,
}

impl AgentSupervisor {
    pub fn new(root_session_id: RootSessionId) -> Self {
        let root = AgentTask::new_root(root_session_id);
        let root_task_id = root.id.clone();
        let mut tasks = HashMap::new();
        tasks.insert(root_task_id.clone(), root);
        let mut mailboxes = HashMap::new();
        mailboxes.insert(root_task_id.clone(), Mailbox::default());
        Self {
            root_task_id,
            tasks,
            children: HashMap::new(),
            mailboxes,
            events: Vec::new(),
        }
    }

    pub fn root_task_id(&self) -> &TaskId {
        &self.root_task_id
    }

    pub fn task(&self, task_id: &TaskId) -> Option<&AgentTask> {
        self.tasks.get(task_id)
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

    pub fn spawn(&mut self, parent_id: TaskId) -> Result<TaskId, SpawnError> {
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
        self.mailboxes.insert(child_id.clone(), Mailbox::default());
        self.children
            .entry(parent_id.clone())
            .or_default()
            .push(child_id.clone());
        self.events.push(SupervisorEvent::TaskSpawned {
            parent_id,
            task_id: child_id.clone(),
        });
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
        let is_parent = recipient_task.parent_id.as_ref() == Some(sender);
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
        Ok(())
    }
}

/// The per-task built-in tool set. Tool calls delegate to the Supervisor; they
/// never create worker state independently.
#[derive(Clone)]
pub struct SupervisorTools {
    supervisor: Arc<Mutex<AgentSupervisor>>,
    caller: TaskId,
}

impl SupervisorTools {
    pub fn new(supervisor: Arc<Mutex<AgentSupervisor>>, caller: TaskId) -> Self {
        Self { supervisor, caller }
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

    async fn call(&self, _args: Value) -> ToolResult {
        let mut supervisor = self
            .tools
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match supervisor.spawn(self.tools.caller.clone()) {
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

    async fn call(&self, _args: Value) -> ToolResult {
        let supervisor = self
            .tools
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let children = supervisor.children_of(&self.tools.caller);
        ToolResult::text(json!({ "status": "waiting", "children": children.iter().map(ToString::to_string).collect::<Vec<_>>() }).to_string())
    }
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
