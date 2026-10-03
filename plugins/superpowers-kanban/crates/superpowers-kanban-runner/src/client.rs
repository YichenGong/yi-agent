use std::path::{Path, PathBuf};

use superpowers_kanban_ipc::client::{self, ClientError};
use superpowers_kanban_ipc::wire::{Command, Reply};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub session_id: String,
    pub root_task_id: String,
}

#[derive(Debug, Clone)]
pub struct BoardDaemon {
    socket: PathBuf,
}

impl BoardDaemon {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// daemon 是否还在应答。最便宜的一次读：拿到任何应答即算活着。
    pub fn is_alive(&self) -> bool {
        client::send(&self.socket, Command::Status).is_ok()
    }

    /// Asks the daemon for a new autonomous session in `workdir`.
    pub fn create_session(
        &self,
        objective: &str,
        workdir: &Path,
    ) -> Result<CreatedSession, ClientError> {
        match client::send(
            &self.socket,
            Command::CreateAutonomousSession {
                objective: objective.to_owned(),
                workdir: workdir.to_string_lossy().to_string(),
            },
        )? {
            Reply::AutonomousSessionCreated {
                session_id,
                root_task_id,
            } => Ok(CreatedSession {
                session_id,
                root_task_id,
            }),
            Reply::Error { code, message } => Err(ClientError::Malformed(format!(
                "daemon refused the session: {code}: {}",
                message.unwrap_or_default()
            ))),
            other => Err(ClientError::Malformed(format!(
                "unexpected reply to CreateAutonomousSession: {other:?}"
            ))),
        }
    }

    /// The daemon's state string for `task_id`, or `None` when it is not listed.
    pub fn task_state(&self, task_id: &str) -> Result<Option<String>, ClientError> {
        match client::send(
            &self.socket,
            Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        )? {
            Reply::TaskSummaries { tasks } => Ok(tasks
                .into_iter()
                .find(|task| task.task_id == task_id)
                .map(|task| task.state)),
            Reply::Error { code, message } => Err(ClientError::Malformed(format!(
                "daemon refused the task listing: {code}: {}",
                message.unwrap_or_default()
            ))),
            other => Err(ClientError::Malformed(format!(
                "unexpected reply to ListTaskSummaries: {other:?}"
            ))),
        }
    }
}
