use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;
use yi_agent_core::{RootSessionId, TaskId};

const LATEST_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error("unknown runtime event kind: {kind}")]
    UnknownEventKind { kind: String },
    #[error("task does not exist: {task}")]
    TaskNotFound { task: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEvent {
    TaskQueued,
    TaskStarted,
    TaskRecoveryRequired,
}

impl RuntimeEvent {
    fn name(self) -> &'static str {
        match self {
            Self::TaskQueued => "task_queued",
            Self::TaskStarted => "task_started",
            Self::TaskRecoveryRequired => "task_recovery_required",
        }
    }

    fn parse(kind: String) -> Result<Self, RepositoryError> {
        match kind.as_str() {
            "task_queued" => Ok(Self::TaskQueued),
            "task_started" => Ok(Self::TaskStarted),
            "task_recovery_required" => Ok(Self::TaskRecoveryRequired),
            _ => Err(RepositoryError::UnknownEventKind { kind }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedEvent {
    pub id: i64,
    pub task_id: TaskId,
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedTask {
    pub task_id: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSubscriptionSnapshot {
    pub high_water_event_id: i64,
    pub tasks: Vec<PersistedTask>,
    pub events: Vec<PersistedEvent>,
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
        transaction.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE state = 'active' AND resource_key NOT LIKE 'worktree:%'",
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

    pub fn events_after(&self, cursor: i64) -> Result<Vec<RuntimeEvent>, RepositoryError> {
        Ok(self
            .event_records_after(cursor)?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    pub fn latest_event_id(&self) -> Result<i64, RepositoryError> {
        Ok(self
            .connection
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })?)
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

    /// Read task snapshots, a high-water mark, and replay in one SQLite read transaction.
    pub fn subscription_snapshot(
        &mut self,
        after_event_id: i64,
    ) -> Result<RuntimeSubscriptionSnapshot, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let high_water_event_id =
            transaction.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })?;
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
        let events = {
            let mut statement = transaction.prepare(
                "SELECT id, task_id, kind FROM events WHERE id > ?1 AND id <= ?2 ORDER BY id",
            )?;
            statement
                .query_map(params![after_event_id, high_water_event_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .map(|row| {
                    let (id, task_id, kind) = row?;
                    Ok(PersistedEvent {
                        id,
                        task_id: task_id.parse().map_err(|_| {
                            RepositoryError::UnknownEventKind {
                                kind: format!("invalid task ID in store: {task_id}"),
                            }
                        })?,
                        event: RuntimeEvent::parse(kind)?,
                    })
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?
        };
        transaction.commit()?;
        Ok(RuntimeSubscriptionSnapshot {
            high_water_event_id,
            tasks,
            events,
        })
    }

    pub fn event_records_after(&self, cursor: i64) -> Result<Vec<PersistedEvent>, RepositoryError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, task_id, kind FROM events WHERE id > ?1 ORDER BY id")?;
        statement
            .query_map(params![cursor], |row| {
                let task_id = row.get::<_, String>(1)?;
                Ok((row.get::<_, i64>(0)?, task_id, row.get::<_, String>(2)?))
            })?
            .map(|row| {
                let (id, task_id, kind) = row?;
                Ok(PersistedEvent {
                    id,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    event: RuntimeEvent::parse(kind)?,
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
    let inserted = transaction.execute(
        "INSERT INTO events (session_id, task_id, actor_json, kind, payload_json)
         SELECT root_session_id, id, '{\"kind\":\"runtime\"}', ?1, '{}'
         FROM tasks WHERE id = ?2",
        params![event.name(), task.to_string()],
    )?;
    if inserted == 0 {
        return Err(RepositoryError::TaskNotFound {
            task: task.to_string(),
        });
    }
    Ok(transaction.last_insert_rowid())
}

fn migrate(connection: &Connection) -> Result<(), RepositoryError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);",
    )?;
    let applied: Option<i64> = connection
        .query_row(
            "SELECT version FROM schema_migrations WHERE version = ?1",
            params![LATEST_SCHEMA_VERSION],
            |row| row.get(0),
        )
        .optional()?;
    if applied.is_some() {
        return Ok(());
    }
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
    transaction.execute(
        "INSERT INTO schema_migrations (version) VALUES (?1)",
        params![LATEST_SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}
