use super::*;

impl RuntimeRepository {
    /// Appends one observation row for a task.
    ///
    /// The write is trimmed to [`TRACE_MAX_ROWS_PER_TASK`] inside the same
    /// transaction that inserts the row, so a reader can never observe a trace
    /// that is over the ceiling. `kind` and `payload_json` are the caller's own
    /// vocabulary: the store does not interpret the payload, which is what lets
    /// the trace evolve without a schema migration.
    pub fn append_trace(
        &mut self,
        task: &TaskId,
        kind: &str,
        payload_json: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO task_trace_events (task_id, kind, payload_json) VALUES (?1, ?2, ?3)",
            params![task.to_string(), kind, payload_json],
        )?;
        trim_trace_rows(&transaction, task)?;
        transaction.commit()?;
        Ok(())
    }

    /// Every retained row after `after_id`, oldest first. `after_id = 0` reads
    /// the whole trace; a reader resuming from [`Self::trace_high_water`] reads
    /// only what it has not seen.
    pub fn trace_after(
        &self,
        task: &TaskId,
        after_id: i64,
    ) -> Result<Vec<PersistedTraceRow>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, task_id, kind, payload_json FROM task_trace_events
             WHERE task_id = ?1 AND id > ?2 ORDER BY id",
        )?;
        let rows = statement
            .query_map(params![task.to_string(), after_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        rows.into_iter()
            .map(|(id, task_id, kind, payload_json)| {
                let task_id = task_id.parse().map_err(|_| RepositoryError::TaskNotFound {
                    task: task_id.clone(),
                })?;
                Ok(PersistedTraceRow {
                    id,
                    task_id,
                    kind,
                    payload_json,
                })
            })
            .collect()
    }

    /// The largest trace id this task has produced, or `0` when it has none.
    /// Ids are allocated by SQLite and never reused, so this is exactly the
    /// cursor a reader stores to resume without gaps or duplicates.
    pub fn trace_high_water(&self, task: &TaskId) -> Result<i64, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM task_trace_events WHERE task_id = ?1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }

    /// How many trace rows this task currently retains.
    pub fn trace_row_count(&self, task: &TaskId) -> Result<i64, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM task_trace_events WHERE task_id = ?1",
            params![task.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Re-applies [`TRACE_MAX_ROWS_PER_TASK`] to a trace that already exists.
    ///
    /// [`Self::append_trace`] already trims on every write; this is the
    /// maintenance entry point for callers that must bound a trace without
    /// adding a row of their own.
    pub fn trim_trace(&mut self, task: &TaskId) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        trim_trace_rows(&transaction, task)?;
        transaction.commit()?;
        Ok(())
    }

    /// Deletes the trace rows of terminal tasks whose rows are older than
    /// [`TRACE_TERMINAL_RETENTION_SECS`], and returns how many rows went away.
    ///
    /// Only *terminal* tasks are eligible. A live task keeps every row it has,
    /// however old: those rows are the head of the sequence a reader replays,
    /// and dropping them would silently truncate an in-flight worker's trace.
    /// Rows of a terminal task that are still inside the window also survive,
    /// so a supervisor has a full day to read how the task ended.
    pub fn prune_terminal_traces(&mut self, now: DateTime<Utc>) -> Result<usize, RepositoryError> {
        let placeholders = (1..=TERMINAL_TASK_STATES.len())
            .map(|position| format!("?{position}"))
            .collect::<Vec<_>>()
            .join(", ");
        let cutoff = TERMINAL_TASK_STATES.len() + 1;
        let sql = format!(
            "DELETE FROM task_trace_events
             WHERE task_id IN (SELECT id FROM tasks WHERE state_json IN ({placeholders}))
               AND created_at < ?{cutoff}"
        );
        let mut values: Vec<String> = TERMINAL_TASK_STATES
            .iter()
            .map(|state| (*state).to_string())
            .collect();
        // `CURRENT_TIMESTAMP` writes 'YYYY-MM-DD HH:MM:SS' in UTC, so the cutoff
        // is rendered the same way: comparing a different format would make the
        // order of two same-day instants depend on the separator character.
        values.push(
            (now - chrono::Duration::seconds(TRACE_TERMINAL_RETENTION_SECS))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
        );

        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let removed = transaction.execute(&sql, rusqlite::params_from_iter(values))?;
        transaction.commit()?;
        Ok(removed)
    }
}
