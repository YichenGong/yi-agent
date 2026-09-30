//! Process-local ownership of subagent supervisors and application workers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Timelike, Utc};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use uuid::Uuid;
use yi_agent_core::ProviderTurnGate;
use yi_agent_core::subagent::mailbox::{
    MailboxMessage, MailboxMessageDraft, MessageKind, ReworkInstruction,
};
use yi_agent_core::subagent::scheduler::{
    AdmissionCursor, AdmissionPriority, LeaseId, LeaseMode, ResourceCoordinator, ResourceRequest,
    ResourceScope,
};
use yi_agent_core::subagent::supervisor::{
    AgentSupervisor, CompletedChildReport, ReviewPersistenceError, SpawnError, WaitMode,
    WaitOutcome,
};
use yi_agent_core::subagent::task::{
    AgentTask, AttemptId, BlockReason, BudgetKind, CancelReason, ChildWriteMode, DeliveryId,
    DeliveryReport, IntegrationValidation, MessageId, PauseReason, PermissionDecision,
    PermissionRequestId, RecoveryEvidence, RootSessionId, TaskFailure, TaskId, TaskState,
    TimeoutKind, WatchdogEvidence as CoreWatchdogEvidence, WorkspaceLeaseId,
};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerRecoveryPreflight,
    WorkerRecoveryPreflightResult, WorkerStart, WorkerWatchdogEvent, WorkerWorkspace,
};

use crate::repository::{
    RepositoryError, RuntimeEvent, RuntimeRepository, ScheduleOccurrenceResult, WatchdogEvidence,
    WatchdogTerminal,
};
use crate::schedule::{MissedRunPolicy, WatchdogOutcome, evaluate_watchdog};

const REVIEW_CONFIRMATION_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum RuntimeCoordinatorError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(
        "controlled recovery persistence failed after acknowledgement error: acknowledgement={acknowledgement}; cleanup={cleanup:?}; recovery={recovery:?}"
    )]
    ControlledRecoveryPersistence {
        acknowledgement: String,
        cleanup: Option<String>,
        recovery: Option<String>,
    },
    #[error("session does not exist: {0}")]
    SessionNotFound(RootSessionId),
    #[error("supervisor error: {0}")]
    Supervisor(String),
    #[error("authority denied: {0}")]
    AuthorityDenied(String),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedApplicationRoot {
    pub session_id: RootSessionId,
    pub root_task_id: TaskId,
    pub message_capability: String,
    pub workspace: WorkerWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    Rework { feedback: String },
    Reject { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPreview {
    pub task_id: TaskId,
    pub delivery_id: DeliveryId,
    pub confirmation_token: String,
    pub expires_in_secs: u64,
    pub decision: ReviewDecision,
}

#[derive(Debug, Clone)]
struct PendingReviewConfirmation {
    task_id: TaskId,
    decision: ReviewDecision,
    delivery_id: DeliveryId,
    delivery_commit: String,
    workspace: Option<WorkerWorkspace>,
    expires_at: Instant,
}

/// Owns all supervisor instances and their worker handles for one daemon.
///
/// The coordinator deliberately receives a factory instead of constructing an
/// `Agent`: provider and tool bootstrapping remain an application concern.
pub struct RuntimeCoordinator {
    repository: Arc<Mutex<RuntimeRepository>>,
    factory: Arc<dyn AgentWorkerFactory>,
    workspace_service: Option<Arc<dyn yi_agent_core::subagent::worker::WorkerWorkspaceProvider>>,
    application_root_workspace_services: Mutex<
        HashMap<RootSessionId, Arc<dyn yi_agent_core::subagent::worker::WorkerWorkspaceProvider>>,
    >,
    supervisors: Mutex<HashMap<RootSessionId, Arc<AsyncMutex<AgentSupervisor>>>>,
    resident_tasks: Mutex<HashSet<TaskId>>,
    resident_leases: Mutex<HashMap<TaskId, LeaseId>>,
    resident_waiting: Mutex<HashSet<TaskId>>,
    resource_coordinator: Arc<Mutex<ResourceCoordinator>>,
    provider_turn_admissions: Option<Arc<ProviderTurnAdmissions>>,
    provider_profile_id: Option<String>,
    recovery_contexts: Mutex<HashMap<TaskId, RecoveryContext>>,
    review_confirmations: Mutex<HashMap<String, PendingReviewConfirmation>>,
    /// Where a task last ran, remembered for as long as the coordinator lives.
    /// Nothing durable owns it: this replaces the retired `task_workspaces` row.
    task_positions: Mutex<HashMap<TaskId, WorkerWorkspace>>,
    application_root_attach_lock: Mutex<()>,
    draining: AtomicBool,
}

struct WorkspaceAssignedFactory<'a> {
    inner: &'a dyn AgentWorkerFactory,
    workspace: WorkerWorkspace,
}

impl AgentWorkerFactory for WorkspaceAssignedFactory<'_> {
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    fn provider_profile_id(&self) -> Option<String> {
        self.inner.provider_profile_id()
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        self.inner.recovery_context()
    }

    fn recovery_context_for(&self, request: &WorkerStart) -> WorkerRecoveryContext {
        self.inner.recovery_context_for(request)
    }

    fn preflight_recovery(
        &self,
        request: WorkerRecoveryPreflight,
    ) -> WorkerRecoveryPreflightResult {
        self.inner.preflight_recovery(request)
    }

    fn start(
        &self,
        request: WorkerStart,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.inner
            .start(request.with_workspace(self.workspace.clone()))
    }

    fn start_with_provider_turn_gate(
        &self,
        request: WorkerStart,
        gate: Option<Arc<dyn ProviderTurnGate>>,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        self.inner
            .start_with_provider_turn_gate(request.with_workspace(self.workspace.clone()), gate)
    }
}

fn legacy_application_root_capability(idempotency_key: &str) -> String {
    format!(
        "app-root-{}",
        digest_hex(&format!("application-root:{idempotency_key}"))
    )
}

fn new_application_root_capability() -> String {
    format!("app-root-{}", uuid::Uuid::new_v4())
}

fn digest_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An in-place position: a task running directly in `path`, owning no branch.
fn in_place_workspace_at(
    repository_root: std::path::PathBuf,
    path: std::path::PathBuf,
) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root,
        path,
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    }
}

