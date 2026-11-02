use super::*;

impl RuntimeRepository {
    /// Persists a permission wait with the immutable request payload before an
    /// operator can see or resolve it.
    pub fn request_permission(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        request: &PermissionRequestId,
        payload_json: &str,
    ) -> Result<i64, RepositoryError> {
        serde_json::from_str::<serde_json::Value>(payload_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'waiting_for_permission', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND active_attempt_id = ?2 AND state_json = 'running'",
            params![task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'waiting_for_permission'
             WHERE id = ?1 AND task_id = ?2",
            params![attempt.to_string(), task.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO permission_requests (id, task_id, payload_json, state)
             VALUES (?1, ?2, ?3, 'pending')",
            params![request.to_string(), task.to_string(), payload_json],
        )?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "request_id": request,
            "request": serde_json::from_str::<serde_json::Value>(payload_json)?,
        }))?;
        let event_id = append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::PermissionRequested,
            &event_payload,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Resolves exactly one pending request and atomically aligns its request
    /// record, task/attempt snapshot, and actor-attributed audit event.
    pub fn resolve_permission(
        &mut self,
        request: &PermissionRequestId,
        decision: PermissionDecision,
        actor_json: &str,
    ) -> Result<i64, RepositoryError> {
        serde_json::from_str::<serde_json::Value>(actor_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task_id = transaction
            .query_row(
                "SELECT task_id FROM permission_requests WHERE id = ?1 AND state = 'pending'",
                [request.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::PermissionRequestNotPending {
                request: request.to_string(),
            })?;
        let task: TaskId = task_id.parse().map_err(|_| RepositoryError::TaskNotFound {
            task: task_id.clone(),
        })?;
        let (request_state, task_state) = match decision {
            PermissionDecision::Allow => ("allowed", "queued"),
            PermissionDecision::Deny => ("denied", "blocked"),
        };
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND state_json = 'waiting_for_permission'",
            params![task_state, task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::PermissionRequestNotPending {
                request: request.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1, ended_at = CASE WHEN ?1 = 'blocked'
                THEN CURRENT_TIMESTAMP ELSE ended_at END
             WHERE id = (SELECT active_attempt_id FROM tasks WHERE id = ?2) AND task_id = ?2",
            params![task_state, task.to_string()],
        )?;
        transaction.execute(
            "UPDATE permission_requests SET state = ?1, decided_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND state = 'pending'",
            params![request_state, request.to_string()],
        )?;
        let event_payload = serde_json::to_string(&serde_json::json!({
            "request_id": request,
            "decision": decision,
            "actor": serde_json::from_str::<serde_json::Value>(actor_json)?,
        }))?;
        let event_id = append_event_with_actor_payload(
            &transaction,
            &task,
            RuntimeEvent::PermissionResolved,
            actor_json,
            &event_payload,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn permission_request_state(
        &self,
        request: &PermissionRequestId,
    ) -> Result<String, RepositoryError> {
        self.connection
            .query_row(
                "SELECT state FROM permission_requests WHERE id = ?1",
                [request.to_string()],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    /// Locates the task owning a still-pending request without accepting an
    /// actor- or client-supplied task ID at the daemon boundary.
    pub fn pending_permission_task(
        &self,
        request: &PermissionRequestId,
    ) -> Result<TaskId, RepositoryError> {
        let (task_id, state) = self
            .connection
            .query_row(
                "SELECT task_id, state FROM permission_requests WHERE id = ?1",
                [request.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::PermissionRequestNotFound {
                request: request.to_string(),
            })?;
        if state != "pending" {
            return Err(RepositoryError::PermissionRequestNotPending {
                request: request.to_string(),
            });
        }
        task_id
            .parse()
            .map_err(|_| RepositoryError::TaskNotFound { task: task_id })
    }
}
