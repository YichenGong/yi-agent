use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_id!(TaskId);
uuid_id!(AttemptId);
uuid_id!(RootSessionId);
uuid_id!(DeliveryId);
uuid_id!(MessageId);
uuid_id!(IntegrationId);
uuid_id!(PermissionRequestId);
uuid_id!(AuthorityId);
uuid_id!(WorkspaceLeaseId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractVersion(pub u32);

impl ContractVersion {
    pub const fn initial() -> Self {
        Self(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskDepth {
    Root,
    Child,
    Leaf,
}

impl TaskDepth {
    pub fn can_spawn_child(self) -> Result<TaskDepth, TaskDepthError> {
        match self {
            Self::Root => Ok(Self::Child),
            Self::Child => Ok(Self::Leaf),
            Self::Leaf => Err(TaskDepthError::MaximumDepthReached),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TaskDepthError {
    #[error("a leaf task cannot spawn a child")]
    MaximumDepthReached,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceWait(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WaitSelector {
    AnyChild,
    AllChildren,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseReason(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockReason(pub String);

/// Core's immutable record of the last progress point used for a watchdog
/// decision. Runtime storage adds the queue timestamp for resource waits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogEvidence {
    pub last_meaningful_event_id: Option<i64>,
    pub last_meaningful_at: DateTime<Utc>,
    pub elapsed_secs: u64,
    pub current_wait: Option<ResourceWait>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeoutKind {
    WallClock,
    Deadline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetKind {
    Turns,
    Tokens,
    WallTime,
    Cost,
    ProviderRetries,
    ToolRetries,
    ReworkCycles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFailure(pub String);

impl TaskFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelReason(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEvidence(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Queued,
    Running,
    WaitingForResource(ResourceWait),
    WaitingForPermission(PermissionRequestId),
    WaitingForChildren(WaitSelector),
    Paused(PauseReason),
    AwaitingParentReview(DeliveryId),
    Completed,
    CompletedNoChanges,
    Blocked(BlockReason),
    Stalled(WatchdogEvidence),
    TimedOut(TimeoutKind),
    BudgetExhausted(BudgetKind),
    Failed(TaskFailure),
    Cancelled(CancelReason),
    RecoveryRequired(RecoveryEvidence),
}

impl TaskState {
    // TaskTransitionError deliberately retains both complete states for durable,
    // diagnosable reducer failures; boxing would make the public error API less
    // ergonomic without reducing the copied state stored in the error.
    #[allow(clippy::result_large_err)]
    pub fn can_transition_to(&self, next: Self) -> Result<(), TaskTransitionError> {
        let legal = matches!(
            (self, &next),
            (
                Self::Queued,
                Self::Running
                    | Self::Paused(_)
                    | Self::Stalled(_)
                    | Self::TimedOut(_)
                    | Self::BudgetExhausted(_)
                    | Self::Cancelled(_)
            ) | (
                Self::Running,
                Self::WaitingForResource(_)
                    | Self::WaitingForPermission(_)
                    | Self::WaitingForChildren(_)
                    | Self::Paused(_)
                    | Self::CompletedNoChanges
                    | Self::AwaitingParentReview(_)
                    | Self::Blocked(_)
                    | Self::Stalled(_)
                    | Self::TimedOut(_)
                    | Self::BudgetExhausted(_)
                    | Self::Failed(_)
                    | Self::Cancelled(_)
                    | Self::RecoveryRequired(_)
            ) | (
                Self::WaitingForResource(_)
                    | Self::WaitingForPermission(_)
                    | Self::WaitingForChildren(_),
                Self::Queued
                    | Self::Paused(_)
                    | Self::Stalled(_)
                    | Self::TimedOut(_)
                    | Self::BudgetExhausted(_)
                    | Self::Cancelled(_)
                    | Self::RecoveryRequired(_)
            ) | (
                Self::Paused(_),
                Self::Queued | Self::Cancelled(_) | Self::RecoveryRequired(_)
            ) | (
                Self::AwaitingParentReview(_),
                Self::Completed | Self::CompletedNoChanges | Self::Blocked(_) | Self::Cancelled(_)
            )
        );

        if legal {
            Ok(())
        } else {
            Err(TaskTransitionError {
                from: self.clone(),
                to: next,
            })
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::CompletedNoChanges
                | Self::Blocked(_)
                | Self::Stalled(_)
                | Self::TimedOut(_)
                | Self::BudgetExhausted(_)
                | Self::Failed(_)
                | Self::Cancelled(_)
                | Self::RecoveryRequired(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("illegal task state transition from {from:?} to {to:?}")]
pub struct TaskTransitionError {
    pub from: TaskState,
    pub to: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryState {
    None,
    ReadyForReview(DeliveryId),
    Accepted {
        delivery: DeliveryId,
        integration: IntegrationValidation,
    },
    ReworkRequested {
        previous: DeliveryId,
        feedback: MessageId,
    },
    Rejected {
        delivery: DeliveryId,
        reason: MessageId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReport {
    pub id: DeliveryId,
    pub commit: String,
    pub base_ref: String,
    pub workspace: WorkspaceLeaseId,
    pub evidence: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_limitations: Vec<String>,
}

impl DeliveryReport {
    pub fn coding(
        commit: impl Into<String>,
        base_ref: impl Into<String>,
        workspace: WorkspaceLeaseId,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            id: DeliveryId::new(),
            commit: commit.into(),
            base_ref: base_ref.into(),
            workspace,
            evidence: evidence.into(),
            changed_files: Vec::new(),
            known_limitations: Vec::new(),
        }
    }

    pub fn with_changed_files(mut self, changed_files: Vec<String>) -> Self {
        self.changed_files = changed_files;
        self
    }

    pub fn with_known_limitations(mut self, known_limitations: Vec<String>) -> Self {
        self.known_limitations = known_limitations;
        self
    }

    fn validate_for(&self, task: &AgentTask) -> Result<(), &'static str> {
        if self.commit.trim().is_empty() {
            return Err("commit is required for coding delivery");
        }
        if self.base_ref.trim().is_empty() {
            return Err("base ref is required for coding delivery");
        }
        if self.evidence.trim().is_empty() {
            return Err("delivery evidence is required");
        }
        if task.workspace.as_ref() != Some(&self.workspace) {
            return Err("delivery workspace does not match task workspace");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationValidation {
    pub id: IntegrationId,
    pub succeeded: bool,
    pub evidence: String,
}

impl IntegrationValidation {
    pub fn passed(evidence: impl Into<String>) -> Self {
        Self {
            id: IntegrationId::new(),
            succeeded: true,
            evidence: evidence.into(),
        }
    }

    pub fn failed(evidence: impl Into<String>) -> Self {
        Self {
            id: IntegrationId::new(),
            succeeded: false,
            evidence: evidence.into(),
        }
    }

    fn is_valid(&self) -> bool {
        self.succeeded && !self.evidence.trim().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StableCheckpoint(pub String);

impl StableCheckpoint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveBudget {
    pub max_turns: Option<u32>,
    pub max_wall_time_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptUsage {
    pub turns: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalReason {
    Completed,
    CompletedNoChanges,
    Blocked(BlockReason),
    Stalled(WatchdogEvidence),
    TimedOut(TimeoutKind),
    BudgetExhausted(BudgetKind),
    Failed(TaskFailure),
    Cancelled(CancelReason),
    RecoveryRequired(RecoveryEvidence),
    ReworkRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAttempt {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub number: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub checkpoint: Option<StableCheckpoint>,
    pub delivery: Option<DeliveryReport>,
    pub budget: EffectiveBudget,
    pub usage: AttemptUsage,
    pub terminal_reason: Option<TerminalReason>,
}

impl TaskAttempt {
    fn initial(task_id: TaskId) -> Self {
        Self {
            id: AttemptId::new(),
            task_id,
            number: 1,
            started_at: Utc::now(),
            ended_at: None,
            checkpoint: None,
            delivery: None,
            budget: EffectiveBudget::default(),
            usage: AttemptUsage::default(),
            terminal_reason: None,
        }
    }

    fn successor(&self) -> Self {
        Self {
            id: AttemptId::new(),
            task_id: self.task_id.clone(),
            number: self.number + 1,
            started_at: Utc::now(),
            ended_at: None,
            checkpoint: None,
            delivery: None,
            budget: self.budget.clone(),
            usage: AttemptUsage::default(),
            terminal_reason: None,
        }
    }
}

/// Whether a task owns a writable git worktree or runs in place read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskWorkspaceMode {
    /// Create a git worktree; the worker may write files and deliver a commit.
    Coding,
    /// No worktree; run in the parent's view with a read-only sandbox and
    /// return a text result.
    ReadOnly,
}

impl TaskWorkspaceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::ReadOnly => "read_only",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "coding" => Some(Self::Coding),
            "read_only" => Some(Self::ReadOnly),
            _ => None,
        }
    }
}

impl Default for TaskWorkspaceMode {
    fn default() -> Self {
        Self::ReadOnly
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTask {
    pub id: TaskId,
    pub root_session_id: RootSessionId,
    pub parent_id: Option<TaskId>,
    pub depth: TaskDepth,
    pub created_at: DateTime<Utc>,
    pub current_contract: ContractVersion,
    pub authority_id: AuthorityId,
    active_attempt: AttemptId,
    state: TaskState,
    pause_request: Option<PauseReason>,
    delivery: DeliveryState,
    pub workspace: Option<WorkspaceLeaseId>,
    attempts: Vec<TaskAttempt>,
}

impl AgentTask {
    pub fn new_root(root_session_id: RootSessionId) -> Self {
        let id = TaskId::new();
        let attempt = TaskAttempt::initial(id.clone());
        Self {
            id,
            root_session_id,
            parent_id: None,
            depth: TaskDepth::Root,
            created_at: Utc::now(),
            current_contract: ContractVersion::initial(),
            authority_id: AuthorityId::new(),
            active_attempt: attempt.id.clone(),
            state: TaskState::Queued,
            pause_request: None,
            delivery: DeliveryState::None,
            workspace: None,
            attempts: vec![attempt],
        }
    }

    /// Reconstructs the minimum controller state required to explicitly
    /// resume a recovered root task. The prior attempt stays durable in the
    /// runtime store; this in-memory view begins at its recovery boundary.
    pub fn recovered_root(
        root_session_id: RootSessionId,
        task_id: TaskId,
        attempt_id: AttemptId,
        attempt_number: u32,
    ) -> Self {
        let attempt = TaskAttempt {
            id: attempt_id.clone(),
            task_id: task_id.clone(),
            number: attempt_number,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            checkpoint: None,
            delivery: None,
            budget: EffectiveBudget::default(),
            usage: AttemptUsage::default(),
            terminal_reason: Some(TerminalReason::RecoveryRequired(RecoveryEvidence(
                "recovered after runtime restart".into(),
            ))),
        };
        Self {
            id: task_id,
            root_session_id,
            parent_id: None,
            depth: TaskDepth::Root,
            created_at: Utc::now(),
            current_contract: ContractVersion::initial(),
            authority_id: AuthorityId::new(),
            active_attempt: attempt_id,
            state: TaskState::RecoveryRequired(RecoveryEvidence(
                "recovered after runtime restart".into(),
            )),
            pause_request: None,
            delivery: DeliveryState::None,
            workspace: None,
            attempts: vec![attempt],
        }
    }

    /// Rehydrates a successor attempt that was durably persisted behind the
    /// recovery gate but not yet attested or admitted to a worker.
    pub fn recovered_gated_root(
        root_session_id: RootSessionId,
        task_id: TaskId,
        attempt_id: AttemptId,
        attempt_number: u32,
    ) -> Self {
        let mut task = Self::recovered_root(root_session_id, task_id, attempt_id, attempt_number);
        task.state = TaskState::Queued;
        let attempt = task.active_attempt_mut();
        attempt.ended_at = None;
        attempt.terminal_reason = None;
        task
    }

    pub fn recovered_child(
        root_session_id: RootSessionId,
        task_id: TaskId,
        parent_id: TaskId,
        depth: TaskDepth,
        attempt_id: AttemptId,
        attempt_number: u32,
    ) -> Self {
        let mut task = Self::recovered_root(root_session_id, task_id, attempt_id, attempt_number);
        task.parent_id = Some(parent_id);
        task.depth = depth;
        task
    }

    pub fn recovered_gated_child(
        root_session_id: RootSessionId,
        task_id: TaskId,
        parent_id: TaskId,
        depth: TaskDepth,
        attempt_id: AttemptId,
        attempt_number: u32,
    ) -> Self {
        let mut task =
            Self::recovered_gated_root(root_session_id, task_id, attempt_id, attempt_number);
        task.parent_id = Some(parent_id);
        task.depth = depth;
        task
    }

    /// Reconstructs reducer state needed to route durable review mail after a
    /// process restart. Historical rows remain authoritative in SQLite.
    #[allow(clippy::too_many_arguments)]
    pub fn hydrated_review_task(
        root_session_id: RootSessionId,
        task_id: TaskId,
        parent_id: Option<TaskId>,
        depth: TaskDepth,
        attempt_id: AttemptId,
        attempt_number: u32,
        state: TaskState,
        delivery: Option<DeliveryReport>,
    ) -> Self {
        let ended_at = state.is_terminal().then(Utc::now);
        let attempt = TaskAttempt {
            id: attempt_id.clone(),
            task_id: task_id.clone(),
            number: attempt_number,
            started_at: Utc::now(),
            ended_at,
            checkpoint: None,
            delivery: delivery.clone(),
            budget: EffectiveBudget::default(),
            usage: AttemptUsage::default(),
            terminal_reason: None,
        };
        let delivery = match (&state, delivery) {
            (TaskState::AwaitingParentReview(_), Some(report)) => {
                DeliveryState::ReadyForReview(report.id)
            }
            _ => DeliveryState::None,
        };
        Self {
            id: task_id,
            root_session_id,
            parent_id,
            depth,
            created_at: Utc::now(),
            current_contract: ContractVersion::initial(),
            authority_id: AuthorityId::new(),
            active_attempt: attempt_id,
            state,
            pause_request: None,
            delivery,
            workspace: None,
            attempts: vec![attempt],
        }
    }

    pub fn new_child(root_session_id: RootSessionId, parent_id: TaskId) -> Self {
        let mut task = Self::new_root(root_session_id);
        task.parent_id = Some(parent_id);
        task.depth = TaskDepth::Child;
        task
    }

    pub fn with_workspace(mut self, workspace: WorkspaceLeaseId) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn active_attempt(&self) -> &TaskAttempt {
        self.attempts
            .iter()
            .find(|attempt| attempt.id == self.active_attempt)
            .expect("active attempt must be retained in attempt history")
    }

    pub fn active_attempt_id(&self) -> &AttemptId {
        &self.active_attempt
    }

    pub fn attempts(&self) -> &[TaskAttempt] {
        &self.attempts
    }

    pub fn state(&self) -> &TaskState {
        &self.state
    }

    pub fn pause_requested(&self) -> bool {
        self.pause_request.is_some()
    }

    pub fn delivery(&self) -> &DeliveryState {
        &self.delivery
    }

    fn active_attempt_mut(&mut self) -> &mut TaskAttempt {
        self.attempts
            .iter_mut()
            .find(|attempt| attempt.id == self.active_attempt)
            .expect("active attempt must be retained in attempt history")
    }

    // Reducer errors preserve complete state-transition evidence for callers.
    #[allow(clippy::result_large_err)]
    pub fn reduce(
        &mut self,
        event: TaskEvent,
        now: DateTime<Utc>,
    ) -> Result<TransitionResult, TaskReduceError> {
        reduce(self, event, now)
    }

    fn start_next_attempt(
        &mut self,
        terminal_reason: TerminalReason,
        clear_delivery: bool,
        now: DateTime<Utc>,
    ) -> Result<TaskAttempt, AttemptLifecycleError> {
        let current = self.active_attempt_mut();
        if current.ended_at.is_none() {
            current.ended_at = Some(now);
            current.terminal_reason = Some(terminal_reason);
        }
        let next = current.successor();
        self.active_attempt = next.id.clone();
        self.attempts.push(next.clone());
        self.state = TaskState::Queued;
        if clear_delivery {
            self.delivery = DeliveryState::None;
        }
        Ok(next)
    }

    fn close_active_attempt(&mut self, reason: TerminalReason, now: DateTime<Utc>) {
        let attempt = self.active_attempt_mut();
        attempt.ended_at = Some(now);
        attempt.terminal_reason = Some(reason);
    }

    fn terminal_reason(&self) -> TerminalReason {
        match &self.state {
            TaskState::Completed => TerminalReason::Completed,
            TaskState::CompletedNoChanges => TerminalReason::CompletedNoChanges,
            TaskState::Blocked(reason) => TerminalReason::Blocked(reason.clone()),
            TaskState::Stalled(evidence) => TerminalReason::Stalled(evidence.clone()),
            TaskState::TimedOut(kind) => TerminalReason::TimedOut(kind.clone()),
            TaskState::BudgetExhausted(kind) => TerminalReason::BudgetExhausted(kind.clone()),
            TaskState::Failed(failure) => TerminalReason::Failed(failure.clone()),
            TaskState::Cancelled(reason) => TerminalReason::Cancelled(reason.clone()),
            TaskState::RecoveryRequired(evidence) => {
                TerminalReason::RecoveryRequired(evidence.clone())
            }
            _ => unreachable!("retry only accepts terminal task states"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AttemptLifecycleError {
    #[error("retry requires a terminal task state")]
    RetryRequiresTerminalState,
    #[error("rework requires awaiting parent review")]
    ReworkRequiresReviewState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskEvent {
    AdmissionGranted {
        attempt_id: AttemptId,
    },
    ResourceUnavailable {
        attempt_id: AttemptId,
        wait: ResourceWait,
    },
    ResourceAvailable {
        attempt_id: AttemptId,
        wait: ResourceWait,
    },
    PermissionRequested {
        attempt_id: AttemptId,
        request: PermissionRequestId,
    },
    PermissionResolved {
        attempt_id: AttemptId,
        request: PermissionRequestId,
        decision: PermissionDecision,
    },
    WorkerDelivered {
        attempt_id: AttemptId,
        delivery: DeliveryReport,
    },
    WorkerCompletedNoChanges {
        attempt_id: AttemptId,
    },
    WorkerFailed {
        attempt_id: AttemptId,
        failure: TaskFailure,
    },
    ReviewAccepted {
        attempt_id: AttemptId,
        actor: TaskId,
        delivery_id: DeliveryId,
        integration: IntegrationValidation,
    },
    ReviewRework {
        attempt_id: AttemptId,
        delivery_id: DeliveryId,
        feedback: MessageId,
    },
    ReviewRejected {
        attempt_id: AttemptId,
        delivery_id: DeliveryId,
        reason: MessageId,
    },
    CancelRequested {
        attempt_id: AttemptId,
        reason: CancelReason,
    },
    PauseRequested {
        attempt_id: AttemptId,
        reason: PauseReason,
    },
    PauseAcknowledged {
        attempt_id: AttemptId,
    },
    ResumeRequested {
        attempt_id: AttemptId,
    },
    RuntimeInterrupted {
        attempt_id: AttemptId,
        evidence: RecoveryEvidence,
    },
    RecoveryConflict {
        attempt_id: AttemptId,
        reason: BlockReason,
    },
    WatchdogStalled {
        attempt_id: AttemptId,
        evidence: WatchdogEvidence,
    },
    WatchdogTimedOut {
        attempt_id: AttemptId,
        kind: TimeoutKind,
    },
    WatchdogBudgetExhausted {
        attempt_id: AttemptId,
        kind: BudgetKind,
    },
    RetryRequested {
        attempt_id: AttemptId,
    },
}

impl TaskEvent {
    fn attempt_id(&self) -> &AttemptId {
        match self {
            Self::AdmissionGranted { attempt_id }
            | Self::ResourceUnavailable { attempt_id, .. }
            | Self::ResourceAvailable { attempt_id, .. }
            | Self::PermissionRequested { attempt_id, .. }
            | Self::PermissionResolved { attempt_id, .. }
            | Self::WorkerDelivered { attempt_id, .. }
            | Self::WorkerCompletedNoChanges { attempt_id }
            | Self::WorkerFailed { attempt_id, .. }
            | Self::ReviewAccepted { attempt_id, .. }
            | Self::ReviewRework { attempt_id, .. }
            | Self::ReviewRejected { attempt_id, .. }
            | Self::CancelRequested { attempt_id, .. }
            | Self::PauseRequested { attempt_id, .. }
            | Self::PauseAcknowledged { attempt_id }
            | Self::ResumeRequested { attempt_id }
            | Self::RuntimeInterrupted { attempt_id, .. }
            | Self::RecoveryConflict { attempt_id, .. }
            | Self::WatchdogStalled { attempt_id, .. }
            | Self::WatchdogTimedOut { attempt_id, .. }
            | Self::WatchdogBudgetExhausted { attempt_id, .. }
            | Self::RetryRequested { attempt_id } => attempt_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionResult {
    pub new_attempt: Option<TaskAttempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TaskReduceError {
    #[error("event attempt {event_attempt} is not active attempt {active_attempt}")]
    StaleAttempt {
        event_attempt: AttemptId,
        active_attempt: AttemptId,
    },
    #[error("event delivery does not match the delivery awaiting review")]
    DeliveryMismatch {
        expected: DeliveryId,
        actual: DeliveryId,
    },
    #[error("permission resolution does not match the active wait")]
    PermissionMismatch {
        expected: PermissionRequestId,
        actual: PermissionRequestId,
    },
    #[error("resource resolution does not match the active wait")]
    ResourceMismatch {
        expected: ResourceWait,
        actual: ResourceWait,
    },
    #[error("invalid coding delivery evidence: {reason}")]
    InvalidDeliveryEvidence { reason: &'static str },
    #[error("review acceptance requires a non-root task")]
    ReviewRequiresParent,
    #[error("review actor does not match direct parent")]
    ReviewActorMismatch { expected: TaskId, actual: TaskId },
    #[error("integration validation did not succeed with evidence")]
    IntegrationNotValidated,
    #[error(transparent)]
    Transition(#[from] TaskTransitionError),
    #[error(transparent)]
    Attempt(#[from] AttemptLifecycleError),
}

// Reducer errors preserve complete state-transition evidence for callers.
#[allow(clippy::result_large_err)]
pub fn reduce(
    task: &mut AgentTask,
    event: TaskEvent,
    now: DateTime<Utc>,
) -> Result<TransitionResult, TaskReduceError> {
    if event.attempt_id() != task.active_attempt_id() {
        return Err(TaskReduceError::StaleAttempt {
            event_attempt: event.attempt_id().clone(),
            active_attempt: task.active_attempt_id().clone(),
        });
    }

    let mut result = TransitionResult { new_attempt: None };
    match event {
        TaskEvent::AdmissionGranted { .. } => transition(task, TaskState::Running, now)?,
        TaskEvent::ResourceUnavailable { wait, .. } => {
            transition(task, TaskState::WaitingForResource(wait), now)?
        }
        TaskEvent::ResourceAvailable { wait, .. } => {
            let TaskState::WaitingForResource(expected) = task.state() else {
                return Err(TaskTransitionError {
                    from: task.state().clone(),
                    to: TaskState::Queued,
                }
                .into());
            };
            if expected != &wait {
                return Err(TaskReduceError::ResourceMismatch {
                    expected: expected.clone(),
                    actual: wait,
                });
            }
            transition(task, TaskState::Queued, now)?;
        }
        TaskEvent::PermissionRequested { request, .. } => {
            transition(task, TaskState::WaitingForPermission(request), now)?
        }
        TaskEvent::PermissionResolved {
            request, decision, ..
        } => {
            let TaskState::WaitingForPermission(expected) = task.state() else {
                return Err(TaskTransitionError {
                    from: task.state().clone(),
                    to: TaskState::Queued,
                }
                .into());
            };
            if expected != &request {
                return Err(TaskReduceError::PermissionMismatch {
                    expected: expected.clone(),
                    actual: request,
                });
            }
            match decision {
                PermissionDecision::Allow => transition(task, TaskState::Queued, now)?,
                PermissionDecision::Deny => transition(
                    task,
                    TaskState::Blocked(BlockReason("permission denied".into())),
                    now,
                )?,
            }
        }
        TaskEvent::WorkerDelivered { delivery, .. } => {
            delivery
                .validate_for(task)
                .map_err(|reason| TaskReduceError::InvalidDeliveryEvidence { reason })?;
            transition(
                task,
                TaskState::AwaitingParentReview(delivery.id.clone()),
                now,
            )?;
            task.active_attempt_mut().delivery = Some(delivery.clone());
            task.delivery = DeliveryState::ReadyForReview(delivery.id);
        }
        TaskEvent::WorkerFailed { failure, .. } => {
            transition(task, TaskState::Failed(failure), now)?
        }
        TaskEvent::WorkerCompletedNoChanges { .. } => {
            transition(task, TaskState::CompletedNoChanges, now)?
        }
        TaskEvent::ReviewAccepted {
            delivery_id,
            actor,
            integration,
            ..
        } => {
            require_review_delivery(task, &delivery_id)?;
            let parent_id = task
                .parent_id
                .as_ref()
                .ok_or(TaskReduceError::ReviewRequiresParent)?;
            if parent_id != &actor {
                return Err(TaskReduceError::ReviewActorMismatch {
                    expected: parent_id.clone(),
                    actual: actor,
                });
            }
            if !integration.is_valid() {
                return Err(TaskReduceError::IntegrationNotValidated);
            }
            task.delivery = DeliveryState::Accepted {
                delivery: delivery_id,
                integration,
            };
            transition(task, TaskState::Completed, now)?;
        }
        TaskEvent::ReviewRework {
            delivery_id,
            feedback,
            ..
        } => {
            require_review_delivery(task, &delivery_id)?;
            task.delivery = DeliveryState::ReworkRequested {
                previous: delivery_id,
                feedback,
            };
            result.new_attempt =
                Some(task.start_next_attempt(TerminalReason::ReworkRequested, false, now)?);
        }
        TaskEvent::ReviewRejected {
            delivery_id,
            reason,
            ..
        } => {
            require_review_delivery(task, &delivery_id)?;
            task.delivery = DeliveryState::Rejected {
                delivery: delivery_id,
                reason,
            };
            transition(
                task,
                TaskState::Blocked(BlockReason("review rejected".into())),
                now,
            )?;
        }
        TaskEvent::CancelRequested { reason, .. } => {
            transition(task, TaskState::Cancelled(reason), now)?
        }
        TaskEvent::PauseRequested { reason, .. } => {
            if task.pause_request.is_none() {
                task.pause_request = Some(reason);
            }
        }
        TaskEvent::PauseAcknowledged { .. } => {
            let reason = task
                .pause_request
                .take()
                .ok_or_else(|| TaskTransitionError {
                    from: task.state().clone(),
                    to: TaskState::Paused(PauseReason("pause was not requested".into())),
                })?;
            transition(task, TaskState::Paused(reason), now)?
        }
        TaskEvent::ResumeRequested { .. } => {
            if !matches!(task.state(), TaskState::Paused(_)) {
                return Err(TaskTransitionError {
                    from: task.state().clone(),
                    to: TaskState::Queued,
                }
                .into());
            }
            transition(task, TaskState::Queued, now)?;
            task.pause_request = None;
        }
        TaskEvent::RuntimeInterrupted { evidence, .. } => {
            transition(task, TaskState::RecoveryRequired(evidence), now)?
        }
        TaskEvent::RecoveryConflict { reason, .. } => {
            transition(task, TaskState::Blocked(reason), now)?
        }
        TaskEvent::WatchdogStalled { evidence, .. } => {
            transition(task, TaskState::Stalled(evidence), now)?
        }
        TaskEvent::WatchdogTimedOut { kind, .. } => {
            transition(task, TaskState::TimedOut(kind), now)?
        }
        TaskEvent::WatchdogBudgetExhausted { kind, .. } => {
            transition(task, TaskState::BudgetExhausted(kind), now)?
        }
        TaskEvent::RetryRequested { .. } => {
            if !task.state().is_terminal() {
                return Err(AttemptLifecycleError::RetryRequiresTerminalState.into());
            }
            result.new_attempt =
                Some(task.start_next_attempt(task.terminal_reason(), true, now)?);
        }
    }
    Ok(result)
}

// The reducer error includes the full unexpected state for operator diagnosis.
#[allow(clippy::result_large_err)]
fn require_review_delivery(
    task: &AgentTask,
    delivery_id: &DeliveryId,
) -> Result<(), TaskReduceError> {
    let TaskState::AwaitingParentReview(expected) = task.state() else {
        return Err(TaskTransitionError {
            from: task.state().clone(),
            to: TaskState::Completed,
        }
        .into());
    };
    if expected != delivery_id {
        return Err(TaskReduceError::DeliveryMismatch {
            expected: expected.clone(),
            actual: delivery_id.clone(),
        });
    }
    Ok(())
}

// The reducer error includes the full unexpected state for operator diagnosis.
#[allow(clippy::result_large_err)]
fn transition(
    task: &mut AgentTask,
    next: TaskState,
    now: DateTime<Utc>,
) -> Result<(), TaskReduceError> {
    task.state.can_transition_to(next.clone())?;
    let terminal_reason = terminal_reason_for(&next);
    task.state = next;
    if let Some(reason) = terminal_reason {
        task.pause_request = None;
        task.close_active_attempt(reason, now);
    }
    Ok(())
}

fn terminal_reason_for(state: &TaskState) -> Option<TerminalReason> {
    match state {
        TaskState::Completed => Some(TerminalReason::Completed),
        TaskState::CompletedNoChanges => Some(TerminalReason::CompletedNoChanges),
        TaskState::Blocked(reason) => Some(TerminalReason::Blocked(reason.clone())),
        TaskState::Stalled(evidence) => Some(TerminalReason::Stalled(evidence.clone())),
        TaskState::TimedOut(kind) => Some(TerminalReason::TimedOut(kind.clone())),
        TaskState::BudgetExhausted(kind) => Some(TerminalReason::BudgetExhausted(kind.clone())),
        TaskState::Failed(failure) => Some(TerminalReason::Failed(failure.clone())),
        TaskState::Cancelled(reason) => Some(TerminalReason::Cancelled(reason.clone())),
        TaskState::RecoveryRequired(evidence) => {
            Some(TerminalReason::RecoveryRequired(evidence.clone()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_workspace() -> (AgentTask, WorkspaceLeaseId) {
        let workspace = WorkspaceLeaseId::new();
        (
            AgentTask::new_root(RootSessionId::new()).with_workspace(workspace.clone()),
            workspace,
        )
    }

    fn coding_delivery(workspace: WorkspaceLeaseId) -> DeliveryReport {
        DeliveryReport::coding("abc123", "main", workspace, "validated commit")
    }

    #[test]
    fn legacy_delivery_json_still_deserializes_without_optional_fields() {
        let workspace = WorkspaceLeaseId::new();
        let delivery = serde_json::from_value::<DeliveryReport>(serde_json::json!({
            "id": DeliveryId::new(),
            "commit": "abc123",
            "base_ref": "main",
            "workspace": workspace,
            "evidence": "validated commit",
        }))
        .unwrap();

        assert!(delivery.changed_files.is_empty());
        assert!(delivery.known_limitations.is_empty());
    }

    #[test]
    fn leaf_cannot_spawn_descendant() {
        assert!(TaskDepth::Leaf.can_spawn_child().is_err());
    }

    #[test]
    fn completed_cannot_return_to_running_without_retry() {
        assert!(
            TaskState::Completed
                .can_transition_to(TaskState::Running)
                .is_err()
        );
    }

    #[test]
    fn basic_state_transitions_are_legal() {
        assert!(
            TaskState::Queued
                .can_transition_to(TaskState::Running)
                .is_ok()
        );
        assert!(
            TaskState::Running
                .can_transition_to(TaskState::AwaitingParentReview(DeliveryId::new()))
                .is_ok()
        );
    }

    #[test]
    fn stale_attempt_events_are_rejected() {
        let mut task = AgentTask::new_root(RootSessionId::new());
        let result = task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: AttemptId::new(),
            },
            Utc::now(),
        );

        assert!(matches!(result, Err(TaskReduceError::StaleAttempt { .. })));
        assert_eq!(task.state(), &TaskState::Queued);
    }

    #[test]
    fn permission_resolution_must_match_the_active_wait() {
        let mut task = AgentTask::new_root(RootSessionId::new());
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        let request = PermissionRequestId::new();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::PermissionRequested {
                attempt_id: attempt_id.clone(),
                request: request.clone(),
            },
            now,
        )
        .unwrap();

        let result = task.reduce(
            TaskEvent::PermissionResolved {
                attempt_id,
                request: PermissionRequestId::new(),
                decision: PermissionDecision::Allow,
            },
            now,
        );
        assert!(matches!(
            result,
            Err(TaskReduceError::PermissionMismatch { .. })
        ));
        assert_eq!(task.state(), &TaskState::WaitingForPermission(request));
    }

    #[test]
    fn resolved_resource_wait_returns_to_queued() {
        let mut task = AgentTask::new_root(RootSessionId::new());
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        let wait = ResourceWait("llm permit".into());
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::ResourceUnavailable {
                attempt_id: attempt_id.clone(),
                wait: wait.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(TaskEvent::ResourceAvailable { attempt_id, wait }, now)
            .unwrap();

        assert_eq!(task.state(), &TaskState::Queued);
    }

    #[test]
    fn worker_delivery_requires_coding_evidence() {
        let (mut task, workspace) = task_with_workspace();
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();

        let result = task.reduce(
            TaskEvent::WorkerDelivered {
                attempt_id,
                delivery: DeliveryReport::coding("", "main", workspace, "validated commit"),
            },
            now,
        );
        assert!(matches!(
            result,
            Err(TaskReduceError::InvalidDeliveryEvidence { .. })
        ));
        assert_eq!(task.state(), &TaskState::Running);
    }

    #[test]
    fn review_events_require_the_current_delivery() {
        let parent_id = TaskId::new();
        let workspace = WorkspaceLeaseId::new();
        let mut task = AgentTask::new_child(RootSessionId::new(), parent_id.clone())
            .with_workspace(workspace.clone());
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        let delivery = coding_delivery(workspace);
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerDelivered {
                attempt_id: attempt_id.clone(),
                delivery: delivery.clone(),
            },
            now,
        )
        .unwrap();

        let result = task.reduce(
            TaskEvent::ReviewAccepted {
                attempt_id,
                actor: parent_id,
                delivery_id: DeliveryId::new(),
                integration: IntegrationValidation::passed("integration test"),
            },
            now,
        );
        assert!(matches!(
            result,
            Err(TaskReduceError::DeliveryMismatch { .. })
        ));
        assert_eq!(task.state(), &TaskState::AwaitingParentReview(delivery.id));
    }

    #[test]
    fn terminal_events_close_the_active_attempt() {
        let mut task = AgentTask::new_root(RootSessionId::new());
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerFailed {
                attempt_id,
                failure: TaskFailure::new("worker failed"),
            },
            now,
        )
        .unwrap();

        assert!(task.active_attempt().ended_at.is_some());
        assert_eq!(
            task.active_attempt().terminal_reason,
            Some(TerminalReason::Failed(TaskFailure::new("worker failed")))
        );
    }

    #[test]
    fn retry_and_rework_create_attempts_without_erasing_evidence() {
        let (mut task, workspace) = task_with_workspace();
        let now = Utc::now();
        let original_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: original_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerFailed {
                attempt_id: original_id.clone(),
                failure: TaskFailure::new("worker failed"),
            },
            now,
        )
        .unwrap();

        let retry = task
            .reduce(
                TaskEvent::RetryRequested {
                    attempt_id: original_id,
                },
                now,
            )
            .unwrap()
            .new_attempt
            .unwrap();
        assert_eq!(task.attempts().len(), 2);
        assert_eq!(
            task.attempts()[0].terminal_reason,
            Some(TerminalReason::Failed(TaskFailure::new("worker failed")))
        );

        let delivery = coding_delivery(workspace);
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: retry.id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerDelivered {
                attempt_id: retry.id.clone(),
                delivery: delivery.clone(),
            },
            now,
        )
        .unwrap();
        let rework = task
            .reduce(
                TaskEvent::ReviewRework {
                    attempt_id: retry.id.clone(),
                    delivery_id: delivery.id.clone(),
                    feedback: MessageId::new(),
                },
                now,
            )
            .unwrap()
            .new_attempt
            .unwrap();
        assert_ne!(rework.id, retry.id);
        assert_eq!(task.attempts().len(), 3);
        assert_eq!(task.attempts()[1].delivery, Some(delivery.clone()));
        assert!(
            matches!(task.delivery(), DeliveryState::ReworkRequested { previous, .. } if *previous == delivery.id)
        );
    }

    #[test]
    fn only_parent_with_successful_integration_can_accept_review() {
        let parent_id = TaskId::new();
        let workspace = WorkspaceLeaseId::new();
        let mut task = AgentTask::new_child(RootSessionId::new(), parent_id.clone())
            .with_workspace(workspace.clone());
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        let delivery = coding_delivery(workspace);
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerDelivered {
                attempt_id: attempt_id.clone(),
                delivery: delivery.clone(),
            },
            now,
        )
        .unwrap();

        let wrong_actor = task.reduce(
            TaskEvent::ReviewAccepted {
                attempt_id: attempt_id.clone(),
                actor: TaskId::new(),
                delivery_id: delivery.id.clone(),
                integration: IntegrationValidation::passed("integration test"),
            },
            now,
        );
        assert!(matches!(
            wrong_actor,
            Err(TaskReduceError::ReviewActorMismatch { .. })
        ));
        let failed_integration = task.reduce(
            TaskEvent::ReviewAccepted {
                attempt_id,
                actor: parent_id,
                delivery_id: delivery.id,
                integration: IntegrationValidation::failed("integration failed"),
            },
            now,
        );
        assert!(matches!(
            failed_integration,
            Err(TaskReduceError::IntegrationNotValidated)
        ));
    }

    #[test]
    fn root_task_cannot_accept_its_own_review() {
        let (mut task, workspace) = task_with_workspace();
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        let delivery = coding_delivery(workspace);
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::WorkerDelivered {
                attempt_id: attempt_id.clone(),
                delivery: delivery.clone(),
            },
            now,
        )
        .unwrap();

        let result = task.reduce(
            TaskEvent::ReviewAccepted {
                attempt_id,
                actor: TaskId::new(),
                delivery_id: delivery.id,
                integration: IntegrationValidation::passed("integration test"),
            },
            now,
        );
        assert!(matches!(result, Err(TaskReduceError::ReviewRequiresParent)));
    }

    #[test]
    fn pause_intent_keeps_running_until_safe_checkpoint_acknowledgement() {
        let (mut task, _) = task_with_workspace();
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::PauseRequested {
                attempt_id: attempt_id.clone(),
                reason: PauseReason("user requested pause".into()),
            },
            now,
        )
        .unwrap();
        assert_eq!(task.state(), &TaskState::Running);
        assert!(task.pause_requested());
        task.reduce(
            TaskEvent::PauseAcknowledged {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        assert!(matches!(task.state(), TaskState::Paused(_)));
        task.reduce(TaskEvent::ResumeRequested { attempt_id }, now)
            .unwrap();
        assert_eq!(task.state(), &TaskState::Queued);
    }

    #[test]
    fn duplicate_pause_requests_preserve_the_original_intent_until_acknowledged() {
        let (mut task, _) = task_with_workspace();
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::PauseRequested {
                attempt_id: attempt_id.clone(),
                reason: PauseReason("first request".into()),
            },
            now,
        )
        .unwrap();
        task.reduce(
            TaskEvent::PauseRequested {
                attempt_id: attempt_id.clone(),
                reason: PauseReason("duplicate request".into()),
            },
            now,
        )
        .unwrap();

        assert_eq!(task.state(), &TaskState::Running);
        task.reduce(TaskEvent::PauseAcknowledged { attempt_id }, now)
            .unwrap();
        assert_eq!(
            task.state(),
            &TaskState::Paused(PauseReason("first request".into()))
        );
    }

    #[test]
    fn watchdog_terminal_keeps_structured_meaningful_progress_evidence() {
        let (mut task, _) = task_with_workspace();
        let now = Utc::now();
        let attempt_id = task.active_attempt_id().clone();
        task.reduce(
            TaskEvent::AdmissionGranted {
                attempt_id: attempt_id.clone(),
            },
            now,
        )
        .unwrap();
        let evidence = WatchdogEvidence {
            last_meaningful_event_id: Some(7),
            last_meaningful_at: now,
            elapsed_secs: 301,
            current_wait: None,
        };

        task.reduce(
            TaskEvent::WatchdogStalled {
                attempt_id,
                evidence: evidence.clone(),
            },
            now,
        )
        .unwrap();

        assert_eq!(task.state(), &TaskState::Stalled(evidence));
    }
}

#[cfg(test)]
mod workspace_mode_tests {
    use super::TaskWorkspaceMode;

    #[test]
    fn workspace_mode_round_trips_through_its_storage_string() {
        for mode in [TaskWorkspaceMode::Coding, TaskWorkspaceMode::ReadOnly] {
            assert_eq!(TaskWorkspaceMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(TaskWorkspaceMode::parse("writable"), None);
    }
}
