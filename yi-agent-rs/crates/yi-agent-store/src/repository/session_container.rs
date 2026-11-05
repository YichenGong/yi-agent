//! Durable session-container role, admission record and binding CAS.
//!
//! The role answers "is this task a session that owns delegation, or an
//! execution that may be settled?" — a question that must survive a restart and
//! must *not* be re-derived from `parent_id`, cwd, thread ids or the watchdog's
//! managed classification. The container record answers a different question:
//! is *admission* open right now, and for which binding generation? A historical
//! `stalled` state answers neither.

use super::*;
use yi_agent_core::subagent::task::TaskRole;
use yi_agent_lifecycle::session_delegation::{
    PrepareSession, SessionFailure, SessionFailureCode, SessionRef,
};

/// One container's durable facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContainerRecord {
    pub root: SessionRef,
    pub closed: bool,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
    pub owner_run_id: Option<String>,
    pub resume_digest: Option<String>,
    pub legacy_upgrade_consumed: bool,
}

/// The single write a coordinator may commit after authenticating a prepare.
///
/// Deliberately not a business request: a client cannot reach this, which is why
/// `binding_epoch` can be trusted as a fence rather than a suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingCommit {
    pub root: SessionRef,
    pub expected_binding_epoch: u64,
    pub operation_id: String,
    pub request_digest: String,
    pub owner_run_id: String,
    pub generation: String,
    pub resume_digest: Option<String>,
}

