use std::path::Path;
use std::str::FromStr;

pub(crate) use chrono::{DateTime, Local, Utc};
pub(crate) use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
pub(crate) use yi_agent_core::subagent::task::{
    BudgetKind, DeliveryReport, MessageId, PermissionDecision, PermissionRequestId, TaskRole,
    TimeoutKind,
};
pub(crate) use yi_agent_core::subagent::worker::{
    WorkerRecoveryAttestation, WorkerRecoveryContext,
};
pub(crate) use yi_agent_core::{
    AttemptId, ChildWriteMode, InheritedSandbox, RootSessionId, TaskId,
};

pub(crate) use crate::schedule::{
    ScheduleDefinition, WatchdogLimits, WatchdogObservation, WatchdogTokenDelta, WatchdogUsage,
};
pub(crate) use crate::terminal::text_completion_report;

mod admission;
pub(super) mod helpers;

// 域子模块统一用 `use super::*`，故这些跨模块辅助函数必须在父模块可见。
// 它们原先就内联在本文件里，`pub(super) use` 只是把可见性落到同一个位置。
use helpers::{
    append_event, append_event_with_actor_payload, append_event_with_payload, insert_attempt,
    migrate, trim_trace_rows, validate_recovery_context,
};
mod application_root;
mod event;
pub mod foreground;
mod mailbox;
mod permission;
mod read;
mod schedule;
mod session_container;
#[cfg(test)]
mod session_container_tests;

pub use session_container::{
    RetiredHostExecution, SessionBindingCommit, SessionContainerRecord, SessionHostExecution,
    SessionHostStop,
};
mod task;
mod trace;
mod watchdog;

/// The newest schema this build can migrate to and read.
///
/// Public (via [`crate::schema_compat::SUPPORTED_SCHEMA_MAX`]) because a client
/// must refuse to open a database written by a *newer* build before it performs
/// any migration, task recovery, or orphan reclamation. The old check
/// (`current_version >= LATEST` means success) silently accepted a future schema
/// and then ran statements against columns that may not mean what it assumes.
pub const LATEST_SCHEMA_VERSION: i64 = 19;

/// Hard ceiling on the trace rows a single task may retain. A worker's trace is
/// observation data, not history: it must stay bounded no matter how long the
/// task runs, so `append_trace` trims the oldest rows in the same transaction
/// that inserts the newest.
pub const TRACE_MAX_ROWS_PER_TASK: usize = 2000;

/// How long a terminal task's trace survives before `prune_terminal_traces`
/// may drop it. One day is long enough for a supervisor to read the tail of a
/// finished worker and short enough that abandoned pass-through rows do not
/// accumulate for the life of the installation.
pub const TRACE_TERMINAL_RETENTION_SECS: i64 = 86_400;

/// The persisted `tasks.state_json` values that mean "this task will never
/// move again".
///
/// `yi_agent_core::subagent::task::TaskState::is_terminal` is the same notion,
/// but it takes the typed enum, and this layer only ever holds the serialized
/// state string. Rather than re-derive the enum just to ask one question, the
/// names are listed here once and pinned by
/// `prune_treats_only_terminal_states_as_finished`.
///
/// `RecoveryRequired` is included on purpose: it is a holding pen for explicit
/// resume, not a live task, and `TaskState::is_terminal` agrees.
///
/// The two other persisted spellings of that state belong here too. Runtime
/// hydration folds `recovery_gated` and `recovery_attested` into
/// `TaskState::RecoveryRequired` (`runtime.rs:3410`), so a task parked at either
/// one is terminal for the same reason `recovery_required` is.
/// `recovery_gated` is written by `activate_successor_attempt` and
/// `recovery_attested` by `attest_recovery_gate`. Leaving them out made those
/// rows permanently uncollectable — the exact unbounded growth the retention
/// window exists to stop — so the behaviour is pinned by
/// `tasks_parked_in_recovery_states_have_their_traces_pruned`.
const TERMINAL_TASK_STATES: [&str; 11] = [
    "completed",
    "completed_no_changes",
    "blocked",
    "stalled",
    "timed_out",
    "budget_exhausted",
    "failed",
    "cancelled",
    "recovery_required",
    "recovery_gated",
    "recovery_attested",
];

/// The settled states whose lease may be released on sight.
///
/// Narrower than [`TERMINAL_TASK_STATES`] on purpose: `recovery_required` and
/// its two spellings are terminal for retention but *parked and resumable*, and
/// the workspace/worktree lease is precisely what an explicit resume needs. A
/// settled task can never be resumed, so nothing is lost by releasing there.
const SETTLED_TASK_STATES: [&str; 8] = [
    "completed",
    "completed_no_changes",
    "blocked",
    "stalled",
    "timed_out",
    "budget_exhausted",
    "failed",
    "cancelled",
];

/// A single persisted row of a task's observation trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTraceRow {
    pub id: i64,
    pub task_id: TaskId,
    pub kind: String,
    pub payload_json: String,
}

