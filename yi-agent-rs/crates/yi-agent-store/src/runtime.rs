//! Process-local ownership of subagent supervisors and application workers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use yi_agent_core::ProviderTurnGate;
use yi_agent_core::subagent::scheduler::{
    AdmissionCursor, AdmissionPriority, LeaseId, LeaseMode, ResourceCoordinator, ResourceRequest,
    ResourceScope,
};
use yi_agent_core::subagent::supervisor::{AgentSupervisor, SpawnError, WaitMode, WaitOutcome};
use yi_agent_core::subagent::task::{MessageId, PauseReason, RootSessionId, TaskId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerRecoveryContext, WorkerRecoveryPreflight,
    WorkerRecoveryPreflightResult,
};

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
    #[error("global queued subagent capacity is exhausted")]
    QueueCapacityExceeded,
    #[error("runtime is draining and rejects new admissions")]
    Draining,
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeStopOptions {
    pub grace: Duration,
}

impl Default for RuntimeStopOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStopSummary {
    pub paused: usize,
    pub recovery_required: usize,
}

/// Owns all supervisor instances and their worker handles for one daemon.
///
/// The coordinator deliberately receives a factory instead of constructing an
/// `Agent`: provider and tool bootstrapping remain an application concern.
pub struct RuntimeCoordinator {
    repository: Arc<Mutex<RuntimeRepository>>,
    factory: Arc<dyn AgentWorkerFactory>,
    supervisors: Mutex<HashMap<RootSessionId, Arc<AsyncMutex<AgentSupervisor>>>>,
    resident_tasks: Mutex<HashSet<TaskId>>,
    resident_leases: Mutex<HashMap<TaskId, LeaseId>>,
    resident_waiting: Mutex<HashSet<TaskId>>,
    resource_coordinator: Arc<Mutex<ResourceCoordinator>>,
    provider_turn_admissions: Option<Arc<ProviderTurnAdmissions>>,
    provider_profile_id: Option<String>,
    recovery_contexts: Mutex<HashMap<TaskId, RecoveryContext>>,
    draining: AtomicBool,
}

impl RuntimeCoordinator {
    pub const DEFAULT_GLOBAL_QUEUED_SUBAGENTS: usize = 64;

    fn persist_resident_cursor(&self) -> Result<(), RuntimeCoordinatorError> {
        let cursor = self
            .resource_coordinator
            .lock()
            .expect("resource coordinator mutex poisoned")
            .admission_cursor("resident:global");
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .save_admission_cursor(
                "resident:global",
                cursor.root_id.as_ref(),
                cursor.parent_id.as_ref(),
                cursor.sequence,
            )?;
        Ok(())
    }

    fn release_resident_lease(&self, task: &TaskId) {
        self.resource_coordinator
            .lock()
            .expect("resource coordinator mutex poisoned")
            .cancel_task_requests(task);
        self.resident_waiting
            .lock()
            .expect("runtime resident wait mutex poisoned")
            .remove(task);
        if let Some(lease_id) = self
            .resident_leases
            .lock()
            .expect("runtime resident lease mutex poisoned")
            .remove(task)
        {
            self.resource_coordinator
                .lock()
                .expect("resource coordinator mutex poisoned")
                .release(lease_id)
                .expect("resident lease release is idempotent");
        }
        self.resident_tasks
            .lock()
            .expect("runtime resident task mutex poisoned")
            .remove(task);
    }

    pub fn resident_admission_cursor(&self) -> AdmissionCursor {
        self.resource_coordinator
            .lock()
            .expect("resource coordinator mutex poisoned")
            .admission_cursor("resident:global")
    }

