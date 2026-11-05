use super::*;

impl RuntimeRepository {
    pub fn application_root_attachment(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<ApplicationRootAttachment>, RepositoryError> {
        self.connection
            .query_row(
                "SELECT idempotency_key, root_session_id, root_task_id, capability_digest,
                        capability_secret, workspace_root, state
                 FROM application_root_attachments WHERE idempotency_key = ?1",
                params![idempotency_key],
                |row| {
                    Ok(ApplicationRootAttachment {
                        idempotency_key: row.get(0)?,
                        root_session_id: row.get::<_, String>(1)?.parse().map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                1,
                                "root_session_id".into(),
                                rusqlite::types::Type::Text,
                            )
                        })?,
                        root_task_id: row.get::<_, String>(2)?.parse().map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                2,
                                "root_task_id".into(),
                                rusqlite::types::Type::Text,
                            )
                        })?,
                        capability_digest: row.get(3)?,
                        capability_secret: row.get(4)?,
                        workspace_root: row.get(5)?,
                        state: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(RepositoryError::from)
    }

    pub fn record_application_root_attachment(
        &mut self,
        idempotency_key: &str,
        root_session_id: &RootSessionId,
        root_task_id: &TaskId,
        capability_digest: &str,
        capability_secret: &str,
        workspace_root: &str,
    ) -> Result<(), RepositoryError> {
        // The attachment, the role and the container record are one fact about
        // one session, so they commit together. A crash between them would leave
        // either a promoted root with no record (no admission, forever blocked)
        // or a record with an attachment still claiming an execution (a session
        // the sweeps may settle).
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO application_root_attachments
                (idempotency_key, root_session_id, root_task_id, capability_digest,
                 capability_secret, workspace_root, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'attached')",
            params![
                idempotency_key,
                root_session_id.to_string(),
                root_task_id.to_string(),
                capability_digest,
                capability_secret,
                workspace_root
            ],
        )?;
        transaction.execute(
            "UPDATE tasks SET role = 'session_container'
             WHERE id = ?1 AND parent_id IS NULL AND depth = 0",
            params![root_task_id.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO session_containers (root_task_id, root_session_id, closed)
             VALUES (?1, ?2, 0)
             ON CONFLICT(root_task_id) DO NOTHING",
            params![root_task_id.to_string(), root_session_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn reattach_application_root(
        &mut self,
        idempotency_key: &str,
    ) -> Result<(), RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE application_root_attachments
             SET state = 'attached', detached_at = NULL
             WHERE idempotency_key = ?1 AND state = 'detached'",
            params![idempotency_key],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: idempotency_key.to_string(),
            });
        }
        Ok(())
    }

    pub fn detach_application_root(
        &mut self,
        root_session_id: &RootSessionId,
        root_task_id: &TaskId,
    ) -> Result<(), RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE application_root_attachments
             SET state = 'detached', detached_at = CURRENT_TIMESTAMP
             WHERE root_session_id = ?1 AND root_task_id = ?2 AND state = 'attached'",
            params![root_session_id.to_string(), root_task_id.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: root_task_id.to_string(),
            });
        }
        Ok(())
    }

    pub fn activate_application_root_once(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        objective: &str,
    ) -> Result<bool, RepositoryError> {
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET state_json = 'running', delivery_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3 AND state_json = 'queued'",
            params![delivery_json, task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            let exists = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1 AND active_attempt_id = ?2)",
                params![task.to_string(), attempt.to_string()],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(RepositoryError::TaskNotFound {
                    task: task.to_string(),
                });
            }
            transaction.commit()?;
            return Ok(false);
        }
        transaction.execute(
            "UPDATE attempts SET state = 'running' WHERE id = ?1 AND task_id = ?2",
            params![attempt.to_string(), task.to_string()],
        )?;
        append_event(&transaction, task, RuntimeEvent::TaskStarted)?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn application_root_capability_matches(
        &self,
        root_session_id: &RootSessionId,
        root_task_id: &TaskId,
        capability_digest: &str,
    ) -> Result<bool, RepositoryError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM application_root_attachments
                    WHERE root_session_id = ?1
                      AND root_task_id = ?2
                      AND capability_digest = ?3
                      AND state = 'attached'
                 )",
                params![
                    root_session_id.to_string(),
                    root_task_id.to_string(),
                    capability_digest
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(RepositoryError::from)
    }

    /// Sessions whose application root attachment is currently detached.
    ///
    /// A detached root sits in `paused`, which is not a terminal state, so it
    /// would never enter a terminal-only sweep. This is the clause that makes a
    /// root worktree reclaimable at all.
    pub fn detached_application_roots(&self) -> Result<Vec<RootSessionId>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT root_session_id FROM application_root_attachments
             WHERE state = 'detached'",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|value| value.parse().ok())
            .collect())
    }

    pub fn application_root_hydration_tasks(
        &self,
        root_session_id: &RootSessionId,
    ) -> Result<Vec<PersistedHydrationTask>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.root_session_id, tasks.id, tasks.parent_id, tasks.depth,
                    tasks.active_attempt_id, attempts.number, tasks.state_json,
                    tasks.delivery_json, attempts.terminal_json
             FROM tasks
             JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.root_session_id = ?1
             ORDER BY tasks.depth, tasks.created_at, tasks.id",
        )?;
        statement
            .query_map(params![root_session_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u8>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })?
            .map(|row| {
                let (
                    session,
                    task,
                    parent,
                    depth,
                    attempt,
                    number,
                    state,
                    delivery_json,
                    terminal_json,
                ) = row?;
                Ok(PersistedHydrationTask {
                    session_id: session
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!(
                                "invalid application root session ID in store: {session}"
                            ),
                        })?,
                    task_id: task
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid application root task ID in store: {task}"),
                        })?,
                    parent_id: parent
                        .map(|value| {
                            value
                                .parse()
                                .map_err(|_| RepositoryError::UnknownEventKind {
                                    kind: format!(
                                        "invalid application root parent ID in store: {value}"
                                    ),
                                })
                        })
                        .transpose()?,
                    depth,
                    attempt_id: attempt
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!(
                                "invalid application root attempt ID in store: {attempt}"
                            ),
                        })?,
                    attempt_number: number,
                    state,
                    delivery_json,
                    terminal_json,
                })
            })
            .collect()
    }
}