/// The minimum recorded position `project_workspace_matches` needs: it compares
/// only `repository_root`.
fn recorded_workspace(repository_root: &str) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: PathBuf::from(repository_root),
        path: PathBuf::from(repository_root),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    }
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
        // Take the lease id out under `resident_leases` alone, then release it
        // under `resource_coordinator` alone. Holding both at once here would
        // invert the `resource_coordinator -> resident_*` order used by
        // `start_worker` and deadlock the reconcile loop.
        let lease_id = self
            .resident_leases
            .lock()
            .expect("runtime resident lease mutex poisoned")
            .remove(task);
        if let Some(lease_id) = lease_id {
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
        repository.release_provider_turn_leases()?;
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
        let review_tasks = repository.review_hydration_tasks()?;
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
                for ancestor in review_tasks.iter().filter(|candidate| {
                    candidate.session_id == task.session_id && candidate.depth < task.depth
                }) {
                    let ancestor_depth = persisted_depth(ancestor.depth)?;
                    let (state, delivery, objective, completion_report) = hydrated_review_state(
                        &ancestor.state,
                        &ancestor.delivery_json,
                        ancestor.terminal_json.as_deref(),
                    )?;
                    let task_id = ancestor.task_id.clone();
                    let hydrated = AgentTask::hydrated_review_task(
                        ancestor.session_id.clone(),
                        task_id.clone(),
                        ancestor.parent_id.clone(),
                        ancestor_depth,
                        ancestor.attempt_id.clone(),
                        ancestor.attempt_number,
                        state,
                        delivery,
                    );
                    if ancestor.parent_id.is_none() {
                        if !supervisors.contains_key(&ancestor.session_id) {
                            supervisors.insert(
                                ancestor.session_id.clone(),
                                Arc::new(AsyncMutex::new({
                                    let mut supervisor = AgentSupervisor::from_hydrated_review_root(
                                        hydrated, objective,
                                    );
                                    hydrate_completion_report(
                                        &mut supervisor,
                                        task_id,
                                        completion_report,
                                    )?;
                                    supervisor
                                })),
                            );
                        }
                        continue;
                    }
                    let supervisor = supervisors.get(&ancestor.session_id).ok_or_else(|| {
                        RuntimeCoordinatorError::Supervisor(
                            "persisted recovery ancestor has no root".into(),
                        )
                    })?;
                    let mut supervisor = supervisor.try_lock().map_err(|_| {
                        RuntimeCoordinatorError::Supervisor(
                            "recovery ancestor hydration is busy".into(),
                        )
                    })?;
                    if supervisor.task(&ancestor.task_id).is_none() {
                        let mode = repository.task_workspace_mode(&ancestor.task_id)?;
                        let model = repository.task_model(&ancestor.task_id)?;
                        supervisor
                            .insert_hydrated_review_child(hydrated, objective, mode, model)
                            .map_err(RuntimeCoordinatorError::Supervisor)?;
                        hydrate_completion_report(&mut supervisor, task_id, completion_report)?;
                    }
                }
                let supervisor = supervisors.get(&task.session_id).ok_or_else(|| {
                    RuntimeCoordinatorError::Supervisor(
                        "persisted recovered child has no recovered root".into(),
                    )
                })?;
                // Read the model before `task.task_id` is moved into the call.
                let model = repository.task_model(&task.task_id)?;
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
                        task.workspace_mode,
                        model,
                    )
                    .map_err(RuntimeCoordinatorError::Supervisor)?;
            } else {
                // A recovered root must keep the mode persisted at spawn time:
                // reattaching an application root early-returns before the
                // per-root service is consulted, so a factory-global guess would
                // wrongly promote a non-git root to `Coding`.
                let root_mode = task.workspace_mode;
                let mut supervisor = if task.recovery_gated || task.recovery_attested {
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
                let root_id = supervisor.root_task_id().clone();
                supervisor.set_workspace_mode(&root_id, root_mode);
                supervisors.insert(task.session_id, Arc::new(AsyncMutex::new(supervisor)));
            }
        }
        for task in &review_tasks {
            if supervisors
                .get(&task.session_id)
                .and_then(|supervisor| supervisor.try_lock().ok())
                .is_some_and(|supervisor| supervisor.task(&task.task_id).is_some())
            {
                continue;
            }
            let depth = persisted_depth(task.depth)?;
            let (state, delivery, objective, completion_report) = hydrated_review_state(
                &task.state,
                &task.delivery_json,
                task.terminal_json.as_deref(),
            )?;
            let task_id = task.task_id.clone();
            let hydrated = AgentTask::hydrated_review_task(
                task.session_id.clone(),
                task_id.clone(),
                task.parent_id.clone(),
                depth,
                task.attempt_id.clone(),
                task.attempt_number,
                state,
                delivery,
            );
            if task.parent_id.is_none() {
                supervisors.insert(
                    task.session_id.clone(),
                    Arc::new(AsyncMutex::new({
                        let mut supervisor =
                            AgentSupervisor::from_hydrated_review_root(hydrated, objective);
                        hydrate_completion_report(&mut supervisor, task_id, completion_report)?;
                        supervisor
                    })),
                );
            } else {
                let mut supervisor = supervisors
                    .get(&task.session_id)
                    .ok_or_else(|| {
                        RuntimeCoordinatorError::Supervisor(
                            "persisted review child has no hydrated root".into(),
                        )
                    })?
                    .try_lock()
                    .map_err(|_| {
                        RuntimeCoordinatorError::Supervisor("review hydration is busy".into())
                    })?;
                let mode = repository.task_workspace_mode(&task.task_id)?;
                let model = repository.task_model(&task.task_id)?;
                supervisor
                    .insert_hydrated_review_child(hydrated, objective, mode, model)
                    .map_err(RuntimeCoordinatorError::Supervisor)?;
                hydrate_completion_report(&mut supervisor, task_id, completion_report)?;
            }
        }
        for task in &review_tasks {
            let messages = repository.mailbox_messages_for_task(&task.task_id)?;
            let supervisor = supervisors.get(&task.session_id).ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "persisted review mailbox has no hydrated supervisor".into(),
                )
            })?;
            let mut supervisor = supervisor.try_lock().map_err(|_| {
                RuntimeCoordinatorError::Supervisor("review mailbox hydration is busy".into())
            })?;
            for message in messages
                .into_iter()
                .filter(|message| message.delivered_at.is_none())
            {
                let message_id: MessageId = message.message_id.parse().map_err(|_| {
                    RuntimeCoordinatorError::Supervisor(
                        "persisted review mailbox has an invalid message ID".into(),
                    )
                })?;
                let payload: serde_json::Value =
                    serde_json::from_str(&message.payload_json).map_err(RepositoryError::from)?;
                let draft = match message.kind.as_str() {
                    "user_override" if payload["kind"] == "user_review_override" => {
                        let body = payload["message"].as_str().ok_or_else(|| {
                            RuntimeCoordinatorError::Supervisor(
                                "persisted review notification has no message".into(),
                            )
                        })?;
                        MailboxMessageDraft::user_override_with_id(
                            message_id,
                            task.task_id.clone(),
                            body,
                        )
                    }
                    "rework" => {
                        let sender = message.sender_task_id.ok_or_else(|| {
                            RuntimeCoordinatorError::Supervisor(
                                "persisted rework message has no direct parent".into(),
                            )
                        })?;
                        let feedback = payload["feedback"].as_str().ok_or_else(|| {
                            RuntimeCoordinatorError::Supervisor(
                                "persisted rework message has no feedback".into(),
                            )
                        })?;
                        MailboxMessageDraft::new_with_id(
                            message_id,
                            sender,
                            task.task_id.clone(),
                            MessageKind::Rework(ReworkInstruction(feedback.into())),
                            Some(task.attempt_id.clone()),
                        )
                    }
                    "review_rejected" => {
                        let sender = message.sender_task_id.ok_or_else(|| {
                            RuntimeCoordinatorError::Supervisor(
                                "persisted rejection message has no direct parent".into(),
                            )
                        })?;
                        MailboxMessageDraft::new_with_id(
                            message_id.clone(),
                            sender,
                            task.task_id.clone(),
                            MessageKind::ReviewRejected(message_id),
                            Some(task.attempt_id.clone()),
                        )
                    }
                    _ => continue,
                };
                supervisor
                    .stage_persisted_message(draft)
                    .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            }
        }
        let provider_profile_id = factory.provider_profile_id();
        let workspace_service = factory.default_workspace_service();
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
            workspace_service,
            application_root_workspace_services: Mutex::new(HashMap::new()),
            supervisors: Mutex::new(supervisors),
            resident_tasks: Mutex::new(HashSet::new()),
            resident_leases: Mutex::new(HashMap::new()),
            resident_waiting: Mutex::new(HashSet::new()),
            resource_coordinator,
            provider_turn_admissions,
            provider_profile_id,
            recovery_contexts: Mutex::new(recovery_contexts),
            review_confirmations: Mutex::new(HashMap::new()),
            task_positions: Mutex::new(HashMap::new()),
            application_root_attach_lock: Mutex::new(()),
            draining: AtomicBool::new(false),
        })
    }

    pub fn create_session(&self) -> Result<RootSessionId, RuntimeCoordinatorError> {
        self.create_session_with_objective("Root session objective not specified.".into())
    }

    /// Creates an isolated root session with an immutable initial objective.
    /// Scheduled fires use this rather than inheriting any interactive session.
    pub fn create_session_with_objective(
        &self,
        objective: String,
    ) -> Result<RootSessionId, RuntimeCoordinatorError> {
        self.create_session_with_objective_and_mode(objective, ChildWriteMode::Coding)
    }

    /// Creates an isolated root session with an immutable initial objective and
    /// an explicit workspace mode. A non-git application root passes
    /// `ReadOnly` so its root runs in place.
    pub fn create_session_with_objective_and_mode(
        &self,
        objective: String,
        workspace_mode: ChildWriteMode,
    ) -> Result<RootSessionId, RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let session_id = RootSessionId::new();
        let mut supervisor =
            AgentSupervisor::new_with_objective(session_id.clone(), objective.clone());
        let root_id = supervisor.root_task_id().clone();
        supervisor.set_workspace_mode(&root_id, workspace_mode);
        let root_attempt = supervisor
            .task(&root_id)
            .expect("new root task exists")
            .active_attempt()
            .clone();
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .create_task_with_attempt_and_objective(
                &root_id,
                &session_id,
                &root_attempt.id,
                root_attempt.number,
                "queued",
                &objective,
                workspace_mode,
                None,
            )?;
        self.supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .insert(session_id.clone(), Arc::new(AsyncMutex::new(supervisor)));
        Ok(session_id)
    }

    // This synchronous guard serializes the durable idempotency check and root
    // creation; it intentionally spans awaits to prevent duplicate roots.
    #[allow(clippy::await_holding_lock)]
    pub async fn attach_application_root(
        &self,
        idempotency_key: &str,
        requested_workspace: &Path,
    ) -> Result<AttachedApplicationRoot, RuntimeCoordinatorError> {
        let _attach_guard = self
            .application_root_attach_lock
            .lock()
            .expect("runtime application root attach mutex poisoned");
        self.ensure_admitting()?;
        if idempotency_key.trim().is_empty() {
            return Err(RuntimeCoordinatorError::Supervisor(
                "application root idempotency key is required".into(),
            ));
        }
        let service = self
            .factory
            .workspace_service_for_project(requested_workspace)
            .ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "application root workspace service is unavailable".into(),
                )
            })?;
        // A root never provisions a worktree: it runs in the project directory.
        let root_mode = ChildWriteMode::ReadOnly;
        let existing = {
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .application_root_attachment(idempotency_key)?
        };
        if let Some(existing) = existing {
            // A root runs in place, so it keeps no workspace row: its project is
            // validated against the durable attachment record, and the in-place
            // position is synthesized from the requested project.
            if let Some(recorded) = existing.workspace_root.as_deref() {
                if !self
                    .factory
                    .project_workspace_matches(requested_workspace, &recorded_workspace(recorded))
                {
                    return Err(RuntimeCoordinatorError::Supervisor(
                        "application root workspace does not match its recorded repository".into(),
                    ));
                }
            }
            let workspace = service
                .read_only_workspace(None, &existing.root_task_id)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            self.remember_task_position(&existing.root_task_id, &workspace);
            if existing.state == "detached" {
                self.repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .reattach_application_root(idempotency_key)?;
            }
            self.application_root_workspace_services
                .lock()
                .expect("runtime application root workspace service mutex poisoned")
                .insert(existing.root_session_id.clone(), service);
            self.ensure_application_root_supervisor(&existing)?;
            let capability = existing
                .capability_secret
                .clone()
                .unwrap_or_else(|| legacy_application_root_capability(idempotency_key));
            return Ok(AttachedApplicationRoot {
                session_id: existing.root_session_id,
                root_task_id: existing.root_task_id,
                message_capability: capability,
                workspace,
            });
        }

        let capability = new_application_root_capability();
        let capability_digest = digest_hex(&capability);
        let session_id = self.create_session_with_objective_and_mode(
            "TUI application root pending activation.".into(),
            root_mode,
        )?;
        self.application_root_workspace_services
            .lock()
            .expect("runtime application root workspace service mutex poisoned")
            .insert(session_id.clone(), service);
        let supervisor_handle = self.supervisor(&session_id)?;
        let mut supervisor = supervisor_handle.lock().await;
        let root_task_id = supervisor.root_task_id().clone();
        let attempt = supervisor
            .task(&root_task_id)
            .expect("root task exists")
            .active_attempt_id()
            .clone();
        let workspace = self
            .prepare_task_workspace(
                &mut supervisor,
                &session_id,
                &root_task_id,
                &attempt,
                root_mode,
            )?
            .ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "application root workspace service is unavailable".into(),
                )
            })?;
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .record_application_root_attachment(
                idempotency_key,
                &session_id,
                &root_task_id,
                &capability_digest,
                &capability,
                &workspace.repository_root.to_string_lossy(),
            )?;
        Ok(AttachedApplicationRoot {
            session_id,
            root_task_id,
            message_capability: capability,
            workspace,
        })
    }

    fn ensure_application_root_supervisor(
        &self,
        attachment: &crate::repository::ApplicationRootAttachment,
    ) -> Result<(), RuntimeCoordinatorError> {
        if self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .contains_key(&attachment.root_session_id)
        {
            return Ok(());
        }
        let tasks = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .application_root_hydration_tasks(&attachment.root_session_id)?;
        if tasks.is_empty() {
            return Err(RuntimeCoordinatorError::SessionNotFound(
                attachment.root_session_id.clone(),
            ));
        }
        let root_mode = ChildWriteMode::ReadOnly;
        let mut hydrated_supervisor = None;
        for task in tasks {
            let depth = persisted_depth(task.depth)?;
            let (state, delivery, objective, completion_report) = hydrated_application_root_state(
                &task.state,
                &task.delivery_json,
                task.terminal_json.as_deref(),
            )?;
            let task_id = task.task_id.clone();
            let hydrated = AgentTask::hydrated_review_task(
                task.session_id.clone(),
                task_id.clone(),
                task.parent_id.clone(),
                depth,
                task.attempt_id.clone(),
                task.attempt_number,
                state,
                delivery,
            );
            if task.parent_id.is_none() {
                let mut supervisor =
                    AgentSupervisor::from_hydrated_review_root(hydrated, objective);
                supervisor.set_workspace_mode(&task_id, root_mode);
                hydrate_completion_report(&mut supervisor, task_id, completion_report)?;
                hydrated_supervisor = Some(supervisor);
            } else {
                let mode = self
                    .repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .task_workspace_mode(&task_id)?;
                let model = self
                    .repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .task_model(&task_id)?;
                let supervisor = hydrated_supervisor.as_mut().ok_or_else(|| {
                    RuntimeCoordinatorError::Supervisor(
                        "application root child has no hydrated root".into(),
                    )
                })?;
                supervisor
                    .insert_hydrated_review_child(hydrated, objective, mode, model)
                    .map_err(RuntimeCoordinatorError::Supervisor)?;
                hydrate_completion_report(supervisor, task_id, completion_report)?;
            }
        }
        let supervisor = hydrated_supervisor.ok_or_else(|| {
            RuntimeCoordinatorError::Supervisor("application root hydration found no root".into())
        })?;
        self.supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .insert(
                attachment.root_session_id.clone(),
                Arc::new(AsyncMutex::new(supervisor)),
            );
        Ok(())
    }

    fn authorize_application_root(
        &self,
        session: &RootSessionId,
        root_task: &TaskId,
        capability: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        let capability_digest = digest_hex(capability);
        let authorized = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .application_root_capability_matches(session, root_task, &capability_digest)?;
        if !authorized {
            return Err(RuntimeCoordinatorError::AuthorityDenied(
                "application root capability is invalid".into(),
            ));
        }
        Ok(())
    }

    pub async fn activate_application_root(
        &self,
        session: &RootSessionId,
        root_task: &TaskId,
        capability: &str,
        objective: String,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.authorize_application_root(session, root_task, capability)?;
        let attempt = {
            let supervisor = self.supervisor(session)?;
            let supervisor = supervisor.lock().await;
            supervisor
                .task(root_task)
                .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
                .active_attempt_id()
                .clone()
        };
        let activated = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .activate_application_root_once(root_task, &attempt, &objective)?;
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        if activated {
            supervisor
                .set_objective(root_task, objective)
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            sync_foreground_root_running(&mut supervisor, root_task)?;
            return Ok(());
        }

        let current_state = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_state(root_task)?;
        match current_state.as_str() {
            "running" => {
                sync_foreground_root_running(&mut supervisor, root_task)?;
                Ok(())
            }
            "paused" => {
                self.repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .transition_task_and_attempt(
                        root_task,
                        &attempt,
                        "running",
                        RuntimeEvent::TaskStarted,
                    )?;
                sync_foreground_root_running(&mut supervisor, root_task)?;
                Ok(())
            }
            _ => Err(RuntimeCoordinatorError::Supervisor(format!(
                "application root cannot be activated from state {current_state}"
            ))),
        }
    }

    pub async fn detach_application_root(
        &self,
        session: &RootSessionId,
        root_task: &TaskId,
        capability: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.authorize_application_root(session, root_task, capability)?;
        let attempt = {
            let supervisor = self.supervisor(session)?;
            let mut supervisor = supervisor.lock().await;
            let attempt = supervisor
                .task(root_task)
                .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
                .active_attempt_id()
                .clone();
            supervisor
                .pause_foreground_task(root_task, PauseReason("foreground TUI detached".into()))
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            attempt
        };
        {
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            repository.transition_task_and_attempt(
                root_task,
                &attempt,
                "paused",
                RuntimeEvent::TaskPaused,
            )?;
            repository.detach_application_root(session, root_task)?;
        }
        // The attachment is durably `detached` here, so a reclaimed root worktree
        // can never be observed as attached. The reclaim itself is seeded by the
        // IPC caller (see `ipc.rs`), because this method takes `&self` and cannot
        // clone the `Arc<RuntimeCoordinator>` a background thread needs.
        Ok(())
    }

    // The arguments mirror the IPC spawn request one-for-one; grouping them
    // would only move the field list.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_application_child(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
        capability: &str,
        objective: String,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
        workdir: Option<PathBuf>,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        self.authorize_application_root(session, parent, capability)?;
        self.spawn_child_and_admit(session, parent, objective, workspace_mode, model, workdir)
            .await
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

    /// Fires all schedules due at `now` into newly isolated root sessions.
    /// The occurrence claim is durable, so repeated daemon ticks cannot create
    /// a duplicate root for the same due time.
    pub fn evaluate_schedules(
        &self,
        now: chrono::DateTime<chrono::Local>,
    ) -> Result<Vec<RootSessionId>, RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let due = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .schedules()?
            .into_iter()
            .filter(|schedule| schedule.state == "active" && schedule.next_run_at <= now)
            .collect::<Vec<_>>();
        let mut sessions = Vec::new();
        for schedule in due {
            let current_minute = now
                .with_second(0)
                .and_then(|value| value.with_nanosecond(0))
                .expect("valid local minute");
            let mut due_at = schedule.next_run_at;
            let mut missed = Vec::new();
            while due_at < current_minute {
                let next = schedule
                    .definition
                    .next_run_after(due_at)
                    .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
                missed.push((due_at, next));
                due_at = next;
            }
            if !missed.is_empty() {
                if schedule.definition.policy.missed_run_policy == MissedRunPolicy::Skip {
                    for (missed_at, next) in missed {
                        self.repository
                            .lock()
                            .expect("runtime repository mutex poisoned")
                            .skip_schedule_occurrence(&schedule.id, missed_at, next, "missed")?;
                    }
                    continue;
                }
                for (missed_at, next) in missed.iter().take(missed.len().saturating_sub(1)) {
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .skip_schedule_occurrence(&schedule.id, *missed_at, *next, "missed")?;
                }
                if let Some((latest, _)) = missed.last() {
                    due_at = *latest;
                }
            }
            let next_run_at = schedule
                .definition
                .next_run_after(due_at)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            let session = RootSessionId::new();
            let supervisor = AgentSupervisor::new_with_objective(
                session.clone(),
                schedule.definition.objective.clone(),
            );
            let root_id = supervisor.root_task_id().clone();
            let root_attempt = supervisor
                .task(&root_id)
                .expect("new root task exists")
                .active_attempt()
                .clone();
            let result = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .evaluate_schedule_occurrence(
                    &schedule.id,
                    due_at,
                    next_run_at,
                    &session,
                    &root_id,
                    &root_attempt.id,
                    &schedule.definition,
                )?;
            if result == ScheduleOccurrenceResult::Fired {
                self.supervisors
                    .lock()
                    .expect("runtime supervisor mutex poisoned")
                    .insert(session.clone(), Arc::new(AsyncMutex::new(supervisor)));
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    pub async fn spawn_child(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        self.spawn_child_with_objective(
            session,
            parent,
            "Complete the delegated task.".into(),
            ChildWriteMode::ReadOnly,
            None,
            None,
        )
        .await
    }

    pub async fn spawn_child_with_objective(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
        objective: String,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
        workdir: Option<PathBuf>,
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
        // A parent names the directory from its own position, so a relative
        // workdir resolves against the parent's workdir, not the daemon's cwd
        // (which is unrelated to any task). Resolving before the spawn means the
        // supervisor stores the absolute path worker start will look up.
        let workdir = workdir.map(|workdir| self.resolve_parent_workdir(parent, &workdir));
        let (child, depth, attempt) = {
            let mut supervisor = supervisor.lock().await;
            let child = supervisor.spawn_with_objective(
                parent.clone(),
                yi_agent_core::subagent::worker::SpawnRequest::new(
                    objective.clone(),
                    workspace_mode,
                    workdir.clone(),
                ),
            )?;
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
                workspace_mode,
                model.clone(),
            )?;
        // Auto-registration: the parent prepares the directory (`git worktree
        // add`) and hands over its path; the daemon only records that position.
        // A bad path fails the spawn rather than deferring to worker start, so
        // the parent learns immediately that its workdir was not usable.
        if let Some(workdir) = workdir.as_deref() {
            let registry = self.factory.worker_workspace_registry().ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "coding child requires a prepared-workspace registry".into(),
                )
            })?;
            let observed = registry
                .observe_workdir(workdir)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            registry.register_prepared(&observed);
        }
        if let Some(model) = model {
            supervisor.lock().await.set_model(&child, model);
        }
        Ok(child)
    }

    /// Resolves a spawn `workdir` the way the parent meant it: an absolute path
    /// stands alone, a relative one is taken from the parent task's own workdir.
    fn resolve_parent_workdir(&self, parent: &TaskId, workdir: &Path) -> PathBuf {
        if workdir.is_absolute() {
            return workdir.to_path_buf();
        }
        self.remembered_task_position(parent)
            .map(|position| position.path.join(workdir))
            .unwrap_or_else(|| workdir.to_path_buf())
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
        workspace_mode: ChildWriteMode,
        model: Option<String>,
        workdir: Option<PathBuf>,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
        let child = self
            .spawn_child_with_objective(session, parent, objective, workspace_mode, model, workdir)
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
        if supervisor.has_worker(task) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "task already owns a worker".into(),
            ));
        }
        let is_subagent = !matches!(
            supervisor
                .task(task)
                .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
                .depth,
            yi_agent_core::TaskDepth::Root
        );
        let attempt = supervisor
            .task(task)
            .expect("worker task exists")
            .active_attempt_id()
            .clone();
        let workspace_mode = supervisor.workspace_mode(task);
        let workspace_assignment = match self.prepare_task_workspace(
            &mut supervisor,
            session,
            task,
            &attempt,
            workspace_mode,
        ) {
            Ok(workspace) => workspace,
            Err(error) => {
                let reason = "workspace_provision_failed";
                let evidence = serde_json::to_string(&serde_json::json!({
                    "reason": reason,
                    "error": error.to_string(),
                }))
                .expect("workspace failure evidence is serializable");
                self.repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .transition_task_and_attempt_with_terminal(
                        task,
                        &attempt,
                        "failed",
                        RuntimeEvent::TaskFailed,
                        &evidence,
                    )?;
                let _ = supervisor.fail_task(task, error.to_string());
                return Err(RuntimeCoordinatorError::Supervisor(error.to_string()));
            }
        };
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
        let mut recovery_request = WorkerStart::new(task.clone(), attempt.clone(), session.clone());
        if let Some(workspace) = workspace_assignment.clone() {
            recovery_request = recovery_request.with_workspace(workspace);
        }
        let mut recovery_context = self.factory.recovery_context_for(&recovery_request);
        if workspace_assignment.is_none()
            && let Some(workspace) = supervisor
                .task(task)
                .expect("worker task exists")
                .workspace
                .as_ref()
        {
            recovery_context.workspace_lease_id = Some(format!("workspace:{workspace}"));
        }
        let rework_messages = supervisor.pending_rework_message_ids(task);
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
        let workspace_factory =
            workspace_assignment
                .clone()
                .map(|workspace| WorkspaceAssignedFactory {
                    inner: self.factory.as_ref(),
                    workspace,
                });
        let worker_factory: &dyn AgentWorkerFactory = workspace_factory
            .as_ref()
            .map(|factory| factory as &dyn AgentWorkerFactory)
            .unwrap_or_else(|| self.factory.as_ref());
        if let Err(error) = supervisor
            .start_worker_with_provider_turn_gate(worker_factory, task, provider_turn_gate)
            .await
        {
            let terminal = serde_json::to_string(&serde_json::json!({
                "reason": "worker_start_failed",
                "error": error.as_str(),
            }))
            .expect("worker start failure payload is serializable");
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .transition_task_and_attempt_with_terminal(
                    task,
                    &attempt,
                    "failed",
                    RuntimeEvent::TaskFailed,
                    &terminal,
                )?;
            if is_subagent {
                self.release_resident_lease(task);
            }
            return Err(RuntimeCoordinatorError::Supervisor(error));
        }
        let acknowledgement = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .mark_rework_messages_delivered(task, &rework_messages);
        if let Err(error) = acknowledgement {
            supervisor
                .interrupt_worker_for_recovery(
                    task,
                    &rework_messages,
                    "rework delivery acknowledgement was not durable",
                )
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            self.recovery_contexts
                .lock()
                .expect("runtime recovery context mutex poisoned")
                .insert(
                    task.clone(),
                    RecoveryContext {
                        workspace_lease_id: recovery_context.workspace_lease_id.clone(),
                        worktree_lease: recovery_context.worktree_lease.clone(),
                        checkpoint_json: Some(recovery_context.checkpoint_json.clone()),
                        tool_state_json: recovery_context.tool_state_json.clone(),
                        recovery_gated: false,
                        recovery_attested: false,
                    },
                );
            if is_subagent {
                self.release_resident_lease(task);
            }
            let recovery = serde_json::to_string(&serde_json::json!({
                "reason": "ambiguous_rework_delivery",
                "message_ids": rework_messages,
            }))
            .expect("recovery evidence is serializable");
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            if let Err(transition_error) = repository
                .transition_task_to_recovery_required_releasing_process_leases(
                    task, &attempt, &recovery,
                )
            {
                return Err(RuntimeCoordinatorError::ControlledRecoveryPersistence {
                    acknowledgement: error.to_string(),
                    cleanup: transition_error.cleanup.map(|error| error.to_string()),
                    recovery: transition_error.recovery.map(|error| error.to_string()),
                });
            }
            return Err(RuntimeCoordinatorError::Repository(error));
        }
        Ok(())
    }

    fn workspace_service_for(
        &self,
        session: &RootSessionId,
    ) -> Option<Arc<dyn yi_agent_core::subagent::worker::WorkerWorkspaceProvider>> {
        self.application_root_workspace_services
            .lock()
            .expect("runtime application root workspace service mutex poisoned")
            .get(session)
            .cloned()
            .or_else(|| self.workspace_service.clone())
    }

    /// The nearest ancestor task's workspace, if any, walking `parent_id`
    /// upward. Used as the read-only execution root so a read-only child sees
    /// its parent's current view rather than a clean baseline.
    /// The workdir a task was given at spawn time, read from its live supervisor.
    ///
    /// Best-effort: a supervisor that is busy or gone yields `None` rather than
    /// blocking, because the only caller renders a review diff.
    fn supervisor_workdir_for(
        &self,
        task: &TaskId,
    ) -> Result<Option<std::path::PathBuf>, RuntimeCoordinatorError> {
        let session = {
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            repository
                .task_detail(task)?
                .session_id
                .parse::<RootSessionId>()
        };
        let Ok(session) = session else {
            return Ok(None);
        };
        let supervisor = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .get(&session)
            .cloned();
        let Some(supervisor) = supervisor else {
            return Ok(None);
        };
        let Ok(supervisor) = supervisor.try_lock() else {
            return Ok(None);
        };
        Ok(supervisor.spawn_workdir(task))
    }

    fn nearest_ancestor_workspace(
        &self,
        supervisor: &AgentSupervisor,
        task: &TaskId,
    ) -> Result<Option<WorkerWorkspace>, RuntimeCoordinatorError> {
        let mut current = supervisor
            .task(task)
            .and_then(|task| task.parent_id.clone());
        while let Some(ancestor) = current {
            // A parent's position is the workdir it runs in, remembered in the
            // supervisor. Nothing is persisted, so an ancestor with no workdir
            // has simply not run yet.
            if let Some(workdir) = supervisor.spawn_workdir(&ancestor) {
                return Ok(Some(in_place_workspace_at(workdir.clone(), workdir)));
            }
            current = supervisor
                .task(&ancestor)
                .and_then(|task| task.parent_id.clone());
        }
        Ok(None)
    }

    fn prepare_task_workspace(
        &self,
        supervisor: &mut AgentSupervisor,
        session: &RootSessionId,
        task: &TaskId,
        attempt: &AttemptId,
        workspace_mode: ChildWriteMode,
    ) -> Result<Option<WorkerWorkspace>, RuntimeCoordinatorError> {
        let Some(provider) = self.workspace_service_for(session) else {
            return Ok(None);
        };
        // A coding task runs where its parent prepared it, when the parent handed
        // over a `workdir`. Without one it runs in place: the runtime resolves a
        // path, it never creates a directory. A read-only task inherits the
        // nearest ancestor's path.
        if workspace_mode == ChildWriteMode::ReadOnly {
            // A read-only task runs in place: it inherits the nearest ancestor's
            // path and records no workspace of its own.
            let parent = self.nearest_ancestor_workspace(supervisor, task)?;
            let workspace = provider
                .read_only_workspace(parent.as_ref(), task)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            supervisor
                .assign_workspace(task, workspace.lease_id.clone())
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            self.remember_task_position(task, &workspace);
            return Ok(Some(workspace));
        }
        // A coding task runs where its parent prepared it, when the parent handed
        // over a `workdir`; without one it falls back to an in-place path. Either
        // way the runtime resolves a path, it never creates a directory.
        let workspace = match supervisor.spawn_workdir(task) {
            Some(workdir) => provider.workspace_in(task, &workdir),
            None => provider.in_place_workspace(session, task, attempt),
        }
        .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        // The position lives only in the supervisor: nothing about it is
        // persisted, so there is no row to write and no worktree to own.
        supervisor
            .assign_workspace(task, workspace.lease_id.clone())
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        self.remember_task_position(task, &workspace);
        Ok(Some(workspace))
    }

    /// Remembers where a task runs for this process's lifetime.
    fn remember_task_position(&self, task: &TaskId, workspace: &WorkerWorkspace) {
        self.task_positions
            .lock()
            .expect("runtime task positions mutex poisoned")
            .insert(task.clone(), workspace.clone());
    }

    /// The position a task was last given in this process, if any.
    fn remembered_task_position(&self, task: &TaskId) -> Option<WorkerWorkspace> {
        self.task_positions
            .lock()
            .expect("runtime task positions mutex poisoned")
            .get(task)
            .cloned()
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

    /// Applies a durable watchdog terminal decision and stops the corresponding
    /// worker. The repository transaction is the authority for duplicate scans.
    pub async fn record_watchdog_terminal(
        &self,
        session: &RootSessionId,
        task: &TaskId,
        terminal: WatchdogTerminal,
        evidence: WatchdogEvidence,
    ) -> Result<bool, RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        let attempt = supervisor
            .task(task)
            .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
            .active_attempt_id()
            .clone();
        let committed = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .record_watchdog_terminal(task, &attempt, terminal.clone(), &evidence)?;
        if committed.is_none() {
            return Ok(false);
        }

        match terminal {
            WatchdogTerminal::Stalled => supervisor.stall_task(
                task,
                CoreWatchdogEvidence {
                    last_meaningful_event_id: evidence.last_meaningful_event_id,
                    last_meaningful_at: evidence.last_meaningful_at,
                    elapsed_secs: evidence.elapsed_secs,
                    current_wait: evidence.current_wait.as_ref().map(|wait| {
                        yi_agent_core::subagent::task::ResourceWait(wait.resource_key.clone())
                    }),
                },
            ),
            WatchdogTerminal::TimedOut(kind) => supervisor.timeout_task(task, kind),
            WatchdogTerminal::BudgetExhausted(kind) => supervisor.exhaust_task_budget(task, kind),
        }
        .map_err(RuntimeCoordinatorError::Supervisor)?;
        drop(supervisor);
        self.release_resident_lease(task);
        Ok(true)
    }

    /// Evaluates durable watchdog snapshots without holding either coordinator
    /// mutex across an await. The terminal transaction makes repeated scans
    /// idempotent even when another scan wins the race.
    pub async fn evaluate_watchdogs(
        &self,
        now: DateTime<Utc>,
    ) -> Result<usize, RuntimeCoordinatorError> {
        let watchdogs = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .active_attempt_watchdogs()?;
        let mut terminal_count = 0;

        for watchdog in watchdogs {
            let Some(outcome) = evaluate_watchdog(
                &watchdog.snapshot.limits,
                &watchdog.snapshot.observation,
                now,
            ) else {
                continue;
            };
            let terminal = match outcome {
                WatchdogOutcome::Stalled => WatchdogTerminal::Stalled,
                WatchdogOutcome::TimedOut(kind) => WatchdogTerminal::TimedOut(kind),
                WatchdogOutcome::BudgetExhausted(kind) => WatchdogTerminal::BudgetExhausted(kind),
            };
            let elapsed_secs = now
                .signed_duration_since(watchdog.snapshot.observation.last_meaningful_at)
                .num_seconds()
                .max(0) as u64;
            let evidence = WatchdogEvidence {
                last_meaningful_event_id: watchdog.snapshot.last_meaningful_event_id,
                last_meaningful_at: watchdog.snapshot.observation.last_meaningful_at,
                elapsed_secs,
                current_wait: watchdog.snapshot.current_wait,
            };
            if self
                .record_watchdog_terminal(
                    &watchdog.session_id,
                    &watchdog.task_id,
                    terminal,
                    evidence,
                )
                .await?
            {
                terminal_count += 1;
            }
        }
        Ok(terminal_count)
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

    /// Records a tool/security permission wait through both durable and
    /// reducer-owned state before it becomes visible to control clients.
    pub async fn request_permission(
        &self,
        session: &RootSessionId,
        task: &TaskId,
        request: PermissionRequestId,
        payload_json: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        let attempt = supervisor
            .task(task)
            .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
            .active_attempt_id()
            .clone();
        if !matches!(
            supervisor.task(task).map(|task| task.state()),
            Some(TaskState::Running)
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "permission requests require a running task".into(),
            ));
        }
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .request_permission(task, &attempt, &request, payload_json)?;
        supervisor
            .request_permission(task, request)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        Ok(())
    }

    /// Applies a daemon-owned local-user decision. The request itself resolves
    /// its task and session, so an IPC caller cannot impersonate a task actor.
    pub async fn resolve_permission(
        &self,
        request: &PermissionRequestId,
        decision: PermissionDecision,
    ) -> Result<(), RuntimeCoordinatorError> {
        let (task, session) = {
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            let task = repository.pending_permission_task(request)?;
            let session = repository
                .task_detail(&task)?
                .session_id
                .parse()
                .map_err(|_| {
                    RuntimeCoordinatorError::Supervisor(
                        "persisted task has an invalid session ID".into(),
                    )
                })?;
            (task, session)
        };
        let supervisor = self.supervisor(&session)?;
        let mut supervisor = supervisor.lock().await;
        if !matches!(
            supervisor.task(&task).map(|task| task.state()),
            Some(TaskState::WaitingForPermission(expected)) if expected == request
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "permission request does not match the active task wait".into(),
            ));
        }
        // Unix-domain IPC peer authentication establishes this local principal;
        // control clients never get to choose an arbitrary task identity.
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .resolve_permission(
                request,
                decision.clone(),
                r#"{"kind":"local_user","source":"daemon"}"#,
            )?;
        supervisor
            .resolve_permission(&task, request.clone(), decision.clone())
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        if matches!(decision, PermissionDecision::Deny)
            && !matches!(
                supervisor.task(&task).expect("task was checked").depth,
                yi_agent_core::TaskDepth::Root
            )
        {
            drop(supervisor);
            self.release_resident_lease(&task);
        }
        Ok(())
    }

    /// Resolves task/session/actor from daemon-owned state and records an
    /// accepted delivery only after successful parent integration evidence.
    pub async fn accept_review(
        &self,
        task: &TaskId,
        integration: IntegrationValidation,
    ) -> Result<(), RuntimeCoordinatorError> {
        let (session, parent, delivery) = self.review_context(task)?;
        let actor_json = daemon_parent_integration_actor_json(&parent)?;
        let supervisor = self.supervisor(&session)?;
        let mut supervisor = supervisor.lock().await;
        if !matches!(
            supervisor.task(task).map(|task| task.state()),
            Some(TaskState::AwaitingParentReview(expected)) if expected == &delivery.id
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "delivery does not match the active review".into(),
            ));
        }
        let staged_integration = integration.clone();
        supervisor
            .stage_review_with_persistence(
                |supervisor| {
                    supervisor.accept_review(
                        task,
                        &parent,
                        delivery.id.clone(),
                        staged_integration,
                    )?;
                    Ok(())
                },
                |_| {
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .accept_delivery_review(
                            task,
                            &delivery.id,
                            &parent,
                            &integration,
                            &actor_json,
                        )
                },
            )
            .map_err(review_persistence_error)?;
        drop(supervisor);
        self.release_resident_lease(task);
        Ok(())
    }

    /// Captures the current review target and issues a short-lived confirmation
    /// token before any review-side mutation happens.
    pub async fn preview_review(
        &self,
        task: &TaskId,
        decision: ReviewDecision,
    ) -> Result<ReviewPreview, RuntimeCoordinatorError> {
        match &decision {
            ReviewDecision::Rework { feedback } if feedback.trim().is_empty() => {
                return Err(RepositoryError::ReviewFeedbackRequired.into());
            }
            ReviewDecision::Reject { reason } if reason.trim().is_empty() => {
                return Err(RepositoryError::ReviewReasonRequired.into());
            }
            _ => {}
        }
        let (session, _parent, delivery) = self.review_context(task)?;
        {
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            if repository.delivery_has_review(&delivery.id)? {
                return Err(RuntimeCoordinatorError::Supervisor(
                    "delivery already has a review decision".into(),
                ));
            }
        }
        // Pinning the position at preview time makes confirm re-inspect the same
        // directory the reviewer saw.
        let preview_workspace = self.remembered_task_position(task);
        let supervisor = self.supervisor(&session)?;
        let supervisor = supervisor.lock().await;
        if !matches!(
            supervisor.task(task).map(|task| task.state()),
            Some(TaskState::AwaitingParentReview(expected)) if expected == &delivery.id
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "delivery does not match the active review".into(),
            ));
        }
        let confirmation_token =
            self.issue_review_confirmation(task, &decision, &delivery, preview_workspace);
        Ok(ReviewPreview {
            task_id: task.clone(),
            delivery_id: delivery.id,
            confirmation_token,
            expires_in_secs: REVIEW_CONFIRMATION_TTL.as_secs(),
            decision,
        })
    }

    /// Consumes a preview token and performs the requested review mutation.
    pub async fn confirm_review(
        &self,
        task: &TaskId,
        decision: ReviewDecision,
        confirmation_token: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        let (_session, _parent, delivery) = self.review_context(task)?;
        let preview_workspace = self
            .review_confirmations
            .lock()
            .expect("runtime review confirmations mutex poisoned")
            .get(confirmation_token)
            .map(|pending| pending.workspace.clone())
            .ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "review confirmation token is invalid or stale".into(),
                )
            })?;
        let inspected_commit = if let Some(workspace) = preview_workspace.as_ref() {
            if let Some(registry) = self.factory.worker_workspace_registry() {
                registry
                    .inspect_delivery(workspace)
                    .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?
                    .commit
            } else {
                delivery.commit.clone()
            }
        } else {
            delivery.commit.clone()
        };
        if !self.consume_review_confirmation(
            task,
            &decision,
            &delivery,
            &inspected_commit,
            confirmation_token,
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "review confirmation token is invalid or stale".into(),
            ));
        }
        match decision {
            ReviewDecision::Approve => self.approve_review(task).await,
            ReviewDecision::Rework { feedback } => self.rework_review(task, &feedback).await,
            ReviewDecision::Reject { reason } => self.reject_review(task, &reason).await,
        }
    }

    /// Records local-user approval and wakes the direct parent. Integration is
    /// intentionally left to the trusted parent path in `accept_review`.
    pub async fn approve_review(&self, task: &TaskId) -> Result<(), RuntimeCoordinatorError> {
        let (session, parent, delivery) = self.review_context(task)?;
        let actor_json = daemon_review_actor_json(&parent)?;
        let supervisor = self.supervisor(&session)?;
        let mut supervisor = supervisor.lock().await;
        if !matches!(
            supervisor.task(task).map(|task| task.state()),
            Some(TaskState::AwaitingParentReview(expected)) if expected == &delivery.id
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "delivery does not match the active review".into(),
            ));
        }
        supervisor
            .can_accept_user_override(&parent)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        let parent_notification = MessageId::new();
        supervisor
            .stage_review_with_persistence(
                |supervisor| {
                    supervisor
                        .stage_persisted_message(MailboxMessageDraft::user_override_with_id(
                            parent_notification.clone(),
                            parent.clone(),
                            review_parent_notification("approved", task, &delivery.id),
                        ))
                        .map_err(|error| error.to_string())
                },
                |_| {
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .approve_delivery_review(
                            task,
                            &delivery.id,
                            &parent,
                            &actor_json,
                            &parent_notification,
                        )
                },
            )
            .map_err(review_persistence_error)?;
        supervisor
            .deliver_committed_user_instruction(&parent, &parent_notification)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        Ok(())
    }

    fn issue_review_confirmation(
        &self,
        task: &TaskId,
        decision: &ReviewDecision,
        delivery: &DeliveryReport,
        workspace: Option<WorkerWorkspace>,
    ) -> String {
        let token = Uuid::new_v4().to_string();
        self.review_confirmations
            .lock()
            .expect("runtime review confirmations mutex poisoned")
            .insert(
                token.clone(),
                PendingReviewConfirmation {
                    task_id: task.clone(),
                    decision: decision.clone(),
                    delivery_id: delivery.id.clone(),
                    delivery_commit: delivery.commit.clone(),
                    workspace,
                    expires_at: Instant::now() + REVIEW_CONFIRMATION_TTL,
                },
            );
        token
    }

    fn consume_review_confirmation(
        &self,
        task: &TaskId,
        decision: &ReviewDecision,
        delivery: &DeliveryReport,
        inspected_commit: &str,
        token: &str,
    ) -> bool {
        let Some(pending) = self
            .review_confirmations
            .lock()
            .expect("runtime review confirmations mutex poisoned")
            .remove(token)
        else {
            return false;
        };
        pending.expires_at > Instant::now()
            && pending.task_id == *task
            && pending.decision == *decision
            && pending.delivery_id == delivery.id
            && pending.delivery_commit == inspected_commit
            && pending.delivery_commit == delivery.commit
    }

    /// Records direct-parent rework feedback, creates the reducer-owned
    /// successor attempt, and starts it with the durable feedback message.
    pub async fn rework_review(
        &self,
        task: &TaskId,
        feedback: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        if feedback.trim().is_empty() {
            return Err(RepositoryError::ReviewFeedbackRequired.into());
        }
        let (session, parent, delivery) = self.review_context(task)?;
        let actor_json = daemon_review_actor_json(&parent)?;
        let supervisor = self.supervisor(&session)?;
        let mut supervisor = supervisor.lock().await;
        if !matches!(
            supervisor.task(task).map(|task| task.state()),
            Some(TaskState::AwaitingParentReview(expected)) if expected == &delivery.id
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "delivery does not match the active review".into(),
            ));
        }
        supervisor
            .can_accept_user_override(&parent)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        let feedback_message = MessageId::new();
        let parent_notification = MessageId::new();
        let (_successor, _) = supervisor
            .stage_review_with_persistence(
                |supervisor| {
                    let successor = supervisor.rework_review(
                        task,
                        &parent,
                        delivery.id.clone(),
                        feedback_message.clone(),
                    )?;
                    supervisor
                        .stage_persisted_message(MailboxMessageDraft::new_with_id(
                            feedback_message.clone(),
                            parent.clone(),
                            task.clone(),
                            MessageKind::Rework(ReworkInstruction(feedback.to_owned())),
                            Some(successor.id.clone()),
                        ))
                        .map_err(|error| error.to_string())?;
                    supervisor
                        .stage_persisted_message(MailboxMessageDraft::user_override_with_id(
                            parent_notification.clone(),
                            parent.clone(),
                            review_parent_notification("rework", task, &delivery.id),
                        ))
                        .map_err(|error| error.to_string())?;
                    Ok(successor)
                },
                |successor| {
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .rework_delivery_review(
                            task,
                            &delivery.id,
                            &parent,
                            feedback,
                            &feedback_message,
                            &successor.id,
                            successor.number,
                            &actor_json,
                            &parent_notification,
                        )
                },
            )
            .map_err(review_persistence_error)?;
        supervisor
            .deliver_committed_user_instruction(&parent, &parent_notification)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        drop(supervisor);
        if self.factory.is_available() {
            self.start_worker(&session, task).await?;
        }
        Ok(())
    }

    /// Records a direct-parent rejection with a non-empty durable reason.
    pub async fn reject_review(
        &self,
        task: &TaskId,
        reason: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        if reason.trim().is_empty() {
            return Err(RepositoryError::ReviewReasonRequired.into());
        }
        let (session, parent, delivery) = self.review_context(task)?;
        let actor_json = daemon_review_actor_json(&parent)?;
        let supervisor = self.supervisor(&session)?;
        let mut supervisor = supervisor.lock().await;
        if !matches!(
            supervisor.task(task).map(|task| task.state()),
            Some(TaskState::AwaitingParentReview(expected)) if expected == &delivery.id
        ) {
            return Err(RuntimeCoordinatorError::Supervisor(
                "delivery does not match the active review".into(),
            ));
        }
        supervisor
            .can_accept_user_override(&parent)
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        let reason_message = MessageId::new();
        let parent_notification = MessageId::new();
        supervisor
            .stage_review_with_persistence(
                |supervisor| {
                    supervisor.reject_review(
                        task,
                        &parent,
                        delivery.id.clone(),
                        reason_message.clone(),
                    )?;
                    supervisor
                        .stage_persisted_message(MailboxMessageDraft::user_override_with_id(
                            parent_notification.clone(),
                            parent.clone(),
                            review_parent_notification("rejected", task, &delivery.id),
                        ))
                        .map_err(|error| error.to_string())?;
                    Ok(())
                },
                |_| {
                    self.repository
                        .lock()
                        .expect("runtime repository mutex poisoned")
                        .reject_delivery_review(
                            task,
                            &delivery.id,
                            &parent,
                            reason,
                            &reason_message,
                            &actor_json,
                            &parent_notification,
                        )
                },
            )
            .map_err(review_persistence_error)?;
        supervisor
            .deliver_committed_user_instruction(&parent, &parent_notification)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        drop(supervisor);
        self.release_resident_lease(task);
        Ok(())
    }

    /// The change a delivered child introduced, as a unified diff.
    ///
    /// A review is only possible if the reviewer can read the code, so this
    /// recomputes the diff from the child's delivered commit rather than
    /// echoing the child's own report. Returns `None` when git cannot produce
    /// one: no commit, no recorded worktree, or a worktree already reclaimed.
    pub fn delivery_diff(&self, task: &TaskId) -> Result<Option<String>, RuntimeCoordinatorError> {
        let (repository_root, path, delivery) = {
            let Some(workdir) = self.supervisor_workdir_for(task)? else {
                return Ok(None);
            };
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            let detail = repository.task_detail(task)?;
            let Ok(delivery) = serde_json::from_str::<DeliveryReport>(&detail.delivery_json) else {
                return Ok(None);
            };
            (workdir.clone(), workdir, delivery)
        };
        if delivery.commit.trim().is_empty() {
            return Ok(None);
        }
        // The branch the child recorded as its base is the intended comparison.
        // When it is unreachable, the commit's own parent is the honest fallback.
        let directory = if path.exists() { path } else { repository_root };
        let mut attempts: Vec<Vec<String>> = Vec::new();
        if !delivery.base_ref.trim().is_empty() {
            attempts.push(vec![
                "diff".into(),
                "--no-color".into(),
                format!("{}...{}", delivery.base_ref, delivery.commit),
            ]);
        }
        attempts.push(vec![
            "diff".into(),
            "--no-color".into(),
            format!("{}^", delivery.commit),
        ]);
        for args in attempts {
            if let Some(diff) = git_capture(&directory, &args) {
                return Ok(Some(truncate_diff(diff)));
            }
        }
        Ok(None)
    }

    fn review_context(
        &self,
        task: &TaskId,
    ) -> Result<(RootSessionId, TaskId, DeliveryReport), RuntimeCoordinatorError> {
        let repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        let detail = repository.task_detail(task)?;
        let session = detail.session_id.parse().map_err(|_| {
            RuntimeCoordinatorError::Supervisor("persisted task has an invalid session ID".into())
        })?;
        let parent = detail
            .parent_task_id
            .ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "root tasks cannot receive delivery reviews".into(),
                )
            })?
            .parse()
            .map_err(|_| {
                RuntimeCoordinatorError::Supervisor(
                    "persisted task has an invalid parent ID".into(),
                )
            })?;
        let delivery = serde_json::from_str::<DeliveryReport>(&detail.delivery_json)
            .map_err(RepositoryError::from)?;
        Ok((session, parent, delivery))
    }

    pub async fn send_application_message(
        &self,
        session: &RootSessionId,
        sender: &TaskId,
        capability: &str,
        recipient: TaskId,
        message: String,
    ) -> Result<(), RuntimeCoordinatorError> {
        if message.trim().is_empty() {
            return Err(RuntimeCoordinatorError::Supervisor(
                "message must be non-empty".into(),
            ));
        }
        self.authorize_application_root(session, sender, capability)?;
        let supervisor = self.supervisor(session)?;
        let mut supervisor = supervisor.lock().await;
        // Reuse supervisor adjacency and terminal-state checks without requiring
        // the daemon-worker-only message capability map.
        supervisor
            .send_user_message(sender, recipient.clone(), message.clone())
            .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .record_user_message(sender, &recipient, &message)?;
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

    /// Authenticates a caller for a child-scoped operation: either the
    /// application root capability or the caller's own worker capability.
    async fn authorize_child_access(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        if self
            .authorize_application_root(session, caller, capability)
            .is_err()
        {
            let supervisor = self.supervisor(session)?;
            supervisor
                .lock()
                .await
                .can_use_worker_capability(caller, capability)
                .map_err(|_| {
                    RuntimeCoordinatorError::AuthorityDenied(
                        "child access capability is invalid".into(),
                    )
                })?;
        }
        Ok(())
    }

    /// Reads one task's detail, but only when it lies in the caller's own
    /// descendant subtree. Unlike `send_message`, a terminal target is fine:
    /// a parent learns a finished child's result through this path.
    pub async fn inspect_child_authorized(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
        target: &TaskId,
    ) -> Result<crate::repository::PersistedTaskDetail, RuntimeCoordinatorError> {
        self.authorize_child_access(session, caller, capability)
            .await?;
        let allowed = self
            .supervisor(session)?
            .lock()
            .await
            .is_descendant_of(caller, target);
        if !allowed {
            return Err(RuntimeCoordinatorError::AuthorityDenied(
                "task is not a descendant of the caller".into(),
            ));
        }
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_detail(target)
            .map_err(RuntimeCoordinatorError::from)
    }

    /// Cancels one task, but only when it lies in the caller's own descendant
    /// subtree. Applies the same cancellation path `confirm_cancel` uses.
    pub async fn cancel_child_authorized(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
        target: &TaskId,
        recursive: bool,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.authorize_child_access(session, caller, capability)
            .await?;
        let allowed = self
            .supervisor(session)?
            .lock()
            .await
            .is_descendant_of(caller, target);
        if !allowed {
            return Err(RuntimeCoordinatorError::AuthorityDenied(
                "task is not a descendant of the caller".into(),
            ));
        }
        self.cancel_task(session, target, recursive).await
    }

    /// Directs one of the caller's own child deliveries to rework, or rejects
    /// it. Authorization is the same capability check `inspect_child_authorized`
    /// uses, plus one stricter rule the human path does not need: the caller
    /// must be the child's *direct* parent, because that is who the review
    /// state machine binds the decision to.
    pub async fn review_child_authorized(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
        target: &TaskId,
        decision: crate::ipc::ChildReviewDecision,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.authorize_child_access(session, caller, capability)
            .await?;
        let allowed = self
            .supervisor(session)?
            .lock()
            .await
            .is_descendant_of(caller, target);
        if !allowed {
            return Err(RuntimeCoordinatorError::AuthorityDenied(
                "task is not a descendant of the caller".into(),
            ));
        }
        let parent = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_detail(target)?
            .parent_task_id
            .and_then(|parent| parent.parse::<TaskId>().ok());
        if parent.as_ref() != Some(caller) {
            return Err(RuntimeCoordinatorError::AuthorityDenied(
                "only a delivery's direct parent may review it".into(),
            ));
        }
        match decision {
            crate::ipc::ChildReviewDecision::Rework { feedback } => {
                self.rework_review(target, &feedback).await
            }
            crate::ipc::ChildReviewDecision::Reject { reason } => {
                self.reject_review(target, &reason).await
            }
        }
    }

    pub async fn wait_for_children_authorized(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
        mode: WaitMode,
    ) -> Result<WaitOutcome, RuntimeCoordinatorError> {
        if self
            .authorize_application_root(session, caller, capability)
            .is_err()
        {
            let supervisor = self.supervisor(session)?;
            supervisor
                .lock()
                .await
                .can_use_worker_capability(caller, capability)
                .map_err(|_| {
                    RuntimeCoordinatorError::AuthorityDenied(
                        "wait_agent capability is invalid".into(),
                    )
                })?;
        }
        self.wait_for_children(session, caller, mode).await
    }

    pub fn direct_children(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
    ) -> Result<Vec<TaskId>, RuntimeCoordinatorError> {
        Ok(self.child_completion_snapshot(session, caller)?.0)
    }

    pub fn child_completion_snapshot(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
    ) -> Result<(Vec<TaskId>, Vec<CompletedChildReport>), RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let supervisor = supervisor
            .try_lock()
            .map_err(|_| RuntimeCoordinatorError::Supervisor("supervisor is busy".into()))?;
        Ok(supervisor.child_completion_snapshot(caller))
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
        let mut deliveries = Vec::new();
        let mut consumed_overrides = Vec::new();
        let mut watchdog_updates = Vec::new();
        for supervisor in &supervisors {
            let mut supervisor = supervisor.lock().await;
            watchdog_updates.extend(supervisor.take_worker_watchdog_events());
            let changed = supervisor
                .reconcile_worker_events()
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            for task_id in changed {
                let task = supervisor.task(&task_id).expect("reconciled task exists");
                let state = task.state();
                let attempt = task.active_attempt_id().clone();
                if matches!(state, yi_agent_core::TaskState::AwaitingParentReview(_)) {
                    let delivery = task
                        .active_attempt()
                        .delivery
                        .clone()
                        .expect("review wait retains its delivery report");
                    deliveries.push((task_id, attempt, delivery));
                    continue;
                }
                let (state, event, terminal_json) = match state {
                    yi_agent_core::TaskState::Paused(_) => {
                        ("paused", RuntimeEvent::TaskPaused, None)
                    }
                    yi_agent_core::TaskState::CompletedNoChanges => (
                        "completed_no_changes",
                        RuntimeEvent::TaskCompleted,
                        Some(text_completion_terminal_json(
                            supervisor.completion_report(&task_id),
                        )?),
                    ),
                    yi_agent_core::TaskState::Blocked(reason) => (
                        "blocked",
                        RuntimeEvent::TaskBlocked,
                        reason
                            .0
                            .strip_prefix("recovery_conflict:")
                            .map(|_| r#"{"reason":"recovery_conflict"}"#.to_string()),
                    ),
                    yi_agent_core::TaskState::Cancelled(_) => {
                        ("cancelled", RuntimeEvent::TaskCancelled, None)
                    }
                    yi_agent_core::TaskState::Failed(failure) => (
                        "failed",
                        RuntimeEvent::TaskFailed,
                        Some(worker_failure_terminal_json(&failure.0)?),
                    ),
                    _ => continue,
                };
                updates.push((task_id, attempt, state, event, terminal_json));
            }
            consumed_overrides.extend(supervisor.pending_user_override_acks().iter().cloned());
        }
        for (task_id, update) in watchdog_updates {
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            let attempt = repository.active_attempt_id(&task_id)?;
            let (turn_delta, token_delta, provider_retry_delta, tool_retry_delta, meaningful) =
                match update {
                    WorkerWatchdogEvent::ProviderUsage {
                        input_tokens,
                        output_tokens,
                    } => (1, input_tokens.saturating_add(output_tokens), 0, 0, false),
                    WorkerWatchdogEvent::ProviderRetry => (0, 0, 1, 0, false),
                    WorkerWatchdogEvent::ToolRetry => (0, 0, 0, 1, false),
                    WorkerWatchdogEvent::MeaningfulProgress => (0, 0, 0, 0, true),
                };
            repository.record_watchdog_progress(
                &task_id,
                &attempt,
                turn_delta,
                token_delta,
                provider_retry_delta,
                tool_retry_delta,
                meaningful,
                Utc::now(),
            )?;
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
        for (task_id, attempt, delivery) in deliveries {
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .record_delivery_for_review(&task_id, &attempt, &delivery)?;
            self.release_resident_lease(&task_id);
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
                    &terminal_json,
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

    pub async fn mailbox_snapshot(
        &self,
        session: &RootSessionId,
        task: &TaskId,
    ) -> Result<Vec<MailboxMessage>, RuntimeCoordinatorError> {
        let supervisor = self.supervisor(session)?;
        let supervisor = supervisor.lock().await;
        let mailbox = supervisor
            .mailbox(task)
            .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?;
        Ok(mailbox.messages().to_vec())
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
    queued: Mutex<HashMap<TaskId, QueuedProviderTurn>>,
    assigned: Mutex<HashMap<TaskId, LeaseId>>,
    notify: Arc<Notify>,
}

fn worker_failure_terminal_json(message: &str) -> Result<String, RuntimeCoordinatorError> {
    const MAX_ERROR_BYTES: usize = 4 * 1024;
    let error = truncate_utf8(message, MAX_ERROR_BYTES);
    serde_json::to_string(&serde_json::json!({
        "reason": "worker_failed",
        "error": error,
    }))
    .map_err(RepositoryError::from)
    .map_err(RuntimeCoordinatorError::from)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn text_completion_terminal_json(report: Option<&str>) -> Result<String, RuntimeCoordinatorError> {
    serde_json::to_string(&serde_json::json!({
        "kind": "text_completion",
        "report": report.unwrap_or(""),
    }))
    .map_err(RepositoryError::from)
    .map_err(RuntimeCoordinatorError::from)
}

fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<serde_json::Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(serde_json::Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(serde_json::Value::as_str))
        .flatten()
        .map(str::to_owned)
}

fn hydrate_completion_report(
    supervisor: &mut AgentSupervisor,
    task_id: TaskId,
    report: Option<String>,
) -> Result<(), RuntimeCoordinatorError> {
    if let Some(report) = report {
        supervisor
            .hydrate_completion_report(task_id, report)
            .map_err(RuntimeCoordinatorError::Supervisor)?;
    }
    Ok(())
}

fn daemon_review_actor_json(parent: &TaskId) -> Result<String, RuntimeCoordinatorError> {
    serde_json::to_string(&serde_json::json!({
        "kind": "task",
        "task_id": parent,
        "source": "daemon",
        "initiated_by": { "kind": "local_user", "source": "daemon" },
    }))
    .map_err(RepositoryError::from)
    .map_err(RuntimeCoordinatorError::from)
}

fn daemon_parent_integration_actor_json(
    parent: &TaskId,
) -> Result<String, RuntimeCoordinatorError> {
    serde_json::to_string(&serde_json::json!({
        "kind": "task",
        "task_id": parent,
        "source": "daemon",
        "initiated_by": { "kind": "parent_integration", "source": "daemon" },
    }))
    .map_err(RepositoryError::from)
    .map_err(RuntimeCoordinatorError::from)
}

fn review_persistence_error(
    error: ReviewPersistenceError<RepositoryError>,
) -> RuntimeCoordinatorError {
    match error {
        ReviewPersistenceError::Supervisor(error) => RuntimeCoordinatorError::Supervisor(error),
        ReviewPersistenceError::Persistence(error) => error.into(),
    }
}

fn persisted_depth(depth: u8) -> Result<yi_agent_core::TaskDepth, RuntimeCoordinatorError> {
    match depth {
        0 => Ok(yi_agent_core::TaskDepth::Root),
        1 => Ok(yi_agent_core::TaskDepth::Child),
        2 => Ok(yi_agent_core::TaskDepth::Leaf),
        _ => Err(RuntimeCoordinatorError::Supervisor(
            "persisted review task has an invalid depth".into(),
        )),
    }
}

fn sync_foreground_root_running(
    supervisor: &mut AgentSupervisor,
    root_task: &TaskId,
) -> Result<(), RuntimeCoordinatorError> {
    let state = supervisor
        .task(root_task)
        .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
        .state()
        .clone();
    match state {
        TaskState::Running => Ok(()),
        TaskState::Queued => supervisor
            .start_task(root_task)
            .map_err(RuntimeCoordinatorError::Supervisor),
        TaskState::Paused(_) => {
            supervisor
                .resume_task(root_task)
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            supervisor
                .start_task(root_task)
                .map_err(RuntimeCoordinatorError::Supervisor)
        }
        other if other.is_terminal() => Err(RuntimeCoordinatorError::Supervisor(
            "terminal application root cannot be activated".into(),
        )),
        _ => Ok(()),
    }
}

fn hydrated_application_root_state(
    state: &str,
    delivery_json: &str,
    terminal_json: Option<&str>,
) -> Result<(TaskState, Option<DeliveryReport>, String, Option<String>), RuntimeCoordinatorError> {
    let payload =
        serde_json::from_str::<serde_json::Value>(delivery_json).map_err(RepositoryError::from)?;
    let objective = payload
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Continue the foreground TUI root safely.")
        .to_owned();
    let (state, delivery) = match state {
        "queued"
        | "running"
        | "waiting_for_resource"
        | "waiting_for_permission"
        | "waiting_for_children" => (TaskState::Queued, None),
        "paused" => (
            TaskState::Paused(PauseReason("persisted application root pause".into())),
            None,
        ),
        "awaiting_parent_review" => {
            let delivery: DeliveryReport =
                serde_json::from_value(payload).map_err(RepositoryError::from)?;
            (
                TaskState::AwaitingParentReview(delivery.id.clone()),
                Some(delivery),
            )
        }
        "completed" => (TaskState::Completed, None),
        "completed_no_changes" => (TaskState::CompletedNoChanges, None),
        "blocked" => (
            TaskState::Blocked(BlockReason("persisted application root block".into())),
            None,
        ),
        "stalled" => (
            TaskState::Stalled(CoreWatchdogEvidence {
                last_meaningful_event_id: None,
                last_meaningful_at: Utc::now(),
                elapsed_secs: 0,
                current_wait: None,
            }),
            None,
        ),
        "timed_out" => (TaskState::TimedOut(TimeoutKind::WallClock), None),
        "budget_exhausted" => (TaskState::BudgetExhausted(BudgetKind::WallTime), None),
        "failed" => (
            TaskState::Failed(TaskFailure::new("persisted application root failure")),
            None,
        ),
        "cancelled" => (
            TaskState::Cancelled(CancelReason(
                "persisted application root cancellation".into(),
            )),
            None,
        ),
        "recovery_required" | "recovery_gated" | "recovery_attested" => (
            TaskState::RecoveryRequired(RecoveryEvidence(
                "persisted application root requires recovery".into(),
            )),
            None,
        ),
        other => {
            return Err(RuntimeCoordinatorError::Supervisor(format!(
                "persisted application root has unsupported state: {other}"
            )));
        }
    };
    let completion_report = text_completion_report(terminal_json);
    Ok((state, delivery, objective, completion_report))
}

fn hydrated_review_state(
    state: &str,
    delivery_json: &str,
    terminal_json: Option<&str>,
) -> Result<(TaskState, Option<DeliveryReport>, String, Option<String>), RuntimeCoordinatorError> {
    let payload =
        serde_json::from_str::<serde_json::Value>(delivery_json).map_err(RepositoryError::from)?;
    let objective = payload
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Resume the durable reviewed task safely.")
        .to_owned();
    let (state, delivery) = match state {
        "queued" => (TaskState::Queued, None),
        "awaiting_parent_review" => {
            let delivery: DeliveryReport =
                serde_json::from_value(payload).map_err(RepositoryError::from)?;
            (
                TaskState::AwaitingParentReview(delivery.id.clone()),
                Some(delivery),
            )
        }
        "paused" => (
            TaskState::Paused(PauseReason("persisted pause".into())),
            None,
        ),
        "blocked" => (
            TaskState::Blocked(BlockReason("persisted review rejection".into())),
            None,
        ),
        "completed" => (TaskState::Completed, None),
        "completed_no_changes" => (TaskState::CompletedNoChanges, None),
        "stalled" => (
            TaskState::Stalled(CoreWatchdogEvidence {
                last_meaningful_event_id: None,
                last_meaningful_at: Utc::now(),
                elapsed_secs: 0,
                current_wait: None,
            }),
            None,
        ),
        "timed_out" => (TaskState::TimedOut(TimeoutKind::WallClock), None),
        "budget_exhausted" => (TaskState::BudgetExhausted(BudgetKind::WallTime), None),
        "failed" => (
            TaskState::Failed(TaskFailure::new("persisted failure")),
            None,
        ),
        "cancelled" => (
            TaskState::Cancelled(CancelReason("persisted cancellation".into())),
            None,
        ),
        "recovery_required"
        | "recovery_gated"
        | "recovery_attested"
        | "running"
        | "waiting_for_resource"
        | "waiting_for_permission"
        | "waiting_for_children" => (
            TaskState::RecoveryRequired(RecoveryEvidence(
                "runtime restarted before review mailbox hydration".into(),
            )),
            None,
        ),
        other => {
            return Err(RuntimeCoordinatorError::Supervisor(format!(
                "persisted review task has unsupported state: {other}"
            )));
        }
    };
    let completion_report = text_completion_report(terminal_json);
    Ok((state, delivery, objective, completion_report))
}

fn review_parent_notification(
    decision: &str,
    subject: &TaskId,
    delivery: &yi_agent_core::subagent::task::DeliveryId,
) -> String {
    format!("Local user review {decision} delivery {delivery} for direct child {subject}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedProviderTurn {
    resource_key: String,
    priority: AdmissionPriority,
}

impl ProviderTurnAdmissions {
    fn new(
        resources: Arc<Mutex<ResourceCoordinator>>,
        repository: Arc<Mutex<RuntimeRepository>>,
    ) -> Self {
        Self {
            resources,
            repository,
            queued: Mutex::new(HashMap::new()),
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
            if priority == AdmissionPriority::Normal {
                return Ok(Some(ProviderTurnLeaseHandle {
                    resources: Arc::clone(&self.resources),
                    repository: Arc::clone(&self.repository),
                    lease_id,
                    notify: Arc::clone(&self.notify),
                }));
            }
            // Mailbox work became coordination-eligible after fair selection
            // but before the worker consumed its assigned normal turn. Yield
            // that assignment and re-enter through the coordination pool.
            self.resources
                .lock()
                .expect("resource coordinator mutex poisoned")
                .release(lease_id.clone())
                .expect("assigned provider turn lease release is idempotent");
            self.repository
                .lock()
                .expect("runtime repository mutex poisoned")
                .release_provider_turn_lease(&lease_id.to_string())?;
            self.notify.notify_waiters();
        }

        let mut resources = self
            .resources
            .lock()
            .expect("resource coordinator mutex poisoned");
        let mut queued = self
            .queued
            .lock()
            .expect("provider turn queue mutex poisoned");
        let queued_request = QueuedProviderTurn {
            resource_key: resource_key.into(),
            priority,
        };
        if queued
            .get(&task_id)
            .is_some_and(|existing| *existing != queued_request)
        {
            // A turn can become coordination-eligible while it waits. Remove
            // its former request before queueing the replacement so one task
            // cannot receive grants from both provider pools.
            resources.cancel_task_requests(&task_id);
            queued.remove(&task_id);
        }
        if !queued.contains_key(&task_id) {
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
            queued.insert(task_id.clone(), queued_request);
        }
        let Some(grant) = resources.grant_next(resource_key) else {
            return Ok(None);
        };
        let cursor = resources.admission_cursor(resource_key);
        if let Err(error) = self
            .repository
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
            )
        {
            // A selected request cannot run until its durable grant and
            // cursor commit. Undo the in-memory selection on write failure so
            // it never leaks capacity until the daemon restarts.
            resources
                .release(grant.lease_id.clone())
                .expect("provider turn grant can be rolled back");
            self.notify.notify_waiters();
            return Err(error);
        }
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

    #[test]
    fn worker_failure_terminal_evidence_is_bounded_without_splitting_utf8() {
        let message = format!("{}終", "x".repeat(4 * 1024));
        let terminal = worker_failure_terminal_json(&message).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&terminal).unwrap();
        assert_eq!(payload["reason"], "worker_failed");
        assert_eq!(payload["error"].as_str().unwrap(), "x".repeat(4 * 1024));
    }

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

    #[test]
    fn failed_provider_turn_persistence_returns_its_in_memory_permit() {
        let mut resources = ResourceCoordinator::new();
        resources.set_capacity("llm:test", 1);
        let root = RootSessionId::new();
        let missing_task = TaskId::new();
        let valid_task = TaskId::new();
        let repository = test_repository(&root, std::slice::from_ref(&valid_task));
        let admissions = ProviderTurnAdmissions::new(Arc::new(Mutex::new(resources)), repository);

        // The foreign-key write fails because this task was never persisted.
        assert!(
            admissions
                .acquire_now(
                    root.clone(),
                    missing_task.clone(),
                    missing_task,
                    "llm:test",
                    AdmissionPriority::Normal,
                )
                .is_err()
        );

        // A later valid task must not inherit a leaked permit from the failed
        // durable grant.
        assert!(
            admissions
                .acquire_now(
                    root,
                    valid_task.clone(),
                    valid_task,
                    "llm:test",
                    AdmissionPriority::Normal,
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
    fn queued_provider_turn_migrates_to_coordination_reserve_without_duplicate_request() {
        let mut resources = ResourceCoordinator::new();
        resources.configure_provider_llm_capacity("test");
        let root = RootSessionId::new();
        let tasks = (0..9).map(|_| TaskId::new()).collect::<Vec<_>>();
        let repository = test_repository(&root, &tasks);
        let admissions =
            ProviderTurnAdmissions::new(Arc::new(Mutex::new(resources)), Arc::clone(&repository));
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

        // An already eligible coordination request prevents a regular turn
        // from borrowing the reserve, so the target is deterministically
        // queued on the regular key.
        admissions
            .resources
            .lock()
            .unwrap()
            .enqueue_with_priority_for_parent_at(
                root.clone(),
                tasks[8].clone(),
                tasks[8].clone(),
                ResourceRequest {
                    scope: ResourceScope::ProviderKey,
                    key: "llm-coordination:test".into(),
                    mode: LeaseMode::Shared,
                    units: 1,
                    deadline: None,
                },
                AdmissionPriority::High,
                Utc::now(),
            );

        assert!(
            admissions
                .acquire_now(
                    root.clone(),
                    tasks[7].clone(),
                    tasks[7].clone(),
                    "llm:test",
                    AdmissionPriority::Normal,
                )
                .unwrap()
                .is_none(),
            "the eighth normal request waits for a regular permit"
        );
        admissions
            .resources
            .lock()
            .unwrap()
            .cancel_task_requests(&tasks[8]);

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
            "a waiting task becomes eligible for the coordination reserve"
        );
        assert_eq!(
            admissions.resources.lock().unwrap().queued_request_count(),
            0,
            "migration removes the obsolete normal queue request"
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

/// Runs git and returns its stdout on success. `None` on any failure keeps the
/// caller's contract simple: a diff that cannot be produced is absent, never an
/// error that would fail an otherwise valid inspection.
fn git_capture(directory: &std::path::Path, args: &[String]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Diff bytes are unbounded in principle. A reviewer needs the shape of the
/// change, not an entire vendored dependency: cap it and say so.
fn truncate_diff(diff: String) -> String {
    const MAX_DIFF_BYTES: usize = 64 * 1024;
    if diff.len() <= MAX_DIFF_BYTES {
        return diff;
    }
    let mut boundary = MAX_DIFF_BYTES;
    while boundary > 0 && !diff.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = diff[..boundary].to_owned();
    truncated.push_str("\n... diff truncated\n");
    truncated
}
