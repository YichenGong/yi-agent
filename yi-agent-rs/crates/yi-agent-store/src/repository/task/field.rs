use super::*;

impl RuntimeRepository {
    pub fn task_state(&self, task: &TaskId) -> Result<String, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT state_json FROM tasks WHERE id = ?1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }

    /// The linked roots a task's session carries, as canonical absolute paths.
    ///
    /// `NULL` (every row written before v15, and every cleared row), a blank
    /// string, and malformed JSON all read as an empty list. Inheritance is a
    /// capability increment: a bad value must never keep a task from starting,
    /// and "no linked roots" has to mean exactly today's single-root behavior.
    pub fn task_extra_roots(&self, task: &TaskId) -> Result<Vec<String>, RepositoryError> {
        let stored = self
            .connection
            .query_row(
                "SELECT extra_roots FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match stored {
            Some(raw) if !raw.trim().is_empty() => {
                Ok(serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default())
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Records a task's linked roots. An empty set is stored as SQL `NULL`, not
    /// as `"[]"`, so a cleared row is indistinguishable from one that predates
    /// the column; both mean "no linked roots".
    ///
    /// Deliberately lenient about a MISSING task: the affected-row count is
    /// ignored, so this returns `Ok` for an unknown id (unlike
    /// `set_task_thread_id`). A missing row is also read as an empty set by
    /// [`Self::task_extra_roots`], so the two agree, and every production call
    /// site runs immediately after the task's INSERT, making this unreachable.
    pub fn set_task_extra_roots(
        &mut self,
        task: &TaskId,
        roots: &[String],
    ) -> Result<(), RepositoryError> {
        let encoded = if roots.is_empty() {
            None
        } else {
            Some(serde_json::to_string(roots)?)
        };
        self.connection.execute(
            "UPDATE tasks SET extra_roots = ?1 WHERE id = ?2",
            params![encoded, task.to_string()],
        )?;
        Ok(())
    }

    /// A task's last transition time, used as the idle clock for reclaim.
    ///
    /// Returns `None` for a task with no row, which callers treat as "not idle
    /// enough to sweep" rather than as an error.
    pub fn task_updated_at(&self, task: &TaskId) -> Result<Option<DateTime<Utc>>, RepositoryError> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT updated_at FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|parsed| parsed.with_timezone(&Utc))
                .or_else(|| {
                    // SQLite's CURRENT_TIMESTAMP is 'YYYY-MM-DD HH:MM:SS' in UTC.
                    chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S")
                        .ok()
                        .map(|naive| naive.and_utc())
                })
        }))
    }

    /// Binds a task to the desktop conversation (`thread`) that owns it.
    ///
    /// The link is metadata: it deliberately does not touch `updated_at`, which
    /// is the idle clock orphan reclamation reads. Binding a thread to an
    /// abandoned task must not make it look freshly alive.
    pub fn set_task_thread_id(
        &mut self,
        task: &TaskId,
        thread_id: &str,
    ) -> Result<(), RepositoryError> {
        let changed = self.connection.execute(
            "UPDATE tasks SET thread_id = ?1 WHERE id = ?2",
            params![thread_id, task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        Ok(())
    }

    /// The conversation bound to a task, or `None` for a task that has never
    /// been bound. A task that does not exist is an error, not `None`.
    pub fn task_thread_id(&self, task: &TaskId) -> Result<Option<String>, RepositoryError> {
        let row: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT thread_id FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        row.ok_or_else(|| RepositoryError::TaskNotFound {
            task: task.to_string(),
        })
    }

    /// The live (non-terminal) children bound to a conversation.
    ///
    /// Roots are excluded on purpose: a root task is never tagged with a
    /// conversation, and a shared directory's root must survive one
    /// conversation being deleted. The `thread_id` filter is exact, so an
    /// untagged task (legacy rows written before the marker existed) matches
    /// nothing and is never collected by this query.
    pub fn active_children_for_thread(
        &self,
        thread_id: &str,
    ) -> Result<Vec<TaskId>, RepositoryError> {
        let placeholders = (1..=TERMINAL_TASK_STATES.len())
            .map(|position| format!("?{position}"))
            .collect::<Vec<_>>()
            .join(", ");
        let cutoff = TERMINAL_TASK_STATES.len() + 1;
        let sql = format!(
            "SELECT id FROM tasks
             WHERE thread_id = ?{cutoff}
               AND parent_id IS NOT NULL
               AND state_json NOT IN ({placeholders})
             ORDER BY created_at, id"
        );
        let mut values: Vec<String> = TERMINAL_TASK_STATES
            .iter()
            .map(|state| (*state).to_string())
            .collect();
        values.push(thread_id.to_string());
        let mut statement = self.connection.prepare(&sql)?;
        statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                row.get::<_, String>(0)
            })?
            .map(|row| {
                let task_id = row?;
                task_id
                    .parse()
                    .map_err(|_| RepositoryError::UnknownEventKind {
                        kind: format!("invalid task ID in store: {task_id}"),
                    })
            })
            .collect()
    }

    pub fn active_attempt_id(&self, task: &TaskId) -> Result<AttemptId, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| RepositoryError::TaskNotFound {
                task: task.to_string(),
            })?;
        value
            .parse()
            .map_err(|_| RepositoryError::InvalidWatchdogSnapshot {
                attempt: value,
                reason: "task has an invalid active attempt ID".into(),
            })
    }

    pub fn update_task_objective(
        &mut self,
        task: &TaskId,
        objective: &str,
    ) -> Result<(), RepositoryError> {
        let delivery_json = serde_json::to_string(&serde_json::json!({ "objective": objective }))?;
        let changed = self.connection.execute(
            "UPDATE tasks SET delivery_json = ?1 WHERE id = ?2",
            params![delivery_json, task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        Ok(())
    }

    /// The per-child model override, or `None` when the child inherits the
    /// parent's model.
    pub fn task_model(&self, task: &TaskId) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT model FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Reads a task's persisted write mode. This lives on the task row, not the
    /// retired `task_workspaces` table.
    pub fn task_workspace_mode(&self, task: &TaskId) -> Result<ChildWriteMode, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT workspace_mode FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match value {
            Some(value) => {
                ChildWriteMode::parse(&value).ok_or_else(|| RepositoryError::UnknownEventKind {
                    kind: format!("invalid workspace_mode in store: {value}"),
                })
            }
            None => Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            }),
        }
    }

    /// Reads a task's inherited sandbox. `None` means no inheritance was
    /// recorded (old rows, or a task spawned before this feature).
    pub fn task_inherited_sandbox(
        &self,
        task: &TaskId,
    ) -> Result<Option<InheritedSandbox>, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT inherited_sandbox FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match value {
            Some(value) => InheritedSandbox::parse(&value).map(Some).ok_or_else(|| {
                RepositoryError::UnknownEventKind {
                    kind: format!("invalid inherited_sandbox in store: {value}"),
                }
            }),
            None => Ok(None),
        }
    }

    pub fn attempt_state(&self, attempt: &AttemptId) -> Result<String, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT state FROM attempts WHERE id = ?1",
            params![attempt.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn attempt_ended_at(&self, attempt: &AttemptId) -> Result<Option<String>, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT ended_at FROM attempts WHERE id = ?1",
            params![attempt.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn attempt_terminal_json_for_task(
        &self,
        task: &TaskId,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT terminal_json FROM attempts WHERE task_id = ?1 ORDER BY number DESC LIMIT 1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }
}