/// Grace period before a task with no application-root attachment at all is
/// treated as unowned. An application root writes its attachment before it
/// spawns any child, so this window only ever covers a boot in progress or a
/// legacy row; it is not a waiting period a healthy session can fall into.
pub const DEFAULT_ORPHAN_GRACE_SECS: i64 = 300;

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
    #[error("worker recovery context is not durable: {reason}")]
    InvalidWorkerRecoveryContext { reason: String },
    #[error("admission cursor is invalid for {key}: {reason}")]
    InvalidAdmissionCursor { key: String, reason: String },
    #[error("watchdog snapshot is invalid for {attempt}: {reason}")]
    InvalidWatchdogSnapshot { attempt: String, reason: String },
    /// The on-disk schema is outside the range this build can read and migrate.
    /// Distinct from `Sql` so a caller can tell "incompatible database" (report
    /// it, keep the file) from "the database is corrupt".
    #[error("schema incompatible: {reason}")]
    SchemaIncompatible { reason: String },
    /// A structured foreground rejection. Kept as a typed variant rather than a
    /// flattened string: callers dispatch on [`ForegroundError::code`], and an
    /// English message cannot be matched on without becoming a parsing problem.
    #[error("foreground root lifecycle: {0}")]
    Foreground(#[from] crate::foreground::ForegroundError),
    /// A foreground fact that cannot be stored as asked (an out-of-range epoch, an
    /// unknown enum spelling). Separate from `Foreground` because it is an internal
    /// contract violation, not a client rejection.
    #[error("foreground state for {root} is invalid: {reason}")]
    InvalidForegroundFact { root: String, reason: String },
    /// A structured session-delegation rejection. Typed for the same reason as
    /// `Foreground`: every caller branches on [`SessionFailureCode`], and matching
    /// on an English message would turn a stable contract into a parsing
    /// exercise.
    #[error("session delegation: {0:?}")]
    SessionDelegation(yi_agent_lifecycle::session_delegation::SessionFailure),
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
    TaskCompleted,
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
}

