//! Process-local ownership of subagent supervisors and application workers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use yi_agent_core::subagent::scheduler::ResourceCoordinator;
use yi_agent_core::subagent::supervisor::{AgentSupervisor, SpawnError, WaitMode, WaitOutcome};
use yi_agent_core::subagent::task::{MessageId, PauseReason, RootSessionId, TaskId};
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
    repository: Mutex<RuntimeRepository>,
    factory: Arc<dyn AgentWorkerFactory>,
    supervisors: Mutex<HashMap<RootSessionId, Arc<AsyncMutex<AgentSupervisor>>>>,
    resident_tasks: Mutex<HashSet<TaskId>>,
    recovery_contexts: Mutex<HashMap<TaskId, RecoveryContext>>,
    draining: AtomicBool,
}

impl RuntimeCoordinator {
    pub fn open(
        database_path: impl AsRef<Path>,
        factory: Arc<dyn AgentWorkerFactory>,
    ) -> Result<Self, RuntimeCoordinatorError> {
        let repository = RuntimeRepository::open(database_path)?;
        let recovered_tasks = repository.recovered_tasks()?;
        let mut supervisors = HashMap::new();
        let mut recovery_contexts = HashMap::new();
        for task in recovered_tasks {
            recovery_contexts.insert(
                task.task_id.clone(),
                RecoveryContext {
                    workspace_lease_id: task.workspace_lease_id.clone(),
                    worktree_lease: task.worktree_lease.clone(),
                    checkpoint_json: task.checkpoint_json.clone(),
                    tool_state_json: task.tool_state_json.clone(),
                },
            );
            if task.parent_id.is_none() {
                supervisors.insert(
                    task.session_id.clone(),
                    Arc::new(AsyncMutex::new(AgentSupervisor::from_recovered_root(
                        task.session_id,
                        task.task_id,
                        task.attempt_id,
                        task.attempt_number,
                    ))),
                );
            } else {
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
                        task.parent_id.expect("child parent was checked"),
                        depth,
                        task.attempt_id,
                        task.attempt_number,
                    )
                    .map_err(RuntimeCoordinatorError::Supervisor)?;
            }
        }
        Ok(Self {
            repository: Mutex::new(repository),
            factory,
            supervisors: Mutex::new(supervisors),
            resident_tasks: Mutex::new(HashSet::new()),
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
        let supervisor = self.supervisor(session)?;
        let (child, depth, attempt) = {
            let mut supervisor = supervisor.lock().await;
            let child = supervisor.spawn_with_objective(parent.clone(), objective)?;
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
            .create_child_task_with_attempt(
                &child,
                session,
                parent,
                depth,
                &attempt.id,
                attempt.number,
                "queued",
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
        let recovery_context = self.factory.recovery_context();
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
        let attempt = supervisor
            .task(task)
            .expect("started worker task exists")
            .active_attempt_id()
            .clone();
        if let Err(error) = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .transition_task_and_attempt_with_recovery_context(
                task,
                &attempt,
                "running",
                RuntimeEvent::TaskStarted,
                &recovery_context,
            )
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
            self.resident_tasks
                .lock()
                .expect("runtime resident task mutex poisoned")
                .remove(&task);
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
        let recovery_attempt = {
            let mut supervisor = supervisor.lock().await;
            if matches!(
                supervisor.task(task).map(|task| task.state()),
                Some(yi_agent_core::TaskState::RecoveryRequired(_))
            ) {
                Some(
                    supervisor
                        .resume_recovery_task(
                            task,
                            recovery_inspection_instruction(&self.recovery_context(task)),
                        )
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
                    "queued",
                    RuntimeEvent::TaskQueued,
                )?;
                self.recovery_contexts
                    .lock()
                    .expect("runtime recovery context mutex poisoned")
                    .remove(task);
            } else {
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
            self.resident_tasks
                .lock()
                .expect("runtime resident task mutex poisoned")
                .remove(&task);
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

    fn recovery_context(&self, task: &TaskId) -> RecoveryContext {
        self.recovery_contexts
            .lock()
            .expect("runtime recovery context mutex poisoned")
            .get(task)
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Clone, Default)]
struct RecoveryContext {
    workspace_lease_id: Option<String>,
    worktree_lease: Option<String>,
    checkpoint_json: Option<String>,
    tool_state_json: String,
}

fn recovery_inspection_instruction(context: &RecoveryContext) -> String {
    if !context.has_safe_base() {
        return format!(
            "RECOVERY_CONTROLLER_UNSAFE: durable recovery evidence is incomplete: workspace={}; worktree={}; checkpoint={}; tool_state={}. Report RecoveryConflict before any provider call, objective, tool, command, or Git action; do not run git status.",
            context
                .workspace_lease_id
                .as_deref()
                .unwrap_or("<none recorded>"),
            context
                .worktree_lease
                .as_deref()
                .unwrap_or("<none recorded>"),
            context
                .checkpoint_json
                .as_deref()
                .unwrap_or("<none recorded>"),
            context.tool_state_json,
        );
    }
    format!(
        "Recovery required before changes: inspect the recorded workspace lease: {}; recorded worktree: {}; run git status, identify the latest commit, inspect required tool state evidence: {}; compare the prior checkpoint evidence: {}; and stop with RecoveryConflict if a safe base cannot be proven. Do not replay prior provider, tool, command, or Git actions.",
        context
            .workspace_lease_id
            .as_deref()
            .unwrap_or("<none recorded>"),
        context
            .worktree_lease
            .as_deref()
            .unwrap_or("<none recorded>"),
        context.tool_state_json,
        context
            .checkpoint_json
            .as_deref()
            .unwrap_or("<none recorded>"),
    )
}

impl RecoveryContext {
    fn has_safe_base(&self) -> bool {
        self.workspace_lease_id.is_some()
            && self.worktree_lease.is_some()
            && self
                .checkpoint_json
                .as_ref()
                .is_some_and(|checkpoint| checkpoint.contains("\"git_head\":\""))
            && self.tool_state_json.contains("\"state\":\"available\"")
    }
}