impl RuntimeRepository {
    /// The persisted role of a task.
    ///
    /// Missing rows are `TaskNotFound`, never a guessed default: a caller that
    /// cannot find the task must not silently treat it as an execution.
    pub fn task_role(&self, task: &TaskId) -> Result<TaskRole, RepositoryError> {
        let role: String = self
            .connection
            .query_row(
                "SELECT role FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::TaskNotFound {
                task: task.to_string(),
            })?;
        match role.as_str() {
            "execution" => Ok(TaskRole::Execution),
            "session_container" => Ok(TaskRole::SessionContainer),
            other => Err(RepositoryError::InvalidForegroundFact {
                root: task.to_string(),
                reason: format!("unknown task role {other}"),
            }),
        }
    }

    pub fn session_container(
        &self,
        task: &TaskId,
    ) -> Result<Option<SessionContainerRecord>, RepositoryError> {
        self.connection
            .query_row(
                "SELECT root_task_id, root_session_id, closed, binding_epoch, driver_epoch,
                        owner_run_id, resume_digest, legacy_upgrade_consumed
                 FROM session_containers WHERE root_task_id = ?1",
                params![task.to_string()],
                |row| {
                    Ok(SessionContainerRecord {
                        root: SessionRef {
                            session_id: row.get(1)?,
                            root_id: row.get(0)?,
                        },
                        closed: row.get::<_, i64>(2)? != 0,
                        binding_epoch: row.get(3)?,
                        driver_epoch: row.get(4)?,
                        owner_run_id: row.get(5)?,
                        resume_digest: row.get(6)?,
                        legacy_upgrade_consumed: row.get::<_, i64>(7)? != 0,
                    })
                },
            )
            .optional()
            .map_err(RepositoryError::from)
    }

    /// Records a diagnostic for an attachment the migration refused to promote.
    pub fn session_migration_error(
        &self,
        attachment_key: &str,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT reason FROM session_migration_errors WHERE attachment_key = ?1",
                params![attachment_key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Persists one authenticated binding transition.
    ///
    /// Idempotent on `(root, operation_id)`: the same operation with the same
    /// request digest returns the committed epochs and never bumps them again.
    /// A different digest under the same id is a conflict — reusing an id is how
    /// a lost response would otherwise be turned into a second grant.
    pub fn commit_session_binding(
        &mut self,
        commit: &SessionBindingCommit,
    ) -> Result<SessionContainerRecord, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, i64, i64)> = transaction
            .query_row(
                "SELECT request_digest, binding_epoch, driver_epoch
                 FROM session_binding_operations
                 WHERE root_task_id = ?1 AND operation_id = ?2",
                params![commit.root.root_id, commit.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((digest, binding_epoch, _)) = existing {
            if digest != commit.request_digest {
                return Err(RepositoryError::SessionDelegation(SessionFailure::new(
                    SessionFailureCode::OwnerConflict,
                    format!(
                        "operation {} was already committed with a different request",
                        commit.operation_id
                    ),
                )));
            }
            let record = read_container(&transaction, &commit.root.root_id)?.ok_or_else(|| {
                RepositoryError::SessionDelegation(SessionFailure::new(
                    SessionFailureCode::CorruptAttachment,
                    format!("container {} disappeared", commit.root.root_id),
                ))
            })?;
            transaction.commit()?;
            let _ = binding_epoch;
            return Ok(record);
        }

        let current: Option<(i64, i64, Option<String>)> = transaction
            .query_row(
                "SELECT binding_epoch, driver_epoch, owner_run_id
                 FROM session_containers WHERE root_task_id = ?1",
                params![commit.root.root_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((binding_epoch, driver_epoch, _owner)) = current else {
            return Err(RepositoryError::SessionDelegation(SessionFailure::new(
                SessionFailureCode::CorruptAttachment,
                format!("{} is not a session container", commit.root.root_id),
            )));
        };
        if binding_epoch as u64 != commit.expected_binding_epoch {
            return Err(RepositoryError::SessionDelegation(SessionFailure::new(
                SessionFailureCode::StaleBinding,
                format!(
                    "expected binding epoch {} but container is at {}",
                    commit.expected_binding_epoch, binding_epoch
                ),
            )));
        }
        let next_binding = binding_epoch + 1;
        let next_driver = driver_epoch + 1;
        transaction.execute(
            "UPDATE session_containers
             SET binding_epoch = ?2, driver_epoch = ?3, owner_run_id = ?4,
                 resume_digest = COALESCE(?5, resume_digest)
             WHERE root_task_id = ?1",
            params![
                commit.root.root_id,
                next_binding,
                next_driver,
                commit.owner_run_id,
                commit.resume_digest,
            ],
        )?;
        transaction.execute(
            "INSERT INTO session_binding_operations
                (root_task_id, operation_id, request_digest, generation,
                 binding_epoch, driver_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                commit.root.root_id,
                commit.operation_id,
                commit.request_digest,
                commit.generation,
                next_binding,
                next_driver,
            ],
        )?;
        let record = read_container(&transaction, &commit.root.root_id)?.ok_or_else(|| {
            RepositoryError::SessionDelegation(SessionFailure::new(
                SessionFailureCode::CorruptAttachment,
                format!("container {} disappeared", commit.root.root_id),
            ))
        })?;
        transaction.commit()?;
        Ok(record)
    }

    /// Marks a session permanently closed and revokes admission.
    ///
    /// Idempotent. This is the tombstone a plain attach or a daemon restart must
    /// never clear; "detach" is a different, non-permanent operation.
    pub fn close_session_container(&mut self, task: &TaskId) -> Result<(), RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE session_containers SET closed = 1 WHERE root_task_id = ?1",
            params![task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        Ok(())
    }

    /// The container facts a prepare decision reads, plus the task role.
    #[allow(dead_code)] // consumed by the coordinator's prepare path (plan Task 5)
    pub(crate) fn session_prepare_facts(
        &self,
        request: &PrepareSession,
    ) -> Result<(TaskRole, SessionContainerRecord), RepositoryError> {
        let task: TaskId =
            request
                .root
                .root_id
                .parse()
                .map_err(|_| RepositoryError::TaskNotFound {
                    task: request.root.root_id.clone(),
                })?;
        let role = self.task_role(&task)?;
        let record = self.session_container(&task)?.ok_or_else(|| {
            RepositoryError::SessionDelegation(SessionFailure::new(
                SessionFailureCode::CorruptAttachment,
                format!("{} has no container record", request.root.root_id),
            ))
        })?;
        Ok((role, record))
    }

    /// The live binding, if any, for a container.
    #[allow(dead_code)] // consumed by the coordinator's prepare path (plan Task 5)
    pub(crate) fn session_binding_epoch(
        &self,
        root_task_id: &str,
    ) -> Result<Option<u64>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT binding_epoch FROM session_containers WHERE root_task_id = ?1",
                params![root_task_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|value| value as u64))
    }
}

fn read_container(
    connection: &Connection,
    root_task_id: &str,
) -> Result<Option<SessionContainerRecord>, RepositoryError> {
    connection
        .query_row(
            "SELECT root_task_id, root_session_id, closed, binding_epoch, driver_epoch,
                    owner_run_id, resume_digest, legacy_upgrade_consumed
             FROM session_containers WHERE root_task_id = ?1",
            params![root_task_id],
            |row| {
                Ok(SessionContainerRecord {
                    root: SessionRef {
                        session_id: row.get(1)?,
                        root_id: row.get(0)?,
                    },
                    closed: row.get::<_, i64>(2)? != 0,
                    binding_epoch: row.get(3)?,
                    driver_epoch: row.get(4)?,
                    owner_run_id: row.get(5)?,
                    resume_digest: row.get(6)?,
                    legacy_upgrade_consumed: row.get::<_, i64>(7)? != 0,
                })
            },
        )
        .optional()
        .map_err(RepositoryError::from)
}
