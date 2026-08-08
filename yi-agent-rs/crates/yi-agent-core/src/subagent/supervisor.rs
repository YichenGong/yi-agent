use std::collections::HashMap;

use thiserror::Error;

use super::task::{AgentTask, RootSessionId, TaskId};

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

pub struct AgentSupervisor {
    root_task_id: TaskId,
    tasks: HashMap<TaskId, AgentTask>,
    children: HashMap<TaskId, Vec<TaskId>>,
    events: Vec<SupervisorEvent>,
}

impl AgentSupervisor {
    pub fn new(root_session_id: RootSessionId) -> Self {
        let root = AgentTask::new_root(root_session_id);
        let root_task_id = root.id.clone();
        let mut tasks = HashMap::new();
        tasks.insert(root_task_id.clone(), root);
        Self {
            root_task_id,
            tasks,
            children: HashMap::new(),
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
}
