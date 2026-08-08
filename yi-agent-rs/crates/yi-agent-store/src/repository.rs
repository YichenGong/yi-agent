use std::path::Path;

use rusqlite::{Connection, params};
use thiserror::Error;
use yi_agent_core::{RootSessionId, TaskId};

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeEvent {
    TaskQueued,
    TaskStarted,
}

impl RuntimeEvent {
    fn name(self) -> &'static str {
        match self {
            Self::TaskQueued => "task_queued",
            Self::TaskStarted => "task_started",
        }
    }
}

pub struct RuntimeRepository {
    connection: Connection,
}

impl RuntimeRepository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RepositoryError> {
        let connection = Connection::open(path)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, root_id TEXT NOT NULL, state TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL, kind TEXT NOT NULL);")?;
        Ok(Self { connection })
    }

    pub fn create_task(
        &mut self,
        task: &TaskId,
        root: &RootSessionId,
        state: &str,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO tasks (id, root_id, state) VALUES (?1, ?2, ?3)",
            params![task.to_string(), root.to_string(), state],
        )?;
        Ok(())
    }

    pub fn append_event(
        &mut self,
        task: &TaskId,
        event: RuntimeEvent,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO events (task_id, kind) VALUES (?1, ?2)",
            params![task.to_string(), event.name()],
        )?;
        Ok(())
    }

    pub fn task_state(&self, task: &TaskId) -> Result<String, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT state FROM tasks WHERE id = ?1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn events_after(&self, cursor: i64) -> Result<Vec<RuntimeEvent>, RepositoryError> {
        let mut statement = self
            .connection
            .prepare("SELECT kind FROM events WHERE id > ?1 ORDER BY id")?;
        let events = statement
            .query_map(params![cursor], |row| {
                match row.get::<_, String>(0)?.as_str() {
                    "task_queued" => Ok(RuntimeEvent::TaskQueued),
                    _ => Ok(RuntimeEvent::TaskStarted),
                }
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(events)
    }
}