impl RuntimeEvent {
    fn name(self) -> &'static str {
        match self {
            Self::RuntimeDraining => "runtime_draining",
            Self::RuntimeRecovered => "runtime_recovered",
            Self::TaskQueued => "task_queued",
            Self::TaskStarted => "task_started",
            Self::TaskCompleted => "task_completed",
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
        }
    }

    fn parse(kind: String) -> Result<Self, RepositoryError> {
        match kind.as_str() {
            "runtime_draining" => Ok(Self::RuntimeDraining),
            "runtime_recovered" => Ok(Self::RuntimeRecovered),
            "task_queued" => Ok(Self::TaskQueued),
            "task_started" => Ok(Self::TaskStarted),
            "task_completed" => Ok(Self::TaskCompleted),
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
    pub capability_secret: Option<String>,
    /// The project workspace the root was attached to. A read-only root keeps no
    /// `task_workspaces` row, so this is the only durable record of its project.
    pub workspace_root: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAttemptWatchdog {
    pub limits: WatchdogLimits,
    pub observation: WatchdogObservation,
    pub last_meaningful_event_id: Option<i64>,
    pub current_wait: Option<WatchdogResourceWait>,
}

/// What one watchdog terminal commit actually did.
///
/// The distinction is load-bearing: `StaleSnapshot` is "someone else already
/// decided", `ForegroundContainer` is "this root is not governed by these budgets",
/// and `ClosedRoot` is "this root is not admissible, so neither settling nor
/// ignoring it is correct". Collapsing them into a bool would let a caller report
/// "settled" for work it skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogTerminalOutcome {
    /// The terminal was written and its audit event appended.
    Committed { event_id: Option<i64> },
    /// The attempt is no longer the task's active attempt, or the task is already
    /// terminal: a later scan is a no-op rather than a second audit event.
    StaleSnapshot,
    /// The root is a confirmed foreground container. Its budgets are the host's,
    /// not this watchdog's, so nothing is written and nothing is kept alive.
    ForegroundContainer,
    /// The root declares a foreground role but may not accept business (pending or
    /// an unrecognised/corrupt classification). The caller must reclaim or diagnose
    /// it; the watchdog must not manufacture a liveness verdict for it.
    ClosedRoot { reason: String },
}

impl WatchdogTerminalOutcome {
    /// The event id when the terminal was actually committed.
    pub fn committed_event_id(&self) -> Option<i64> {
        match self {
            Self::Committed { event_id } => *event_id,
            Self::StaleSnapshot | Self::ForegroundContainer | Self::ClosedRoot { .. } => None,
        }
    }

    pub fn committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTaskSummary {
    pub task_id: String,
    pub state: String,
    pub is_root: bool,
    /// The task's parent, absent for a root task. Lets a reader rebuild the
    /// task tree without a second query.
    pub parent_task_id: Option<String>,
    /// The conversation this task belongs to, when it was tagged with one.
    /// Lets a reader scope tasks to a conversation without a second query.
    pub thread_id: Option<String>,
    /// 该 task 所有 attempt 的累计用量。
    ///
    /// 按 task 聚合而非取最后一次：重试是真实消耗，只算最新一次会低估。
    pub usage: crate::ipc::IpcTaskUsage,
    /// `execution` or `session_container`.
    ///
    /// A container is a conversation's place to delegate from, not an attempt at
    /// work: its `state` is history and says nothing about whether it may still
    /// spawn. Readers that ask "can this go on?" must look at this role and at
    /// [`Self::container_activity`], never at the state.
    pub role: String,
    /// What a container is doing *now*; `unknown` for executions.
    ///
    /// Deliberately not derived from `state`: a container whose historical state
    /// reads `stalled` is still open and still usable. Only the container row can
    /// answer this, so an absent row yields `unknown` rather than a guess.
    pub container_activity: String,
    /// The execution state, only for containers, and only as history.
    ///
    /// Kept separate so a UI can label it "last attempt" instead of showing a
    /// stale `stalled` as if it described the conversation's capability.
    pub historical_execution_state: Option<String>,
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
    pub terminal_json: Option<String>,
}

/// Active resource ownership which a destructive-control preview must expose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedActiveLease {
    pub lease_id: String,
    pub task_id: TaskId,
    pub resource_key: String,
}

/// A delivery a destructive-control preview must disclose as not yet merged
/// into its parent's branch.
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
    pub workspace_mode: ChildWriteMode,
    pub inherited_sandbox: Option<InheritedSandbox>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedHydrationTask {
    pub session_id: RootSessionId,
    pub task_id: TaskId,
    pub parent_id: Option<TaskId>,
    pub depth: u8,
    pub attempt_id: AttemptId,
    pub attempt_number: u32,
    pub state: String,
    pub delivery_json: String,
    pub terminal_json: Option<String>,
    /// The persisted role. Hydration must restore it rather than assume
    /// `Execution`: a restarted daemon that rebuilt a container as an execution
    /// would refuse every later spawn on the strength of a historical state.
    pub role: TaskRole,
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

    /// Runs one statement against the fixture database.
    ///
    /// Test-only escape hatch: `foreground_class` treats a hand-made contradiction
    /// as `Corrupt`, and the production writer cannot make one (that is the point of
    /// the migration writing no classification rows). A test that wants the reader's
    /// verdict on a corrupt row therefore has to build the row itself.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn execute_test_statement(
        &self,
        statement: &str,
        parameter: &str,
    ) -> Result<(), RepositoryError> {
        self.connection
            .execute(statement, rusqlite::params![parameter])?;
        Ok(())
    }

    /// Runs one query against the fixture database and returns a single number.
    ///
    /// Test-only, next to `execute_test_statement` and for the same reason: some
    /// assertions are about what the writer left behind (how many attempts exist, what
    /// a settled attempt's terminal says) and reading that back through a projection
    /// the test is meant to distrust would defeat the purpose.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn execute_test_scalar(
        &self,
        statement: &str,
        parameter: &str,
    ) -> Result<i64, RepositoryError> {
        Ok(self
            .connection
            .query_row(statement, rusqlite::params![parameter], |row| row.get(0))?)
    }

    /// Runs one statement against the fixture database with several parameters.
    ///
    /// The single-parameter variant covers most fixtures; settling a task *and* its
    /// attempt in one statement needs more than one value, and splitting it would let
    /// the two rows disagree between the two writes.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn execute_test_statement_params(
        &self,
        statement: &str,
        parameters: &[&str],
    ) -> Result<(), RepositoryError> {
        let parameters: Vec<&dyn rusqlite::ToSql> = parameters
            .iter()
            .map(|value| value as &dyn rusqlite::ToSql)
            .collect();
        self.connection.execute(statement, parameters.as_slice())?;
        Ok(())
    }

    /// The text twin of `execute_test_scalar`, for one optional column.
    #[cfg(any(test, feature = "foreground-test-support"))]
    pub fn execute_test_scalar_text(
        &self,
        statement: &str,
        parameter: &str,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(statement, rusqlite::params![parameter], |row| row.get(0))
            .optional()?)
    }

    pub fn schema_version(&self) -> Result<i64, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    /// A read-only handle on the underlying connection so migration tests can
    /// assert on the schema itself, without going through a read API that the
    /// migration under test has not added yet.
    #[cfg(test)]
    pub(crate) fn connection_for_test(&self) -> &Connection {
        &self.connection
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
}

#[cfg(test)]
mod runtime_event_tests {

    #[test]
    fn recycle_events_round_trip_through_name_and_parse() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_thirteen_adds_the_trace_table_and_thread_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        let repository = RuntimeRepository::open(&path).unwrap();

        let connection = repository.connection_for_test();
        let table_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_trace_events')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(table_exists);

