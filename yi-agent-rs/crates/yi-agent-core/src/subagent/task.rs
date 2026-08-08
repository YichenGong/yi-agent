use std::fmt;

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
    };
}

uuid_id!(TaskId);
uuid_id!(AttemptId);
uuid_id!(RootSessionId);
uuid_id!(DeliveryId);
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogEvidence(pub String);

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
    pub fn can_transition_to(&self, next: Self) -> Result<(), TaskTransitionError> {
        let legal = matches!(
            (self, &next),
            (
                Self::Queued,
                Self::Running | Self::Paused(_) | Self::Cancelled(_)
            ) | (
                Self::Running,
                Self::WaitingForResource(_)
                    | Self::WaitingForPermission(_)
                    | Self::WaitingForChildren(_)
                    | Self::Paused(_)
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
                Self::Queued | Self::Paused(_) | Self::Cancelled(_) | Self::RecoveryRequired(_)
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
    Accepted { delivery: DeliveryId },
    ReworkRequested { previous: DeliveryId },
    Rejected { delivery: DeliveryId },
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
            budget: self.budget.clone(),
            usage: AttemptUsage::default(),
            terminal_reason: None,
        }
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
    pub active_attempt: AttemptId,
    pub state: TaskState,
    pub delivery: DeliveryState,
    pub workspace: Option<WorkspaceLeaseId>,
    pub attempts: Vec<TaskAttempt>,
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
            delivery: DeliveryState::None,
            workspace: None,
            attempts: vec![attempt],
        }
    }

    pub fn active_attempt(&self) -> &TaskAttempt {
        self.attempts
            .iter()
            .find(|attempt| attempt.id == self.active_attempt)
            .expect("active attempt must be retained in attempt history")
    }

    pub fn active_attempt_mut(&mut self) -> &mut TaskAttempt {
        self.attempts
            .iter_mut()
            .find(|attempt| attempt.id == self.active_attempt)
            .expect("active attempt must be retained in attempt history")
    }

    pub fn transition_to(&mut self, next: TaskState) -> Result<(), TaskTransitionError> {
        self.state.can_transition_to(next.clone())?;
        self.state = next;
        Ok(())
    }

    pub fn retry(&mut self) -> Result<TaskAttempt, AttemptLifecycleError> {
        if !self.state.is_terminal() {
            return Err(AttemptLifecycleError::RetryRequiresTerminalState);
        }
        self.start_next_attempt(self.terminal_reason())
    }

    pub fn rework(&mut self) -> Result<TaskAttempt, AttemptLifecycleError> {
        if !matches!(self.state, TaskState::AwaitingParentReview(_)) {
            return Err(AttemptLifecycleError::ReworkRequiresReviewState);
        }
        self.start_next_attempt(TerminalReason::ReworkRequested)
    }

    fn start_next_attempt(
        &mut self,
        terminal_reason: TerminalReason,
    ) -> Result<TaskAttempt, AttemptLifecycleError> {
        let current = self.active_attempt_mut();
        if current.ended_at.is_some() {
            return Err(AttemptLifecycleError::ActiveAttemptAlreadyClosed);
        }
        current.ended_at = Some(Utc::now());
        current.terminal_reason = Some(terminal_reason);
        let next = current.successor();
        self.active_attempt = next.id.clone();
        self.attempts.push(next.clone());
        self.state = TaskState::Queued;
        self.delivery = DeliveryState::None;
        Ok(next)
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
    #[error("the active attempt is already closed")]
    ActiveAttemptAlreadyClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn retry_and_rework_create_attempts_without_erasing_evidence() {
        let mut task = AgentTask::new_root(RootSessionId::new());
        let original_id = task.active_attempt().id.clone();
        task.active_attempt_mut().checkpoint = Some(StableCheckpoint::new("before failure"));
        task.transition_to(TaskState::Running).unwrap();
        task.transition_to(TaskState::Failed(TaskFailure::new("worker failed")))
            .unwrap();

        let retry = task.retry().unwrap();
        assert_ne!(retry.id, original_id);
        assert_eq!(task.attempts.len(), 2);
        assert_eq!(task.attempts[0].id, original_id);
        assert_eq!(
            task.attempts[0].checkpoint,
            Some(StableCheckpoint::new("before failure"))
        );

        task.transition_to(TaskState::Running).unwrap();
        task.transition_to(TaskState::AwaitingParentReview(DeliveryId::new()))
            .unwrap();
        let rework = task.rework().unwrap();
        assert_ne!(rework.id, retry.id);
        assert_eq!(task.attempts.len(), 3);
        assert_eq!(task.attempts[1].id, retry.id);
    }
}