    pub fn open(
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, RuntimeCoordinatorError> {
        let mut repository = RuntimeRepository::open(database_path)?;
        // Provider-turn leases are process-local. A restarted daemon must not
        // count an abandoned request against the new process's capacity.
        repository.release_process_local_leases()?;
        let resident_cursor = repository.admission_cursor("resident:global")?;
        let mut resource_coordinator = ResourceCoordinator::new();
        if let Some(cursor) = resident_cursor {
            resource_coordinator.restore_admission_cursor(
                "resident:global",
                AdmissionCursor {
                    root_id: cursor.root_id,
                    parent_id: cursor.parent_id,
                    sequence: cursor.sequence,
                },
            );
        }
        let recovered_tasks = repository.recovered_tasks()?;
        let mut supervisors: HashMap<RootSessionId, Arc<AsyncMutex<AgentSupervisor>>> =
            HashMap::new();
        let mut recovery_contexts = HashMap::new();
        for task in recovered_tasks {
            recovery_contexts.insert(
                task.task_id.clone(),
                RecoveryContext {
                    workspace_lease_id: task.workspace_lease_id.clone(),
                    worktree_lease: task.worktree_lease.clone(),
                    checkpoint_json: task.checkpoint_json.clone(),
                    tool_state_json: task.tool_state_json.clone(),
                    recovery_gated: task.recovery_gated,
                    recovery_attested: task.recovery_attested,
                },
            );
            if let Some(parent_id) = task.parent_id {
                let depth = match task.depth {
                    1 => yi_agent_core::TaskDepth::Child,
                    2 => yi_agent_core::TaskDepth::Leaf,
                    _ => {
                        return Err(RuntimeCoordinatorError::Supervisor(
                            "persisted recovered child has an invalid depth".into(),
                        ));
                    }
                };
                let supervisor = supervisors.get(&task.session_id).ok_or_else(|| {
                    RuntimeCoordinatorError::Supervisor(
                        "persisted recovered child has no recovered root".into(),
                    )
                })?;
                supervisor
                    .try_lock()
                    .map_err(|_| {
                        RuntimeCoordinatorError::Supervisor("recovery hydration is busy".into())
                    })?
                    .insert_recovered_child(
                        task.task_id,
                        parent_id,
                        depth,
                        task.attempt_id,
                        task.attempt_number,
                        task.recovery_gated || task.recovery_attested,
                        task.objective,
                    )
                    .map_err(RuntimeCoordinatorError::Supervisor)?;
            } else {
                let supervisor = if task.recovery_gated || task.recovery_attested {
                    AgentSupervisor::from_recovered_gated_root(
                        task.session_id.clone(),
                        task.task_id,
                        task.attempt_id,
                        task.attempt_number,
                        task.objective,
                    )
                } else {
                    AgentSupervisor::from_recovered_root(
                        task.session_id.clone(),
                        task.task_id,
                        task.attempt_id,
                        task.attempt_number,
                        task.objective,
                    )
                };
                supervisors.insert(task.session_id, Arc::new(AsyncMutex::new(supervisor)));
            }
        }
        let provider_profile_id = factory.provider_profile_id();
        if let Some(profile_id) = &provider_profile_id {
            resource_coordinator.configure_provider_llm_capacity(profile_id);
            for resource_key in [
                format!("llm:{profile_id}"),
                format!("llm-coordination:{profile_id}"),
            ] {
                if let Some(cursor) = repository.admission_cursor(&resource_key)? {
                    resource_coordinator.restore_admission_cursor(
                        &resource_key,
                        AdmissionCursor {
                            root_id: cursor.root_id,
                            parent_id: cursor.parent_id,
                            sequence: cursor.sequence,
                        },
                    );
                }
            }
        }
        let repository = Arc::new(Mutex::new(repository));
        let resource_coordinator = Arc::new(Mutex::new(resource_coordinator));
        let provider_turn_admissions = provider_profile_id.as_ref().map(|_| {
            Arc::new(ProviderTurnAdmissions::new(
                Arc::clone(&resource_coordinator),
                Arc::clone(&repository),
            ))
        });
        Ok(Self {
            repository,
            factory,
            supervisors: Mutex::new(supervisors),
            resident_tasks: Mutex::new(HashSet::new()),
            resident_leases: Mutex::new(HashMap::new()),
            resident_waiting: Mutex::new(HashSet::new()),
            resource_coordinator,
            provider_turn_admissions,
            provider_profile_id,
            recovery_contexts: Mutex::new(recovery_contexts),
            draining: AtomicBool::new(false),
        })
    }

    pub fn create_session(&self) -> Result<RootSessionId, RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let session_id = RootSessionId::new();
        let supervisor = AgentSupervisor::new(session_id.clone());
        let root_id = supervisor.root_task_id().clone();
        let root_attempt = supervisor
            .task(&root_id)
            .expect("new root task exists")
            .active_attempt()
            .clone();
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .create_task_with_attempt(
                &root_id,
                &session_id,
                &root_attempt.id,
                root_attempt.number,
                "queued",
            )?;
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
        self.ensure_admitting()?;
        if self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .queued_task_count()?
            >= Self::DEFAULT_GLOBAL_QUEUED_SUBAGENTS
        {
            return Err(RuntimeCoordinatorError::QueueCapacityExceeded);
        }
        let supervisor = self.supervisor(session)?;
        let (child, depth, attempt) = {
            let mut supervisor = supervisor.lock().await;
            let child = supervisor.spawn_with_objective(parent.clone(), objective.clone())?;
            let depth = match supervisor
                .task(&child)
                .expect("newly spawned task exists")
                .depth
            {
                yi_agent_core::TaskDepth::Root => 0,
                yi_agent_core::TaskDepth::Child => 1,
                yi_agent_core::TaskDepth::Leaf => 2,
            };
            let attempt = supervisor
                .task(&child)
                .expect("newly spawned task exists")
                .active_attempt()
                .clone();
            (child, depth, attempt)
        };
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .create_child_task_with_attempt_and_objective(
                &child,
                session,
                parent,
                depth,
                &attempt.id,
                attempt.number,
                "queued",
                &objective,
            )?;
        Ok(child)
    }