        let has_thread_id: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_thread_id);
        assert_eq!(
            LATEST_SCHEMA_VERSION as u32,
            crate::schema_compat::SUPPORTED_SCHEMA_MAX
        );
    }

    #[test]
    fn a_version_twelve_database_gains_the_trace_table_and_thread_id() {
        // The fresh-open test above never exercises the upgrade an existing
        // install performs, so roll a database back to exactly the v12 shape
        // and reopen it. v12 is the shape a store already has once the
        // inherited-sandbox migration has run, which is exactly the case that
        // would break if the trace table had been folded into v12.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE task_trace_events;
                 ALTER TABLE tasks DROP COLUMN thread_id;
                 DELETE FROM schema_migrations WHERE version >= 13;",
            )
            .unwrap();
        drop(connection);

        let repository = RuntimeRepository::open(&path).unwrap();
        // The `migrate` path always lands on the newest version, so asserting the
        // latest here keeps this test about the upgrade method rather than about a
        // number that every later migration would have to edit.
        assert_eq!(repository.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        let connection = repository.connection_for_test();
        assert!(
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_trace_events')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap(),
            "a v12 database must gain the trace table"
        );
        assert!(
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap(),
            "a v12 database must gain tasks.thread_id"
        );
    }

    #[test]
    fn a_version_eleven_database_gains_both_later_migrations() {
        // Renumbering the trace work to v13 must not strand a v11 database: it
        // has neither the sandbox column nor the trace, so one open has to run
        // both migrations in order.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE task_trace_events;
                 ALTER TABLE tasks DROP COLUMN thread_id;
                 ALTER TABLE tasks DROP COLUMN inherited_sandbox;
                 DELETE FROM schema_migrations WHERE version >= 12;",
            )
            .unwrap();
        drop(connection);

        let repository = RuntimeRepository::open(&path).unwrap();
        // The `migrate` path always lands on the newest version, so asserting the
        // latest here keeps this test about the upgrade method rather than about a
        // number that every later migration would have to edit.
        assert_eq!(repository.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        let connection = repository.connection_for_test();
        for (probe, what) in [
            (
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_trace_events')",
                "the trace table",
            ),
            (
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id')",
                "tasks.thread_id",
            ),
            (
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'inherited_sandbox')",
                "tasks.inherited_sandbox",
            ),
        ] {
            assert!(
                connection
                    .query_row(probe, [], |row| row.get::<_, bool>(0))
                    .unwrap(),
                "a v11 database must gain {what}"
            );
        }
    }

    /// Builds a `running` root task through the production insert path, so the
    /// trace tests start from a row that really exists.
    fn seed_running_task(repository: &mut RuntimeRepository) -> (RootSessionId, TaskId, AttemptId) {
        let session = RootSessionId::new();
        let task = TaskId::new();
        let attempt = AttemptId::new();
        repository
            .create_task_with_attempt(&task, &session, &attempt, 1, "running")
            .unwrap();
        (session, task, attempt)
    }

    /// 同一 task 的多次 attempt 用量必须**求和**：重试是真实消耗，只算最后一次
    /// 会低估这个子任务真正烧掉的钱。
    #[test]
    fn task_summaries_sum_usage_across_attempts() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let session = RootSessionId::new();
        let task = TaskId::new();
        // 第一次 attempt。
        let attempt_one = AttemptId::new();
        repository
            .create_task_with_attempt(&task, &session, &attempt_one, 1, "running")
            .unwrap();
        repository
            .record_watchdog_progress(
                &task,
                &attempt_one,
                1,
                50,
                0,
                0,
                false,
                Utc::now(),
                WatchdogTokenDelta {
                    input_tokens: 40,
                    output_tokens: 10,
                    cache_read_tokens: 900,
                },
            )
            .unwrap();
        // 重试：走生产的 successor 路径建第二个 attempt，带自己的记账。
        let attempt_two = AttemptId::new();
        repository
            .activate_successor_attempt(
                &task,
                &attempt_two,
                2,
                "running",
                RuntimeEvent::TaskStarted,
            )
            .unwrap();
        repository
            .record_watchdog_progress(
                &task,
                &attempt_two,
                1,
                30,
                0,
                0,
                false,
                Utc::now(),
                WatchdogTokenDelta {
                    input_tokens: 25,
                    output_tokens: 5,
                    cache_read_tokens: 100,
                },
            )
            .unwrap();

        let summaries = repository.task_summaries(Some(&session), false).unwrap();
        let summary = summaries
            .iter()
            .find(|summary| summary.task_id == task.to_string())
            .expect("the task must be listed");

        assert_eq!(summary.usage.input_tokens, 65);
        assert_eq!(summary.usage.output_tokens, 15);
        assert_eq!(summary.usage.cache_read_tokens, 1000);
    }

    /// 没有任何 attempt 记账的 task 读到全零，而不是缺行或报错。
    #[test]
    fn task_summaries_default_usage_to_zero_without_watchdog_rows() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let session = RootSessionId::new();
        let task = TaskId::new();
        repository.create_task(&task, &session, "queued").unwrap();

        let summaries = repository.task_summaries(Some(&session), false).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].usage, crate::ipc::IpcTaskUsage::default());
    }

    /// `usage_json` 是自由格式 JSON 文本列，本次加字段不能要求迁移：本次改造前
    /// 落盘的快照没有这三个键，读出来必须是 0 而不是反序列化失败。
    #[test]
    fn watchdog_usage_without_breakdown_fields_reads_as_zero() {
        let legacy = r#"{"turns":2,"tokens":300,"cost_micros":5,"provider_retries":1,"tool_retries":0,"rework_cycles":0}"#;
        let usage: WatchdogUsage = serde_json::from_str(legacy).unwrap();
        assert_eq!(usage.tokens, 300);
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
    }

    /// `record_watchdog_progress` 累加三个分项，同时**不改变** `tokens` 语义：
    /// 后者仍是 input+output 之和，watchdog 的预算判断依赖它。
    #[test]
    fn record_watchdog_progress_accumulates_token_breakdown() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let (_session, task, attempt) = seed_running_task(&mut repository);

        // 第一次调用：input 40 / output 10 / cache read 900，tokens 记 50。
        repository
            .record_watchdog_progress(
                &task,
                &attempt,
                1,
                50,
                0,
                0,
                false,
                Utc::now(),
                WatchdogTokenDelta {
                    input_tokens: 40,
                    output_tokens: 10,
                    cache_read_tokens: 900,
                },
            )
            .unwrap();
        // 第二次：再累加一次，验证是"累加"而不是"覆盖"。
        repository
            .record_watchdog_progress(
                &task,
                &attempt,
                1,
                30,
                0,
                0,
                false,
                Utc::now(),
                WatchdogTokenDelta {
                    input_tokens: 25,
                    output_tokens: 5,
                    cache_read_tokens: 100,
                },
            )
            .unwrap();

        let usage = repository.watchdog_usage(&task, &attempt).unwrap();
        assert_eq!(usage.input_tokens, 65);
        assert_eq!(usage.output_tokens, 15);
        assert_eq!(usage.cache_read_tokens, 1000);
        // 语义锁死：tokens 仍只跟 token_delta 走（input+output），不含 cache read。
        assert_eq!(usage.tokens, 80);
    }

    #[test]
    fn trace_rows_are_trimmed_to_the_per_task_ceiling() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let (_session, task, _attempt) = seed_running_task(&mut repository);

        for index in 0..(TRACE_MAX_ROWS_PER_TASK + 5) {
            repository
                .append_trace(
                    &task,
                    "state_note",
                    &format!(r#"{{"note":"step {index}"}}"#),
                )
                .unwrap();
        }

        assert_eq!(
            repository.trace_row_count(&task).unwrap(),
            TRACE_MAX_ROWS_PER_TASK as i64
        );
        let rows = repository.trace_after(&task, 0).unwrap();
        assert_eq!(rows.len(), TRACE_MAX_ROWS_PER_TASK);
        // The newest rows survive; the oldest are gone.
        assert_eq!(rows.first().unwrap().payload_json, r#"{"note":"step 5"}"#);
        assert_eq!(
            rows.last().unwrap().payload_json,
            format!(r#"{{"note":"step {}"}}"#, TRACE_MAX_ROWS_PER_TASK + 4)
        );
        assert_eq!(
            repository.trace_high_water(&task).unwrap(),
            rows.last().unwrap().id
        );
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "rows must come back in id order: {ids:?}"
        );

        // Reading from the high-water mark yields nothing, because a reader
        // already owns every row up to that id.
        assert!(
            repository
                .trace_after(&task, repository.trace_high_water(&task).unwrap())
                .unwrap()
                .is_empty()
        );

        // `trim_trace` is idempotent: a trace at the ceiling is already trimmed.
        repository.trim_trace(&task).unwrap();
        assert_eq!(
            repository.trace_row_count(&task).unwrap(),
            TRACE_MAX_ROWS_PER_TASK as i64
        );
    }

    #[test]
    fn trim_trace_enforces_the_ceiling_on_an_existing_trace() {
        // `append_trace` already trims on every write, so this pins the
        // maintenance entry point against a trace that is over the ceiling for
        // some other reason.
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let (_session, task, _attempt) = seed_running_task(&mut repository);
        {
            let connection = repository.connection_for_test();
            for index in 0..(TRACE_MAX_ROWS_PER_TASK + 5) {
                connection
                    .execute(
                        "INSERT INTO task_trace_events (task_id, kind, payload_json)
                         VALUES (?1, ?2, ?3)",
                        params![
                            task.to_string(),
                            "state_note",
                            format!(r#"{{"note":"step {index}"}}"#)
                        ],
                    )
                    .unwrap();
            }
        }
        assert_eq!(
            repository.trace_row_count(&task).unwrap(),
            (TRACE_MAX_ROWS_PER_TASK + 5) as i64
        );

        repository.trim_trace(&task).unwrap();

        assert_eq!(
            repository.trace_row_count(&task).unwrap(),
            TRACE_MAX_ROWS_PER_TASK as i64
        );
        let rows = repository.trace_after(&task, 0).unwrap();
        assert_eq!(rows.first().unwrap().payload_json, r#"{"note":"step 5"}"#);
    }

    #[test]
    fn terminal_traces_are_pruned_after_the_retention_window() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let (_session, task, _attempt) = seed_running_task(&mut repository);
        repository
            .append_trace(&task, "state_note", r#"{"note":"done"}"#)
            .unwrap();
        repository
            .transition_task(&task, "completed_no_changes", RuntimeEvent::TaskCompleted)
            .unwrap();

        let removed = repository
            .prune_terminal_traces(
                Utc::now() + chrono::Duration::seconds(TRACE_TERMINAL_RETENTION_SECS + 1),
            )
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(repository.trace_row_count(&task).unwrap(), 0);
    }

    #[test]
    fn prune_treats_only_terminal_states_as_finished() {
        // The retention window is cleanup for finished work. A task that is
        // still live keeps its rows no matter how old they are, because they
        // are the head of the sequence a reader replays.
        let terminal = [
            "completed",
            "completed_no_changes",
            "blocked",
            "stalled",
            "timed_out",
            "budget_exhausted",
            "failed",
            "cancelled",
            "recovery_required",
            // Terminal spellings of the same held state: hydration folds these
            // into `TaskState::RecoveryRequired`, which is terminal.
            "recovery_gated",
            "recovery_attested",
        ];
        let live = ["running", "queued", "paused"];
        // Pin the production set itself, so drift in either direction — a state
        // quietly added, or one quietly dropped — fails here rather than in the
        // field, where the symptom would be a truncated or uncollectable trace.
        assert_eq!(TERMINAL_TASK_STATES, terminal);

        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let session = RootSessionId::new();
        let mut tasks = Vec::new();
        for state in terminal.iter().chain(live.iter()) {
            let task = TaskId::new();
            let attempt = AttemptId::new();
            repository
                .create_task_with_attempt(&task, &session, &attempt, 1, state)
                .unwrap();
            repository
                .append_trace(&task, "state_note", r#"{"note":"old"}"#)
                .unwrap();
            tasks.push((state, task));
        }

        let removed = repository
            .prune_terminal_traces(
                Utc::now() + chrono::Duration::seconds(TRACE_TERMINAL_RETENTION_SECS * 400),
            )
            .unwrap();
        assert_eq!(removed, terminal.len());
        for (state, task) in tasks {
            let expected = if terminal.contains(state) { 0 } else { 1 };
            assert_eq!(
                repository.trace_row_count(&task).unwrap(),
                expected,
                "state {state} kept the wrong number of trace rows"
            );
        }
    }

    /// The pin above compares the constant against a hand-written list, so it
    /// can only catch a name the author also forgot here. This test asks the
    /// database instead: the retention window must actually delete the trace
    /// rows of a task parked in each recovery state. That is the property the
    /// constant exists to serve, and it is the one that catches an omission.
    #[test]
    fn tasks_parked_in_recovery_states_have_their_traces_pruned() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let session = RootSessionId::new();

        // `recovery_gated` and `recovery_attested` are written by production
        // paths (`activate_successor_attempt`, `attest_recovery_gate`), so the
        // rows below are shapes the store really persists.
        let mut gated = None;
        let mut attested = None;
        for state in ["recovery_gated", "recovery_attested"] {
            let task = TaskId::new();
            let attempt = AttemptId::new();
            repository
                .create_task_with_attempt(&task, &session, &attempt, 1, state)
                .unwrap();
            repository
                .append_trace(&task, "state_note", r#"{"note":"parked"}"#)
                .unwrap();
            if state == "recovery_gated" {
                gated = Some(task);
            } else {
                attested = Some(task);
            }
        }
        let gated = gated.unwrap();
        let attested = attested.unwrap();

        // A task parked in recovery is terminal, so past the window its rows go.
        let removed = repository
            .prune_terminal_traces(
                Utc::now() + chrono::Duration::seconds(TRACE_TERMINAL_RETENTION_SECS + 1),
            )
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(repository.trace_row_count(&gated).unwrap(), 0);
        assert_eq!(repository.trace_row_count(&attested).unwrap(), 0);
    }

    #[test]
    fn task_thread_id_round_trips_and_unknown_tasks_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let (_session, task, _attempt) = seed_running_task(&mut repository);

        // The column is additive: a task created today starts with no desktop
        // conversation bound to it.
        assert_eq!(repository.task_thread_id(&task).unwrap(), None);

        repository.set_task_thread_id(&task, "thread-42").unwrap();
        assert_eq!(
            repository.task_thread_id(&task).unwrap().as_deref(),
            Some("thread-42")
        );

        // Re-binding updates the existing row instead of inserting a second one.
        repository.set_task_thread_id(&task, "thread-43").unwrap();
        assert_eq!(
            repository.task_thread_id(&task).unwrap().as_deref(),
            Some("thread-43")
        );

        let unknown = TaskId::new();
        assert!(matches!(
            repository.set_task_thread_id(&unknown, "thread-44"),
            Err(RepositoryError::TaskNotFound { .. })
        ));
        assert!(matches!(
            repository.task_thread_id(&unknown),
            Err(RepositoryError::TaskNotFound { .. })
        ));
    }
}

#[cfg(test)]
mod extra_roots_tests {
    use super::*;

    fn repository_with_one_task() -> (RuntimeRepository, TaskId, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let mut repository =
            RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
        let task = TaskId::new();
        repository
            .create_task_with_attempt(&task, &RootSessionId::new(), &AttemptId::new(), 1, "queued")
            .unwrap();
        (repository, task, directory)
    }

    #[test]
    fn extra_roots_round_trip_and_default_to_empty() {
        let (mut repository, task, _directory) = repository_with_one_task();
        assert_eq!(
            repository.task_extra_roots(&task).unwrap(),
            Vec::<String>::new()
        );
        repository
            .set_task_extra_roots(&task, &["/tmp/a".to_string(), "/tmp/b".to_string()])
            .unwrap();
        assert_eq!(
            repository.task_extra_roots(&task).unwrap(),
            vec!["/tmp/a".to_string(), "/tmp/b".to_string()]
        );
    }

    /// `NULL` is what every row written before this change carries, and what a
    /// cleared root set writes. It must read as "no linked roots", never as an
    /// error: inheritance is a capability increment, so a bad value must not be
    /// able to keep a task from starting.
    #[test]
    fn null_blank_and_malformed_extra_roots_all_read_as_empty() {
        let (repository, task, _directory) = repository_with_one_task();
        for stored in [None, Some(""), Some("   "), Some("not json"), Some("{}")] {
            repository
                .connection_for_test()
                .execute(
                    "UPDATE tasks SET extra_roots = ?1 WHERE id = ?2",
                    params![stored, task.to_string()],
                )
                .unwrap();
            assert_eq!(
                repository.task_extra_roots(&task).unwrap(),
                Vec::<String>::new(),
                "stored value {stored:?} must read as an empty root set"
            );
        }
    }

    /// An empty set is stored as SQL `NULL`, so a row written by this code is
    /// indistinguishable from one that predates the column.
    #[test]
    fn setting_no_roots_stores_sql_null() {
        let (mut repository, task, _directory) = repository_with_one_task();
        repository
            .set_task_extra_roots(&task, &["/tmp/a".to_string()])
            .unwrap();
        repository.set_task_extra_roots(&task, &[]).unwrap();
        let stored: Option<String> = repository
            .connection_for_test()
            .query_row(
                "SELECT extra_roots FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored.is_none(), "empty roots must clear the column");
    }

    #[test]
    fn the_schema_ceiling_matches_the_latest_migration() {
        // Deliberately not a hardcoded number: bumping the migration must not
        // require editing this test, and the test's job is the *agreement* between
        // the constant and the probe anyway.
        assert_eq!(
            LATEST_SCHEMA_VERSION as u32,
            crate::schema_compat::SUPPORTED_SCHEMA_MAX
        );
        assert_eq!(LATEST_SCHEMA_VERSION, 19);
    }

    /// v16 records who last owned a root and why it ended.
    #[test]
    fn a_version_fifteen_database_gains_the_root_owners_table() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        // Roll back to the v15 shape. `migrate` gates on `MAX(version)`, so deleting
        // only the v16 row would leave a higher row behind and v16 would never re-run;
        // every later version must go too. Written relative to the ceiling so each
        // future migration does not have to edit this test.
        connection
            .execute_batch(
                "DROP TABLE IF EXISTS root_owners;
                 DELETE FROM schema_migrations WHERE version >= 16;",
            )
            .unwrap();
        drop(connection);

        let repository = RuntimeRepository::open(&path).unwrap();
        let version = repository.schema_version().unwrap();
        assert_eq!(version, LATEST_SCHEMA_VERSION);
        let connection = rusqlite::Connection::open(&path).unwrap();
        let has_table: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='root_owners')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_table, "v16 must create root_owners");
    }

    /// v18 adds the freeze tables, and they survive a reopen.
    ///
    /// The point is not "the DDL ran" but "a restarted daemon can still learn what a
    /// freeze covered": after a crash the in-memory set is gone, and without this the
    /// only available answer would be "nothing was frozen", which reads as stopped.
    #[test]
    fn a_version_seventeen_database_gains_the_freeze_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE IF EXISTS foreground_frozen_executions;
                 DROP TABLE IF EXISTS foreground_freeze;
                 DELETE FROM schema_migrations WHERE version >= 18;",
            )
            .unwrap();
        drop(connection);

        let repository = RuntimeRepository::open(&path).unwrap();
        assert_eq!(repository.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        let connection = rusqlite::Connection::open(&path).unwrap();
        for table in ["foreground_freeze", "foreground_frozen_executions"] {
            let present: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "v18 must create {table}");
        }
    }

    /// v19 persists the container role and its admission record.
    ///
    /// Rolled back to the v18 shape so this exercises the real upgrade an existing
    /// installation performs, not a fresh database that was never anything else.
    #[test]
    fn a_version_eighteen_database_gains_session_container_tables() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TABLE IF EXISTS session_host_executions;
                 DROP TABLE IF EXISTS session_migration_errors;
                 DROP TABLE IF EXISTS session_binding_operations;
                 DROP TABLE IF EXISTS session_containers;
                 ALTER TABLE tasks DROP COLUMN role;
                 DELETE FROM schema_migrations WHERE version >= 19;",
            )
            .unwrap();
        drop(connection);

        let repository = RuntimeRepository::open(&path).unwrap();
        assert_eq!(repository.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        let connection = rusqlite::Connection::open(&path).unwrap();
        for table in [
            "session_containers",
            "session_binding_operations",
            "session_migration_errors",
            "session_host_executions",
        ] {
            let present: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(present, "v19 must create {table}");
        }
    }

    /// Re-running the v16 step must not fail on the objects it already created.
    ///
    /// SQLite cannot roll back `ALTER TABLE`/`CREATE TABLE` inside the same
    /// transaction that a later statement aborts, so a crash between the DDL and the
    /// version row leaves the object without the version. The step probes first for
    /// exactly that reason.
    #[test]
    fn the_v16_step_is_rerunnable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        // Simulate "DDL landed, version row did not".
        connection
            .execute_batch("DELETE FROM schema_migrations WHERE version = 16;")
            .unwrap();
        drop(connection);

        // Reopening replays v16 over the already-created table.
        RuntimeRepository::open(&path).unwrap();
    }

    /// The fresh-open tests never exercise the upgrade an existing install
    /// performs, so roll a database back to exactly the v14 shape and reopen.
    #[test]
    fn a_version_fourteen_database_gains_the_extra_roots_column() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite");
        RuntimeRepository::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE tasks DROP COLUMN extra_roots;
                 DELETE FROM schema_migrations WHERE version >= 15;",
            )
            .unwrap();
        drop(connection);

        let repository = RuntimeRepository::open(&path).unwrap();
        // The `migrate` path always lands on the newest version, so asserting the
        // latest here keeps this test about the upgrade method rather than about a
        // number that every later migration would have to edit.
        assert_eq!(repository.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        let connection = repository.connection_for_test();
        assert!(
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'extra_roots')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap(),
            "a v14 database must gain tasks.extra_roots"
        );
    }
}

