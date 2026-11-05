//! Durable foreground-root facts: classification, activity and recovery receipts.
//!
//! Three questions, three tables (created by the v17 migration):
//!
//! * **classification** — is this root a worker attempt or a foreground container,
//!   and is it currently allowed to do business?
//! * **activity** — what is the conversation doing, independent of `TaskState`?
//! * **recovery** — which recovery operations already happened, so a retry replays
//!   the receipt instead of minting a second successor attempt?
//!
//! The rules that decide *whether* a write is legal live in [`crate::foreground`]
//! (pure, no database). This module only persists their verdict. That split is what
//! keeps "a late finish must not overwrite a newer turn" testable twice: once as a
//! reducer unit test, and once against a real row.

use super::*;
use crate::foreground::{ActivityApply, ForegroundError, RootExecutionClass, reduce_activity};
use yi_agent_lifecycle::foreground::{
    ActivityPhase, ActivitySnapshot, ActivityUpdate, ExecutionIdentity, ExecutionKind,
    ForegroundFence, RecoveryResult, StopEvidence,
};

/// The migration's `classification` column, as a value rather than a string.
///
/// `Legacy` is the only shape an existing installation can have: an attachment
/// written before this feature. It is still recognised as a foreground session, but
/// it keeps the `Execution` budget policy until an authenticated prepare moves it,
/// because silently promoting a *running* root into an unbounded container would
/// remove the only thing that ever stops a dead delegation tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundClassification {
    Legacy,
    Pending,
    Managed,
}

impl ForegroundClassification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Pending => "pending",
            Self::Managed => "managed",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryError> {
        match value {
            "legacy" => Ok(Self::Legacy),
            "pending" => Ok(Self::Pending),
            "managed" => Ok(Self::Managed),
            _ => Err(RepositoryError::Foreground(ForegroundError::CorruptBinding)),
        }
    }

    /// The watchdog policy this classification asks for.
    ///
    /// `Legacy` deliberately maps to `Execution`: the migration only records the
    /// foreground *role*, it does not grant the new budget policy.
    pub fn execution_class(self) -> RootExecutionClass {
        match self {
            Self::Legacy => RootExecutionClass::Execution,
            Self::Pending => RootExecutionClass::ForegroundPending,
            Self::Managed => RootExecutionClass::ForegroundManaged,
        }
    }
}

/// Whether the root may accept new business, as far as persistence can know.
///
/// The plan's schema only allows `closed` and `frozen` here, and that is the whole
/// point: **open is runtime-only state**. A restarted daemon has no authorization, so
/// it always opens with every root closed; if "open" were a persisted value, a crash
/// between two writes could leave a root admitting business on a generation that
/// never authenticated it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionState {
    /// Persisted default: no business until a live generation authorizes this root.
    Closed,
    /// A recovery freeze. Also refuses business, and additionally refuses the
    /// delegation that recovery has not yet proven stopped.
    Frozen,
}

impl AdmissionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Frozen => "frozen",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryError> {
        match value {
            "closed" => Ok(Self::Closed),
            "frozen" => Ok(Self::Frozen),
            _ => Err(RepositoryError::Foreground(ForegroundError::CorruptBinding)),
        }
    }

    /// No persisted admission state admits business. A caller that needs "may I
    /// dispatch?" must consult the live authority table, not this row.
    pub fn admits_business(self) -> bool {
        false
    }
}

/// One persisted foreground row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedForegroundRoot {
    pub root: TaskId,
    pub classification: ForegroundClassification,
    pub binding_epoch: u64,
    pub driver_epoch: u64,
    pub owner_client_id: Option<String>,
    pub admission: AdmissionState,
    pub driver_bind_operation_id: Option<String>,
}

/// What a driver bind committed, as the daemon must report it back.
///
/// Deliberately no capability: the narrow driver secret is minted and held by the
/// coordinator's live authority, never by a persisted row. A row that could be read
/// back would hand a future process the rights of a dead driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverBindingRecord {
    /// The epoch this driver owns. Monotonic per root and never reused.
    pub driver_epoch: u64,
    /// The last activity sequence this root has committed. The driver's first report
    /// is `last_seq + 1`; recovery must not depend on a client guessing zero.
    pub last_seq: u64,
    /// The session the root belongs to, so the caller can build a complete fence
    /// without a second read that could observe a different attempt.
    pub session: String,
    /// The attempt the root is on *now*, read in the same transaction as the epoch.
    pub attempt: String,
}

