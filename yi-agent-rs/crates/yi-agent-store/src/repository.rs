use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Local, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use yi_agent_core::subagent::task::{
    BudgetKind, DeliveryId, DeliveryReport, IntegrationValidation, TimeoutKind,
};
use yi_agent_core::subagent::task::{
    MessageId, PermissionDecision, PermissionRequestId, WorkspaceLeaseId,
};
use yi_agent_core::subagent::worker::{
    WorkerRecoveryAttestation, WorkerRecoveryContext, WorkerWorkspace,
};
use yi_agent_core::{AttemptId, RootSessionId, TaskId};

use crate::schedule::{ScheduleDefinition, WatchdogLimits, WatchdogObservation, WatchdogUsage};

const LATEST_SCHEMA_VERSION: i64 = 7;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unknown runtime event kind: {kind}")]
    UnknownEventKind { kind: String },
    #[error("task does not exist: {task}")]
    TaskNotFound { task: String },
    #[error("external mailbox message does not exist: {message_id}")]
    MailboxMessageNotFound { message_id: String },
    #[error("permission request is not pending: {request}")]
    PermissionRequestNotPending { request: String },
    #[error("permission request does not exist: {request}")]
    PermissionRequestNotFound { request: String },
    #[error("delivery requires a direct parent task: {task}")]
    DeliveryRequiresParent { task: String },
    #[error("task is not running its expected attempt for delivery: {task}")]
    TaskNotReadyForDelivery { task: String },
    #[error("delivery is not awaiting review for task {task}: {delivery}")]
    DeliveryNotAwaitingReview { task: String, delivery: String },
    #[error("review actor is not the direct parent of task {task}: {actor}")]
    ReviewActorMismatch { task: String, actor: String },
    #[error("accepted review requires successful integration evidence")]
    IntegrationNotValidated,
    #[error("rework review requires non-empty feedback")]
    ReviewFeedbackRequired,
    #[error("rejected review requires a non-empty reason")]
    ReviewReasonRequired,
    #[error("worker recovery context is not durable: {reason}")]
    InvalidWorkerRecoveryContext { reason: String },
    #[error("admission cursor is invalid for {key}: {reason}")]
    InvalidAdmissionCursor { key: String, reason: String },
    #[error("watchdog snapshot is invalid for {attempt}: {reason}")]
    InvalidWatchdogSnapshot { attempt: String, reason: String },
    #[error("task workspace does not exist: {task}")]
    TaskWorkspaceNotFound { task: String },
    #[error("task workspace is invalid for {task}: {reason}")]
    InvalidTaskWorkspace { task: String, reason: String },
}

#[derive(Debug, Error)]
#[error("controlled recovery transition failed: cleanup={cleanup:?}; recovery={recovery:?}")]
pub struct ControlledRecoveryTransitionError {
    pub cleanup: Option<RepositoryError>,
    pub recovery: Option<RepositoryError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEvent {
    RuntimeDraining,
    RuntimeRecovered,
    TaskQueued,
    TaskStarted,
    TaskCancelled,
    TaskPauseRequested,
    TaskPaused,
    TaskProgress,
    TaskBlocked,
    TaskStalled,
    TaskTimedOut,
    TaskBudgetExhausted,
    TaskFailed,
    TaskRecoveryRequired,
    TaskRecoveryAttested,
    TaskDelivered,
    MailboxMessageQueued,
    MailboxMessageConsumed,
    PermissionRequested,
    PermissionResolved,
    ReviewApproved,
    ReviewAccepted,
    ReviewRework,
    ReviewRejected,
}

impl RuntimeEvent {
    fn name(self) -> &'static str {
        match self {
            Self::RuntimeDraining => "runtime_draining",
            Self::RuntimeRecovered => "runtime_recovered",
            Self::TaskQueued => "task_queued",
            Self::TaskStarted => "task_started",
            Self::TaskCancelled => "task_cancelled",
            Self::TaskPauseRequested => "task_pause_requested",
            Self::TaskPaused => "task_paused",
            Self::TaskProgress => "task_progress",
            Self::TaskBlocked => "task_blocked",
            Self::TaskStalled => "task_stalled",
            Self::TaskTimedOut => "task_timed_out",
            Self::TaskBudgetExhausted => "task_budget_exhausted",
            Self::TaskFailed => "task_failed",
            Self::TaskRecoveryRequired => "task_recovery_required",
            Self::TaskRecoveryAttested => "task_recovery_attested",
            Self::TaskDelivered => "task_delivered",
            Self::MailboxMessageQueued => "mailbox_message_queued",
            Self::MailboxMessageConsumed => "mailbox_message_consumed",
            Self::PermissionRequested => "permission_requested",
            Self::PermissionResolved => "permission_resolved",
            Self::ReviewApproved => "review_approved",
            Self::ReviewAccepted => "review_accepted",
            Self::ReviewRework => "review_rework",
            Self::ReviewRejected => "review_rejected",
        }
    }

    fn parse(kind: String) -> Result<Self, RepositoryError> {
        match kind.as_str() {
            "runtime_draining" => Ok(Self::RuntimeDraining),
            "runtime_recovered" => Ok(Self::RuntimeRecovered),
            "task_queued" => Ok(Self::TaskQueued),
            "task_started" => Ok(Self::TaskStarted),
            "task_cancelled" => Ok(Self::TaskCancelled),
            "task_pause_requested" => Ok(Self::TaskPauseRequested),
            "task_paused" => Ok(Self::TaskPaused),
            "task_progress" => Ok(Self::TaskProgress),
            "task_blocked" => Ok(Self::TaskBlocked),
            "task_stalled" => Ok(Self::TaskStalled),
            "task_timed_out" => Ok(Self::TaskTimedOut),
            "task_budget_exhausted" => Ok(Self::TaskBudgetExhausted),
            "task_failed" => Ok(Self::TaskFailed),
            "task_recovery_required" => Ok(Self::TaskRecoveryRequired),
            "task_recovery_attested" => Ok(Self::TaskRecoveryAttested),
            "task_delivered" => Ok(Self::TaskDelivered),
            "mailbox_message_queued" => Ok(Self::MailboxMessageQueued),
            "mailbox_message_consumed" => Ok(Self::MailboxMessageConsumed),
            "permission_requested" => Ok(Self::PermissionRequested),
            "permission_resolved" => Ok(Self::PermissionResolved),
            "review_approved" => Ok(Self::ReviewApproved),
            "review_accepted" => Ok(Self::ReviewAccepted),
            "review_rework" => Ok(Self::ReviewRework),
            "review_rejected" => Ok(Self::ReviewRejected),
            _ => Err(RepositoryError::UnknownEventKind { kind }),
        }
    }
}

/// The queued resource wait that made an attempt eligible for a watchdog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogResourceWait {
    pub resource_key: String,
    pub queued_at: DateTime<Utc>,
}

