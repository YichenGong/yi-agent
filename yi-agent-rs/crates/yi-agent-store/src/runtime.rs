//! Process-local ownership of subagent supervisors and application workers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use yi_agent_core::subagent::scheduler::ResourceCoordinator;
use yi_agent_core::subagent::supervisor::{AgentSupervisor, SpawnError};
use yi_agent_core::subagent::task::{RootSessionId, TaskId};
use yi_agent_core::subagent::worker::AgentWorkerFactory;

use crate::repository::{RepositoryError, RuntimeEvent, RuntimeRepository};

#[derive(Debug, Error)]
pub enum RuntimeCoordinatorError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error("session does not exist: {0}")]
    SessionNotFound(RootSessionId),
    #[error("supervisor error: {0}")]
    Supervisor(String),
    #[error(transparent)]
    Spawn(#[from] SpawnError),
    #[error("global resident subagent capacity is exhausted")]
    ResidentCapacityExhausted,
}

/// Owns all supervisor instances and their worker handles for one daemon.
///
/// The coordinator deliberately receives a factory instead of constructing an
/// `Agent`: provider and tool bootstrapping remain an application concern.
pub struct RuntimeCoordinator {
    repository: Mutex<RuntimeRepository>,
    factory: Arc<dyn AgentWorkerFactory>,
    supervisors: Mutex<HashMap<RootSessionId, Arc<AsyncMutex<AgentSupervisor>>>>,
    resident_tasks: Mutex<HashSet<TaskId>>,
}

impl RuntimeCoordinator {
    pub fn open(
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, RuntimeCoordinatorError> {
        Ok(Self {
            repository: Mutex::new(RuntimeRepository::open(database_path)?),
            factory,
            supervisors: Mutex::new(HashMap::new()),
            resident_tasks: Mutex::new(HashSet::new()),
        })
    }

    pub fn create_session(&self) -> Result<RootSessionId, RuntimeCoordinatorError> {
        let session_id = RootSessionId::new();
        let supervisor = AgentSupervisor::new(session_id.clone());
        let root_id = supervisor.root_task_id().clone();
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .create_task(&root_id, &session_id, "queued")?;
        self.supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .insert(session_id.clone(), Arc::new(AsyncMutex::new(supervisor)));
        Ok(session_id)
    }

    pub fn root_task_id(&self, session: &RootSessionId) -> Result<TaskId, RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let root_id = supervisor
            .try_lock()
            .map_err(|_| RuntimeCoordinatorError::Supervisor("session is busy".into()))?
            .root_task_id()
            .clone();
        Ok(root_id)
    }

    pub async fn spawn_child(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        self.spawn_child_with_objective(session, parent, "Complete the delegated task.".into())
            .await
    }

    pub async fn spawn_child_with_objective(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
        objective: String,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let child = supervisor
            .lock()
            .await
            .spawn_with_objective(parent.clone(), objective)?;
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .create_task(&child, session, "queued")?;
        Ok(child)
    }

    pub async fn start_worker(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<(), RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        let is_subagent = !matches!(
            supervisor
                .task(task)
                .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
                .depth,
            yi_agent_core::TaskDepth::Root
        );
        if is_subagent {
            let mut residents = self
                .resident_tasks
                .lock()
                .expect("runtime resident task mutex poisoned");
            if residents.len()
                >= usize::from(ResourceCoordinator::DEFAULT_GLOBAL_RESIDENT_SUBAGENTS)
            {
                return Err(RuntimeCoordinatorError::ResidentCapacityExhausted);
            }
            residents.insert(task.clone());
        }
        supervisor
            .start_worker(self.factory.as_ref(), task)
            .await
            .map_err(|error| {
                if is_subagent {
                    self.resident_tasks
                        .lock()
                        .expect("runtime resident task mutex poisoned")
                        .remove(task);
                }
                RuntimeCoordinatorError::Supervisor(error)
            })?;
        if let Err(error) = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .transition_task(task, "running", RuntimeEvent::TaskStarted)
        {
            let _ = supervisor.cancel_task_tree(task, false);
            if is_subagent {
                self.resident_tasks
                    .lock()
                    .expect("runtime resident task mutex poisoned")
                    .remove(task);
            }
            return Err(error.into());
        }
        Ok(())
    }

    pub async fn cancel_task(
        &self,
        session: &RootSessionId,
        task: &TaskId,
        recursive: bool,
    ) -> Result<(), RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        let cancelled = supervisor
            .cancel_task_tree(task, recursive)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        let mut repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        for task in cancelled {
            repository.transition_task(&task, "cancelled", RuntimeEvent::TaskCancelled)?;
            self.resident_tasks
                .lock()
                .expect("runtime resident task mutex poisoned")
                .remove(&task);
        }
        Ok(())
    }

    /// Drains facts emitted by application workers and persists their reducer
    /// outcomes. Workers themselves never write task snapshots or events.
    pub async fn reconcile_worker_events(&self) -> Result<(), RuntimeCoordinatorError> {
        let supervisors = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut updates = Vec::new();
        for supervisor in supervisors {
            let mut supervisor = supervisor.lock().await;
            let changed = supervisor
                .reconcile_worker_events()
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            for task_id in changed {
                let state = supervisor
                    .task(&task_id)
                    .expect("reconciled task exists")
                    .state();
                let (state, event) = match state {
                    yi_agent_core::TaskState::Cancelled(_) => {
                        ("cancelled", RuntimeEvent::TaskCancelled)
                    }
                    yi_agent_core::TaskState::Failed(_) => ("failed", RuntimeEvent::TaskFailed),
                    _ => continue,
                };
                updates.push((task_id, state, event));
            }
        }
        let mut repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        for (task_id, state, event) in updates {
            repository.transition_task(&task_id, state, event)?;
            self.resident_tasks
                .lock()
                .expect("runtime resident task mutex poisoned")
                .remove(&task_id);
        }
        Ok(())
    }

    pub async fn worker_cancellation(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<tokio_util::sync::CancellationToken, RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        supervisor
            .lock()
            .await
            .worker_cancellation(task)
            .ok_or_else(|| RuntimeCoordinatorError::Supervisor("worker does not exist".into()))
    }

    pub fn task_state(&self, task: &TaskId) -> Result<String, RuntimeCoordinatorError> {
        Ok(self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_state(task)?)
    }

    fn supervisor(
        &self,
        session: &RootSessionId,
    ) -> Result<Arc<AsyncMutex<AgentSupervisor>>, RuntimeCoordinatorError> {
        self.supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .get(session)
            .cloned()
            .ok_or_else(|| RuntimeCoordinatorError::SessionNotFound(session.clone()))
    }
}
