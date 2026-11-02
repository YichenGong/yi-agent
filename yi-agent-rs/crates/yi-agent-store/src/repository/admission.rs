use super::*;

impl RuntimeRepository {
    pub fn save_admission_cursor(
        &mut self,
        resource_key: &str,
        root_id: Option<&RootSessionId>,
        parent_id: Option<&TaskId>,
        sequence: u64,
    ) -> Result<(), RepositoryError> {
        let sequence =
            i64::try_from(sequence).map_err(|_| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: "sequence exceeds SQLite integer range".into(),
            })?;
        self.connection.execute(
            "INSERT INTO resource_admission_cursors
             (resource_key, root_session_id, parent_task_id, sequence)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(resource_key) DO UPDATE SET
                 root_session_id = excluded.root_session_id,
                 parent_task_id = excluded.parent_task_id,
                 sequence = excluded.sequence,
                 updated_at = CURRENT_TIMESTAMP",
            params![
                resource_key,
                root_id.map(ToString::to_string),
                parent_id.map(ToString::to_string),
                sequence,
            ],
        )?;
        Ok(())
    }

    pub fn admission_cursor(
        &self,
        resource_key: &str,
    ) -> Result<Option<PersistedAdmissionCursor>, RepositoryError> {
        let row = self
            .connection
            .query_row(
                "SELECT root_session_id, parent_task_id, sequence
                 FROM resource_admission_cursors WHERE resource_key = ?1",
                [resource_key],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((root_id, parent_id, sequence)) = row else {
            return Ok(None);
        };
        let root_id = root_id
            .map(|value| RootSessionId::from_str(&value))
            .transpose()
            .map_err(|error| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: format!("invalid root session ID: {error}"),
            })?;
        let parent_id = parent_id
            .map(|value| TaskId::from_str(&value))
            .transpose()
            .map_err(|error| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: format!("invalid parent task ID: {error}"),
            })?;
        let sequence =
            u64::try_from(sequence).map_err(|_| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: "sequence is negative".into(),
            })?;
        Ok(Some(PersistedAdmissionCursor {
            root_id,
            parent_id,
            sequence,
        }))
    }

    /// Records a short process-local provider turn lease and its new fairness
    /// cursor in one transaction before the selected worker is notified.
    pub fn save_provider_turn_grant(
        &mut self,
        lease_id: &str,
        task: &TaskId,
        resource_key: &str,
        root_id: &RootSessionId,
        parent_id: &TaskId,
        sequence: u64,
    ) -> Result<(), RepositoryError> {
        let sequence =
            i64::try_from(sequence).map_err(|_| RepositoryError::InvalidAdmissionCursor {
                key: resource_key.into(),
                reason: "sequence exceeds SQLite integer range".into(),
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES (?1, ?2, ?3, 'shared', 1, 'active')
             ON CONFLICT(id) DO UPDATE SET state = 'active', released_at = NULL",
            params![lease_id, task.to_string(), resource_key],
        )?;
        transaction.execute(
            "INSERT INTO resource_admission_cursors
             (resource_key, root_session_id, parent_task_id, sequence)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(resource_key) DO UPDATE SET
                 root_session_id = excluded.root_session_id,
                 parent_task_id = excluded.parent_task_id,
                 sequence = excluded.sequence,
                 updated_at = CURRENT_TIMESTAMP",
            params![
                resource_key,
                root_id.to_string(),
                parent_id.to_string(),
                sequence
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Idempotently closes one provider-turn lease without touching unrelated
    /// worker resources.
    pub fn release_provider_turn_lease(&mut self, lease_id: &str) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND state = 'active'",
            [lease_id],
        )?;
        Ok(())
    }

    /// Provider turns are process-local. Other leases retain their recovery
    /// semantics and must not be released merely because the daemon restarted.
    pub fn release_provider_turn_leases(&mut self) -> Result<usize, RepositoryError> {
        Ok(self.connection.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE state = 'active'
               AND (resource_key LIKE 'llm:%'
                    OR resource_key LIKE 'llm-coordination:%')",
            [],
        )?)
    }

    pub fn release_process_leases_for_task(
        &mut self,
        task: &TaskId,
    ) -> Result<usize, RepositoryError> {
        Ok(self.connection.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE task_id = ?1
               AND state = 'active'
               AND resource_key NOT LIKE 'workspace:%'
               AND resource_key NOT LIKE 'worktree:%'",
            params![task.to_string()],
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
}
