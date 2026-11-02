use std::path::Path;
use std::str::FromStr;

pub(crate) use chrono::{DateTime, Local, Utc};
pub(crate) use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
pub(crate) use yi_agent_core::subagent::task::{
    BudgetKind, DeliveryReport, MessageId, PermissionDecision, PermissionRequestId, TimeoutKind,
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
mod application_root;
mod event;
mod mailbox;
mod permission;
mod read;
mod schedule;
mod task;
mod trace;
mod watchdog;

const LATEST_SCHEMA_VERSION: i64 = 15;

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
    // A read-only worker runs in place, so there is no worktree to recover into.
    // The checkpoint kind is the explicit permission: it is never inferred from
    // an absent lease, because a coding worker that lost its lease would then be
    // admitted as if it were read-only.
    let checkpoint: serde_json::Value =
        serde_json::from_str(&context.checkpoint_json).map_err(|error| {
            RepositoryError::InvalidWorkerRecoveryContext {
                reason: format!("checkpoint evidence is invalid JSON: {error}"),
            }
        })?;
    let is_read_only =
        checkpoint.get("kind").and_then(serde_json::Value::as_str) == Some("read_only_checkpoint");
    if !is_read_only {
        let worktree = context
            .worktree_lease
            .as_deref()
            .filter(|lease| lease.starts_with("worktree:") && lease.len() > "worktree:".len())
            .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
                reason: "worktree lease is missing".into(),
            })?;
        if worktree == "worktree:" {
            return Err(RepositoryError::InvalidWorkerRecoveryContext {
                reason: "resource lease key is empty".into(),
            });
        }
    }
    if workspace == "workspace:" {
        return Err(RepositoryError::InvalidWorkerRecoveryContext {
            reason: "resource lease key is empty".into(),
        });
    }
    serde_json::from_str::<serde_json::Value>(&context.tool_state_json).map_err(|error| {
        RepositoryError::InvalidWorkerRecoveryContext {
            reason: format!("tool-state evidence is invalid JSON: {error}"),
        }
    })?;
    Ok(())
}

/// Deletes a task's oldest trace rows until it retains at most
/// [`TRACE_MAX_ROWS_PER_TASK`].
///
/// `id` is the insertion order (`AUTOINCREMENT`), so the 2000th newest id is the
/// watermark: everything strictly below it is the tail the ceiling drops. When
/// the trace is at or under the ceiling the inner query lands on the oldest
/// retained row (or nothing), and nothing is deleted. The caller owns the
/// transaction so the trim and the insert it belongs to commit together.
fn trim_trace_rows(transaction: &Transaction<'_>, task: &TaskId) -> Result<(), RepositoryError> {
    transaction.execute(
        "DELETE FROM task_trace_events
         WHERE task_id = ?1 AND id < (
             SELECT id FROM task_trace_events WHERE task_id = ?1
             ORDER BY id DESC LIMIT 1 OFFSET ?2
         )",
        params![task.to_string(), TRACE_MAX_ROWS_PER_TASK as i64 - 1],
    )?;
    Ok(())
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
            "CREATE TABLE application_root_attachments (
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

    if current_version < 8 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('application_root_attachments')
                WHERE name = 'capability_secret'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            transaction.execute_batch(
                "ALTER TABLE application_root_attachments ADD COLUMN capability_secret TEXT;",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (8)", [])?;
        transaction.commit()?;
    }

    if current_version < 9 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'workspace_mode'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Existing rows own worktrees, so backfill them as coding. New
            // inserts always specify the mode explicitly.
            transaction.execute_batch(
                "ALTER TABLE tasks ADD COLUMN workspace_mode TEXT NOT NULL DEFAULT 'coding';",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (9)", [])?;
        transaction.commit()?;
    }

    if current_version < 10 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'model'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Null means the child inherits its parent's model.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN model TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (10)", [])?;
        transaction.commit()?;
    }

    if current_version < 11 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('application_root_attachments')
                WHERE name = 'workspace_root'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // A read-only application root keeps no `task_workspaces` row, so its
            // project workspace is recorded here instead.
            transaction.execute_batch(
                "ALTER TABLE application_root_attachments ADD COLUMN workspace_root TEXT;",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (11)", [])?;
        transaction.commit()?;
    }

    if current_version < 12 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'inherited_sandbox'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // NULL means "no inheritance recorded": the worker factory falls
            // back to its configured sandbox, preserving pre-change behavior.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN inherited_sandbox TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (12)", [])?;
        transaction.commit()?;
    }

    // v13 rather than a widened v12: a database that already recorded v12 for
    // the inherited sandbox would never re-run v12, so folding the trace table
    // into it would leave those installations without `task_trace_events`.
    if current_version < 13 {
        let transaction = connection.unchecked_transaction()?;
        // A worker's trace lives apart from the audit `events` table: it is
        // high-volume, prunable observation data, not durable history.
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS task_trace_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES tasks(id),
                kind TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS task_trace_task_id_idx ON task_trace_events(task_id, id);",
        )?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Null means the task belongs to no desktop conversation, which is
            // every task that predates this column.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN thread_id TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (13)", [])?;
        transaction.commit()?;
    }

    // v14：父审状态下线。历史行改写为 `completed`，并清掉 review-only 的
    // mailbox 消息与事件 kind——否则删除枚举后 `RuntimeEvent::parse` 会对
    // 残留 kind 直接报 UnknownEventKind，让 ReadTaskEvents 整条读取失败。
    // 父审通知行 kind 是仍在用的 'user_override'，靠 payload 里的
    // 'user_review_override' 区分；它们同样是 review 时代产物，必须一并清掉，
    // 否则重启后会被当成真的用户指令重放给父级。
    if current_version < 14 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "UPDATE tasks SET state_json = 'completed'
                 WHERE state_json = 'awaiting_parent_review';
             UPDATE attempts SET state = 'completed'
                 WHERE state = 'awaiting_parent_review';
             DELETE FROM mailbox_messages
                 WHERE kind IN ('rework', 'review_rejected')
                    OR (kind = 'user_override'
                        AND sender_task_id IS NULL
                        AND json_extract(payload_json, '$.kind') = 'user_review_override');
             UPDATE events SET kind = 'task_completed'
                 WHERE kind IN ('review_approved', 'review_accepted',
                                'review_rework', 'review_rejected');",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (14)", [])?;
        transaction.commit()?;
    }

    // v15：根会话的关联根。会话创建时快照进根任务行，子任务按父链继承。
    if current_version < 15 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'extra_roots'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // JSON array of canonical paths. NULL means "no linked roots": the
            // worker falls back to its single-root behavior, so pre-change rows
            // keep today's semantics.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN extra_roots TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (15)", [])?;
        transaction.commit()?;
    }
    Ok(())
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
        assert_eq!(LATEST_SCHEMA_VERSION, 15);
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
        assert_eq!(repository.schema_version().unwrap(), 15);
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
        assert_eq!(repository.schema_version().unwrap(), 15);
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
    fn schema_version_is_fifteen() {
        assert_eq!(LATEST_SCHEMA_VERSION, 15);
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
        assert_eq!(repository.schema_version().unwrap(), 15);
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
