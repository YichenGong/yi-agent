use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;
use yi_agent_core::subagent::task::MessageId;
use yi_agent_core::subagent::worker::{WorkerRecoveryAttestation, WorkerRecoveryContext};
use yi_agent_core::{AttemptId, RootSessionId, TaskId};

const LATEST_SCHEMA_VERSION: i64 = 2;

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
    #[error("worker recovery context is not durable: {reason}")]
    InvalidWorkerRecoveryContext { reason: String },
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
    TaskBlocked,
    TaskFailed,
    TaskRecoveryRequired,
    TaskRecoveryAttested,
    MailboxMessageQueued,
    MailboxMessageConsumed,
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
            Self::TaskBlocked => "task_blocked",
            Self::TaskFailed => "task_failed",
            Self::TaskRecoveryRequired => "task_recovery_required",
            Self::TaskRecoveryAttested => "task_recovery_attested",
            Self::MailboxMessageQueued => "mailbox_message_queued",
            Self::MailboxMessageConsumed => "mailbox_message_consumed",
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
            "task_blocked" => Ok(Self::TaskBlocked),
            "task_failed" => Ok(Self::TaskFailed),
            "task_recovery_required" => Ok(Self::TaskRecoveryRequired),
            "task_recovery_attested" => Ok(Self::TaskRecoveryAttested),
            "mailbox_message_queued" => Ok(Self::MailboxMessageQueued),
            "mailbox_message_consumed" => Ok(Self::MailboxMessageConsumed),
            _ => Err(RepositoryError::UnknownEventKind { kind }),
        }
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
pub struct PersistedTask {
    pub task_id: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTaskDetail {
    pub task_id: String,
    pub session_id: String,
    pub parent_task_id: Option<String>,
    pub depth: u8,
    pub state: String,
    pub delivery_json: String,
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
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}') ON CONFLICT(id) DO NOTHING",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, ?3, 1, ?4, '{}')",
            params![task.to_string(), root.to_string(), state, attempt.to_string()],
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

    pub fn task_state(&self, task: &TaskId) -> Result<String, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT state_json FROM tasks WHERE id = ?1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
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
        let mut statement = self
            .connection
            .prepare("SELECT id, state_json FROM tasks ORDER BY created_at, id")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PersistedTask {
                    task_id: row.get(0)?,
                    state: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
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

    pub fn task_detail(&self, task: &TaskId) -> Result<PersistedTaskDetail, RepositoryError> {
        self.connection
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
                    })
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => RepositoryError::TaskNotFound {
                    task: task.to_string(),
                },
                error => RepositoryError::Sql(error),
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
            let mut statement =
                transaction.prepare("SELECT id, state_json FROM tasks ORDER BY created_at, id")?;
            statement
                .query_map([], |row| {
                    Ok(PersistedTask {
                        task_id: row.get(0)?,
                        state: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
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
    let inserted = transaction.execute(
        "INSERT INTO events (session_id, task_id, actor_json, kind, payload_json)
         SELECT root_session_id, id, '{\"kind\":\"runtime\"}', ?1, ?2
         FROM tasks WHERE id = ?3",
        params![event.name(), payload_json, task.to_string()],
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
    Ok(())
}