/// Durable evidence for a watchdog decision. Generated model text is excluded:
/// callers update this only from persisted meaningful worker events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogEvidence {
    pub last_meaningful_event_id: Option<i64>,
    pub last_meaningful_at: DateTime<Utc>,
    pub elapsed_secs: u64,
    pub current_wait: Option<WatchdogResourceWait>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationRootAttachment {
    pub idempotency_key: String,
    pub root_session_id: RootSessionId,
    pub root_task_id: TaskId,
    pub capability_digest: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAttemptWatchdog {
    pub limits: WatchdogLimits,
    pub observation: WatchdogObservation,
    pub last_meaningful_event_id: Option<i64>,
    pub current_wait: Option<WatchdogResourceWait>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedWatchdogTask {
    pub session_id: RootSessionId,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub snapshot: PersistedAttemptWatchdog,
}

/// One validated schedule and its next locally evaluated occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSchedule {
    pub id: String,
    pub definition: ScheduleDefinition,
    pub state: String,
    pub next_run_at: DateTime<Local>,
}

/// Result of evaluating one unique scheduled occurrence in a repository
/// transaction. A duplicate means a prior daemon tick already owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleOccurrenceResult {
    Fired,
    SkippedOverlap,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogTerminal {
    Stalled,
    TimedOut(TimeoutKind),
    BudgetExhausted(BudgetKind),
}

impl WatchdogTerminal {
    const fn state_name(&self) -> &'static str {
        match self {
            Self::Stalled => "stalled",
            Self::TimedOut(_) => "timed_out",
            Self::BudgetExhausted(_) => "budget_exhausted",
        }
    }

    const fn event(&self) -> RuntimeEvent {
        match self {
            Self::Stalled => RuntimeEvent::TaskStalled,
            Self::TimedOut(_) => RuntimeEvent::TaskTimedOut,
            Self::BudgetExhausted(_) => RuntimeEvent::TaskBudgetExhausted,
        }
    }

    fn terminal_json(&self, evidence: &WatchdogEvidence) -> Result<String, serde_json::Error> {
        let outcome = match self {
            Self::Stalled => serde_json::json!({ "kind": "stalled" }),
            Self::TimedOut(kind) => serde_json::json!({
                "kind": "timed_out",
                "timeout_kind": kind,
            }),
            Self::BudgetExhausted(kind) => serde_json::json!({
                "kind": "budget_exhausted",
                "budget_kind": kind,
            }),
        };
        serde_json::to_string(&serde_json::json!({
            "watchdog": evidence,
            "outcome": outcome,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedEvent {
    pub id: i64,
    pub task_id: TaskId,
    pub event: RuntimeEvent,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedMailboxMessage {
    pub message_id: String,
    pub recipient_task_id: TaskId,
    pub sender_task_id: Option<TaskId>,
    pub kind: String,
    pub priority: i64,
    pub payload_json: String,
    pub delivered_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTask {
    pub task_id: String,
    pub state: String,
    pub workspace: Option<WorkerWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAdmissionCursor {
    pub root_id: Option<RootSessionId>,
    pub parent_id: Option<TaskId>,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTaskDetail {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub depth: u8,
    pub state: String,
    pub delivery_json: String,
    pub workspace: Option<WorkerWorkspace>,
}

/// Active resource ownership which a destructive-control preview must expose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedActiveLease {
    pub lease_id: String,
    pub task_id: TaskId,
    pub resource_key: String,
}

/// A delivery not yet accepted by a direct-parent review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedUnmergedDelivery {
    pub delivery_id: String,
    pub task_id: TaskId,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedCancelScope {
    pub task_ids: Vec<TaskId>,
    pub active_leases: Vec<PersistedActiveLease>,
    pub unmerged_deliveries: Vec<PersistedUnmergedDelivery>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedRecoveredTask {
    pub session_id: RootSessionId,
    pub task_id: TaskId,
    pub parent_id: Option<TaskId>,
    pub depth: u8,
    pub attempt_id: AttemptId,
    pub attempt_number: u32,
    pub workspace_lease_id: Option<String>,
    pub worktree_lease: Option<String>,
    pub checkpoint_json: Option<String>,
    pub tool_state_json: String,
    pub objective: String,
    pub recovery_gated: bool,
    pub recovery_attested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedReviewHydrationTask {
    pub session_id: RootSessionId,
    pub task_id: TaskId,
    pub parent_id: Option<TaskId>,
    pub depth: u8,
    pub attempt_id: AttemptId,
    pub attempt_number: u32,
    pub state: String,
    pub delivery_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSubscriptionSnapshot {
    pub high_water_event_id: i64,
    pub cursor_state: RuntimeCursorState,
    pub tasks: Vec<PersistedTask>,
    pub events: Vec<PersistedEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeCursorState {
    Fresh,
    Replayable,
    Expired { oldest_replayable_event_id: i64 },
}

pub struct RuntimeRepository {
    connection: Connection,
}

impl RuntimeRepository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let connection = Connection::open(path)?;
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        migrate(&connection)?;
        Ok(Self { connection })
    }

    pub fn schema_version(&self) -> Result<i64, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    /// Persists a validated schedule without coupling it to an interactive
    /// session. Each later fire receives a new isolated root session.
    pub fn create_schedule(
        &mut self,
        definition: &ScheduleDefinition,
        next_run_at: DateTime<Local>,
    ) -> Result<PersistedSchedule, RepositoryError> {
        let schedule = PersistedSchedule {
            id: uuid::Uuid::new_v4().to_string(),
            definition: definition.clone(),
            state: "active".into(),
            next_run_at,
        };
        self.connection.execute(
            "INSERT INTO schedules (id, definition_json, state, next_run_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                schedule.id,
                serde_json::to_string(&schedule.definition)?,
                schedule.state,
                schedule.next_run_at.to_rfc3339(),
            ],
        )?;
        Ok(schedule)
    }

    pub fn schedules(&self) -> Result<Vec<PersistedSchedule>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, definition_json, state, next_run_at
             FROM schedules ORDER BY created_at, id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, definition_json, state, next_run_at) = row?;
                let definition = serde_json::from_str(&definition_json)?;
                let next_run_at = DateTime::parse_from_rfc3339(&next_run_at)
                    .map_err(|error| RepositoryError::UnknownEventKind {
                        kind: format!("invalid schedule next-run timestamp: {error}"),
                    })?
                    .with_timezone(&Local);
                Ok(PersistedSchedule {
                    id,
                    definition,
                    state,
                    next_run_at,
                })
            })
            .collect()
    }

    pub fn delete_schedule(&mut self, schedule_id: &str) -> Result<bool, RepositoryError> {
        let changed = self
            .connection
            .execute("DELETE FROM schedules WHERE id = ?1", [schedule_id])?;
        Ok(changed == 1)
    }

    /// Claims a due occurrence exactly once. A duplicate tick returns false.
    pub fn claim_schedule_occurrence(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
    ) -> Result<bool, RepositoryError> {
        let changed = self.connection.execute(
            "INSERT INTO schedule_occurrences (schedule_id, due_at, outcome)
             VALUES (?1, ?2, 'claimed')
             ON CONFLICT(schedule_id, due_at) DO NOTHING",
            params![schedule_id, due_at.to_rfc3339()],
        )?;
        Ok(changed == 1)
    }

    pub fn advance_schedule(
        &mut self,
        schedule_id: &str,
        next_run_at: DateTime<Local>,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
            params![next_run_at.to_rfc3339(), schedule_id],
        )?;
        Ok(())
    }

    pub fn schedule_has_active_instance(&self, schedule_id: &str) -> Result<bool, RepositoryError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                SELECT 1 FROM schedule_occurrences
                JOIN tasks ON tasks.root_session_id = schedule_occurrences.root_session_id
                WHERE schedule_occurrences.schedule_id = ?1
                  AND tasks.depth = 0
                  AND tasks.state_json NOT IN (
                    'completed', 'cancelled', 'failed', 'blocked', 'stalled',
                    'timed_out', 'budget_exhausted'
                  )
            )",
                [schedule_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(RepositoryError::from)
    }

    pub fn record_schedule_occurrence_outcome(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        outcome: &str,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE schedule_occurrences SET outcome = ?1
             WHERE schedule_id = ?2 AND due_at = ?3",
            params![outcome, schedule_id, due_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn attach_schedule_occurrence_root(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        root_session_id: &RootSessionId,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE schedule_occurrences
             SET outcome = 'fired', root_session_id = ?1
             WHERE schedule_id = ?2 AND due_at = ?3",
            params![
                root_session_id.to_string(),
                schedule_id,
                due_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn schedule_occurrence_outcomes(
        &self,
        schedule_id: &str,
    ) -> Result<Vec<String>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT outcome FROM schedule_occurrences
             WHERE schedule_id = ?1 ORDER BY due_at",
        )?;
        statement
            .query_map([schedule_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    /// Atomically records one occurrence and, when admitted, its isolated
    /// root. A failed root insert rolls the claim back with the transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_schedule_occurrence(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        next_run_at: DateTime<Local>,
        root: &RootSessionId,
        task: &TaskId,
        attempt: &AttemptId,
        definition: &ScheduleDefinition,
    ) -> Result<ScheduleOccurrenceResult, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let claimed = transaction.execute(
            "INSERT INTO schedule_occurrences (schedule_id, due_at, outcome)
             VALUES (?1, ?2, 'claimed') ON CONFLICT(schedule_id, due_at) DO NOTHING",
            params![schedule_id, due_at.to_rfc3339()],
        )?;
        if claimed == 0 {
            transaction.commit()?;
            return Ok(ScheduleOccurrenceResult::Duplicate);
        }
        let active_instance = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM schedule_occurrences
                JOIN tasks ON tasks.root_session_id = schedule_occurrences.root_session_id
                WHERE schedule_occurrences.schedule_id = ?1 AND tasks.depth = 0
                  AND tasks.state_json NOT IN (
                    'completed', 'completed_no_changes', 'cancelled', 'failed', 'blocked',
                    'stalled', 'timed_out', 'budget_exhausted'
                  )
            )",
            [schedule_id],
            |row| row.get::<_, bool>(0),
        )?;
        if active_instance {
            transaction.execute(
                "UPDATE schedule_occurrences SET outcome = 'skipped_overlap'
                 WHERE schedule_id = ?1 AND due_at = ?2",
                params![schedule_id, due_at.to_rfc3339()],
            )?;
            transaction.execute(
                "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
                params![next_run_at.to_rfc3339(), schedule_id],
            )?;
            transaction.commit()?;
            return Ok(ScheduleOccurrenceResult::SkippedOverlap);
        }
        let delivery_json = serde_json::to_string(&serde_json::json!({
            "objective": definition.objective,
            "schedule_policy": definition.policy,
        }))?;
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}')",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, 'queued', 1, ?3, ?4)",
            params![task.to_string(), root.to_string(), attempt.to_string(), delivery_json],
        )?;
        insert_attempt(&transaction, attempt, task, 1, "queued")?;
        let limits = WatchdogLimits {
            max_turns: Some(definition.policy.runtime.max_turns),
            max_wall_time_secs: Some(definition.policy.runtime.max_wall_time_secs),
            ..WatchdogLimits::default()
        };
        transaction.execute(
            "UPDATE attempt_watchdogs SET limits_json = ?1 WHERE attempt_id = ?2",
            params![serde_json::to_string(&limits)?, attempt.to_string()],
        )?;
        transaction.execute(
            "UPDATE schedule_occurrences SET outcome = 'fired', root_session_id = ?1
             WHERE schedule_id = ?2 AND due_at = ?3",
            params![root.to_string(), schedule_id, due_at.to_rfc3339()],
        )?;
        transaction.execute(
            "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
            params![next_run_at.to_rfc3339(), schedule_id],
        )?;
        transaction.commit()?;
        Ok(ScheduleOccurrenceResult::Fired)
    }

    /// Persists a non-firing occurrence and advances the schedule as one
    /// transaction, so an offline daemon cannot accumulate duplicate backlog.
    pub fn skip_schedule_occurrence(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        next_run_at: DateTime<Local>,
        outcome: &str,
    ) -> Result<bool, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let claimed = transaction.execute(
            "INSERT INTO schedule_occurrences (schedule_id, due_at, outcome)
             VALUES (?1, ?2, ?3) ON CONFLICT(schedule_id, due_at) DO NOTHING",
            params![schedule_id, due_at.to_rfc3339(), outcome],
        )?;
        if claimed != 0 {
            transaction.execute(
                "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
                params![next_run_at.to_rfc3339(), schedule_id],
            )?;
        }
        transaction.commit()?;
        Ok(claimed != 0)
    }

    pub fn has_table(&self, table: &str) -> Result<bool, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn save_admission_cursor(
        &mut self,
        resource_key: &str,
        root_id: Option<&RootSessionId>,
        parent_id: Option<&TaskId>,
        sequence: u64,
    ) -> Result<(), RepositoryError> {
        let sequence =
            i64::try_from(sequence).map_err(|_| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: "sequence exceeds SQLite integer range".into(),
            })?;
        self.connection.execute(
            "INSERT INTO resource_admission_cursors
             (resource_key, root_session_id, parent_task_id, sequence)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(resource_key) DO UPDATE SET
                 root_session_id = excluded.root_session_id,
                 parent_task_id = excluded.parent_task_id,
                 sequence = excluded.sequence,
                 updated_at = CURRENT_TIMESTAMP",
            params![
                resource_key,
                root_id.map(ToString::to_string),
                parent_id.map(ToString::to_string),
                sequence,
            ],
        )?;
        Ok(())
    }

    pub fn admission_cursor(
        &self,
        resource_key: &str,
    ) -> Result<Option<PersistedAdmissionCursor>, RepositoryError> {
        let row = self
            .connection
            .query_row(
                "SELECT root_session_id, parent_task_id, sequence
                 FROM resource_admission_cursors WHERE resource_key = ?1",
                [resource_key],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((root_id, parent_id, sequence)) = row else {
            return Ok(None);
        };
        let root_id = root_id
            .map(|value| RootSessionId::from_str(&value))
            .transpose()
            .map_err(|error| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: format!("invalid root session ID: {error}"),
            })?;
        let parent_id = parent_id
            .map(|value| TaskId::from_str(&value))
            .transpose()
            .map_err(|error| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: format!("invalid parent task ID: {error}"),
            })?;
        let sequence =
            u64::try_from(sequence).map_err(|_| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: "sequence is negative".into(),
            })?;
        Ok(Some(PersistedAdmissionCursor {
            root_id,
            parent_id,
            sequence,
        }))
    }

    /// Records a short process-local provider turn lease and its new fairness
    /// cursor in one transaction before the selected worker is notified.
    pub fn save_provider_turn_grant(
        &mut self,
        lease_id: &str,
        task: &TaskId,
        resource_key: &str,
        root_id: &RootSessionId,
        parent_id: &TaskId,
        sequence: u64,
    ) -> Result<(), RepositoryError> {
        let sequence =
            i64::try_from(sequence).map_err(|_| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: "sequence exceeds SQLite integer range".into(),
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES (?1, ?2, ?3, 'shared', 1, 'active')
             ON CONFLICT(id) DO UPDATE SET state = 'active', released_at = NULL",
            params![lease_id, task.to_string(), resource_key],
        )?;
        transaction.execute(
            "INSERT INTO resource_admission_cursors
             (resource_key, root_session_id, parent_task_id, sequence)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(resource_key) DO UPDATE SET
                 root_session_id = excluded.root_session_id,
                 parent_task_id = excluded.parent_task_id,
                 sequence = excluded.sequence,
                 updated_at = CURRENT_TIMESTAMP",
            params![
                resource_key,
                root_id.to_string(),
                parent_id.to_string(),
                sequence
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Idempotently closes one provider-turn lease without touching unrelated
    /// worker resources.
    pub fn release_provider_turn_lease(&mut self, lease_id: &str) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND state = 'active'",
            [lease_id],
        )?;
        Ok(())
    }

    /// Provider turns are process-local. Other leases retain their recovery
    /// semantics and must not be released merely because the daemon restarted.
    pub fn release_provider_turn_leases(&mut self) -> Result<usize, RepositoryError> {
        Ok(self.connection.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE state = 'active'
               AND (resource_key LIKE 'llm:%'
                    OR resource_key LIKE 'llm-coordination:%')",
            [],
        )?)
    }

    pub fn release_process_leases_for_task(
        &mut self,
        task: &TaskId,
    ) -> Result<usize, RepositoryError> {
        Ok(self.connection.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE task_id = ?1
               AND state = 'active'
               AND resource_key NOT LIKE 'workspace:%'
               AND resource_key NOT LIKE 'worktree:%'",
            params![task.to_string()],
        )?)
    }

    pub fn create_task(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        state: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        // The compatibility entry point creates an otherwise-empty session snapshot.
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}') ON CONFLICT(id) DO NOTHING",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, ?3, 1, '', '{}')",
            params![task.to_string(), root.to_string(), state],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Creates a task and its active attempt in one transaction. Runtime-owned
    /// tasks use this path so crash recovery never sees an empty attempt ID.
    pub fn create_task_with_attempt(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.create_task_with_attempt_and_objective(
            task,
            root,
            attempt,
            attempt_number,
            state,
            "Root session objective not specified.",
        )
    }

    pub fn create_task_with_attempt_and_objective(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
        objective: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}') ON CONFLICT(id) DO NOTHING",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, ?3, 1, ?4, ?5)",
            params![task.to_string(), root.to_string(), state, attempt.to_string(), delivery_json],
        )?;
        insert_attempt(&transaction, attempt, task, attempt_number, state)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_child_task(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        parent: &TaskId,
        depth: u8,
        state: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, '', '{}')",
            params![
                task.to_string(),
                root.to_string(),
                parent.to_string(),
                depth,
                state,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_child_task_with_attempt(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        parent: &TaskId,
        depth: u8,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.create_child_task_with_attempt_and_objective(
            task,
            root,
            parent,
            depth,
            attempt,
            attempt_number,
            state,
            "Complete the delegated task.",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_child_task_with_attempt_and_objective(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        parent: &TaskId,
        depth: u8,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
        objective: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
            params![
                task.to_string(),
                root.to_string(),
                parent.to_string(),
                depth,
                state,
                attempt.to_string(),
                delivery_json,
            ],
        )?;
        insert_attempt(&transaction, attempt, task, attempt_number, state)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn append_event(
        &mut self,
        task: &TaskId,
        event: RuntimeEvent,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_attempt(
        &mut self,
        attempt: &AttemptId,
        task: &TaskId,
        number: u32,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO attempts (id, task_id, number, state, budget_json, usage_json)
             VALUES (?1, ?2, ?3, ?4, '{}', '{}')",
            params![attempt.to_string(), task.to_string(), number, state],
        )?;
        Ok(())
    }

    /// Stores watchdog facts separately from recovery's tool-state payload.
    /// The upsert lets the daemon persist each meaningful progress or resource
    /// queue transition without creating another attempt.
    pub fn save_attempt_watchdog_snapshot(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        limits: &WatchdogLimits,
        usage: &WatchdogUsage,
        last_meaningful_event_id: Option<i64>,
        last_meaningful_at: DateTime<Utc>,
        current_wait: Option<&WatchdogResourceWait>,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE id = ?1 AND task_id = ?2)",
            params![attempt.to_string(), task.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "INSERT INTO attempt_watchdogs (
                attempt_id, task_id, limits_json, usage_json, last_meaningful_event_id,
                last_meaningful_at, resource_wait_key, resource_wait_started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(attempt_id) DO UPDATE SET
                limits_json = excluded.limits_json,
                usage_json = excluded.usage_json,
                last_meaningful_event_id = excluded.last_meaningful_event_id,
                last_meaningful_at = excluded.last_meaningful_at,
                resource_wait_key = excluded.resource_wait_key,
                resource_wait_started_at = excluded.resource_wait_started_at",
            params![
                attempt.to_string(),
                task.to_string(),
                serde_json::to_string(limits)?,
                serde_json::to_string(usage)?,
                last_meaningful_event_id,
                last_meaningful_at.to_rfc3339(),
                current_wait.map(|wait| wait.resource_key.as_str()),
                current_wait.map(|wait| wait.queued_at.to_rfc3339()),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn attempt_watchdog_snapshot(
        &self,
        attempt: &AttemptId,
    ) -> Result<Option<PersistedAttemptWatchdog>, RepositoryError> {
        let row = self
            .connection
            .query_row(
                "SELECT attempts.started_at, attempt_watchdogs.limits_json,
                        attempt_watchdogs.usage_json, attempt_watchdogs.last_meaningful_event_id,
                        attempt_watchdogs.last_meaningful_at,
                        attempt_watchdogs.resource_wait_key,
                        attempt_watchdogs.resource_wait_started_at
                 FROM attempt_watchdogs JOIN attempts ON attempts.id = attempt_watchdogs.attempt_id
                 WHERE attempt_watchdogs.attempt_id = ?1",
                params![attempt.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                started_at,
                limits_json,
                usage_json,
                last_event,
                last_meaningful_at,
                wait_key,
                wait_started_at,
            )| {
                let parse_time = |value: &str| {
                    DateTime::parse_from_rfc3339(value)
                        .map(|time| time.with_timezone(&Utc))
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                                .map(|time| time.and_utc())
                        })
                        .map_err(|error| RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt.to_string(),
                            reason: format!("invalid timestamp {value:?}: {error}"),
                        })
                };
                Ok(PersistedAttemptWatchdog {
                    limits: serde_json::from_str(&limits_json).map_err(|error| {
                        RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt.to_string(),
                            reason: format!("invalid limits JSON: {error}"),
                        }
                    })?,
                    observation: WatchdogObservation {
                        attempt_started_at: parse_time(&started_at)?,
                        last_meaningful_at: parse_time(&last_meaningful_at)?,
                        resource_wait_started_at: wait_started_at
                            .as_deref()
                            .map(parse_time)
                            .transpose()?,
                        usage: serde_json::from_str(&usage_json).map_err(|error| {
                            RepositoryError::InvalidWatchdogSnapshot {
                                attempt: attempt.to_string(),
                                reason: format!("invalid usage JSON: {error}"),
                            }
                        })?,
                    },
                    last_meaningful_event_id: last_event,
                    current_wait: match (wait_key, wait_started_at) {
                        (Some(resource_key), Some(queued_at)) => Some(WatchdogResourceWait {
                            resource_key,
                            queued_at: parse_time(&queued_at)?,
                        }),
                        (None, None) => None,
                        _ => {
                            return Err(RepositoryError::InvalidWatchdogSnapshot {
                                attempt: attempt.to_string(),
                                reason: "resource wait key and timestamp must both be present"
                                    .into(),
                            });
                        }
                    },
                })
            },
        )
        .transpose()
    }

    /// Atomically adds durable worker usage and, for a non-generated progress
    /// point, records the event ID used by the idle watchdog.
    pub fn record_watchdog_progress(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        turn_delta: u32,
        token_delta: u64,
        provider_retry_delta: u16,
        tool_retry_delta: u16,
        meaningful: bool,
        now: DateTime<Utc>,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (usage_json, exists): (String, bool) = transaction.query_row(
            "SELECT attempt_watchdogs.usage_json,
                    tasks.active_attempt_id = attempt_watchdogs.attempt_id
             FROM attempt_watchdogs JOIN tasks ON tasks.id = attempt_watchdogs.task_id
             WHERE attempt_watchdogs.task_id = ?1 AND attempt_watchdogs.attempt_id = ?2",
            params![task.to_string(), attempt.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !exists {
            return Ok(());
        }
        let mut usage: WatchdogUsage = serde_json::from_str(&usage_json).map_err(|error| {
            RepositoryError::InvalidWatchdogSnapshot {
                attempt: attempt.to_string(),
                reason: format!("invalid usage JSON: {error}"),
            }
        })?;
        usage.turns = usage.turns.saturating_add(turn_delta);
        usage.tokens = usage.tokens.saturating_add(token_delta);
        usage.provider_retries = usage.provider_retries.saturating_add(provider_retry_delta);
        usage.tool_retries = usage.tool_retries.saturating_add(tool_retry_delta);
        let progress_event_id = meaningful
            .then(|| append_event(&transaction, task, RuntimeEvent::TaskProgress))
            .transpose()?;
        transaction.execute(
            "UPDATE attempt_watchdogs
             SET usage_json = ?1,
                 last_meaningful_event_id = CASE WHEN ?2 THEN ?3 ELSE last_meaningful_event_id END,
                 last_meaningful_at = CASE WHEN ?2 THEN ?4 ELSE last_meaningful_at END
             WHERE task_id = ?5 AND attempt_id = ?6",
            params![
                serde_json::to_string(&usage)?,
                meaningful,
                progress_event_id,
                now.to_rfc3339(),
                task.to_string(),
                attempt.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns watchdog facts only for the currently active attempt of a task
    /// that remains eligible for a terminal watchdog decision.
    pub fn active_attempt_watchdogs(&self) -> Result<Vec<PersistedWatchdogTask>, RepositoryError> {
        let rows = {
            let mut statement = self.connection.prepare(
                "SELECT tasks.root_session_id, tasks.id, attempt_watchdogs.attempt_id
                 FROM attempt_watchdogs
                 JOIN tasks ON tasks.id = attempt_watchdogs.task_id
                 WHERE tasks.active_attempt_id = attempt_watchdogs.attempt_id
                   AND tasks.state_json IN (
                       'queued', 'running', 'waiting_for_resource',
                       'waiting_for_permission', 'waiting_for_children'
                   )",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        rows.into_iter()
            .map(|(session_id, task_id, attempt_id)| {
                let attempt_id =
                    attempt_id
                        .parse()
                        .map_err(|_| RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt_id.clone(),
                            reason: "invalid attempt ID".into(),
                        })?;
                let snapshot = self
                    .attempt_watchdog_snapshot(&attempt_id)?
                    .ok_or_else(|| RepositoryError::InvalidWatchdogSnapshot {
                        attempt: attempt_id.to_string(),
                        reason: "active watchdog snapshot disappeared".into(),
                    })?;
                Ok(PersistedWatchdogTask {
                    session_id: session_id.parse().map_err(|_| {
                        RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt_id.to_string(),
                            reason: "invalid root session ID".into(),
                        }
                    })?,
                    task_id: task_id.parse().map_err(|_| {
                        RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt_id.to_string(),
                            reason: "invalid task ID".into(),
                        }
                    })?,
                    attempt_id,
                    snapshot,
                })
            })
            .collect()
    }

    /// Test/support API for recording the durable facts a resumed worker must
    /// inspect before it can issue new side effects.
    pub fn record_recovery_context(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        workspace_lease: &str,
        worktree_lease: &str,
        checkpoint_json: &str,
        tool_state_json: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE tasks SET workspace_lease_id = ?1 WHERE id = ?2",
            params![workspace_lease, task.to_string()],
        )?;
        transaction.execute(
            "UPDATE attempts SET checkpoint_json = ?1, usage_json = ?2 WHERE id = ?3 AND task_id = ?4",
            params![checkpoint_json, tool_state_json, attempt.to_string(), task.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')",
            params![
                format!("recovery-worktree-{}", task),
                task.to_string(),
                worktree_lease
            ],
        )?;
        transaction.execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')",
            params![
                format!("recovery-workspace-{}", task),
                task.to_string(),
                workspace_lease
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persists a daemon-routed user instruction and its event together so a
    /// reconnecting observer never sees an event without its mailbox record.
    pub fn record_user_message(
        &mut self,
        sender: &TaskId,
        recipient: &TaskId,
        message: &str,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO mailbox_messages (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
             VALUES (?1, ?2, ?3, 'user_instruction', 2, ?4)",
            params![
                MessageId::new().to_string(),
                recipient.to_string(),
                sender.to_string(),
                serde_json::to_string(&serde_json::json!({ "message": message }))?,
            ],
        )?;
        let event_id = append_event(&transaction, recipient, RuntimeEvent::MailboxMessageQueued)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Persists an external user intervention without attributing it to a
    /// task. The mailbox row and audit event commit atomically.
    pub fn record_user_override_message(
        &mut self,
        recipient: &TaskId,
        message: &str,
    ) -> Result<i64, RepositoryError> {
        self.record_user_override_message_with_id(&MessageId::new(), recipient, message)
    }

    /// Uses the caller-supplied ID so the durable row and the in-memory
    /// worker inbox can acknowledge the exact same external override.
    pub fn record_user_override_message_with_id(
        &mut self,
        message_id: &MessageId,
        recipient: &TaskId,
        message: &str,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO mailbox_messages (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
            VALUES (?1, ?2, NULL, 'user_override', 2, ?3)",
            params![
                message_id.to_string(),
                recipient.to_string(),
                serde_json::to_string(&serde_json::json!({ "message": message }))?,
            ],
        )?;
        let event_id = append_event(&transaction, recipient, RuntimeEvent::MailboxMessageQueued)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Records the application worker's prompt-injection checkpoint. The
    /// row update and audit event commit atomically and are idempotent after a
    /// successful acknowledgement.
    pub fn mark_user_override_consumed(
        &mut self,
        recipient: &TaskId,
        message_id: &MessageId,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE mailbox_messages
             SET delivered_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND recipient_task_id = ?2
               AND sender_task_id IS NULL AND kind = 'user_override'
               AND delivered_at IS NULL",
            params![message_id.to_string(), recipient.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::MailboxMessageNotFound {
                message_id: message_id.to_string(),
            });
        }
        let event_id = append_event(
            &transaction,
            recipient,
            RuntimeEvent::MailboxMessageConsumed,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn mailbox_message_count(&self) -> Result<i64, RepositoryError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM mailbox_messages", [], |row| {
                row.get(0)
            })?)
    }

    /// Commits a child delivery, its review wait, and the direct parent's
    /// high-priority mailbox notification as one durable transaction.
    pub fn record_delivery_for_review(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        delivery: &DeliveryReport,
    ) -> Result<i64, RepositoryError> {
        let delivery_json = serde_json::to_string(delivery)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let parent_id = transaction
            .query_row(
                "SELECT parent_id FROM tasks WHERE id = ?1",
                [task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::TaskNotFound {
                task: task.to_string(),
            })?
            .ok_or_else(|| RepositoryError::DeliveryRequiresParent {
                task: task.to_string(),
            })?;
        let parent: TaskId = parent_id
            .parse()
            .map_err(|_| RepositoryError::TaskNotFound {
                task: parent_id.clone(),
            })?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET state_json = 'awaiting_parent_review', delivery_json = ?1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3 AND state_json = 'running'",
            params![delivery_json, task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotReadyForDelivery {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'awaiting_parent_review'
             WHERE id = ?1 AND task_id = ?2",
            params![attempt.to_string(), task.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO deliveries (id, task_id, attempt_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                delivery.id.to_string(),
                task.to_string(),
                attempt.to_string(),
                serde_json::to_string(delivery)?,
            ],
        )?;
        let message_id = MessageId::new();
        transaction.execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, correlation_id, payload_json)
             VALUES (?1, ?2, ?3, 'completed', 1, ?4, ?5)",
            params![
                message_id.to_string(),
                parent.to_string(),
                task.to_string(),
                attempt.to_string(),
                serde_json::to_string(delivery)?,
            ],
        )?;
        let event_id = append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::TaskDelivered,
            &serde_json::to_string(&serde_json::json!({ "delivery_id": delivery.id }))?,
        )?;
        append_event_with_payload(
            &transaction,
            &parent,
            RuntimeEvent::MailboxMessageQueued,
            &serde_json::to_string(&serde_json::json!({
                "message_id": message_id,
                "sender_task_id": task,
                "delivery_id": delivery.id,
            }))?,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Atomically records a direct-parent acceptance and completes the exact
    /// attempt whose delivery was reviewed.
    pub fn accept_delivery_review(
        &mut self,
        task: &TaskId,
        delivery: &DeliveryId,
        actor: &TaskId,
        integration: &IntegrationValidation,
        actor_json: &str,
    ) -> Result<i64, RepositoryError> {
        let actor_value = serde_json::from_str::<serde_json::Value>(actor_json)?;
        if !integration.succeeded || integration.evidence.trim().is_empty() {
            return Err(RepositoryError::IntegrationNotValidated);
        }
        let integration_json = serde_json::to_string(integration)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let review_target = transaction
            .query_row(
                "SELECT tasks.parent_id, tasks.active_attempt_id, deliveries.attempt_id
                 FROM deliveries
                 JOIN tasks ON tasks.id = deliveries.task_id
                 JOIN attempts ON attempts.id = deliveries.attempt_id
                 WHERE deliveries.id = ?1 AND deliveries.task_id = ?2
                   AND tasks.state_json = 'awaiting_parent_review'
                   AND tasks.delivery_json = deliveries.payload_json
                   AND attempts.task_id = tasks.id
                   AND attempts.state = 'awaiting_parent_review'",
                params![delivery.to_string(), task.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            })?;
        let Some(parent_id) = review_target.0 else {
            return Err(RepositoryError::DeliveryRequiresParent {
                task: task.to_string(),
            });
        };
        if parent_id != actor.to_string() {
            return Err(RepositoryError::ReviewActorMismatch {
                task: task.to_string(),
                actor: actor.to_string(),
            });
        }
        if review_target.1 != review_target.2 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'completed', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND active_attempt_id = ?2
               AND state_json = 'awaiting_parent_review'",
            params![task.to_string(), review_target.1],
        )?;
        if changed != 1 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        let changed = transaction.execute(
            "UPDATE attempts SET state = 'completed', ended_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND task_id = ?2 AND state = 'awaiting_parent_review'",
            params![review_target.2, task.to_string()],
        )?;
        if changed != 1 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        transaction.execute(
            "INSERT INTO reviews (id, delivery_id, actor_json, decision, evidence_json)
             VALUES (?1, ?2, ?3, 'accepted', ?4)",
            params![
                Uuid::new_v4().to_string(),
                delivery.to_string(),
                actor_json,
                integration_json,
            ],
        )?;
        let payload_json = serde_json::to_string(&serde_json::json!({
            "delivery_id": delivery,
            "actor_task_id": actor,
            "actor": actor_value,
            "decision": "accepted",
            "before_state": "awaiting_parent_review",
            "after_state": "completed",
            "integration": integration,
        }))?;
        let event_id = append_event_with_actor_payload(
            &transaction,
            task,
            RuntimeEvent::ReviewAccepted,
            actor_json,
            &payload_json,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Audits local-user approval and wakes the canonical direct parent without
    /// claiming that parent integration has happened.
    pub fn approve_delivery_review(
        &mut self,
        task: &TaskId,
        delivery: &DeliveryId,
        actor: &TaskId,
        actor_json: &str,
        parent_notification: &MessageId,
    ) -> Result<i64, RepositoryError> {
        let actor_value = serde_json::from_str::<serde_json::Value>(actor_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let parent_id = transaction
            .query_row(
                "SELECT tasks.parent_id
                 FROM deliveries
                 JOIN tasks ON tasks.id = deliveries.task_id
                 JOIN attempts ON attempts.id = deliveries.attempt_id
                 WHERE deliveries.id = ?1 AND deliveries.task_id = ?2
                   AND tasks.state_json = 'awaiting_parent_review'
                   AND tasks.delivery_json = deliveries.payload_json
                   AND tasks.active_attempt_id = deliveries.attempt_id
                   AND attempts.task_id = tasks.id
                   AND attempts.state = 'awaiting_parent_review'",
                params![delivery.to_string(), task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            })?
            .ok_or_else(|| RepositoryError::DeliveryRequiresParent {
                task: task.to_string(),
            })?;
        if parent_id != actor.to_string() {
            return Err(RepositoryError::ReviewActorMismatch {
                task: task.to_string(),
                actor: actor.to_string(),
            });
        }
        let evidence_json = serde_json::to_string(&serde_json::json!({
            "delivery_id": delivery,
            "user_approved": true,
            "integration_completed": false,
        }))?;
        transaction.execute(
            "INSERT INTO reviews (id, delivery_id, actor_json, decision, evidence_json)
             VALUES (?1, ?2, ?3, 'approved', ?4)",
            params![
                Uuid::new_v4().to_string(),
                delivery.to_string(),
                actor_json,
                evidence_json,
            ],
        )?;
        let event_id = append_event_with_actor_payload(
            &transaction,
            task,
            RuntimeEvent::ReviewApproved,
            actor_json,
            &serde_json::to_string(&serde_json::json!({
                "delivery_id": delivery,
                "actor_task_id": actor,
                "actor": actor_value,
                "decision": "approved",
                "state": "awaiting_parent_review",
                "integration_completed": false,
            }))?,
        )?;
        insert_review_parent_notification(
            &transaction,
            parent_notification,
            actor,
            task,
            delivery,
            "approved",
            actor_json,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Persists a rework decision, preserves the reviewed attempt, and starts
    /// its durable successor with the same feedback mailbox identity.
    #[allow(clippy::too_many_arguments)]
    pub fn rework_delivery_review(
        &mut self,
        task: &TaskId,
        delivery: &DeliveryId,
        actor: &TaskId,
        feedback: &str,
        feedback_message: &MessageId,
        successor_attempt: &AttemptId,
        successor_number: u32,
        actor_json: &str,
        parent_notification: &MessageId,
    ) -> Result<i64, RepositoryError> {
        let actor_value = serde_json::from_str::<serde_json::Value>(actor_json)?;
        if feedback.trim().is_empty() {
            return Err(RepositoryError::ReviewFeedbackRequired);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (parent_id, active_attempt, delivery_attempt) = transaction
            .query_row(
                "SELECT tasks.parent_id, tasks.active_attempt_id, deliveries.attempt_id
                 FROM deliveries
                 JOIN tasks ON tasks.id = deliveries.task_id
                 JOIN attempts ON attempts.id = deliveries.attempt_id
                 WHERE deliveries.id = ?1 AND deliveries.task_id = ?2
                   AND tasks.state_json = 'awaiting_parent_review'
                   AND tasks.delivery_json = deliveries.payload_json
                   AND attempts.task_id = tasks.id
                   AND attempts.state = 'awaiting_parent_review'",
                params![delivery.to_string(), task.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            })?;
        let parent_id = parent_id.ok_or_else(|| RepositoryError::DeliveryRequiresParent {
            task: task.to_string(),
        })?;
        if parent_id != actor.to_string() {
            return Err(RepositoryError::ReviewActorMismatch {
                task: task.to_string(),
                actor: actor.to_string(),
            });
        }
        if active_attempt != delivery_attempt {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'queued', active_attempt_id = ?1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3
               AND state_json = 'awaiting_parent_review'",
            params![
                successor_attempt.to_string(),
                task.to_string(),
                active_attempt
            ],
        )?;
        if changed != 1 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        let changed = transaction.execute(
            "UPDATE attempts SET state = 'rework_requested', ended_at = CURRENT_TIMESTAMP,
                 terminal_json = ?1
             WHERE id = ?2 AND task_id = ?3 AND state = 'awaiting_parent_review'",
            params![
                serde_json::to_string(&serde_json::json!({
                    "kind": "rework_requested",
                    "delivery_id": delivery,
                    "feedback_message_id": feedback_message,
                }))?,
                delivery_attempt,
                task.to_string(),
            ],
        )?;
        if changed != 1 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        insert_attempt(
            &transaction,
            successor_attempt,
            task,
            successor_number,
            "queued",
        )?;
        let evidence_json = serde_json::to_string(&serde_json::json!({
            "feedback": feedback,
            "feedback_message_id": feedback_message,
            "successor_attempt_id": successor_attempt,
        }))?;
        transaction.execute(
            "INSERT INTO reviews (id, delivery_id, actor_json, decision, evidence_json)
             VALUES (?1, ?2, ?3, 'rework', ?4)",
            params![
                Uuid::new_v4().to_string(),
                delivery.to_string(),
                actor_json,
                evidence_json,
            ],
        )?;
        transaction.execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, correlation_id, payload_json)
             VALUES (?1, ?2, ?3, 'rework', 1, ?4, ?5)",
            params![
                feedback_message.to_string(),
                task.to_string(),
                parent_id,
                successor_attempt.to_string(),
                evidence_json,
            ],
        )?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "delivery_id": delivery,
            "actor_task_id": actor,
            "actor": actor_value,
            "decision": "rework",
            "feedback": feedback,
            "feedback_message_id": feedback_message,
            "before_state": "awaiting_parent_review",
            "after_state": "queued",
            "successor_attempt_id": successor_attempt,
        }))?;
        let event_id = append_event_with_actor_payload(
            &transaction,
            task,
            RuntimeEvent::ReviewRework,
            actor_json,
            &event_payload,
        )?;
        append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::MailboxMessageQueued,
            &serde_json::to_string(&serde_json::json!({
                "message_id": feedback_message,
                "sender_task_id": actor,
                "kind": "rework",
            }))?,
        )?;
        insert_review_parent_notification(
            &transaction,
            parent_notification,
            actor,
            task,
            delivery,
            "rework",
            actor_json,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Persists a rejected delivery and its durable reason without discarding
    /// the reviewed delivery or attempt evidence.
    pub fn reject_delivery_review(
        &mut self,
        task: &TaskId,
        delivery: &DeliveryId,
        actor: &TaskId,
        reason: &str,
        reason_message: &MessageId,
        actor_json: &str,
        parent_notification: &MessageId,
    ) -> Result<i64, RepositoryError> {
        let actor_value = serde_json::from_str::<serde_json::Value>(actor_json)?;
        if reason.trim().is_empty() {
            return Err(RepositoryError::ReviewReasonRequired);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (parent_id, active_attempt, delivery_attempt) = transaction
            .query_row(
                "SELECT tasks.parent_id, tasks.active_attempt_id, deliveries.attempt_id
                 FROM deliveries
                 JOIN tasks ON tasks.id = deliveries.task_id
                 JOIN attempts ON attempts.id = deliveries.attempt_id
                 WHERE deliveries.id = ?1 AND deliveries.task_id = ?2
                   AND tasks.state_json = 'awaiting_parent_review'
                   AND tasks.delivery_json = deliveries.payload_json
                   AND attempts.task_id = tasks.id
                   AND attempts.state = 'awaiting_parent_review'",
                params![delivery.to_string(), task.to_string()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            })?;
        let parent_id = parent_id.ok_or_else(|| RepositoryError::DeliveryRequiresParent {
            task: task.to_string(),
        })?;
        if parent_id != actor.to_string() {
            return Err(RepositoryError::ReviewActorMismatch {
                task: task.to_string(),
                actor: actor.to_string(),
            });
        }
        if active_attempt != delivery_attempt {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'blocked', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND active_attempt_id = ?2
               AND state_json = 'awaiting_parent_review'",
            params![task.to_string(), active_attempt],
        )?;
        if changed != 1 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        let evidence_json = serde_json::to_string(&serde_json::json!({
            "reason": reason,
            "reason_message_id": reason_message,
        }))?;
        let changed = transaction.execute(
            "UPDATE attempts SET state = 'blocked', ended_at = CURRENT_TIMESTAMP,
                 terminal_json = ?1
             WHERE id = ?2 AND task_id = ?3 AND state = 'awaiting_parent_review'",
            params![evidence_json, delivery_attempt, task.to_string()],
        )?;
        if changed != 1 {
            return Err(RepositoryError::DeliveryNotAwaitingReview {
                task: task.to_string(),
                delivery: delivery.to_string(),
            });
        }
        transaction.execute(
            "INSERT INTO reviews (id, delivery_id, actor_json, decision, evidence_json)
             VALUES (?1, ?2, ?3, 'rejected', ?4)",
            params![
                Uuid::new_v4().to_string(),
                delivery.to_string(),
                actor_json,
                evidence_json,
            ],
        )?;
        transaction.execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, correlation_id, payload_json)
             VALUES (?1, ?2, ?3, 'review_rejected', 1, ?4, ?5)",
            params![
                reason_message.to_string(),
                task.to_string(),
                parent_id,
                delivery_attempt,
                evidence_json,
            ],
        )?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "delivery_id": delivery,
            "actor_task_id": actor,
            "actor": actor_value,
            "decision": "rejected",
            "reason": reason,
            "reason_message_id": reason_message,
            "before_state": "awaiting_parent_review",
            "after_state": "blocked",
        }))?;
        let event_id = append_event_with_actor_payload(
            &transaction,
            task,
            RuntimeEvent::ReviewRejected,
            actor_json,
            &event_payload,
        )?;
        append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::MailboxMessageQueued,
            &serde_json::to_string(&serde_json::json!({
                "message_id": reason_message,
                "sender_task_id": actor,
                "kind": "review_rejected",
            }))?,
        )?;
        insert_review_parent_notification(
            &transaction,
            parent_notification,
            actor,
            task,
            delivery,
            "rejected",
            actor_json,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn mailbox_message_delivered_at(
        &self,
        message_id: &MessageId,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT delivered_at FROM mailbox_messages WHERE id = ?1",
                params![message_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn mark_rework_messages_delivered(
        &mut self,
        task: &TaskId,
        message_ids: &[MessageId],
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for message_id in message_ids {
            let changed = transaction.execute(
                "UPDATE mailbox_messages SET delivered_at = CURRENT_TIMESTAMP
                 WHERE id = ?1 AND recipient_task_id = ?2 AND kind = 'rework'
                   AND delivered_at IS NULL",
                params![message_id.to_string(), task.to_string()],
            )?;
            if changed != 1 {
                return Err(RepositoryError::MailboxMessageNotFound {
                    message_id: message_id.to_string(),
                });
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns a task's durable mailbox in enqueue order without acknowledging it.
    pub fn mailbox_messages_for_task(
        &self,
        task: &TaskId,
    ) -> Result<Vec<PersistedMailboxMessage>, RepositoryError> {
        self.task_detail(task)?;
        let mut statement = self.connection.prepare(
            "SELECT id, recipient_task_id, sender_task_id, kind, priority, payload_json,
                    delivered_at, created_at
             FROM mailbox_messages
             WHERE recipient_task_id = ?1
             ORDER BY rowid",
        )?;
        statement
            .query_map(params![task.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .map(|row| {
                let (
                    message_id,
                    recipient_task_id,
                    sender_task_id,
                    kind,
                    priority,
                    payload_json,
                    delivered_at,
                    created_at,
                ) = row?;
                Ok(PersistedMailboxMessage {
                    message_id,
                    recipient_task_id: recipient_task_id.parse().map_err(|_| {
                        RepositoryError::UnknownEventKind {
                            kind: format!(
                                "invalid recipient task ID in store: {recipient_task_id}"
                            ),
                        }
                    })?,
                    sender_task_id: sender_task_id
                        .map(|sender| {
                            sender
                                .parse()
                                .map_err(|_| RepositoryError::UnknownEventKind {
                                    kind: format!("invalid sender task ID in store: {sender}"),
                                })
                        })
                        .transpose()?,
                    kind,
                    priority,
                    payload_json,
                    delivered_at,
                    created_at,
                })
            })
            .collect()
    }

    /// Atomically changes the task snapshot and appends its corresponding journal entry.
    pub fn transition_task(
        &mut self,
        task: &TaskId,
        state: &str,
        event: RuntimeEvent,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2",
            params![state, task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        let event_id = append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Keeps the task snapshot, active attempt, and audit event aligned.
    pub fn transition_task_and_attempt(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        state: &str,
        event: RuntimeEvent,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3",
            params![state, task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1, ended_at = CASE
                WHEN ?1 IN ('paused', 'blocked', 'cancelled', 'failed', 'recovery_required')
                THEN CURRENT_TIMESTAMP ELSE ended_at END
             WHERE id = ?2 AND task_id = ?3",
            params![state, attempt.to_string(), task.to_string()],
        )?;
        // A worker that has reached a terminal state no longer owns admission
        // resources, including failures reported after a factory accepted it.
        if matches!(state, "blocked" | "cancelled" | "failed") {
            transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE task_id = ?1 AND state = 'active'",
                params![task.to_string()],
            )?;
        }
        let event_id = append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Commits worker admission and restart evidence together, so a running
    /// attempt never becomes durable without a recoverable safety boundary.
    pub fn transition_task_and_attempt_with_recovery_context(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        state: &str,
        event: RuntimeEvent,
        context: &WorkerRecoveryContext,
    ) -> Result<i64, RepositoryError> {
        self.transition_task_and_attempt_with_recovery_context_and_resident_lease(
            task, attempt, state, event, context, None,
        )
    }

    pub fn transition_task_and_attempt_with_recovery_context_and_resident_lease(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        state: &str,
        event: RuntimeEvent,
        context: &WorkerRecoveryContext,
        resident_resource_key: Option<&str>,
    ) -> Result<i64, RepositoryError> {
        validate_recovery_context(context)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, workspace_lease_id = ?2, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?3 AND active_attempt_id = ?4",
            params![
                state,
                context.workspace_lease_id,
                task.to_string(),
                attempt.to_string()
            ],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1, checkpoint_json = ?2, usage_json = ?3,
                 ended_at = NULL, terminal_json = NULL
             WHERE id = ?4 AND task_id = ?5",
            params![
                state,
                context.checkpoint_json,
                context.tool_state_json,
                attempt.to_string(),
                task.to_string()
            ],
        )?;
        if let Some(worktree_lease) = &context.worktree_lease {
            transaction.execute(
                "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
                 VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')
                 ON CONFLICT(id) DO UPDATE SET resource_key = excluded.resource_key,
                     state = 'active', released_at = NULL",
                params![
                    format!("worker-worktree-context-{attempt}"),
                    task.to_string(),
                    worktree_lease,
                ],
            )?;
        }
        if let Some(workspace_lease) = &context.workspace_lease_id {
            transaction.execute(
                "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
                 VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')
                 ON CONFLICT(id) DO UPDATE SET resource_key = excluded.resource_key,
                     state = 'active', released_at = NULL",
                params![
                    format!("worker-workspace-context-{attempt}"),
                    task.to_string(),
                    workspace_lease,
                ],
            )?;
        }
        if let Some(resource_key) = resident_resource_key {
            transaction.execute(
                "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
                 VALUES (?1, ?2, ?3, 'shared', 1, 'active')
                 ON CONFLICT(id) DO UPDATE SET resource_key = excluded.resource_key,
                     state = 'active', released_at = NULL",
                params![
                    format!("worker-resident-{attempt}"),
                    task.to_string(),
                    resource_key,
                ],
            )?;
        }
        let event_id = append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn transition_task_and_attempt_with_terminal(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        state: &str,
        event: RuntimeEvent,
        terminal_json: &str,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3",
            params![state, task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1, ended_at = CURRENT_TIMESTAMP, terminal_json = ?2
             WHERE id = ?3 AND task_id = ?4",
            params![state, terminal_json, attempt.to_string(), task.to_string()],
        )?;
        transaction.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE task_id = ?1 AND state = 'active'",
            params![task.to_string()],
        )?;
        let event_id = append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Enters controlled recovery while releasing process-local leases and
    /// retaining workspace/worktree leases needed for explicit resume.
    pub fn transition_task_to_recovery_required_releasing_process_leases(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        terminal_json: &str,
    ) -> Result<i64, ControlledRecoveryTransitionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| ControlledRecoveryTransitionError {
                cleanup: None,
                recovery: Some(error.into()),
            })?;
        let cleanup_result = transaction
            .execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE task_id = ?1
                   AND state = 'active'
                   AND resource_key NOT LIKE 'workspace:%'
                   AND resource_key NOT LIKE 'worktree:%'",
                params![task.to_string()],
            )
            .map(|_| ())
            .map_err(RepositoryError::from);
        let recovery_result = (|| -> Result<i64, RepositoryError> {
            let changed = transaction.execute(
                "UPDATE tasks SET state_json = 'recovery_required', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1 AND active_attempt_id = ?2",
                params![task.to_string(), attempt.to_string()],
            )?;
            if changed == 0 {
                return Err(RepositoryError::TaskNotFound {
                    task: task.to_string(),
                });
            }
            transaction.execute(
                "UPDATE attempts SET state = 'recovery_required',
                     ended_at = CURRENT_TIMESTAMP, terminal_json = ?1
                 WHERE id = ?2 AND task_id = ?3",
                params![terminal_json, attempt.to_string(), task.to_string()],
            )?;
            append_event(&transaction, task, RuntimeEvent::TaskRecoveryRequired)
        })();
        let cleanup_error = cleanup_result.err();
        let (event_id, recovery_error) = match recovery_result {
            Ok(event_id) => (Some(event_id), None),
            Err(error) => (None, Some(error)),
        };
        if cleanup_error.is_some() || recovery_error.is_some() {
            drop(transaction);
            let cleanup_error = if cleanup_error.is_none() && recovery_error.is_some() {
                self.release_process_leases_for_task(task).err()
            } else {
                cleanup_error
            };
            return Err(ControlledRecoveryTransitionError {
                cleanup: cleanup_error,
                recovery: recovery_error,
            });
        }
        let event_id = event_id.expect("recovery result checked above");
        if let Err(error) = transaction.commit() {
            let cleanup = self.release_process_leases_for_task(task).err();
            return Err(ControlledRecoveryTransitionError {
                cleanup,
                recovery: Some(error.into()),
            });
        }
        Ok(event_id)
    }

    /// Commits a watchdog outcome exactly once for an active attempt. A later
    /// scan sees the terminal task state and becomes a no-op rather than
    /// duplicating the terminal audit event or lease release.
    pub fn record_watchdog_terminal(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        terminal: WatchdogTerminal,
        evidence: &WatchdogEvidence,
    ) -> Result<Option<i64>, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3
               AND state_json IN ('queued', 'running', 'waiting_for_resource',
                                  'waiting_for_permission', 'waiting_for_children')",
            params![terminal.state_name(), task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(None);
        }

        let terminal_json = terminal.terminal_json(evidence)?;
        transaction.execute(
            "UPDATE attempts SET state = ?1, ended_at = CURRENT_TIMESTAMP, terminal_json = ?2
             WHERE id = ?3 AND task_id = ?4",
            params![
                terminal.state_name(),
                terminal_json,
                attempt.to_string(),
                task.to_string()
            ],
        )?;
        transaction.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE task_id = ?1 AND state = 'active'",
            params![task.to_string()],
        )?;
        let event_id =
            append_event_with_payload(&transaction, task, terminal.event(), &terminal_json)?;
        transaction.commit()?;
        Ok(Some(event_id))
    }

    /// Activates a successor attempt while returning the task to the queue.
    /// The task's active-attempt pointer and attempt row must commit together
    /// before a worker may receive the new attempt ID.
    pub fn activate_successor_attempt(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
        event: RuntimeEvent,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let recovery_context = if state == "recovery_gated" {
            Some(transaction.query_row(
                "SELECT attempts.checkpoint_json, attempts.usage_json
                 FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
                 WHERE tasks.id = ?1",
                params![task.to_string()],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )?)
        } else {
            None
        };
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, active_attempt_id = ?2, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?3",
            params![state, attempt.to_string(), task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        insert_attempt(&transaction, attempt, task, attempt_number, state)?;
        if let Some((checkpoint_json, tool_state_json)) = recovery_context {
            transaction.execute(
                "UPDATE attempts SET checkpoint_json = ?1, usage_json = ?2 WHERE id = ?3",
                params![checkpoint_json, tool_state_json, attempt.to_string()],
            )?;
        }
        let event_id = append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Commits deterministic inspection evidence before a gated successor can
    /// enter ordinary worker admission.
    pub fn attest_recovery_gate(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        attestation: &WorkerRecoveryAttestation,
    ) -> Result<i64, RepositoryError> {
        serde_json::from_str::<serde_json::Value>(&attestation.checkpoint_json)?;
        serde_json::from_str::<serde_json::Value>(&attestation.tool_state_json)?;
        serde_json::from_str::<serde_json::Value>(&attestation.evidence_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'recovery_attested', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND active_attempt_id = ?2 AND state_json = 'recovery_gated'",
            params![task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'recovery_attested', checkpoint_json = ?1, usage_json = ?2
             WHERE id = ?3 AND task_id = ?4 AND state = 'recovery_gated'",
            params![
                attestation.checkpoint_json,
                attestation.tool_state_json,
                attempt.to_string(),
                task.to_string()
            ],
        )?;
        let event_id = append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::TaskRecoveryAttested,
            &attestation.evidence_json,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn recover_inflight_tasks(&mut self) -> Result<usize, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM tasks WHERE state_json IN ('running', 'waiting_for_resource', 'waiting_for_permission', 'waiting_for_children')",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        // v2 compatibility rows were allowed to have no active attempt. Give
        // each live task a closed recovery attempt so hydration can resume it.
        for task_id in &task_ids {
            let active_attempt: String = transaction.query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                params![task_id],
                |row| row.get(0),
            )?;
            if active_attempt.is_empty() {
                let task: TaskId =
                    task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?;
                let attempt = AttemptId::new();
                let number: u32 = transaction.query_row(
                    "SELECT COALESCE(MAX(number), 0) + 1 FROM attempts WHERE task_id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "UPDATE tasks SET active_attempt_id = ?1 WHERE id = ?2",
                    params![attempt.to_string(), task_id],
                )?;
                insert_attempt(&transaction, &attempt, &task, number, "running")?;
            }
        }
        let recovered_attempts: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM attempts WHERE state IN ('running', 'waiting_for_resource', 'waiting_for_permission', 'waiting_for_children')",
            [], |row| row.get(0),
        )?;
        for task_id in &task_ids {
            transaction.execute(
                "UPDATE tasks SET state_json = 'recovery_required', updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                params![task_id],
            )?;
            let task = task_id
                .parse()
                .map_err(|_| RepositoryError::UnknownEventKind {
                    kind: format!("invalid task ID in store: {task_id}"),
                })?;
            append_event(&transaction, &task, RuntimeEvent::TaskRecoveryRequired)?;
        }
        // Events are task-addressable on the v1 IPC surface. One recovered
        // task carries the daemon-level recovery marker so subscribers still
        // observe a single, durable restart boundary without a protocol break.
        if let Some(task_id) = task_ids.first() {
            let task = task_id
                .parse()
                .map_err(|_| RepositoryError::UnknownEventKind {
                    kind: format!("invalid task ID in store: {task_id}"),
                })?;
            let released_process_leases = transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE state = 'active'
                   AND resource_key NOT LIKE 'worktree:%'
                   AND resource_key NOT LIKE 'workspace:%'",
                [],
            )?;
            let retained_workspace_worktree_leases: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM resource_leases WHERE state = 'active'
                 AND (resource_key LIKE 'worktree:%' OR resource_key LIKE 'workspace:%')",
                [],
                |row| row.get(0),
            )?;
            let payload = serde_json::to_string(&serde_json::json!({
                "recovered_tasks": task_ids.len(),
                "recovered_attempts": recovered_attempts,
                "released_process_leases": released_process_leases,
                "retained_workspace_worktree_leases": retained_workspace_worktree_leases,
            }))?;
            append_event_with_payload(
                &transaction,
                &task,
                RuntimeEvent::RuntimeRecovered,
                &payload,
            )?;
        }
        if task_ids.is_empty() {
            transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE state = 'active'
                   AND resource_key NOT LIKE 'worktree:%'
                   AND resource_key NOT LIKE 'workspace:%'",
                [],
            )?;
        }
        transaction.execute(
            "UPDATE attempts SET state = 'recovery_required', ended_at = CURRENT_TIMESTAMP
             WHERE state IN ('running', 'waiting_for_resource', 'waiting_for_permission', 'waiting_for_children')",
            [],
        )?;
        transaction.commit()?;
        Ok(task_ids.len())
    }

    /// Persists a permission wait with the immutable request payload before an
    /// operator can see or resolve it.
    pub fn request_permission(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        request: &PermissionRequestId,
        payload_json: &str,
    ) -> Result<i64, RepositoryError> {
        serde_json::from_str::<serde_json::Value>(payload_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'waiting_for_permission', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND active_attempt_id = ?2 AND state_json = 'running'",
            params![task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'waiting_for_permission'
             WHERE id = ?1 AND task_id = ?2",
            params![attempt.to_string(), task.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO permission_requests (id, task_id, payload_json, state)
             VALUES (?1, ?2, ?3, 'pending')",
            params![request.to_string(), task.to_string(), payload_json],
        )?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "request_id": request,
            "request": serde_json::from_str::<serde_json::Value>(payload_json)?,
        }))?;
        let event_id = append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::PermissionRequested,
            &event_payload,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Resolves exactly one pending request and atomically aligns its request
    /// record, task/attempt snapshot, and actor-attributed audit event.
    pub fn resolve_permission(
        &mut self,
        request: &PermissionRequestId,
        decision: PermissionDecision,
        actor_json: &str,
    ) -> Result<i64, RepositoryError> {
        serde_json::from_str::<serde_json::Value>(actor_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task_id = transaction
            .query_row(
                "SELECT task_id FROM permission_requests WHERE id = ?1 AND state = 'pending'",
                [request.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::PermissionRequestNotPending {
                request: request.to_string(),
            })?;
        let task: TaskId = task_id.parse().map_err(|_| RepositoryError::TaskNotFound {
            task: task_id.clone(),
        })?;
        let (request_state, task_state) = match decision {
            PermissionDecision::Allow => ("allowed", "queued"),
            PermissionDecision::Deny => ("denied", "blocked"),
        };
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND state_json = 'waiting_for_permission'",
            params![task_state, task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::PermissionRequestNotPending {
                request: request.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1, ended_at = CASE WHEN ?1 = 'blocked'
                THEN CURRENT_TIMESTAMP ELSE ended_at END
             WHERE id = (SELECT active_attempt_id FROM tasks WHERE id = ?2) AND task_id = ?2",
            params![task_state, task.to_string()],
        )?;
        transaction.execute(
            "UPDATE permission_requests SET state = ?1, decided_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND state = 'pending'",
            params![request_state, request.to_string()],
        )?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "request_id": request,
            "decision": decision,
            "actor": serde_json::from_str::<serde_json::Value>(actor_json)?,
        }))?;
        let event_id = append_event_with_actor_payload(
            &transaction,
            &task,
            RuntimeEvent::PermissionResolved,
            actor_json,
            &event_payload,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn permission_request_state(
        &self,
        request: &PermissionRequestId,
    ) -> Result<String, RepositoryError> {
        self.connection
            .query_row(
                "SELECT state FROM permission_requests WHERE id = ?1",
                [request.to_string()],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    /// Locates the task owning a still-pending request without accepting an
    /// actor- or client-supplied task ID at the daemon boundary.
    pub fn pending_permission_task(
        &self,
        request: &PermissionRequestId,
    ) -> Result<TaskId, RepositoryError> {
        let (task_id, state) = self
            .connection
            .query_row(
                "SELECT task_id, state FROM permission_requests WHERE id = ?1",
                [request.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::PermissionRequestNotFound {
                request: request.to_string(),
            })?;
        if state != "pending" {
            return Err(RepositoryError::PermissionRequestNotPending {
                request: request.to_string(),
            });
        }
        task_id
            .parse()
            .map_err(|_| RepositoryError::TaskNotFound { task: task_id })
    }

    pub fn task_state(&self, task: &TaskId) -> Result<String, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT state_json FROM tasks WHERE id = ?1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn active_attempt_id(&self, task: &TaskId) -> Result<AttemptId, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::TaskNotFound {
                task: task.to_string(),
            })?;
        value
            .parse()
            .map_err(|_| RepositoryError::InvalidWatchdogSnapshot {
                attempt: value,
                reason: "task has an invalid active attempt ID".into(),
            })
    }

    pub fn application_root_attachment(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<ApplicationRootAttachment>, RepositoryError> {
        self.connection
            .query_row(
                "SELECT idempotency_key, root_session_id, root_task_id, capability_digest, state
                 FROM application_root_attachments WHERE idempotency_key = ?1",
                params![idempotency_key],
                |row| {
                    Ok(ApplicationRootAttachment {
                        idempotency_key: row.get(0)?,
                        root_session_id: row.get::<_, String>(1)?.parse().map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                1,
                                "root_session_id".into(),
                                rusqlite::types::Type::Text,
                            )
                        })?,
                        root_task_id: row.get::<_, String>(2)?.parse().map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                2,
                                "root_task_id".into(),
                                rusqlite::types::Type::Text,
                            )
                        })?,
                        capability_digest: row.get(3)?,
                        state: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(RepositoryError::from)
    }

    pub fn record_application_root_attachment(
        &mut self,
        idempotency_key: &str,
        root_session_id: &RootSessionId,
        root_task_id: &TaskId,
        capability_digest: &str,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO application_root_attachments
                (idempotency_key, root_session_id, root_task_id, capability_digest, state)
             VALUES (?1, ?2, ?3, ?4, 'attached')",
            params![
                idempotency_key,
                root_session_id.to_string(),
                root_task_id.to_string(),
                capability_digest
            ],
        )?;
        Ok(())
    }

    pub fn detach_application_root(
        &mut self,
        root_session_id: &RootSessionId,
        root_task_id: &TaskId,
    ) -> Result<(), RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE application_root_attachments
             SET state = 'detached', detached_at = CURRENT_TIMESTAMP
             WHERE root_session_id = ?1 AND root_task_id = ?2 AND state = 'attached'",
            params![root_session_id.to_string(), root_task_id.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: root_task_id.to_string(),
            });
        }
        Ok(())
    }

    pub fn update_task_objective(
        &mut self,
        task: &TaskId,
        objective: &str,
    ) -> Result<(), RepositoryError> {
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        let changed = self.connection.execute(
            "UPDATE tasks SET delivery_json = ?1 WHERE id = ?2",
            params![delivery_json, task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        Ok(())
    }

    pub fn application_root_capability_matches(
        &self,
        root_session_id: &RootSessionId,
        root_task_id: &TaskId,
        capability_digest: &str,
    ) -> Result<bool, RepositoryError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM application_root_attachments
                    WHERE root_session_id = ?1
                      AND root_task_id = ?2
                      AND capability_digest = ?3
                      AND state = 'attached'
                 )",
                params![
                    root_session_id.to_string(),
                    root_task_id.to_string(),
                    capability_digest
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(RepositoryError::from)
    }

    pub fn record_task_workspace(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        workspace: &WorkerWorkspace,
    ) -> Result<(), RepositoryError> {
        validate_task_workspace(task, workspace)?;
        let repository_root =
            workspace_path_str(task, "repository_root", &workspace.repository_root)?;
        let path = workspace_path_str(task, "path", &workspace.path)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let active_attempt_matches = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1
                FROM tasks
                JOIN attempts ON attempts.id = ?2 AND attempts.task_id = tasks.id
                WHERE tasks.id = ?1 AND tasks.active_attempt_id = ?2
             )",
            params![task.to_string(), attempt.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if !active_attempt_matches {
            return Err(RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: "workspace attempt is not the task's active attempt".into(),
            });
        }

        let existing = transaction
            .query_row(
                "SELECT attempt_id, lease_id, repository_root, path, branch, parent_branch, base_commit
                 FROM task_workspaces WHERE task_id = ?1",
                params![task.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        if let Some((
            existing_attempt,
            lease_id,
            repository_root,
            path,
            branch,
            parent_branch,
            base_commit,
        )) = existing
        {
            let existing_workspace = persisted_task_workspace(
                task,
                lease_id,
                repository_root,
                path,
                branch,
                parent_branch,
                base_commit,
            )?;
            if existing_attempt == attempt.to_string() && existing_workspace == *workspace {
                transaction.commit()?;
                return Ok(());
            }
            return Err(RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: "workspace assignment conflicts with an existing task workspace".into(),
            });
        }

        let conflicting_task = transaction
            .query_row(
                "SELECT task_id FROM task_workspaces
                 WHERE lease_id = ?1 OR path = ?2 OR (repository_root = ?3 AND branch = ?4)
                 LIMIT 1",
                params![
                    workspace.lease_id.to_string(),
                    path,
                    repository_root,
                    workspace.branch.as_str(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(conflicting_task) = conflicting_task {
            return Err(RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: format!("workspace assignment conflicts with task {conflicting_task}"),
            });
        }

        transaction
            .execute(
                "INSERT INTO task_workspaces (
                task_id, attempt_id, lease_id, repository_root, path,
                branch, parent_branch, base_commit
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task.to_string(),
                    attempt.to_string(),
                    workspace.lease_id.to_string(),
                    repository_root,
                    path,
                    workspace.branch.as_str(),
                    workspace.parent_branch.as_str(),
                    workspace.base_commit.as_str(),
                ],
            )
            .map_err(|error| task_workspace_insert_error(task, error))?;
        let updated = transaction.execute(
            "UPDATE tasks SET workspace_lease_id = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3",
            params![
                workspace.lease_id.to_string(),
                task.to_string(),
                attempt.to_string(),
            ],
        )?;
        if updated == 0 {
            return Err(RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: "workspace attempt is no longer active".into(),
            });
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn task_workspace(&self, task: &TaskId) -> Result<WorkerWorkspace, RepositoryError> {
        self.task_workspace_optional(task)?
            .ok_or_else(|| RepositoryError::TaskWorkspaceNotFound {
                task: task.to_string(),
            })
    }

    pub fn task_workspace_optional(
        &self,
        task: &TaskId,
    ) -> Result<Option<WorkerWorkspace>, RepositoryError> {
        let row = self
            .connection
            .query_row(
                "SELECT lease_id, repository_root, path, branch, parent_branch, base_commit
                 FROM task_workspaces WHERE task_id = ?1",
                params![task.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        if let Some((lease_id, repository_root, path, branch, parent_branch, base_commit)) = row {
            return persisted_task_workspace(
                task,
                lease_id,
                repository_root,
                path,
                branch,
                parent_branch,
                base_commit,
            )
            .map(Some);
        }
        if self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
            params![task.to_string()],
            |row| row.get::<_, bool>(0),
        )? {
            Ok(None)
        } else {
            Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            })
        }
    }

    pub fn attempt_state(&self, attempt: &AttemptId) -> Result<String, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT state FROM attempts WHERE id = ?1",
            params![attempt.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn has_active_lease_prefix(
        &self,
        task: &TaskId,
        prefix: &str,
    ) -> Result<bool, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_leases
               WHERE task_id = ?1 AND state = 'active' AND resource_key LIKE ?2)",
            params![task.to_string(), format!("{prefix}%")],
            |row| row.get(0),
        )?)
    }

    pub fn attempt_ended_at(&self, attempt: &AttemptId) -> Result<Option<String>, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT ended_at FROM attempts WHERE id = ?1",
            params![attempt.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn attempt_terminal_json_for_task(
        &self,
        task: &TaskId,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT terminal_json FROM attempts WHERE task_id = ?1 ORDER BY number DESC LIMIT 1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn events_after(&self, cursor: i64) -> Result<Vec<RuntimeEvent>, RepositoryError> {
        Ok(self
            .event_records_after(cursor)?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    pub fn latest_event_id(&self) -> Result<i64, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn task_snapshots(&self) -> Result<Vec<PersistedTask>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.id, tasks.state_json, task_workspaces.lease_id,
                    task_workspaces.repository_root, task_workspaces.path,
                    task_workspaces.branch, task_workspaces.parent_branch,
                    task_workspaces.base_commit
             FROM tasks
             LEFT JOIN task_workspaces ON task_workspaces.task_id = tasks.id
             ORDER BY tasks.created_at, tasks.id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?
            .map(|row| {
                let (task_id, state, lease_id, root, path, branch, parent_branch, base_commit) =
                    row?;
                let task = task_id
                    .parse()
                    .map_err(|_| RepositoryError::InvalidTaskWorkspace {
                        task: task_id.clone(),
                        reason: "task ID in workspace snapshot is invalid".into(),
                    })?;
                Ok(PersistedTask {
                    task_id,
                    state,
                    workspace: optional_persisted_task_workspace(
                        &task,
                        lease_id,
                        root,
                        path,
                        branch,
                        parent_branch,
                        base_commit,
                    )?,
                })
            })
            .collect()
    }

    pub fn queued_task_count(&self) -> Result<usize, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM tasks WHERE state_json = 'queued' AND parent_id IS NOT NULL",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn recovered_tasks(&self) -> Result<Vec<PersistedRecoveredTask>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.root_session_id, tasks.id, tasks.parent_id, tasks.depth, attempts.id, attempts.number,
                    tasks.workspace_lease_id,
                    (SELECT resource_key FROM resource_leases
                     WHERE task_id = tasks.id AND state = 'active' AND resource_key LIKE 'worktree:%'
                     ORDER BY acquired_at DESC, id DESC LIMIT 1),
                    attempts.checkpoint_json, attempts.usage_json, tasks.state_json, tasks.delivery_json
             FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.state_json IN ('recovery_required', 'recovery_gated', 'recovery_attested')
             ORDER BY tasks.root_session_id, tasks.depth, tasks.created_at, tasks.id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u8>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                ))
            })?
            .map(|row| {
                let (
                    session_id,
                    task_id,
                    parent_id,
                    depth,
                    attempt_id,
                    attempt_number,
                    workspace_lease_id,
                    worktree_lease,
                    checkpoint_json,
                    tool_state_json,
                    task_state,
                    delivery_json,
                ) = row?;
                Ok(PersistedRecoveredTask {
                    session_id: session_id.parse().map_err(|_| {
                        RepositoryError::UnknownEventKind {
                            kind: format!("invalid session ID in store: {session_id}"),
                        }
                    })?,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    parent_id: parent_id
                        .map(|parent_id| {
                            parent_id
                                .parse()
                                .map_err(|_| RepositoryError::UnknownEventKind {
                                    kind: format!("invalid parent task ID in store: {parent_id}"),
                                })
                        })
                        .transpose()?,
                    depth,
                    attempt_id: attempt_id.parse().map_err(|_| {
                        RepositoryError::UnknownEventKind {
                            kind: format!("invalid attempt ID in store: {attempt_id}"),
                        }
                    })?,
                    attempt_number,
                    workspace_lease_id,
                    worktree_lease,
                    checkpoint_json,
                    tool_state_json,
                    objective: serde_json::from_str::<serde_json::Value>(&delivery_json)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("objective")
                                .and_then(|value| value.as_str())
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| {
                            "Recover safely: inspect the recorded worktree before changes.".into()
                        }),
                    recovery_gated: task_state == "recovery_gated",
                    recovery_attested: task_state == "recovery_attested",
                })
            })
            .collect()
    }

    /// Returns complete task trees for sessions that own durable review mail.
    /// Tree order lets the runtime rebuild parents before children.
    pub fn review_hydration_tasks(
        &self,
    ) -> Result<Vec<PersistedReviewHydrationTask>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.root_session_id, tasks.id, tasks.parent_id, tasks.depth,
                    tasks.active_attempt_id, attempts.number, tasks.state_json,
                    tasks.delivery_json
             FROM tasks
             JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.root_session_id IN (
                 SELECT DISTINCT recipient.root_session_id
                 FROM mailbox_messages
                 JOIN tasks AS recipient ON recipient.id = mailbox_messages.recipient_task_id
                 WHERE mailbox_messages.kind IN ('rework', 'review_rejected')
                    OR (mailbox_messages.kind = 'user_override'
                        AND json_extract(mailbox_messages.payload_json, '$.kind') = 'user_review_override')
             )
             ORDER BY tasks.root_session_id, tasks.depth, tasks.created_at, tasks.id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u8>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .map(|row| {
                let (session, task, parent, depth, attempt, number, state, delivery_json) = row?;
                Ok(PersistedReviewHydrationTask {
                    session_id: session
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid review session ID in store: {session}"),
                        })?,
                    task_id: task
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid review task ID in store: {task}"),
                        })?,
                    parent_id: parent
                        .map(|value| {
                            value
                                .parse()
                                .map_err(|_| RepositoryError::UnknownEventKind {
                                    kind: format!("invalid review parent ID in store: {value}"),
                                })
                        })
                        .transpose()?,
                    depth,
                    attempt_id: attempt
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid review attempt ID in store: {attempt}"),
                        })?,
                    attempt_number: number,
                    state,
                    delivery_json,
                })
            })
            .collect()
    }

    pub fn task_detail(&self, task: &TaskId) -> Result<PersistedTaskDetail, RepositoryError> {
        let detail = self
            .connection
            .query_row(
                "SELECT id, root_session_id, parent_id, depth, state_json, delivery_json
                 FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| {
                    Ok(PersistedTaskDetail {
                        task_id: row.get(0)?,
                        session_id: row.get(1)?,
                        parent_task_id: row.get(2)?,
                        depth: row.get(3)?,
                        state: row.get(4)?,
                        delivery_json: row.get(5)?,
                        workspace: None,
                    })
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => RepositoryError::TaskNotFound {
                    task: task.to_string(),
                },
                error => RepositoryError::Sql(error),
            })?;
        Ok(PersistedTaskDetail {
            workspace: self.task_workspace_optional(task)?,
            ..detail
        })
    }

    /// Returns a task and, when requested, all descendants in stable tree order.
    pub fn task_tree_ids(
        &self,
        task: &TaskId,
        recursive: bool,
    ) -> Result<Vec<TaskId>, RepositoryError> {
        self.task_detail(task)?;
        let mut statement = self.connection.prepare(
            "WITH RECURSIVE affected(id, tree_depth) AS (
                SELECT id, 0 FROM tasks WHERE id = ?1
                UNION ALL
                SELECT tasks.id, affected.tree_depth + 1
                FROM tasks JOIN affected ON tasks.parent_id = affected.id
                WHERE ?2
             )
             SELECT id FROM affected ORDER BY tree_depth, id",
        )?;
        statement
            .query_map(params![task.to_string(), recursive], |row| {
                row.get::<_, String>(0)
            })?
            .map(|row| {
                let task_id = row?;
                task_id
                    .parse()
                    .map_err(|_| RepositoryError::UnknownEventKind {
                        kind: format!("invalid task ID in store: {task_id}"),
                    })
            })
            .collect()
    }

    /// Resolves every durable effect a cancellation confirmation describes.
    /// The stable ordering makes it safe to bind the preview to its action.
    pub fn cancel_scope(
        &self,
        task: &TaskId,
        recursive: bool,
    ) -> Result<PersistedCancelScope, RepositoryError> {
        let task_ids = self.task_tree_ids(task, recursive)?;
        let mut active_leases = Vec::new();
        let mut unmerged_deliveries = Vec::new();
        for task_id in &task_ids {
            let mut leases = self.connection.prepare(
                "SELECT id, resource_key FROM resource_leases
                 WHERE task_id = ?1 AND state = 'active' ORDER BY resource_key, id",
            )?;
            active_leases.extend(
                leases
                    .query_map([task_id.to_string()], |row| {
                        Ok(PersistedActiveLease {
                            lease_id: row.get(0)?,
                            task_id: task_id.clone(),
                            resource_key: row.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let mut deliveries = self.connection.prepare(
                "SELECT deliveries.id, deliveries.payload_json FROM deliveries
                 WHERE deliveries.task_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM reviews
                       WHERE reviews.delivery_id = deliveries.id AND reviews.decision = 'accepted'
                   )
                 ORDER BY deliveries.created_at, deliveries.id",
            )?;
            unmerged_deliveries.extend(
                deliveries
                    .query_map([task_id.to_string()], |row| {
                        Ok(PersistedUnmergedDelivery {
                            delivery_id: row.get(0)?,
                            task_id: task_id.clone(),
                            payload_json: row.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(PersistedCancelScope {
            task_ids,
            active_leases,
            unmerged_deliveries,
        })
    }

    /// Read task snapshots, a high-water mark, and replay in one SQLite read transaction.
    pub fn subscription_snapshot(
        &mut self,
        after_event_id: i64,
    ) -> Result<RuntimeSubscriptionSnapshot, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let high_water_event_id: i64 = transaction.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
            [],
            |row| row.get(0),
        )?;
        let oldest_replayable_event_id: i64 = transaction.query_row(
            "SELECT value FROM runtime_metadata WHERE key = 'event_replay_floor'",
            [],
            |row| row.get(0),
        )?;
        let cursor_state = if after_event_id < oldest_replayable_event_id.saturating_sub(1)
            || after_event_id > high_water_event_id
        {
            RuntimeCursorState::Expired {
                oldest_replayable_event_id,
            }
        } else if after_event_id == 0 {
            RuntimeCursorState::Fresh
        } else {
            RuntimeCursorState::Replayable
        };
        let tasks = {
            let mut statement = transaction.prepare(
                "SELECT tasks.id, tasks.state_json, task_workspaces.lease_id,
                            task_workspaces.repository_root, task_workspaces.path,
                            task_workspaces.branch, task_workspaces.parent_branch,
                            task_workspaces.base_commit
                     FROM tasks
                     LEFT JOIN task_workspaces ON task_workspaces.task_id = tasks.id
                     ORDER BY tasks.created_at, tasks.id",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                })?
                .map(|row| {
                    let (
                        task_id,
                        state,
                        lease_id,
                        repository_root,
                        path,
                        branch,
                        parent_branch,
                        base_commit,
                    ) = row?;
                    let task =
                        task_id
                            .parse()
                            .map_err(|_| RepositoryError::InvalidTaskWorkspace {
                                task: task_id.clone(),
                                reason: "task ID in workspace snapshot is invalid".into(),
                            })?;
                    Ok(PersistedTask {
                        task_id,
                        state,
                        workspace: optional_persisted_task_workspace(
                            &task,
                            lease_id,
                            repository_root,
                            path,
                            branch,
                            parent_branch,
                            base_commit,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?
        };
        let events = if matches!(cursor_state, RuntimeCursorState::Replayable) {
            let mut statement = transaction.prepare(
                "SELECT id, task_id, kind, payload_json FROM events WHERE id > ?1 AND id <= ?2 ORDER BY id",
            )?;
            statement
                .query_map(params![after_event_id, high_water_event_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .map(|row| {
                    let (id, task_id, kind, payload_json) = row?;
                    Ok(PersistedEvent {
                        id,
                        task_id: task_id.parse().map_err(|_| {
                            RepositoryError::UnknownEventKind {
                                kind: format!("invalid task ID in store: {task_id}"),
                            }
                        })?,
                        event: RuntimeEvent::parse(kind)?,
                        payload_json,
                    })
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?
        } else {
            Vec::new()
        };
        transaction.commit()?;
        Ok(RuntimeSubscriptionSnapshot {
            high_water_event_id,
            cursor_state,
            tasks,
            events,
        })
    }

    /// Advances the replay window without deleting the append-only audit journal.
    pub fn advance_event_replay_floor_through(
        &mut self,
        event_id: i64,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let high_water_event_id: i64 = transaction.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
            [],
            |row| row.get(0),
        )?;
        let requested_floor = event_id
            .saturating_add(1)
            .clamp(1, high_water_event_id.saturating_add(1));
        transaction.execute(
            "UPDATE runtime_metadata
             SET value = MAX(value, ?1)
             WHERE key = 'event_replay_floor'",
            params![requested_floor],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn event_records_after(&self, cursor: i64) -> Result<Vec<PersistedEvent>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, task_id, kind, payload_json FROM events WHERE id > ?1 ORDER BY id",
        )?;
        statement
            .query_map(params![cursor], |row| {
                let task_id = row.get::<_, String>(1)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    task_id,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, task_id, kind, payload_json) = row?;
                Ok(PersistedEvent {
                    id,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    event: RuntimeEvent::parse(kind)?,
                    payload_json,
                })
            })
            .collect()
    }

    /// Returns the append-only audit history for one task after the supplied cursor.
    pub fn event_records_for_task_after(
        &self,
        task: &TaskId,
        cursor: i64,
    ) -> Result<Vec<PersistedEvent>, RepositoryError> {
        self.task_detail(task)?;
        let mut statement = self.connection.prepare(
            "SELECT id, task_id, kind, payload_json
             FROM events
             WHERE task_id = ?1 AND id > ?2
             ORDER BY id",
        )?;
        statement
            .query_map(params![task.to_string(), cursor], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, task_id, kind, payload_json) = row?;
                Ok(PersistedEvent {
                    id,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    event: RuntimeEvent::parse(kind)?,
                    payload_json,
                })
            })
            .collect()
    }
}

fn append_event(
    transaction: &Transaction<'_>,
    task: &TaskId,
    event: RuntimeEvent,
) -> Result<i64, RepositoryError> {
    append_event_with_payload(transaction, task, event, "{}")
}

fn append_event_with_payload(
    transaction: &Transaction<'_>,
    task: &TaskId,
    event: RuntimeEvent,
    payload_json: &str,
) -> Result<i64, RepositoryError> {
    append_event_with_actor_payload(
        transaction,
        task,
        event,
        r#"{"kind":"runtime"}"#,
        payload_json,
    )
}

fn append_event_with_actor_payload(
    transaction: &Transaction<'_>,
    task: &TaskId,
    event: RuntimeEvent,
    actor_json: &str,
    payload_json: &str,
) -> Result<i64, RepositoryError> {
    let inserted = transaction.execute(
        "INSERT INTO events (session_id, task_id, actor_json, kind, payload_json)
         SELECT root_session_id, id, ?1, ?2, ?3
         FROM tasks WHERE id = ?4",
        params![actor_json, event.name(), payload_json, task.to_string()],
    )?;
    if inserted == 0 {
        return Err(RepositoryError::TaskNotFound {
            task: task.to_string(),
        });
    }
    Ok(transaction.last_insert_rowid())
}

fn insert_review_parent_notification(
    transaction: &Transaction<'_>,
    message_id: &MessageId,
    parent: &TaskId,
    subject: &TaskId,
    delivery: &DeliveryId,
    decision: &str,
    actor_json: &str,
) -> Result<(), RepositoryError> {
    let message =
        format!("Local user review {decision} delivery {delivery} for direct child {subject}");
    let payload_json = serde_json::to_string(&serde_json::json!({
        "message": message,
        "kind": "user_review_override",
        "decision": decision,
        "subject_task_id": subject,
        "delivery_id": delivery,
    }))?;
    transaction.execute(
        "INSERT INTO mailbox_messages
         (id, recipient_task_id, sender_task_id, kind, priority, correlation_id, payload_json)
         VALUES (?1, ?2, NULL, 'user_override', 1, ?3, ?4)",
        params![
            message_id.to_string(),
            parent.to_string(),
            delivery.to_string(),
            payload_json,
        ],
    )?;
    append_event_with_actor_payload(
        transaction,
        parent,
        RuntimeEvent::MailboxMessageQueued,
        actor_json,
        &serde_json::to_string(&serde_json::json!({
            "message_id": message_id,
            "sender": "local_user",
            "kind": "user_review_override",
            "decision": decision,
            "subject_task_id": subject,
            "delivery_id": delivery,
        }))?,
    )?;
    Ok(())
}

fn insert_attempt(
    transaction: &Transaction<'_>,
    attempt: &AttemptId,
    task: &TaskId,
    number: u32,
    state: &str,
) -> Result<(), RepositoryError> {
    transaction.execute(
        "INSERT INTO attempts (id, task_id, number, state, budget_json, usage_json)
         VALUES (?1, ?2, ?3, ?4, '{}', '{}')",
        params![attempt.to_string(), task.to_string(), number, state],
    )?;
    let limits = WatchdogLimits::default();
    transaction.execute(
        "INSERT INTO attempt_watchdogs (
            attempt_id, task_id, limits_json, usage_json, last_meaningful_event_id,
            last_meaningful_at, resource_wait_key, resource_wait_started_at
         ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, NULL, NULL)",
        params![
            attempt.to_string(),
            task.to_string(),
            serde_json::to_string(&limits)?,
            serde_json::to_string(&WatchdogUsage::default())?,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn validate_recovery_context(context: &WorkerRecoveryContext) -> Result<(), RepositoryError> {
    let workspace = context
        .workspace_lease_id
        .as_deref()
        .filter(|lease| lease.starts_with("workspace:") && lease.len() > "workspace:".len())
        .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
            reason: "workspace lease is missing".into(),
        })?;
    let worktree = context
        .worktree_lease
        .as_deref()
        .filter(|lease| lease.starts_with("worktree:") && lease.len() > "worktree:".len())
        .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
            reason: "worktree lease is missing".into(),
        })?;
    if workspace == "workspace:" || worktree == "worktree:" {
        return Err(RepositoryError::InvalidWorkerRecoveryContext {
            reason: "resource lease key is empty".into(),
        });
    }
    serde_json::from_str::<serde_json::Value>(&context.checkpoint_json).map_err(|error| {
        RepositoryError::InvalidWorkerRecoveryContext {
            reason: format!("checkpoint evidence is invalid JSON: {error}"),
        }
    })?;
    serde_json::from_str::<serde_json::Value>(&context.tool_state_json).map_err(|error| {
        RepositoryError::InvalidWorkerRecoveryContext {
            reason: format!("tool-state evidence is invalid JSON: {error}"),
        }
    })?;
    Ok(())
}

fn validate_task_workspace(
    task: &TaskId,
    workspace: &WorkerWorkspace,
) -> Result<(), RepositoryError> {
    workspace_path_str(task, "repository_root", &workspace.repository_root)?;
    workspace_path_str(task, "path", &workspace.path)?;
    for (field, value) in [
        ("branch", workspace.branch.as_str()),
        ("parent_branch", workspace.parent_branch.as_str()),
        ("base_commit", workspace.base_commit.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: format!("{field} is empty"),
            });
        }
    }
    Ok(())
}

fn workspace_path_str<'a>(
    task: &TaskId,
    field: &str,
    path: &'a Path,
) -> Result<&'a str, RepositoryError> {
    let value = path
        .to_str()
        .ok_or_else(|| RepositoryError::InvalidTaskWorkspace {
            task: task.to_string(),
            reason: format!("{field} is not valid UTF-8"),
        })?;
    if value.is_empty() {
        return Err(RepositoryError::InvalidTaskWorkspace {
            task: task.to_string(),
            reason: format!("{field} is empty"),
        });
    }
    Ok(value)
}

fn persisted_task_workspace(
    task: &TaskId,
    lease_id: String,
    repository_root: String,
    path: String,
    branch: String,
    parent_branch: String,
    base_commit: String,
) -> Result<WorkerWorkspace, RepositoryError> {
    if lease_id.trim().is_empty() {
        return Err(RepositoryError::InvalidTaskWorkspace {
            task: task.to_string(),
            reason: "lease_id is empty".into(),
        });
    }
    let workspace = WorkerWorkspace {
        lease_id: lease_id.parse::<WorkspaceLeaseId>().map_err(|_| {
            RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: "lease_id is invalid".into(),
            }
        })?,
        repository_root: PathBuf::from(repository_root),
        path: PathBuf::from(path),
        branch,
        parent_branch,
        base_commit,
    };
    validate_task_workspace(task, &workspace)?;
    Ok(workspace)
}

fn optional_persisted_task_workspace(
    task: &TaskId,
    lease_id: Option<String>,
    repository_root: Option<String>,
    path: Option<String>,
    branch: Option<String>,
    parent_branch: Option<String>,
    base_commit: Option<String>,
) -> Result<Option<WorkerWorkspace>, RepositoryError> {
    match (
        lease_id,
        repository_root,
        path,
        branch,
        parent_branch,
        base_commit,
    ) {
        (None, None, None, None, None, None) => Ok(None),
        (
            Some(lease_id),
            Some(repository_root),
            Some(path),
            Some(branch),
            Some(parent_branch),
            Some(base_commit),
        ) => persisted_task_workspace(
            task,
            lease_id,
            repository_root,
            path,
            branch,
            parent_branch,
            base_commit,
        )
        .map(Some),
        _ => Err(RepositoryError::InvalidTaskWorkspace {
            task: task.to_string(),
            reason: "workspace row is partially persisted".into(),
        }),
    }
}

fn task_workspace_insert_error(task: &TaskId, error: rusqlite::Error) -> RepositoryError {
    match &error {
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if sqlite_error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            RepositoryError::InvalidTaskWorkspace {
                task: task.to_string(),
                reason: "workspace assignment conflicts with persisted workspace state".into(),
            }
        }
        _ => RepositoryError::Sql(error),
    }
}

fn migrate(connection: &Connection) -> Result<(), RepositoryError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);",
    )?;
    let current_version: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current_version >= LATEST_SCHEMA_VERSION {
        return Ok(());
    }
    if current_version < 1 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY, project_root TEXT NOT NULL, state TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            config_json TEXT NOT NULL
        );
        CREATE TABLE tasks (
            id TEXT PRIMARY KEY, root_session_id TEXT NOT NULL REFERENCES sessions(id),
            parent_id TEXT REFERENCES tasks(id), depth INTEGER NOT NULL, state_json TEXT NOT NULL,
            contract_version INTEGER NOT NULL, active_attempt_id TEXT NOT NULL, delivery_json TEXT NOT NULL,
            workspace_lease_id TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE attempts (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), number INTEGER NOT NULL,
            state TEXT NOT NULL, checkpoint_json TEXT, budget_json TEXT NOT NULL, usage_json TEXT NOT NULL,
            started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, ended_at TEXT, terminal_json TEXT,
            UNIQUE(task_id, number)
        );
        CREATE TABLE contracts (
            task_id TEXT NOT NULL REFERENCES tasks(id), version INTEGER NOT NULL, payload_json TEXT NOT NULL,
            digest TEXT NOT NULL, created_event_id INTEGER NOT NULL, PRIMARY KEY(task_id, version)
        );
        CREATE TABLE contract_amendments (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), from_version INTEGER NOT NULL,
            to_version INTEGER NOT NULL, payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE mailbox_messages (
            id TEXT PRIMARY KEY, recipient_task_id TEXT NOT NULL REFERENCES tasks(id), sender_task_id TEXT,
            kind TEXT NOT NULL, priority INTEGER NOT NULL, correlation_id TEXT, causation_id TEXT,
            payload_json TEXT NOT NULL, coalesced_count INTEGER NOT NULL DEFAULT 0, delivered_at TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE resource_leases (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), resource_key TEXT NOT NULL,
            mode TEXT NOT NULL, units INTEGER NOT NULL, state TEXT NOT NULL,
            acquired_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, released_at TEXT
        );
        CREATE TABLE deliveries (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), attempt_id TEXT NOT NULL,
            payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE reviews (
            id TEXT PRIMARY KEY, delivery_id TEXT NOT NULL REFERENCES deliveries(id), actor_json TEXT NOT NULL,
            decision TEXT NOT NULL, evidence_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE permission_requests (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), payload_json TEXT NOT NULL,
            state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, decided_at TEXT
        );
        CREATE TABLE schedules (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), definition_json TEXT NOT NULL,
            state TEXT NOT NULL, next_run_at TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE events (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
            task_id TEXT REFERENCES tasks(id), attempt_id TEXT, actor_json TEXT NOT NULL, kind TEXT NOT NULL,
            payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX tasks_root_state_idx ON tasks(root_session_id, state_json);
        CREATE INDEX mailbox_recipient_idx ON mailbox_messages(recipient_task_id, delivered_at, priority);
        CREATE INDEX leases_resource_idx ON resource_leases(resource_key, state);
        CREATE INDEX events_session_id_idx ON events(session_id, id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
        transaction.commit()?;
    }
    if current_version < 2 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE runtime_metadata (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO runtime_metadata (key, value) VALUES ('event_replay_floor', 1);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (2)", [])?;
        transaction.commit()?;
    }
    if current_version < 3 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE resource_admission_cursors (
                resource_key TEXT PRIMARY KEY,
                root_session_id TEXT,
                parent_task_id TEXT,
                sequence INTEGER NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX admission_cursor_root_idx
                ON resource_admission_cursors(root_session_id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (3)", [])?;
        transaction.commit()?;
    }
    if current_version < 4 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE attempt_watchdogs (
                attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
                task_id TEXT NOT NULL REFERENCES tasks(id),
                limits_json TEXT NOT NULL,
                usage_json TEXT NOT NULL,
                last_meaningful_event_id INTEGER,
                last_meaningful_at TEXT NOT NULL,
                resource_wait_key TEXT,
                resource_wait_started_at TEXT
            );
            CREATE INDEX attempt_watchdogs_task_idx ON attempt_watchdogs(task_id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (4)", [])?;
        transaction.commit()?;
    }
    if current_version < 5 {
        let transaction = connection.unchecked_transaction()?;
        // Schedules formerly referenced one session, but each fire now owns a
        // new root. Rebuild the unused early table without that false link.
        let schedules_exist = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schedules')",
                [],
                |row| row.get::<_, bool>(0),
            )?;
        if schedules_exist {
            transaction.execute_batch(
                "ALTER TABLE schedules RENAME TO schedules_legacy;
             CREATE TABLE schedules (
                id TEXT PRIMARY KEY,
                definition_json TEXT NOT NULL,
                state TEXT NOT NULL,
                next_run_at TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO schedules (id, definition_json, state, next_run_at, created_at)
             SELECT id, definition_json, state, COALESCE(next_run_at, CURRENT_TIMESTAMP), created_at
             FROM schedules_legacy;
             DROP TABLE schedules_legacy;
             CREATE TABLE schedule_occurrences (
                schedule_id TEXT NOT NULL REFERENCES schedules(id),
                due_at TEXT NOT NULL,
                outcome TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(schedule_id, due_at)
             );
             CREATE INDEX schedule_next_run_idx ON schedules(state, next_run_at);",
            )?;
        } else {
            transaction.execute_batch(
                "CREATE TABLE schedules (
                    id TEXT PRIMARY KEY,
                    definition_json TEXT NOT NULL,
                    state TEXT NOT NULL,
                    next_run_at TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );
             CREATE TABLE schedule_occurrences (
                schedule_id TEXT NOT NULL REFERENCES schedules(id),
                due_at TEXT NOT NULL,
                outcome TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(schedule_id, due_at)
             );
             CREATE INDEX schedule_next_run_idx ON schedules(state, next_run_at);",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (5)", [])?;
        transaction.commit()?;
    }
    if current_version < 6 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "ALTER TABLE schedule_occurrences
             ADD COLUMN root_session_id TEXT REFERENCES sessions(id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (6)", [])?;
        transaction.commit()?;
    }
    if current_version < 7 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE task_workspaces (
                task_id TEXT PRIMARY KEY REFERENCES tasks(id),
                attempt_id TEXT NOT NULL REFERENCES attempts(id),
                lease_id TEXT NOT NULL UNIQUE,
                repository_root TEXT NOT NULL,
                path TEXT NOT NULL UNIQUE,
                branch TEXT NOT NULL,
                parent_branch TEXT NOT NULL,
                base_commit TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(repository_root, branch)
             );
             CREATE TABLE application_root_attachments (
                idempotency_key TEXT PRIMARY KEY,
                root_session_id TEXT NOT NULL,
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                capability_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                detached_at TEXT
             );",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (7)", [])?;
        transaction.commit()?;
    }
    Ok(())
}