/// The attempt one root's row currently points at.
///
/// Read inside a write transaction so a caller cannot observe a CAS that already
/// moved, and `None` for a root whose task has vanished -- which is a corrupt row, not
/// an empty answer.
fn active_attempt(
    transaction: &rusqlite::Transaction<'_>,
    root: &TaskId,
) -> Result<Option<String>, RepositoryError> {
    Ok(transaction
        .query_row(
            "SELECT active_attempt_id FROM tasks WHERE id = ?1",
            params![root.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// The committed activity sequence of one root, if a snapshot exists.
fn activity_seq_of(
    connection: &rusqlite::Connection,
    root: &TaskId,
) -> Result<Option<u64>, RepositoryError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT activity_json FROM foreground_root_state WHERE root_task_id = ?1",
            params![root.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(stored) = stored else {
        return Ok(None);
    };
    let snapshot: ActivitySnapshot = serde_json::from_str(&stored)?;
    Ok(Some(snapshot.update.seq))
}

impl RuntimeRepository {
    /// Persists the initial (or replacement) foreground identity of one root.
    ///
    /// This is the *only* writer of `classification`, and it is not reachable from
    /// the wire: Task 3 authenticates a prepare, Task 7 proves the stop boundary.
    /// A direct call here is how the migration tests build a `managed` row to check
    /// hydration; it does not mean the root passed authentication.
    pub fn record_foreground_root(
        &mut self,
        row: &PersistedForegroundRoot,
    ) -> Result<(), RepositoryError> {
        let binding_epoch = i64::try_from(row.binding_epoch).map_err(|_| {
            RepositoryError::InvalidForegroundFact {
                root: row.root.to_string(),
                reason: "binding epoch exceeds the SQLite integer range".into(),
            }
        })?;
        let driver_epoch = i64::try_from(row.driver_epoch).map_err(|_| {
            RepositoryError::InvalidForegroundFact {
                root: row.root.to_string(),
                reason: "driver epoch exceeds the SQLite integer range".into(),
            }
        })?;
        self.connection.execute(
            "INSERT INTO foreground_root_state
                (root_task_id, binding_epoch, driver_epoch, owner_client_id,
                 admission_state, driver_bind_operation_id, activity_json, classification)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)
             ON CONFLICT(root_task_id) DO UPDATE SET
                 binding_epoch = excluded.binding_epoch,
                 driver_epoch = excluded.driver_epoch,
                 owner_client_id = excluded.owner_client_id,
                 admission_state = excluded.admission_state,
                 driver_bind_operation_id = excluded.driver_bind_operation_id,
                 classification = excluded.classification",
            params![
                row.root.to_string(),
                binding_epoch,
                driver_epoch,
                row.owner_client_id,
                row.admission.as_str(),
                row.driver_bind_operation_id,
                row.classification.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn persisted_foreground_root(
        &self,
        root: &TaskId,
    ) -> Result<Option<PersistedForegroundRoot>, RepositoryError> {
        let row = self
            .connection
            .query_row(
                "SELECT root_task_id, classification, binding_epoch, driver_epoch,
                        owner_client_id, admission_state, driver_bind_operation_id
                 FROM foreground_root_state WHERE root_task_id = ?1",
                params![root.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((root_task_id, classification, binding_epoch, driver_epoch, owner, admission, op)) =
            row
        else {
            return Ok(None);
        };
        Ok(Some(PersistedForegroundRoot {
            root: root_task_id
                .parse()
                .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
            classification: ForegroundClassification::parse(&classification)?,
            binding_epoch: u64::try_from(binding_epoch)
                .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
            driver_epoch: u64::try_from(driver_epoch)
                .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
            owner_client_id: owner,
            admission: AdmissionState::parse(&admission)?,
            driver_bind_operation_id: op,
        }))
    }

    /// Classifies one root, from persistence plus the attachment it must agree with.
    ///
    /// The order matters: a persisted row wins (it is a decision someone
    /// authenticated), and only a root with no row falls back to "does an
    /// attachment exist". A persisted row whose root disappeared, or whose
    /// attachment contradicts it, is `Corrupt` — never silently reclassified,
    /// because guessing an exemption is how an immortal container gets created.
    pub fn foreground_class(&self, root: &TaskId) -> Result<RootExecutionClass, RepositoryError> {
        let Some(persisted) = self.persisted_foreground_root(root)? else {
            // No explicit row. Every root is an execution task until an authenticated
            // prepare says otherwise, so an attachment alone changes nothing: that is
            // exactly the legacy policy (the plan's `application` assertion).
            return Ok(RootExecutionClass::Execution);
        };
        if !self.task_exists(&persisted.root)? {
            return Ok(RootExecutionClass::Corrupt);
        }
        // A `legacy` row claims a foreground role, so it must still agree with an
        // attachment. The rows this migration can produce never do this (it writes
        // none); a mismatch means a row was written by hand or a task was deleted
        // from under it.
        if persisted.classification == ForegroundClassification::Legacy
            && !self.root_has_attachment(&persisted.root)?
        {
            return Ok(RootExecutionClass::Corrupt);
        }
        Ok(persisted.classification.execution_class())
    }

    /// Whether the worker watchdog may settle this root.
    ///
    /// One call site for the policy, so the candidate query and the terminal commit
    /// cannot disagree about it (Task 5 re-checks it inside the commit transaction).
    pub fn root_is_watchdog_governed(&self, root: &TaskId) -> Result<bool, RepositoryError> {
        Ok(crate::foreground::worker_watchdog_applies(
            self.foreground_class(root)?,
        ))
    }

    fn task_exists(&self, task: &TaskId) -> Result<bool, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
            params![task.to_string()],
            |row| row.get::<_, bool>(0),
        )?)
    }

    /// Does this root (task or its session) carry an application attachment?
    ///
    /// An attachment on a *different* task of the same session does not count: it
    /// names the root, and matching only the session would let a child inherit its
    /// root's exemption.
    fn root_has_attachment(&self, root: &TaskId) -> Result<bool, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM application_root_attachments a
                 WHERE a.root_task_id = ?1
             )",
            params![root.to_string()],
            |row| row.get::<_, bool>(0),
        )?)
    }

    /// Binds (or re-binds) the driver of one managed root.
    ///
    /// The whole decision is one transaction, for the same reason the admission gate
    /// is one critical section: "is this operation id the current binding?" and
    /// "issue the next driver epoch" must not be separable, or a retry that lands
    /// between them mints a second epoch for the same bind.
    ///
    /// Three outcomes, all decided here rather than by the caller:
    ///
    /// * the same `operation_id` as the recorded binding replays that binding — the
    ///   transport may have lost the first answer, and re-minting would silently
    ///   invalidate the driver that is actually running;
    /// * a *different* operation against a root whose current binding was never
    ///   proven stopped is refused. Task 7 is what proves a stop; until then the only
    ///   legal second bind is a replay;
    /// * a root with no binding yet gets driver epoch 1.
    ///
    /// `binding_epoch` and the attempt come from the caller's validated authority, not
    /// from this row: a row that disagreed with the authority is a corrupt binding,
    /// not something to quietly overwrite.
    pub fn bind_foreground_driver(
        &mut self,
        root: &TaskId,
        owner: &str,
        binding_epoch: u64,
        operation_id: &str,
    ) -> Result<DriverBindingRecord, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Named rather than a six-tuple: the fields end up in three different checks, and
        // positional access is how `owner_id` and `admission` get swapped.
        struct BindingRow {
            classification: String,
            persisted_epoch: i64,
            driver_epoch: i64,
            owner_id: Option<String>,
            admission: String,
            operation: Option<String>,
        }
        let row = transaction
            .query_row(
                "SELECT classification, binding_epoch, driver_epoch, owner_client_id,
                        admission_state, driver_bind_operation_id
                 FROM foreground_root_state WHERE root_task_id = ?1",
                params![root.to_string()],
                |row| {
                    Ok(BindingRow {
                        classification: row.get(0)?,
                        persisted_epoch: row.get(1)?,
                        driver_epoch: row.get(2)?,
                        owner_id: row.get(3)?,
                        admission: row.get(4)?,
                        operation: row.get(5)?,
                    })
                },
            )
            .optional()?;
        let Some(BindingRow {
            classification,
            persisted_epoch,
            driver_epoch,
            owner_id,
            admission,
            operation,
        }) = row
        else {
            // No managed row: a driver may only bind a root an authenticated prepare
            // classified. Creating the row here would let the wire mint the
            // classification that the prepare is supposed to be the only writer of.
            return Err(RepositoryError::Foreground(
                ForegroundError::RecoveryAuthorityMissing,
            ));
        };
        if ForegroundClassification::parse(&classification)? != ForegroundClassification::Managed {
            return Err(RepositoryError::Foreground(
                ForegroundError::RecoveryAuthorityMissing,
            ));
        }
        if AdmissionState::parse(&admission)? == AdmissionState::Frozen {
            return Err(RepositoryError::Foreground(
                ForegroundError::AdmissionClosed,
            ));
        }
        let persisted_epoch = u64::try_from(persisted_epoch)
            .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
        let driver_epoch = u64::try_from(driver_epoch)
            .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
        if persisted_epoch != binding_epoch {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        // Ownership is part of the authority, not a hint: another owner's operation id
        // must not be able to replay (and thereby adopt) this root's driver.
        if owner_id.as_deref() != Some(owner) {
            return Err(RepositoryError::Foreground(
                ForegroundError::AuthorityDenied,
            ));
        }
        // Session and attempt are read in the same transaction as the epoch, so a
        // fence built from this record cannot mix two attempts.
        let (session, attempt) = transaction.query_row(
            "SELECT root_session_id, active_attempt_id FROM tasks WHERE id = ?1",
            params![root.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        let last_seq = activity_seq_of(&transaction, root)?.unwrap_or(0);

        if operation.as_deref() == Some(operation_id) {
            // A replay answers with what was issued, never with a new epoch.
            return Ok(DriverBindingRecord {
                driver_epoch,
                last_seq,
                session,
                attempt,
            });
        }
        if driver_epoch > 0 && operation.is_some() {
            // A live binding exists and no stop proof was supplied. Refusing is the
            // honest answer: Task 7 is the only caller allowed to rotate a driver.
            return Err(RepositoryError::Foreground(ForegroundError::StopUnproven(
                vec![driver_epoch.to_string()],
            )));
        }
        let next_epoch = driver_epoch.saturating_add(1);
        let changed = transaction.execute(
            "UPDATE foreground_root_state
             SET driver_epoch = ?1, driver_bind_operation_id = ?2
             WHERE root_task_id = ?3 AND driver_epoch = ?4
               AND (driver_bind_operation_id IS ?5)",
            params![
                i64::try_from(next_epoch)
                    .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
                operation_id,
                root.to_string(),
                i64::try_from(driver_epoch)
                    .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
                operation,
            ],
        )?;
        if changed == 0 {
            // Someone else bound in between: the CAS is what makes the epoch
            // monotonic even when two binds genuinely race (a Task 7 rotation and a
            // transport retry, say).
            return Err(RepositoryError::Foreground(
                ForegroundError::OperationConflict,
            ));
        }
        transaction.commit()?;
        Ok(DriverBindingRecord {
            driver_epoch: next_epoch,
            last_seq,
            session,
            attempt,
        })
    }

    /// Freezes one root for a rebind and returns the execution set that freeze owns.
    ///
    /// The whole point is that "close admission" and "here is the complete set" are
    /// one step: the store writes the freeze row, the frozen set and the version in a
    /// single transaction, so a later read cannot observe a frozen root with a set
    /// that still admits members. The version increments per freeze, which is what
    /// makes evidence from an earlier freeze unable to license a later one.
    ///
    /// Membership comes from the caller because only the coordinator knows its live
    /// slots; the store's job is to persist them atomically and to bump the version.
    pub fn freeze_foreground_root(
        &mut self,
        fence: &ForegroundFence,
        executions: &[ExecutionIdentity],
        reason: &str,
    ) -> Result<(u64, Vec<ExecutionIdentity>), RepositoryError> {
        if reason.trim().is_empty() {
            // A freeze without a reason cannot be shown to a user, and an empty
            // string is a placeholder, not an explanation.
            return Err(RepositoryError::Foreground(ForegroundError::CorruptBinding));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // The fence must still describe the row: freezing an attempt that has since
        // been replaced would close the wrong conversation.
        let persisted = transaction
            .query_row(
                "SELECT f.binding_epoch, f.driver_epoch, t.active_attempt_id, t.root_session_id
                 FROM foreground_root_state f JOIN tasks t ON t.id = f.root_task_id
                 WHERE f.root_task_id = ?1",
                params![fence.root.root_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((binding_epoch, driver_epoch, attempt, session)) = persisted else {
            return Err(RepositoryError::Foreground(
                ForegroundError::RecoveryAuthorityMissing,
            ));
        };
        if u64::try_from(binding_epoch).ok() != Some(fence.binding_epoch)
            || u64::try_from(driver_epoch).ok() != Some(fence.driver_epoch)
        {
            return Err(RepositoryError::Foreground(ForegroundError::StaleDriver));
        }
        if attempt != fence.root.attempt_id || session != fence.root.session_id {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        let previous: Option<i64> = transaction
            .query_row(
                "SELECT execution_set_version FROM foreground_freeze WHERE root_task_id = ?1",
                params![fence.root.root_id],
                |row| row.get(0),
            )
            .optional()?;
        let version = previous
            .map(|value| {
                u64::try_from(value)
                    .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))
            })
            .transpose()?
            .unwrap_or(0)
            .saturating_add(1);
        transaction.execute(
            "INSERT INTO foreground_freeze (root_task_id, execution_set_version)
             VALUES (?1, ?2)
             ON CONFLICT(root_task_id) DO UPDATE SET
                 execution_set_version = excluded.execution_set_version,
                 frozen_at = CURRENT_TIMESTAMP",
            params![
                fence.root.root_id,
                i64::try_from(version)
                    .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?
            ],
        )?;
        // A new freeze replaces the previous set rather than appending to it: the old
        // members were proven against the old version and cannot be carried forward.
        transaction.execute(
            "DELETE FROM foreground_frozen_executions WHERE root_task_id = ?1",
            params![fence.root.root_id],
        )?;
        let mut frozen: Vec<ExecutionIdentity> = Vec::new();
        for execution in executions {
            if execution.attempt_id != fence.root.attempt_id {
                // A member from another attempt is not part of this root's current
                // execution set. Persisting it would make the set unprovable by
                // construction, so refuse the freeze instead of silently dropping it.
                return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
            }
            if !frozen.iter().any(|seen| seen == execution) {
                transaction.execute(
                    "INSERT OR IGNORE INTO foreground_frozen_executions
                         (root_task_id, execution_set_version, execution_id, task_id,
                          attempt_id, kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        fence.root.root_id,
                        i64::try_from(version).map_err(|_| RepositoryError::Foreground(
                            ForegroundError::CorruptBinding
                        ))?,
                        execution.execution_id,
                        execution.task_id,
                        execution.attempt_id,
                        match execution.kind {
                            ExecutionKind::Driver => "driver",
                            ExecutionKind::Worker => "worker",
                        },
                    ],
                )?;
                frozen.push(execution.clone());
            }
        }
        // The row itself records the freeze, so `bind` refuses a rebind until a
        // recovery clears it. `closed` would look like an ordinary exit.
        transaction.execute(
            "UPDATE foreground_root_state SET admission_state = 'frozen'
             WHERE root_task_id = ?1",
            params![fence.root.root_id],
        )?;
        transaction.commit()?;
        frozen.sort();
        Ok((version, frozen))
    }

    /// The executions belonging to one root's current freeze, with its version.
    ///
    /// Returns `None` when the root was never frozen, so "no freeze" stays
    /// distinguishable from "a freeze whose set happens to be empty".
    pub fn frozen_execution_set(
        &self,
        root: &TaskId,
    ) -> Result<Option<(u64, Vec<ExecutionIdentity>)>, RepositoryError> {
        let version: Option<i64> = self
            .connection
            .query_row(
                "SELECT execution_set_version FROM foreground_freeze WHERE root_task_id = ?1",
                params![root.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(version) = version else {
            return Ok(None);
        };
        let version = u64::try_from(version)
            .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
        let mut statement = self.connection.prepare(
            "SELECT execution_id, task_id, attempt_id, kind
             FROM foreground_frozen_executions
             WHERE root_task_id = ?1 AND execution_set_version = ?2",
        )?;
        let rows = statement.query_map(params![root.to_string(), version], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut frozen = Vec::new();
        for row in rows {
            let (execution_id, task_id, attempt_id, kind) = row?;
            frozen.push(ExecutionIdentity {
                execution_id,
                task_id,
                attempt_id,
                kind: match kind.as_str() {
                    "driver" => ExecutionKind::Driver,
                    "worker" => ExecutionKind::Worker,
                    _ => return Err(RepositoryError::Foreground(ForegroundError::CorruptBinding)),
                },
            });
        }
        frozen.sort();
        Ok(Some((version, frozen)))
    }

    /// Records one trusted party's observation about an execution.
    ///
    /// Two parties may write about the same execution (the daemon's join, the
    /// guardian's tool groups), and the surviving record must carry both facts. So a
    /// second write merges rather than replaces: `joined` and `tools_stopped` are
    /// each sticky once proven, and the source lists stay append-only by keeping the
    /// first provenance. Writing a `false` never erases an earlier `true`, which
    /// would let a slow observer undo a proof.
    pub fn record_stop_evidence(
        &mut self,
        evidence: &StopEvidence,
    ) -> Result<StopEvidence, RepositoryError> {
        // A record that disagrees with itself cannot be merged meaningfully: the
        // attempt in the row key and the attempt inside the execution would then
        // describe two different runs, and whichever won would make a proof look
        // wider than it is.
        if evidence.attempt_id != evidence.execution.attempt_id
            || evidence.attempt_id != evidence.fence.root.attempt_id
        {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let executions = transaction.query_row(
            "SELECT COUNT(*) FROM foreground_frozen_executions
             WHERE root_task_id = ?1 AND execution_id = ?2 AND attempt_id = ?3",
            params![
                evidence.fence.root.root_id,
                evidence.execution.execution_id,
                evidence.execution.attempt_id
            ],
            |row| row.get::<_, i64>(0),
        )?;
        if executions == 0 {
            // Evidence for an execution that is not in the frozen set describes
            // something else, and accepting it would let a client accumulate proofs
            // for executions it wants to appear stopped.
            return Err(RepositoryError::Foreground(ForegroundError::StopUnproven(
                vec![evidence.execution.execution_id.clone()],
            )));
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT evidence_json FROM foreground_stop_evidence
                 WHERE root_task_id = ?1 AND attempt_id = ?2 AND execution_id = ?3",
                params![
                    evidence.fence.root.root_id,
                    evidence.attempt_id,
                    evidence.execution.execution_id
                ],
                |row| row.get(0),
            )
            .optional()?;
        let merged = match existing {
            Some(json) => {
                let previous: StopEvidence = serde_json::from_str(&json)?;
                StopEvidence {
                    evidence_id: previous.evidence_id.clone(),
                    fence: previous.fence.clone(),
                    execution: previous.execution.clone(),
                    attempt_id: previous.attempt_id.clone(),
                    source: previous.source,
                    joined: previous.joined || evidence.joined,
                    tools_stopped: previous.tools_stopped || evidence.tools_stopped,
                    process_identity: previous
                        .process_identity
                        .clone()
                        .or_else(|| evidence.process_identity.clone()),
                }
            }
            None => evidence.clone(),
        };
        transaction.execute(
            "INSERT INTO foreground_stop_evidence
                 (root_task_id, attempt_id, execution_id, evidence_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(root_task_id, attempt_id, execution_id) DO UPDATE SET
                 evidence_json = excluded.evidence_json",
            params![
                evidence.fence.root.root_id,
                merged.attempt_id,
                merged.execution.execution_id,
                serde_json::to_string(&merged)?
            ],
        )?;
        transaction.commit()?;
        Ok(merged)
    }

    /// Every stored record for one root's current attempt.
    ///
    /// Scoped to the attempt so a proof from the previous attempt can never be
    /// returned as if it described the current one.
    pub fn stop_evidence_for(
        &self,
        root: &TaskId,
        attempt: &str,
    ) -> Result<Vec<StopEvidence>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT evidence_json FROM foreground_stop_evidence
             WHERE root_task_id = ?1 AND attempt_id = ?2
             ORDER BY execution_id",
        )?;
        let rows = statement.query_map(params![root.to_string(), attempt], |row| {
            row.get::<_, String>(0)
        })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(serde_json::from_str(&row?)?);
        }
        Ok(records)
    }

    /// The proof a caller referenced, as the daemon itself stored it.
    ///
    /// A `QuiescenceRef` is only an address. The coordinator must not believe the
    /// address alone: it reads back the record and re-checks the fence, the
    /// execution and the two facts, so a caller cannot mint "stopped" by naming a
    /// row that either does not exist or proves only half of it.
    pub fn stop_evidence_by_id(
        &self,
        root: &TaskId,
        evidence_id: &str,
    ) -> Result<Option<StopEvidence>, RepositoryError> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT evidence_json FROM foreground_stop_evidence
                 WHERE root_task_id = ?1 AND execution_id = ?2",
                params![root.to_string(), evidence_id],
                |row| row.get(0),
            )
            .optional()?;
        match stored {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// The committed activity snapshot, if one exists.
    ///
    /// A historical `running` phase is projected to `Unknown`: after a restart the
    /// daemon cannot know whether that turn is still alive, and claiming `idle`
    /// would turn a possibly-live turn into permission to start another one. The
    /// stored snapshot is left untouched because it is diagnostic evidence.
    pub fn foreground_activity(
        &self,
        root: &TaskId,
    ) -> Result<Option<ActivitySnapshot>, RepositoryError> {
        let stored: Option<(Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT activity_json, classification FROM foreground_root_state
                 WHERE root_task_id = ?1",
                params![root.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((activity_json, _)) = stored else {
            return Ok(None);
        };
        let Some(activity_json) = activity_json else {
            return Ok(None);
        };
        let mut snapshot: ActivitySnapshot = serde_json::from_str(&activity_json)?;
        snapshot.update.phase = project_reloaded_phase(snapshot.update.phase);
        Ok(Some(snapshot))
    }

    /// Applies one activity report, or explains why it may not be applied.
    ///
    /// The reduction is the pure [`reduce_activity`]; this function only supplies the
    /// committed row and writes the result. `Duplicate` and `Stale` write nothing, so
    /// a replayed report cannot refresh `changed_at` and thereby extend anything.
    pub fn apply_foreground_activity(
        &mut self,
        update: &ActivityUpdate,
    ) -> Result<ActivitySnapshot, RepositoryError> {
        let root: TaskId = update
            .fence
            .root
            .root_id
            .parse()
            .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // The fence is validated against the *persisted* identity, in the same
        // transaction that writes the snapshot. Reading it from a cached copy would
        // let a bind committed after the read be overwritten by a report that was
        // minted for the previous driver.
        let identity: Option<(String, i64, i64, String)> = transaction
            .query_row(
                "SELECT classification, binding_epoch, driver_epoch, admission_state
                 FROM foreground_root_state WHERE root_task_id = ?1",
                params![root.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((classification, binding_epoch, driver_epoch, admission)) = identity else {
            // An activity report for a root no authenticated prepare ever
            // classified: there is no driver this daemon could have issued, so the
            // report cannot be about one.
            return Err(RepositoryError::Foreground(
                ForegroundError::RecoveryAuthorityMissing,
            ));
        };
        if ForegroundClassification::parse(&classification)? != ForegroundClassification::Managed {
            return Err(RepositoryError::Foreground(
                ForegroundError::RecoveryAuthorityMissing,
            ));
        }
        if AdmissionState::parse(&admission)? == AdmissionState::Frozen {
            return Err(RepositoryError::Foreground(
                ForegroundError::AdmissionClosed,
            ));
        }
        if i64::try_from(update.fence.binding_epoch).ok() != Some(binding_epoch) {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        if i64::try_from(update.fence.driver_epoch).ok() != Some(driver_epoch) {
            // A superseded driver is a different answer from a superseded attempt:
            // the first means "reconnect and re-bind", the second means "this
            // conversation has moved on".
            return Err(RepositoryError::Foreground(ForegroundError::StaleDriver));
        }
        let (active_session, active_attempt) = transaction.query_row(
            "SELECT root_session_id, active_attempt_id FROM tasks WHERE id = ?1",
            params![root.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        if update.fence.root.session_id != active_session {
            // A report that names another session's path to this root is not about
            // this root, whatever else agrees: refuse rather than reconcile.
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        if update.fence.root.attempt_id != active_attempt {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        let current: Option<String> = transaction
            .query_row(
                "SELECT activity_json FROM foreground_root_state WHERE root_task_id = ?1",
                params![root.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let committed = current
            .as_deref()
            .map(serde_json::from_str::<ActivitySnapshot>)
            .transpose()?;

        let Some(committed) = committed else {
            // The first report ever. It is still not exempt from the turn rules:
            // a synthetic `idle` baseline at the same fence is ordered against the
            // real reducer, so a driver cannot open the conversation in
            // `waiting_for_children` (a state only meaningful inside a turn) or in
            // `unknown` (a phase only the daemon may produce). `seq` starts at 0,
            // so the first report must be at least 1.
            let baseline = ActivityUpdate {
                fence: update.fence.clone(),
                turn_id: None,
                seq: 0,
                phase: ActivityPhase::Idle,
            };
            match reduce_activity(&baseline, update)? {
                ActivityApply::Applied | ActivityApply::Duplicate => {}
                ActivityApply::Stale => {
                    return Err(RepositoryError::Foreground(
                        ForegroundError::InvalidActivity,
                    ));
                }
            }
            let snapshot = ActivitySnapshot {
                update: update.clone(),
                changed_at: Utc::now().to_rfc3339(),
            };
            transaction.execute(
                "UPDATE foreground_root_state SET activity_json = ?1 WHERE root_task_id = ?2",
                params![serde_json::to_string(&snapshot)?, root.to_string()],
            )?;
            transaction.commit()?;
            return Ok(snapshot);
        };

        // `Duplicate` and `Stale` return the committed snapshot unchanged: replaying a
        // report must not refresh `changed_at`, or an old message could look newer
        // than the turn that actually replaced it.
        match reduce_activity(&committed.update, update)? {
            ActivityApply::Applied => {}
            ActivityApply::Duplicate | ActivityApply::Stale => {
                transaction.commit()?;
                return Ok(committed);
            }
        }

        let snapshot = ActivitySnapshot {
            update: update.clone(),
            changed_at: Utc::now().to_rfc3339(),
        };
        transaction.execute(
            "UPDATE foreground_root_state SET activity_json = ?1 WHERE root_task_id = ?2",
            params![serde_json::to_string(&snapshot)?, root.to_string()],
        )?;
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Whether one root's active attempt carries a `stalled` watchdog terminal.
    ///
    /// Recovery is only for a root the watchdog settled by mistake. Anything else --
    /// completed, failed, cancelled, `recovery_required` -- is a real terminal state,
    /// and reading it as recoverable would let a caller undo a decision that was
    /// actually made.
    pub fn foreground_attempt_is_stalled(&self, root: &TaskId) -> Result<bool, RepositoryError> {
        let terminal: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT attempts.terminal_json
                 FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
                 WHERE tasks.id = ?1",
                params![root.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(terminal.flatten().is_some_and(|json| {
            serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|value| {
                    value
                        .get("outcome")
                        .and_then(|outcome| outcome.get("kind"))
                        .and_then(|kind| kind.as_str())
                        .map(|kind| kind == "stalled")
                })
                .unwrap_or(false)
        }))
    }

    /// The terminal JSON of the root's first attempt, which is the settled one.
    ///
    /// Keyed by attempt number rather than "everything but the active attempt": before
    /// a recovery the stalled attempt *is* active, and after it the same row is
    /// history, so only the number names the same row on both sides.
    pub fn first_attempt_terminal_json(
        &self,
        root: &TaskId,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT terminal_json FROM attempts
                 WHERE task_id = ?1 ORDER BY number ASC LIMIT 1",
                params![root.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The durable receipt of a committed recovery, if that exact operation happened.
    ///
    /// `None` means "no committed result"; it does **not** mean "safe to start a new
    /// operation" — an in-flight prepare is also `None` here and is answered by
    /// `foreground_recovery_phase`.
    pub fn foreground_recovery_result(
        &self,
        root: &TaskId,
        operation: &str,
    ) -> Result<Option<RecoveryResult>, RepositoryError> {
        let result: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT result_json FROM foreground_recovery_operations
                 WHERE root_task_id = ?1 AND operation_id = ?2 AND phase = 'committed'",
                params![root.to_string(), operation],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        match result {
            None | Some(None) => Ok(None),
            Some(Some(json)) => Ok(Some(serde_json::from_str(&json)?)),
        }
    }

    /// The stored phase of one recovery operation, including the non-success ones.
    pub fn foreground_recovery_phase(
        &self,
        root: &TaskId,
        operation: &str,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT phase FROM foreground_recovery_operations
                 WHERE root_task_id = ?1 AND operation_id = ?2",
                params![root.to_string(), operation],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Records the prepared shape of one recovery operation.
    ///
    /// Written before any stop request goes out, because the set being stopped must
    /// be reconstructible after a crash: a daemon that stopped executions and then
    /// died would otherwise leave a root with no record of what it had already
    /// closed. The fingerprint binds the request, so a different request under the
    /// same operation id is a conflict rather than a silent reuse.
    pub fn record_foreground_recovery_prepare(
        &mut self,
        root: &TaskId,
        operation: &str,
        expected_attempt: &str,
        fingerprint: &str,
        request_json: &str,
        impact: &yi_agent_lifecycle::foreground::RecoveryImpact,
    ) -> Result<(), RepositoryError> {
        if operation.trim().is_empty() || fingerprint.trim().is_empty() {
            return Err(RepositoryError::Foreground(
                ForegroundError::OperationConflict,
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = transaction
            .query_row(
                "SELECT request_fingerprint, expected_attempt_id
                 FROM foreground_recovery_operations
                 WHERE root_task_id = ?1 AND operation_id = ?2",
                params![root.to_string(), operation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored, stored_attempt)) = existing {
            if stored != fingerprint {
                // Same id, different request: replaying either answer would be a lie
                // about one of the two.
                return Err(RepositoryError::Foreground(
                    ForegroundError::OperationConflict,
                ));
            }
            if stored_attempt != expected_attempt {
                return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
            }
            transaction.execute(
                "UPDATE foreground_recovery_operations
                 SET phase = 'ready', impact_json = ?1
                 WHERE root_task_id = ?2 AND operation_id = ?3 AND phase != 'committed'",
                params![serde_json::to_string(impact)?, root.to_string(), operation],
            )?;
            transaction.commit()?;
            return Ok(());
        }
        let ready = active_attempt(&transaction, root)?
            .filter(|attempt| attempt == expected_attempt)
            .ok_or(RepositoryError::Foreground(ForegroundError::StaleAttempt))?;
        let _ = ready;
        transaction.execute(
            "INSERT INTO foreground_recovery_operations
                (root_task_id, operation_id, expected_attempt_id, request_fingerprint,
                 original_request_json, phase, impact_json, result_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 'ready', ?6, NULL)",
            params![
                root.to_string(),
                operation,
                expected_attempt,
                fingerprint,
                request_json,
                serde_json::to_string(impact)?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Commits one successor attempt, or answers with the receipt already stored.
    ///
    /// Everything is re-checked inside the transaction: the operation's fingerprint,
    /// the expected attempt, the root's live state and the frozen set. That is not
    /// belt-and-braces -- the confirmation was issued earlier, so between it and this
    /// commit the attempt could have moved, and acting on the old view would create a
    /// successor for a conversation that had already recovered.
    ///
    /// A failed step rolls the whole transaction back, so a refusal never leaves half
    /// an attempt behind.
    pub fn commit_foreground_recovery(
        &mut self,
        root: &TaskId,
        operation: &str,
        fingerprint: &str,
        expected_attempt: &str,
        successor: &str,
    ) -> Result<RecoveryResult, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<(String, String, String, Option<String>)> = transaction
            .query_row(
                "SELECT request_fingerprint, expected_attempt_id, phase, result_json
                 FROM foreground_recovery_operations
                 WHERE root_task_id = ?1 AND operation_id = ?2",
                params![root.to_string(), operation],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_fingerprint, stored_attempt, phase, result)) = stored else {
            return Err(RepositoryError::Foreground(
                ForegroundError::ConfirmationRequired,
            ));
        };
        if stored_fingerprint != fingerprint {
            return Err(RepositoryError::Foreground(
                ForegroundError::OperationConflict,
            ));
        }
        if stored_attempt != expected_attempt {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        if phase == "committed" {
            // A replay answers with the receipt. Re-creating the attempt would give
            // one operation two successors.
            let Some(result) = result else {
                return Err(RepositoryError::Foreground(ForegroundError::CorruptBinding));
            };
            return Ok(serde_json::from_str(&result)?);
        }
        if phase != "ready" {
            return Err(RepositoryError::Foreground(
                ForegroundError::ConfirmationRequired,
            ));
        }
        // The old attempt must still be the active one and still be stalled. A root
        // that recovered, settled or moved on in the meantime has nothing to recover.
        let (active, session): (String, String) = transaction.query_row(
            "SELECT active_attempt_id, root_session_id FROM tasks WHERE id = ?1",
            params![root.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if active != expected_attempt {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        let stalled: Option<String> = transaction
            .query_row(
                "SELECT terminal_json FROM attempts WHERE id = ?1",
                params![expected_attempt],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let stalled_reason = stalled.as_deref().unwrap_or_default();
        if !stalled_reason.contains("stalled") {
            // Recovery is for a root the watchdog settled by mistake. A completed,
            // failed or cancelled root is not "recoverable" -- that would resurrect a
            // real terminal state.
            return Err(RepositoryError::Foreground(
                ForegroundError::RecoveryRequired,
            ));
        }
        let number: i64 = transaction.query_row(
            "SELECT number FROM attempts WHERE id = ?1",
            params![expected_attempt],
            |row| row.get(0),
        )?;
        let next = u32::try_from(number)
            .ok()
            .and_then(|number| number.checked_add(1))
            .ok_or(RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
        let successor_id: AttemptId = successor
            .parse()
            .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
        // The successor has no worker: recovery re-points the root, it does not
        // silently restart work the user never asked to resume.
        insert_attempt(&transaction, &successor_id, root, next, "running")?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'running', active_attempt_id = ?1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3",
            params![successor, root.to_string(), expected_attempt],
        )?;
        if changed == 0 {
            return Err(RepositoryError::Foreground(ForegroundError::StaleAttempt));
        }
        // The old attempt keeps its stalled state, its evidence and its timestamps:
        // history is what lets a user see why the recovery happened.
        let binding_epoch: i64 = transaction.query_row(
            "SELECT binding_epoch FROM foreground_root_state WHERE root_task_id = ?1",
            params![root.to_string()],
            |row| row.get(0),
        )?;
        let driver_epoch: i64 = transaction.query_row(
            "SELECT driver_epoch FROM foreground_root_state WHERE root_task_id = ?1",
            params![root.to_string()],
            |row| row.get(0),
        )?;
        let next_binding = binding_epoch.saturating_add(1);
        let next_driver = driver_epoch.saturating_add(1);
        transaction.execute(
            "UPDATE foreground_root_state
             SET binding_epoch = ?1, driver_epoch = ?2, activity_json = NULL,
                 driver_bind_operation_id = NULL, admission_state = 'closed'
             WHERE root_task_id = ?3",
            params![next_binding, next_driver, root.to_string()],
        )?;
        // The freeze belonged to the attempt that is now history.
        transaction.execute(
            "DELETE FROM foreground_frozen_executions WHERE root_task_id = ?1",
            params![root.to_string()],
        )?;
        transaction.execute(
            "DELETE FROM foreground_freeze WHERE root_task_id = ?1",
            params![root.to_string()],
        )?;
        let result = RecoveryResult {
            operation_id: operation.to_string(),
            previous: yi_agent_lifecycle::foreground::RootRef {
                session_id: session,
                root_id: root.to_string(),
                attempt_id: expected_attempt.to_string(),
            },
            successor: yi_agent_lifecycle::foreground::RootRef {
                session_id: transaction.query_row(
                    "SELECT root_session_id FROM tasks WHERE id = ?1",
                    params![root.to_string()],
                    |row| row.get(0),
                )?,
                root_id: root.to_string(),
                attempt_id: successor.to_string(),
            },
            binding_epoch: u64::try_from(next_binding)
                .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
            driver_epoch: u64::try_from(next_driver)
                .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?,
        };
        transaction.execute(
            "UPDATE foreground_recovery_operations
             SET phase = 'committed', result_json = ?1
             WHERE root_task_id = ?2 AND operation_id = ?3",
            params![serde_json::to_string(&result)?, root.to_string(), operation],
        )?;
        transaction.commit()?;
        Ok(result)
    }

    /// Rewrites every uncommitted recovery operation to `needs_preview`.
    ///
    /// The confirmation token lives in memory, so after a restart a `ready`
    /// operation cannot be committed; the freeze is preserved but the host must
    /// prepare again under valid authority. Calling this is part of startup, not of
    /// a request path.
    pub fn expire_uncommitted_recovery_operations(&mut self) -> Result<usize, RepositoryError> {
        Ok(self.connection.execute(
            "UPDATE foreground_recovery_operations SET phase = 'needs_preview'
             WHERE phase IN ('preparing', 'blocked', 'ready')",
            [],
        )?)
    }

    /// Cancels foreground roots that were still `pending` when the daemon died.
    ///
    /// A `pending` root is a reservation made by a prepare that never completed. It
    /// holds no execution — nothing ran under it — but it also holds no surviving
    /// authorization: the prepare, its operation id and its deadline all lived in the
    /// dead generation. Leaving it alone would leave a root that refuses business
    /// forever, and promoting it to `managed` would hand out an exemption nobody
    /// proved. Cancelling is the only honest outcome.
    ///
    /// Only rows whose task is still live are touched, so a root that already settled
    /// keeps its real terminal state and its real evidence.
    pub fn cancel_pending_foreground_roots(&mut self) -> Result<Vec<TaskId>, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let pending = {
            let mut statement = transaction.prepare(
                "SELECT f.root_task_id FROM foreground_root_state f
                 JOIN tasks t ON t.id = f.root_task_id
                 WHERE f.classification = 'pending'
                   AND t.state_json IN ('queued', 'running', 'waiting_for_resource',
                                        'waiting_for_permission', 'waiting_for_children')",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut cancelled = Vec::new();
        for value in pending {
            let root: TaskId = value
                .parse()
                .map_err(|_| RepositoryError::Foreground(ForegroundError::CorruptBinding))?;
            let terminal_json = serde_json::to_string(&serde_json::json!({
                "reason": "pending_foreground_root_abandoned",
            }))?;
            transaction.execute(
                "UPDATE tasks SET state_json = 'cancelled', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1",
                params![root.to_string()],
            )?;
            transaction.execute(
                "UPDATE attempts SET state = 'cancelled', ended_at = CURRENT_TIMESTAMP,
                     terminal_json = ?1
                 WHERE id = (SELECT active_attempt_id FROM tasks WHERE id = ?2)",
                params![terminal_json, root.to_string()],
            )?;
            transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE task_id = ?1 AND state = 'active'",
                params![root.to_string()],
            )?;
            transaction.execute(
                "UPDATE foreground_root_state SET classification = 'legacy',
                     admission_state = 'closed'
                 WHERE root_task_id = ?1",
                params![root.to_string()],
            )?;
            append_event(&transaction, &root, RuntimeEvent::TaskCancelled)?;
            cancelled.push(root);
        }
        transaction.commit()?;
        Ok(cancelled)
    }

    /// Closes business admission for every persisted foreground root.
    ///
    /// This is startup's default: a new daemon generation holds no authorization, so
    /// a `managed` root keeps its identity (it must not be demoted to `Execution`,
    /// which would hand it back to the worker watchdog) but may not do business until
    /// it is re-authorized. `Pending` roots are additionally reaped by the startup
    /// sweep, because nothing authenticates them any more.
    pub fn close_foreground_admission(&mut self) -> Result<usize, RepositoryError> {
        Ok(self.connection.execute(
            "UPDATE foreground_root_state SET admission_state = 'closed'",
            [],
        )?)
    }
}

/// A phase as it must be *read* after a restart.
///
/// Only `running` is rewritten. `waiting_*` is already an unknown wait detail (it is
/// legal for a live daemon to be unsure whether it is waiting on children or on a
/// permission prompt), and `idle`/`unknown` are already correct.
fn project_reloaded_phase(phase: ActivityPhase) -> ActivityPhase {
    match phase {
        ActivityPhase::Running => ActivityPhase::Unknown,
        other => other,
    }
}

#[cfg(test)]
#[path = "foreground_tests.rs"]
mod tests;
