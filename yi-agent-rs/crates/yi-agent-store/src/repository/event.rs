use super::*;

impl RuntimeRepository {
    pub fn append_event(
        &mut self,
        task: &TaskId,
        event: RuntimeEvent,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn events_after(&self, cursor: i64) -> Result<Vec<RuntimeEvent>, RepositoryError> {
        Ok(self
            .event_records_after(cursor)?
            .into_iter()
            .map(|record| record.event)
            .collect())
    }

    pub fn latest_event_id(&self) -> Result<i64, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
            [],
            |row| row.get(0),
        )?)
    }

    /// Advances the replay window without deleting the append-only audit journal.
    pub fn advance_event_replay_floor_through(
        &mut self,
        event_id: i64,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let high_water_event_id: i64 = transaction.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
            [],
            |row| row.get(0),
        )?;
        let requested_floor = event_id
            .saturating_add(1)
            .clamp(1, high_water_event_id.saturating_add(1));
        transaction.execute(
            "UPDATE runtime_metadata
             SET value = MAX(value, ?1)
             WHERE key = 'event_replay_floor'",
            params![requested_floor],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn event_records_after(&self, cursor: i64) -> Result<Vec<PersistedEvent>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, task_id, kind, payload_json FROM events WHERE id > ?1 ORDER BY id",
        )?;
        statement
            .query_map(params![cursor], |row| {
                let task_id = row.get::<_, String>(1)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    task_id,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, task_id, kind, payload_json) = row?;
                Ok(PersistedEvent {
                    id,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    event: RuntimeEvent::parse(kind)?,
                    payload_json,
                })
            })
            .collect()
    }

    /// Returns the append-only audit history for one task after the supplied cursor.
    pub fn event_records_for_task_after(
        &self,
        task: &TaskId,
        cursor: i64,
    ) -> Result<Vec<PersistedEvent>, RepositoryError> {
        self.task_detail(task)?;
        let mut statement = self.connection.prepare(
            "SELECT id, task_id, kind, payload_json
             FROM events
             WHERE task_id = ?1 AND id > ?2
             ORDER BY id",
        )?;
        statement
            .query_map(params![task.to_string(), cursor], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, task_id, kind, payload_json) = row?;
                Ok(PersistedEvent {
                    id,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    event: RuntimeEvent::parse(kind)?,
                    payload_json,
                })
            })
            .collect()
    }
}