#[cfg(test)]
mod read_only_checkpoint_tests {
    use super::*;

    fn context(kind: &str) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:/tmp/plain".into()),
            worktree_lease: None,
            checkpoint_json: format!(r#"{{"kind":"{kind}"}}"#),
            tool_state_json: r#"{"state":"available"}"#.into(),
        }
    }

    /// A read-only worker runs in place, so it has no worktree to recover into.
    /// The checkpoint kind is what says so; an absent lease alone must not be
    /// read as permission, or a coding worker that forgot its lease would slip
    /// through.
    #[test]
    fn a_read_only_checkpoint_admits_a_worker_without_a_worktree_lease() {
        validate_recovery_context(&context("read_only_checkpoint"))
            .expect("a read-only position is a legitimate place to run");
    }

    #[test]
    fn an_unknown_checkpoint_kind_still_requires_a_worktree_lease() {
        let error = validate_recovery_context(&context("worker_admission"))
            .expect_err("an ordinary admission must still name a worktree");
        assert!(
            error.to_string().contains("worktree lease is missing"),
            "{error}"
        );
    }
}

impl RuntimeRepository {
    /// Record which client last attached `root`, and in which generation.
    ///
    /// `ended_reason` of `None` clears a previous terminal reason: a root that is
    /// being rebound in a new generation is live again, and a stale "client left"
    /// row would make the next restart refuse to reuse it.
    pub fn record_root_owner(
        &self,
        root: &str,
        owner: &str,
        generation: &str,
        ended_reason: Option<&str>,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO root_owners (root, owner, generation, ended_reason)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(root) DO UPDATE SET
                 owner = excluded.owner,
                 generation = excluded.generation,
                 ended_reason = excluded.ended_reason,
                 updated_at = CURRENT_TIMESTAMP",
            params![root, owner, generation, ended_reason],
        )?;
        Ok(())
    }

    /// Record that `root` stopped accepting work, keeping the first reason.
    pub fn mark_root_exited(&self, root: &str, reason: &str) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE root_owners
                SET ended_reason = COALESCE(ended_reason, ?2),
                    updated_at = CURRENT_TIMESTAMP
              WHERE root = ?1",
            params![root, reason],
        )?;
        Ok(())
    }

    /// The last recorded owner and reason for a root, if any.
    pub fn root_owner(
        &self,
        root: &str,
    ) -> Result<Option<(String, String, Option<String>)>, RepositoryError> {
        let mut statement = self
            .connection
            .prepare("SELECT owner, generation, ended_reason FROM root_owners WHERE root = ?1")?;
        let mut rows = statement.query(params![root])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?))),
            None => Ok(None),
        }
    }
}