    /// Performs non-blocking scheduler admission after the task has been
    /// durably queued. A capacity wait is represented by the existing queued
    /// state; a real application worker starts immediately when capacity is
    /// available.
    pub async fn spawn_child_and_admit(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
        objective: String,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        let child = self
            .spawn_child_with_objective(session, parent, objective)
            .await?;
        if self.factory.is_available() {
            match self.start_worker(session, &child).await {
                Ok(()) | Err(RuntimeCoordinatorError::ResidentCapacityExhausted) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(child)
    }

    pub async fn start_worker(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let supervisor_handle = self.supervisor(session)?;
        let mut supervisor = supervisor_handle.lock().await;
        let recovery_boundary = self
            .recovery_contexts
            .lock()
            .expect("runtime recovery context mutex poisoned")
            .get(task)
            .cloned();
        if recovery_boundary
            .as_ref()
            .is_some_and(|context| !context.recovery_gated && !context.recovery_attested)
        {
            return Err(RuntimeCoordinatorError::Supervisor(
                "recovery-required task must be explicitly resumed".into(),
            ));
        }
        let is_subagent = !matches!(
            supervisor
                .task(task)
                .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
                .depth,
            yi_agent_core::TaskDepth::Root
        );
        if is_subagent {
            let parent_id = supervisor
                .task(task)
                .expect("worker task exists")
                .parent_id
                .clone()
                .unwrap_or_else(|| task.clone());
            let already_admitted = self
                .resident_leases
                .lock()
                .expect("runtime resident lease mutex poisoned")
                .contains_key(task);
            if !already_admitted {
                let already_waiting = self
                    .resident_waiting
                    .lock()
                    .expect("runtime resident wait mutex poisoned")
                    .contains(task);
                let mut coordinator = self
                    .resource_coordinator
                    .lock()
                    .expect("resource coordinator mutex poisoned");
                if !already_waiting {
                    coordinator.enqueue_with_priority_for_parent_at(
                        session.clone(),
                        parent_id,
                        task.clone(),
                        ResourceRequest {
                            scope: ResourceScope::Global,
                            key: "resident:global".into(),
                            mode: LeaseMode::Shared,
                            units: 1,
                            deadline: None,
                        },
                        AdmissionPriority::Normal,
                        Utc::now(),
                    );
                    self.resident_waiting
                        .lock()
                        .expect("runtime resident wait mutex poisoned")
                        .insert(task.clone());
                }
                let Some(lease) = coordinator.grant_next("resident:global") else {
                    return Err(RuntimeCoordinatorError::ResidentCapacityExhausted);
                };
                let admitted_task = lease.task_id.clone();
                self.resident_waiting
                    .lock()
                    .expect("runtime resident wait mutex poisoned")
                    .remove(&admitted_task);
                self.resident_leases
                    .lock()
                    .expect("runtime resident lease mutex poisoned")
                    .insert(admitted_task.clone(), lease.lease_id);
                self.resident_tasks
                    .lock()
                    .expect("runtime resident task mutex poisoned")
                    .insert(admitted_task.clone());
                drop(coordinator);
                if let Err(error) = self.persist_resident_cursor() {
                    self.release_resident_lease(&admitted_task);
                    return Err(error);
                }
                if admitted_task != *task {
                    return Err(RuntimeCoordinatorError::ResidentCapacityExhausted);
                }
            }
        }
        let attempt = supervisor
            .task(task)
            .expect("worker task exists")
            .active_attempt_id()
            .clone();
        let gate = recovery_boundary
            .as_ref()
            .filter(|context| context.recovery_gated)
            .cloned();
        if let Some(gate) = gate {
            match self.factory.preflight_recovery(WorkerRecoveryPreflight {
                task_id: task.clone(),
                attempt_id: attempt.clone(),
                context: gate.worker_context(),
            }) {
                WorkerRecoveryPreflightResult::Attested(attestation) => {
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .attest_recovery_gate(task, &attempt, &attestation)?;
                    self.recovery_contexts
                        .lock()
                        .expect("runtime recovery context mutex poisoned")
                        .remove(task);
                }
                WorkerRecoveryPreflightResult::Conflict(reason) => {
                    supervisor
                        .record_recovery_conflict(task, reason.clone())
                        .map_err(RuntimeCoordinatorError::Supervisor)?;
                    let evidence = serde_json::to_string(&serde_json::json!({
                        "reason": "recovery_conflict",
                        "evidence": reason,
                    }))
                    .expect("recovery conflict payload is serializable");
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .transition_task_and_attempt_with_terminal(
                            task,
                            &attempt,
                            "blocked",
                            RuntimeEvent::TaskBlocked,
                            &evidence,
                        )?;
                    self.recovery_contexts
                        .lock()
                        .expect("runtime recovery context mutex poisoned")
                        .remove(task);
                    if is_subagent {
                        self.release_resident_lease(task);
                    }
                    return Ok(());
                }
            }
        }
        let recovery_context = self.factory.recovery_context();
        let admission = {
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .transition_task_and_attempt_with_recovery_context_and_resident_lease(
                    task,
                    &attempt,
                    "running",
                    RuntimeEvent::TaskStarted,
                    &recovery_context,
                    is_subagent.then_some("resident:global"),
                )
        };
        if let Err(error) = admission {
            supervisor
                .start_task(task)
                .and_then(|()| supervisor.fail_task(task, error.to_string()))
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .transition_task_and_attempt_with_terminal(
                    task,
                    &attempt,
                    "failed",
                    RuntimeEvent::TaskFailed,
                    r#"{"reason":"invalid_worker_recovery_context"}"#,
                )?;
            if is_subagent {
                self.release_resident_lease(task);
            }
            return Err(RuntimeCoordinatorError::Repository(error));
        }
        self.recovery_contexts
            .lock()
            .expect("runtime recovery context mutex poisoned")
            .remove(task);
        let provider_turn_gate = self
            .provider_turn_admissions
            .as_ref()
            .and_then(|admissions| {
                self.provider_profile_id.as_ref().map(|profile_id| {
                    Arc::new(RuntimeProviderTurnGate {
                        admissions: Arc::clone(admissions),
                        root_id: session.clone(),
                        parent_id: supervisor
                            .task(task)
                            .expect("worker task exists")
                            .parent_id
                            .clone()
                            .unwrap_or_else(|| task.clone()),
                        task_id: task.clone(),
                        profile_id: profile_id.clone(),
                        supervisor: Arc::clone(&supervisor_handle),
                    }) as Arc<dyn ProviderTurnGate>
                })
            });
        if let Err(error) = supervisor
            .start_worker_with_provider_turn_gate(self.factory.as_ref(), task, provider_turn_gate)
            .await
        {
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .transition_task_and_attempt_with_terminal(
                    task,
                    &attempt,
                    "failed",
                    RuntimeEvent::TaskFailed,
                    r#"{"reason":"worker_start_failed"}"#,
                )?;
            if is_subagent {
                self.release_resident_lease(task);
            }
            return Err(RuntimeCoordinatorError::Supervisor(error));
        }
        Ok(())
    }

    pub async fn retry_task(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let supervisor = self.supervisor(session)?;
        let attempt = supervisor
            .lock()
            .await
            .retry_task(task)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        {
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            repository.activate_successor_attempt(
                task,
                &attempt.id,
                attempt.number,
                "queued",
                RuntimeEvent::TaskQueued,
            )?;
        }
        if self.factory.is_available() {
            self.start_worker(session, task).await?;
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
        let cancelled_attempts = cancelled
            .iter()
            .map(|task| {
                let attempt = supervisor
                    .task(task)
                    .expect("cancelled task exists")
                    .active_attempt_id()
                    .clone();
                (task.clone(), attempt)
            })
            .collect::<Vec<_>>();
        let mut repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        for (task, attempt) in cancelled_attempts {
            repository.transition_task_and_attempt_with_terminal(
                &task,
                &attempt,
                "cancelled",
                RuntimeEvent::TaskCancelled,
                r#"{"reason":"cancelled"}"#,
            )?;
            self.release_resident_lease(&task);
        }
        Ok(())
    }

    pub async fn pause_task(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<(), RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        supervisor
            .lock()
            .await
            .pause_task(task, PauseReason("paused by user".into()))
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .append_event(task, RuntimeEvent::TaskPauseRequested)?;
        Ok(())
    }

    pub async fn resume_task(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let supervisor = self.supervisor(session)?;
        let already_prepared = self
            .recovery_contexts
            .lock()
            .expect("runtime recovery context mutex poisoned")
            .get(task)
            .is_some_and(|context| context.recovery_gated || context.recovery_attested);
        let recovery_attempt = {
            let mut supervisor = supervisor.lock().await;
            if already_prepared {
                None
            } else if matches!(
                supervisor.task(task).map(|task| task.state()),
                Some(yi_agent_core::TaskState::RecoveryRequired(_))
            ) {
                Some(
                    supervisor
                        .resume_recovery_task(task)
                        .map_err(RuntimeCoordinatorError::Supervisor)?,
                )
            } else {
                supervisor
                    .resume_task(task)
                    .map_err(RuntimeCoordinatorError::Supervisor)?;
                None
            }
        };
        {
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            if let Some(attempt) = recovery_attempt {
                repository.activate_successor_attempt(
                    task,
                    &attempt.id,
                    attempt.number,
                    "recovery_gated",
                    RuntimeEvent::TaskQueued,
                )?;
                self.recovery_contexts
                    .lock()
                    .expect("runtime recovery context mutex poisoned")
                    .entry(task.clone())
                    .or_default()
                    .recovery_gated = true;
            } else if !already_prepared {
                repository.transition_task(task, "queued", RuntimeEvent::TaskQueued)?;
            }
        }
        if self.factory.is_available() {
            self.start_worker(session, task).await?;
        }
        Ok(())
    }

    /// Routes external task messages through the owning supervisor before
    /// recording an auditable mailbox row and runtime event.
    pub async fn send_message(
        &self,
        session: &RootSessionId,
        sender: &TaskId,
        worker_capability: &str,
        recipient: TaskId,
        message: String,
    ) -> Result<(), RuntimeCoordinatorError> {
        if message.trim().is_empty() {
            return Err(RuntimeCoordinatorError::Supervisor(
                "message must be non-empty".into(),
            ));
        }
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        supervisor
            .can_send_worker_message(sender, worker_capability, &recipient)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .record_user_message(sender, &recipient, &message)?;
        supervisor
            .send_worker_message(sender, worker_capability, recipient, message)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        Ok(())
    }

    /// Resolves a task's owning session server-side so an external TUI/CLI
    /// user cannot impersonate an adjacent agent as the message sender.
    pub async fn send_user_override(
        &self,
        recipient: &TaskId,
        message: String,
    ) -> Result<(), RuntimeCoordinatorError> {
        if message.trim().is_empty() {
            return Err(RuntimeCoordinatorError::Supervisor(
                "message must be non-empty".into(),
            ));
        }
        let session_id = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_detail(recipient)?
            .session_id;
        let session = session_id.parse().map_err(|_| {
            RuntimeCoordinatorError::Supervisor("persisted task has an invalid session ID".into())
        })?;
        let supervisor = self.supervisor(&session)?;
        let mut supervisor = supervisor.lock().await;
        supervisor
            .can_accept_user_override(recipient)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        let message_id = MessageId::new();
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .record_user_override_message_with_id(&message_id, recipient, &message)?;
        supervisor
            .send_user_override_with_id(message_id, recipient.clone(), message)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        Ok(())
    }

    /// Waits without retaining the supervisor lock, allowing child lifecycle
    /// events and cancellation to continue while the parent is suspended.
    pub async fn wait_for_children(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        mode: WaitMode,
    ) -> Result<WaitOutcome, RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let mut updates = supervisor.lock().await.subscribe_updates();
        loop {
            if let Some(outcome) = supervisor.lock().await.wait_outcome(caller, mode) {
                return Ok(outcome);
            }
            updates.changed().await.map_err(|_| {
                RuntimeCoordinatorError::Supervisor("supervisor is no longer available".into())
            })?;
        }
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
        let mut consumed_overrides = Vec::new();
        for supervisor in &supervisors {
            let mut supervisor = supervisor.lock().await;
            let changed = supervisor
                .reconcile_worker_events()
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            for task_id in changed {
                let state = supervisor
                    .task(&task_id)
                    .expect("reconciled task exists")
                    .state();
                let attempt = supervisor
                    .task(&task_id)
                    .expect("reconciled task exists")
                    .active_attempt_id()
                    .clone();
                let (state, event, terminal_json) = match state {
                    yi_agent_core::TaskState::Paused(_) => {
                        ("paused", RuntimeEvent::TaskPaused, None)
                    }
                    yi_agent_core::TaskState::Blocked(reason) => (
                        "blocked",
                        RuntimeEvent::TaskBlocked,
                        reason
                            .0
                            .strip_prefix("recovery_conflict:")
                            .map(|_| r#"{"reason":"recovery_conflict"}"#),
                    ),
                    yi_agent_core::TaskState::Cancelled(_) => {
                        ("cancelled", RuntimeEvent::TaskCancelled, None)
                    }
                    yi_agent_core::TaskState::Failed(_) => {
                        ("failed", RuntimeEvent::TaskFailed, None)
                    }
                    _ => continue,
                };
                updates.push((task_id, attempt, state, event, terminal_json));
            }
            consumed_overrides.extend(supervisor.pending_user_override_acks().iter().cloned());
        }
        for (task_id, message_id) in &consumed_overrides {
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .mark_user_override_consumed(task_id, message_id)?;
            for supervisor in &supervisors {
                supervisor
                    .lock()
                    .await
                    .confirm_user_override_consumed(task_id, message_id);
            }
        }
        for (task_id, attempt, state, event, terminal_json) in updates {
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            if let Some(terminal_json) = terminal_json {
                repository.transition_task_and_attempt_with_terminal(
                    &task_id,
                    &attempt,
                    state,
                    event,
                    terminal_json,
                )?;
            } else {
                repository.transition_task_and_attempt(&task_id, &attempt, state, event)?;
            }
            self.release_resident_lease(&task_id);
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

    /// Starts the stop admission barrier before workers are asked to reach a
    /// safe checkpoint. Once set, it deliberately never reopens for this
    /// daemon process; restart owns recovery and future admissions.
    pub async fn begin_draining(&self) -> Result<(), RuntimeCoordinatorError> {
        self.draining.store(true, Ordering::Release);
        let supervisors = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut worker_tasks = Vec::new();
        for supervisor in supervisors {
            let supervisor = supervisor.lock().await;
            worker_tasks.extend(supervisor.worker_task_ids().cloned());
        }
        let mut repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        for task in worker_tasks {
            repository.append_event(&task, RuntimeEvent::RuntimeDraining)?;
        }
        Ok(())
    }

    /// Requests cooperative checkpoints only after `begin_draining` has
    /// durably announced the transition. Paused snapshots are persisted by
    /// normal worker reconciliation after each worker acknowledges.
    pub async fn request_safe_checkpoints(&self) -> Result<(), RuntimeCoordinatorError> {
        let supervisors = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut requested = Vec::new();
        for supervisor in supervisors {
            requested.extend(
                supervisor
                    .lock()
                    .await
                    .request_safe_checkpoints()
                    .map_err(RuntimeCoordinatorError::Supervisor)?,
            );
        }
        let mut repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        for task in requested {
            repository.append_event(&task, RuntimeEvent::TaskPauseRequested)?;
        }
        Ok(())
    }

    /// Forces the recovery boundary after the daemon's grace period. Only
    /// workers still held by supervisors are included: acknowledged pauses
    /// have already relinquished their handles and resident permits.
    pub async fn interrupt_unacknowledged_workers(&self) -> Result<(), RuntimeCoordinatorError> {
        let supervisors = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut interrupted = Vec::new();
        for supervisor in supervisors {
            let mut supervisor = supervisor.lock().await;
            let tasks = supervisor
                .interrupt_unacknowledged_workers()
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            for task in tasks {
                let attempt = supervisor
                    .task(&task)
                    .expect("interrupted task is retained by its supervisor")
                    .active_attempt_id()
                    .clone();
                interrupted.push((task, attempt));
            }
        }
        let mut repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        for (task, attempt) in interrupted {
            // The core reducer retains the same active attempt while marking
            // an interruption, so the durable attempt closes with the task.
            repository.transition_task_and_attempt(
                &task,
                &attempt,
                "recovery_required",
                RuntimeEvent::TaskRecoveryRequired,
            )?;
            self.release_resident_lease(&task);
        }
        Ok(())
    }

    /// Applies the graceful-stop worker lifecycle. Admission closes and the
    /// draining event is durable before a worker receives its pause signal;
    /// any worker that misses the configured deadline crosses a recovery
    /// boundary rather than being treated as a user cancellation.
    pub async fn graceful_stop(
        &self,
        options: RuntimeStopOptions,
    ) -> Result<RuntimeStopSummary, RuntimeCoordinatorError> {
        let draining_tasks = self.active_worker_task_ids().await;
        self.begin_draining().await?;
        self.request_safe_checkpoints().await?;

        let deadline = Instant::now() + options.grace;
        while !self.active_worker_task_ids().await.is_empty() && Instant::now() < deadline {
            self.reconcile_worker_events().await?;
            if !self.active_worker_task_ids().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        self.reconcile_worker_events().await?;
        if !self.active_worker_task_ids().await.is_empty() {
            self.interrupt_unacknowledged_workers().await?;
        }

        let repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        let mut summary = RuntimeStopSummary {
            paused: 0,
            recovery_required: 0,
        };
        for task in draining_tasks {
            match repository.task_state(&task)?.as_str() {
                "paused" => summary.paused += 1,
                "recovery_required" => summary.recovery_required += 1,
                _ => {}
            }
        }
        Ok(summary)
    }

    async fn active_worker_task_ids(&self) -> Vec<TaskId> {
        let supervisors = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut tasks = Vec::new();
        for supervisor in supervisors {
            tasks.extend(supervisor.lock().await.worker_task_ids().cloned());
        }
        tasks
    }

    fn ensure_admitting(&self) -> Result<(), RuntimeCoordinatorError> {
        if self.draining.load(Ordering::Acquire) {
            return Err(RuntimeCoordinatorError::Draining);
        }
        Ok(())
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

struct ProviderTurnAdmissions {
    resources: Arc<Mutex<ResourceCoordinator>>,
    repository: Arc<Mutex<RuntimeRepository>>,
    queued: Mutex<HashSet<TaskId>>,
    assigned: Mutex<HashMap<TaskId, LeaseId>>,
    notify: Arc<Notify>,
}

impl ProviderTurnAdmissions {
    fn new(
        resources: Arc<Mutex<ResourceCoordinator>>,
        repository: Arc<Mutex<RuntimeRepository>>,
    ) -> Self {
        Self {
            resources,
            repository,
            queued: Mutex::new(HashSet::new()),
            assigned: Mutex::new(HashMap::new()),
            notify: Arc::new(Notify::new()),
        }
    }

    fn acquire_now(
        &self,
        root_id: RootSessionId,
        parent_id: TaskId,
        task_id: TaskId,
        resource_key: &str,
        priority: AdmissionPriority,
    ) -> Result<Option<ProviderTurnLeaseHandle>, RepositoryError> {
        if let Some(lease_id) = self
            .assigned
            .lock()
            .expect("provider turn assignment mutex poisoned")
            .remove(&task_id)
        {
            return Ok(Some(ProviderTurnLeaseHandle {
                resources: Arc::clone(&self.resources),
                repository: Arc::clone(&self.repository),
                lease_id,
                notify: Arc::clone(&self.notify),
            }));
        }

        let mut resources = self
            .resources
            .lock()
            .expect("resource coordinator mutex poisoned");
        let mut queued = self
            .queued
            .lock()
            .expect("provider turn queue mutex poisoned");
        if queued.insert(task_id.clone()) {
            resources.enqueue_with_priority_for_parent_at(
                root_id,
                parent_id,
                task_id.clone(),
                ResourceRequest {
                    scope: ResourceScope::ProviderKey,
                    key: resource_key.into(),
                    mode: LeaseMode::Shared,
                    units: 1,
                    deadline: None,
                },
                priority,
                Utc::now(),
            );
        }
        let Some(grant) = resources.grant_next(resource_key) else {
            return Ok(None);
        };
        let cursor = resources.admission_cursor(resource_key);
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .save_provider_turn_grant(
                &grant.lease_id.to_string(),
                &grant.task_id,
                resource_key,
                &grant.root_id,
                cursor
                    .parent_id
                    .as_ref()
                    .expect("granted lease has parent cursor"),
                cursor.sequence,
            )?;
        queued.remove(&grant.task_id);
        let granted_task = grant.task_id.clone();
        let lease_id = grant.lease_id;
        if granted_task == task_id {
            return Ok(Some(ProviderTurnLeaseHandle {
                resources: Arc::clone(&self.resources),
                repository: Arc::clone(&self.repository),
                lease_id,
                notify: Arc::clone(&self.notify),
            }));
        }
        self.assigned
            .lock()
            .expect("provider turn assignment mutex poisoned")
            .insert(granted_task, lease_id);
        Ok(None)
    }

    fn cancel_task(&self, task_id: &TaskId) {
        self.resources
            .lock()
            .expect("resource coordinator mutex poisoned")
            .cancel_task_requests(task_id);
        self.queued
            .lock()
            .expect("provider turn queue mutex poisoned")
            .remove(task_id);
        if let Some(lease_id) = self
            .assigned
            .lock()
            .expect("provider turn assignment mutex poisoned")
            .remove(task_id)
        {
            self.resources
                .lock()
                .expect("resource coordinator mutex poisoned")
                .release(lease_id.clone())
                .expect("provider turn lease release is idempotent");
            let _ = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .release_provider_turn_lease(&lease_id.to_string());
        }
        self.notify.notify_waiters();
    }
}

struct ProviderTurnLeaseHandle {
    resources: Arc<Mutex<ResourceCoordinator>>,
    repository: Arc<Mutex<RuntimeRepository>>,
    lease_id: LeaseId,
    notify: Arc<Notify>,
}

impl Drop for ProviderTurnLeaseHandle {
    fn drop(&mut self) {
        self.resources
            .lock()
            .expect("resource coordinator mutex poisoned")
            .release(self.lease_id.clone())
            .expect("provider turn lease release is idempotent");
        let _ = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .release_provider_turn_lease(&self.lease_id.to_string());
        self.notify.notify_waiters();
    }
}

struct RuntimeProviderTurnGate {
    admissions: Arc<ProviderTurnAdmissions>,
    root_id: RootSessionId,
    parent_id: TaskId,
    task_id: TaskId,
    profile_id: String,
    supervisor: Arc<AsyncMutex<AgentSupervisor>>,
}

impl ProviderTurnGate for RuntimeProviderTurnGate {
    fn acquire(
        &self,
    ) -> futures::future::BoxFuture<
        'static,
        Result<Box<dyn yi_agent_core::ProviderTurnLease>, String>,
    > {
        let admissions = Arc::clone(&self.admissions);
        let root_id = self.root_id.clone();
        let parent_id = self.parent_id.clone();
        let task_id = self.task_id.clone();
        let profile_id = self.profile_id.clone();
        let supervisor = Arc::clone(&self.supervisor);
        Box::pin(async move {
            let mut cleanup = ProviderTurnWaitCleanup {
                admissions: Arc::clone(&admissions),
                task_id: task_id.clone(),
                active: true,
            };
            loop {
                let notified = admissions.notify.notified();
                let priority = supervisor
                    .lock()
                    .await
                    .provider_turn_admission_priority(&task_id);
                let resource_key = if priority == AdmissionPriority::Normal {
                    format!("llm:{profile_id}")
                } else {
                    format!("llm-coordination:{profile_id}")
                };
                if let Some(lease) = admissions
                    .acquire_now(
                        root_id.clone(),
                        parent_id.clone(),
                        task_id.clone(),
                        &resource_key,
                        priority,
                    )
                    .map_err(|error| error.to_string())?
                {
                    cleanup.active = false;
                    return Ok(Box::new(lease) as Box<dyn yi_agent_core::ProviderTurnLease>);
                }
                notified.await;
            }
        })
    }
}

struct ProviderTurnWaitCleanup {
    admissions: Arc<ProviderTurnAdmissions>,
    task_id: TaskId,
    active: bool,
}

impl Drop for ProviderTurnWaitCleanup {
    fn drop(&mut self) {
        if self.active {
            self.admissions.cancel_task(&self.task_id);
        }
    }
}

#[cfg(test)]
mod provider_turn_admission_tests {
    use super::*;

    fn test_repository(root: &RootSessionId, tasks: &[TaskId]) -> Arc<Mutex<RuntimeRepository>> {
        let mut repository = RuntimeRepository::open(":memory:").unwrap();
        for task in tasks {
            repository.create_task(task, root, "running").unwrap();
        }
        Arc::new(Mutex::new(repository))
    }

    #[test]
    fn queued_provider_turn_is_assigned_to_its_selected_task_after_release() {
        let mut resources = ResourceCoordinator::new();
        resources.set_capacity("llm:test", 1);
        let root = RootSessionId::new();
        let first_task = TaskId::new();
        let second_task = TaskId::new();
        let repository = test_repository(&root, &[first_task.clone(), second_task.clone()]);
        let admissions =
            ProviderTurnAdmissions::new(Arc::new(Mutex::new(resources)), Arc::clone(&repository));

        let first = admissions
            .acquire_now(
                root.clone(),
                first_task.clone(),
                first_task.clone(),
                "llm:test",
                AdmissionPriority::Normal,
            )
            .unwrap();
        assert!(first.is_some());
        assert!(
            repository
                .lock()
                .unwrap()
                .has_active_lease_prefix(&first_task, "llm:")
                .unwrap()
        );
        assert!(
            repository
                .lock()
                .unwrap()
                .admission_cursor("llm:test")
                .unwrap()
                .is_some()
        );
        assert!(
            admissions
                .acquire_now(
                    root.clone(),
                    second_task.clone(),
                    second_task.clone(),
                    "llm:test",
                    AdmissionPriority::Normal,
                )
                .unwrap()
                .is_none()
        );

        drop(first);
        assert!(
            !repository
                .lock()
                .unwrap()
                .has_active_lease_prefix(&first_task, "llm:")
                .unwrap()
        );

        assert!(
            admissions
                .acquire_now(
                    root,
                    second_task.clone(),
                    second_task,
                    "llm:test",
                    AdmissionPriority::Normal
                )
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn waiting_provider_turn_gate_wakes_after_a_lease_releases() {
        let mut resources = ResourceCoordinator::new();
        resources.set_capacity("llm:test", 1);
        let root = RootSessionId::new();
        let supervisor = Arc::new(AsyncMutex::new(AgentSupervisor::new(root.clone())));
        let first_task = supervisor.lock().await.root_task_id().clone();
        let second_task = TaskId::new();
        let repository = test_repository(&root, &[first_task.clone(), second_task.clone()]);
        let admissions = Arc::new(ProviderTurnAdmissions::new(
            Arc::new(Mutex::new(resources)),
            repository,
        ));
        let first = admissions
            .acquire_now(
                root.clone(),
                first_task.clone(),
                first_task,
                "llm:test",
                AdmissionPriority::Normal,
            )
            .unwrap()
            .unwrap();
        let gate = RuntimeProviderTurnGate {
            admissions,
            root_id: root,
            parent_id: second_task.clone(),
            task_id: second_task,
            profile_id: "test".into(),
            supervisor,
        };

        let waiter = tokio::spawn(async move { gate.acquire().await });
        tokio::task::yield_now().await;
        drop(first);

        let lease = tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("release must wake the queued provider turn")
            .unwrap()
            .unwrap();
        drop(lease);
    }

    #[test]
    fn coordination_provider_turn_admits_after_regular_pool_is_saturated() {
        let mut resources = ResourceCoordinator::new();
        resources.configure_provider_llm_capacity("test");
        let root = RootSessionId::new();
        let tasks = (0..8).map(|_| TaskId::new()).collect::<Vec<_>>();
        let repository = test_repository(&root, &tasks);
        let admissions = ProviderTurnAdmissions::new(Arc::new(Mutex::new(resources)), repository);
        let mut regular_leases = Vec::new();

        for task in &tasks[..7] {
            regular_leases.push(
                admissions
                    .acquire_now(
                        root.clone(),
                        task.clone(),
                        task.clone(),
                        "llm:test",
                        AdmissionPriority::Normal,
                    )
                    .unwrap()
                    .expect("regular pool has seven permits"),
            );
        }

        let coordination = admissions
            .acquire_now(
                root,
                tasks[7].clone(),
                tasks[7].clone(),
                "llm-coordination:test",
                AdmissionPriority::High,
            )
            .unwrap();
        assert!(
            coordination.is_some(),
            "parent coordination uses the reserve"
        );
        drop(coordination);
        drop(regular_leases);
    }

    #[test]
    fn startup_releases_provider_leases_and_restores_the_cursor() {
        struct ProfileFactory;
        impl AgentWorkerFactory for ProfileFactory {
            fn provider_profile_id(&self) -> Option<String> {
                Some("test".into())
            }
            fn start(
                &self,
                request: yi_agent_core::subagent::worker::WorkerStart,
            ) -> futures::future::BoxFuture<
                'static,
                Result<
                    yi_agent_core::subagent::worker::WorkerHandle,
                    yi_agent_core::subagent::worker::WorkerError,
                >,
            > {
                Box::pin(async move {
                    Ok(yi_agent_core::subagent::worker::WorkerHandle::new(
                        request.cancellation,
                    ))
                })
            }
        }

        let directory = tempfile::TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let root = RootSessionId::new();
        let task = TaskId::new();
        let mut repository = RuntimeRepository::open(&database).unwrap();
        repository.create_task(&task, &root, "queued").unwrap();
        repository
            .save_provider_turn_grant("interrupted-turn", &task, "llm:test", &root, &task, 41)
            .unwrap();
        drop(repository);

        let coordinator = RuntimeCoordinator::open(&database, Arc::new(ProfileFactory)).unwrap();
        assert_eq!(
            coordinator
                .resource_coordinator
                .lock()
                .unwrap()
                .admission_cursor("llm:test")
                .sequence,
            41
        );
        assert!(
            !RuntimeRepository::open(&database)
                .unwrap()
                .has_active_lease_prefix(&task, "llm:")
                .unwrap()
        );
    }
}

#[derive(Clone, Default)]
struct RecoveryContext {
    workspace_lease_id: Option<String>,
    worktree_lease: Option<String>,
    checkpoint_json: Option<String>,
    tool_state_json: String,
    recovery_gated: bool,
    recovery_attested: bool,
}

impl RecoveryContext {
    fn worker_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: self.workspace_lease_id.clone(),
            worktree_lease: self.worktree_lease.clone(),
            checkpoint_json: self.checkpoint_json.clone().unwrap_or_default(),
            tool_state_json: self.tool_state_json.clone(),
        }
    }
}
