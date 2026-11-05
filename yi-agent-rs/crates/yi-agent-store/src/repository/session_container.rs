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
    /// Set only for the one prepare that spends a legacy capability. Re-checked
    /// inside the transaction so two concurrent upgrades cannot both succeed.
    pub consume_legacy_upgrade: bool,
    /// Executions of the driver being replaced, each with the trusted evidence
    /// that retired it. Written in the same transaction as the new epochs: a
    /// binding that moved while its predecessor still reads `running` would be
    /// claiming a stop the daemon never proved.
    pub retired_executions: Vec<RetiredHostExecution>,
}

/// One old-generation execution retired by the commit that replaces its driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredHostExecution {
    pub execution_id: String,
    pub evidence_json: String,
    pub execution_set_version: u64,
}

/// The evidence one host records for one of its own executions.
///
/// A struct rather than seven arguments: the fence is the security-critical part,
/// and a positional call site is exactly where the wrong epoch would slip in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHostStop {
    pub execution_id: String,
    pub root: String,
    pub generation: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
    pub evidence_json: String,
    pub execution_set_version: u64,
}

/// One row of the host execution set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHostExecution {
    pub execution_id: String,
    pub root_task_id: String,
    pub generation: String,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
    pub turn_id: String,
    pub identity_json: String,
    pub phase: String,
    pub evidence_json: Option<String>,
    pub execution_set_version: u64,
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

        let legacy_consumed: i64 = transaction
            .query_row(
                "SELECT legacy_upgrade_consumed FROM session_containers WHERE root_task_id = ?1",
                params![commit.root.root_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if commit.consume_legacy_upgrade && legacy_consumed != 0 {
            return Err(RepositoryError::SessionDelegation(SessionFailure::new(
                SessionFailureCode::AuthorityDenied,
                "this legacy capability was already consumed".to_string(),
            )));
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
                 resume_digest = COALESCE(?5, resume_digest),
                 legacy_upgrade_consumed = legacy_upgrade_consumed | ?6
             WHERE root_task_id = ?1",
            params![
                commit.root.root_id,
                next_binding,
                next_driver,
                commit.owner_run_id,
                commit.resume_digest,
                i64::from(commit.consume_legacy_upgrade),
            ],
        )?;
        for retired in &commit.retired_executions {
            let changed = transaction.execute(
                "UPDATE session_host_executions
                 SET phase = 'stopped', evidence_json = ?2, execution_set_version = ?3
                 WHERE execution_id = ?1
                   AND root_task_id = ?4
                   AND phase IN ('starting','running','unknown')",
                params![
                    retired.execution_id,
                    retired.evidence_json,
                    retired.execution_set_version,
                    commit.root.root_id,
                ],
            )?;
            if changed == 0 {
                return Err(RepositoryError::SessionDelegation(SessionFailure::new(
                    SessionFailureCode::CorruptAttachment,
                    format!(
                        "execution {} is not a live execution of this container",
                        retired.execution_id
                    ),
                )));
            }
        }
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

    /// Writes the resume digest of a container that does not have one yet.
    ///
    /// Create-only on purpose: a second attach must not be able to replace the
    /// secret a live host already holds, which is how "attach" would otherwise
    /// become "take over".
    pub fn set_session_resume_digest(
        &mut self,
        task: &TaskId,
        digest: &str,
    ) -> Result<bool, RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE session_containers SET resume_digest = ?2
             WHERE root_task_id = ?1 AND resume_digest IS NULL",
            params![task.to_string(), digest],
        )?;
        Ok(changed != 0)
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

impl RuntimeRepository {
    /// Executions of one container that are not proven stopped.
    ///
    /// `unknown` is included on purpose: it is the honest record of "we lost the
    /// channel". Treating it as absent would turn a missing proof into permission.
    pub fn unsettled_session_host_executions(
        &self,
        root: &TaskId,
    ) -> Result<Vec<SessionHostExecution>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT execution_id, root_task_id, generation, binding_epoch, driver_epoch,
                    turn_id, identity_json, phase, evidence_json, execution_set_version
             FROM session_host_executions
             WHERE root_task_id = ?1 AND phase IN ('starting','running','unknown')
             ORDER BY execution_id",
        )?;
        let rows = statement.query_map(params![root.to_string()], read_host_execution)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    /// Registers one execution of the current driver before it starts.
    ///
    /// Written first, answered second: a host that dies between the two leaves a
    /// `starting` row, which blocks the next driver rather than silently passing
    /// for "never started".
    pub fn register_session_host_execution(
        &mut self,
        execution: &SessionHostExecution,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "INSERT INTO session_host_executions
                (execution_id, root_task_id, generation, binding_epoch, driver_epoch,
                 turn_id, identity_json, phase, evidence_json, execution_set_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                execution.execution_id,
                execution.root_task_id,
                execution.generation,
                execution.binding_epoch,
                execution.driver_epoch,
                execution.turn_id,
                execution.identity_json,
                execution.phase,
                execution.evidence_json,
                execution.execution_set_version,
            ],
        )?;
        Ok(())
    }

    /// Records trusted evidence for exactly one execution of one fence.
    ///
    /// The fence is part of the `WHERE`: a host cannot retire an execution that
    /// belongs to another generation, binding or driver, even by guessing its id.
    pub fn record_session_host_stop(
        &mut self,
        stop: &SessionHostStop,
    ) -> Result<(), RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE session_host_executions
             SET phase = 'stopped', evidence_json = ?6, execution_set_version = ?7
             WHERE execution_id = ?1 AND root_task_id = ?2 AND generation = ?3
               AND binding_epoch = ?4 AND driver_epoch = ?5
               AND phase IN ('starting','running','unknown')",
            params![
                stop.execution_id,
                stop.root,
                stop.generation,
                stop.binding_epoch,
                stop.driver_epoch,
                stop.evidence_json,
                stop.execution_set_version,
            ],
        )?;
        if changed == 0 {
            return Err(RepositoryError::SessionDelegation(SessionFailure::new(
                SessionFailureCode::StaleBinding,
                "no live execution matches this fence".to_string(),
            )));
        }
        Ok(())
    }
}

fn read_host_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionHostExecution> {
    Ok(SessionHostExecution {
        execution_id: row.get(0)?,
        root_task_id: row.get(1)?,
        generation: row.get(2)?,
        binding_epoch: row.get::<_, i64>(3)? as u64,
        driver_epoch: row.get::<_, i64>(4)? as u64,
        turn_id: row.get(5)?,
        identity_json: row.get(6)?,
        phase: row.get(7)?,
        evidence_json: row.get(8)?,
        execution_set_version: row.get::<_, i64>(9)? as u64,
    })
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
