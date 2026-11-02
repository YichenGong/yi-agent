use super::*;

impl RuntimeRepository {
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
        //
        // This is "did the task settle?" rather than a list of settled states:
        // the list used to be `blocked | cancelled | failed`, which silently
        // omitted `completed`. The reducer sends a Completed with no delivery
        // evidence (terminal replay) down exactly this path, so every such child
        // stranded its `worker-worktree-context-*` and
        // `worker-workspace-context-*` leases as `active` forever. Recovery
        // states are excluded on purpose: those are parked and resumable, and
        // their lease is what a resume needs.
        if SETTLED_TASK_STATES.contains(&state) {
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
    #[allow(clippy::result_large_err)]
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
}
