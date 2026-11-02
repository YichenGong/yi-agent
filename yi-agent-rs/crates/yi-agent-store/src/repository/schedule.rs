use super::*;

impl RuntimeRepository {
    /// Persists a validated schedule without coupling it to an interactive
    /// session. Each later fire receives a new isolated root session.
    pub fn create_schedule(
        &mut self,
        definition: &ScheduleDefinition,
        next_run_at: DateTime<Local>,
    ) -> Result<PersistedSchedule, RepositoryError> {
        let schedule = PersistedSchedule {
            id: uuid::Uuid::new_v4().to_string(),
            definition: definition.clone(),
            state: "active".into(),
            next_run_at,
        };
        self.connection.execute(
            "INSERT INTO schedules (id, definition_json, state, next_run_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                schedule.id,
                serde_json::to_string(&schedule.definition)?,
                schedule.state,
                schedule.next_run_at.to_rfc3339(),
            ],
        )?;
        Ok(schedule)
    }

    pub fn schedules(&self) -> Result<Vec<PersistedSchedule>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT id, definition_json, state, next_run_at
             FROM schedules ORDER BY created_at, id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .map(|row| {
                let (id, definition_json, state, next_run_at) = row?;
                let definition = serde_json::from_str(&definition_json)?;
                let next_run_at = DateTime::parse_from_rfc3339(&next_run_at)
                    .map_err(|error| RepositoryError::UnknownEventKind {
                        kind: format!("invalid schedule next-run timestamp: {error}"),
                    })?
                    .with_timezone(&Local);
                Ok(PersistedSchedule {
                    id,
                    definition,
                    state,
                    next_run_at,
                })
            })
            .collect()
    }

    pub fn delete_schedule(&mut self, schedule_id: &str) -> Result<bool, RepositoryError> {
        let changed = self
            .connection
            .execute("DELETE FROM schedules WHERE id = ?1", [schedule_id])?;
        Ok(changed == 1)
    }

    /// Claims a due occurrence exactly once. A duplicate tick returns false.
    pub fn claim_schedule_occurrence(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
    ) -> Result<bool, RepositoryError> {
        let changed = self.connection.execute(
            "INSERT INTO schedule_occurrences (schedule_id, due_at, outcome)
             VALUES (?1, ?2, 'claimed')
             ON CONFLICT(schedule_id, due_at) DO NOTHING",
            params![schedule_id, due_at.to_rfc3339()],
        )?;
        Ok(changed == 1)
    }

    pub fn advance_schedule(
        &mut self,
        schedule_id: &str,
        next_run_at: DateTime<Local>,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
            params![next_run_at.to_rfc3339(), schedule_id],
        )?;
        Ok(())
    }

    pub fn schedule_has_active_instance(&self, schedule_id: &str) -> Result<bool, RepositoryError> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                SELECT 1 FROM schedule_occurrences
                JOIN tasks ON tasks.root_session_id = schedule_occurrences.root_session_id
                WHERE schedule_occurrences.schedule_id = ?1
                  AND tasks.depth = 0
                  AND tasks.state_json NOT IN (
                    'completed', 'cancelled', 'failed', 'blocked', 'stalled',
                    'timed_out', 'budget_exhausted'
                  )
            )",
                [schedule_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(RepositoryError::from)
    }

    pub fn record_schedule_occurrence_outcome(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        outcome: &str,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE schedule_occurrences SET outcome = ?1
             WHERE schedule_id = ?2 AND due_at = ?3",
            params![outcome, schedule_id, due_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn attach_schedule_occurrence_root(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        root_session_id: &RootSessionId,
    ) -> Result<(), RepositoryError> {
        self.connection.execute(
            "UPDATE schedule_occurrences
             SET outcome = 'fired', root_session_id = ?1
             WHERE schedule_id = ?2 AND due_at = ?3",
            params![
                root_session_id.to_string(),
                schedule_id,
                due_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn schedule_occurrence_outcomes(
        &self,
        schedule_id: &str,
    ) -> Result<Vec<String>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT outcome FROM schedule_occurrences
             WHERE schedule_id = ?1 ORDER BY due_at",
        )?;
        statement
            .query_map([schedule_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    /// Atomically records one occurrence and, when admitted, its isolated
    /// root. A failed root insert rolls the claim back with the transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn evaluate_schedule_occurrence(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        next_run_at: DateTime<Local>,
        root: &RootSessionId,
        task: &TaskId,
        attempt: &AttemptId,
        definition: &ScheduleDefinition,
    ) -> Result<ScheduleOccurrenceResult, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let claimed = transaction.execute(
            "INSERT INTO schedule_occurrences (schedule_id, due_at, outcome)
             VALUES (?1, ?2, 'claimed') ON CONFLICT(schedule_id, due_at) DO NOTHING",
            params![schedule_id, due_at.to_rfc3339()],
        )?;
        if claimed == 0 {
            transaction.commit()?;
            return Ok(ScheduleOccurrenceResult::Duplicate);
        }
        let active_instance = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM schedule_occurrences
                JOIN tasks ON tasks.root_session_id = schedule_occurrences.root_session_id
                WHERE schedule_occurrences.schedule_id = ?1 AND tasks.depth = 0
                  AND tasks.state_json NOT IN (
                    'completed', 'completed_no_changes', 'cancelled', 'failed', 'blocked',
                    'stalled', 'timed_out', 'budget_exhausted'
                  )
            )",
            [schedule_id],
            |row| row.get::<_, bool>(0),
        )?;
        if active_instance {
            transaction.execute(
                "UPDATE schedule_occurrences SET outcome = 'skipped_overlap'
                 WHERE schedule_id = ?1 AND due_at = ?2",
                params![schedule_id, due_at.to_rfc3339()],
            )?;
            transaction.execute(
                "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
                params![next_run_at.to_rfc3339(), schedule_id],
            )?;
            transaction.commit()?;
            return Ok(ScheduleOccurrenceResult::SkippedOverlap);
        }
        let delivery_json = serde_json::to_string(&serde_json::json!({
            "objective": definition.objective,
            "schedule_policy": definition.policy,
        }))?;
        transaction.execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}')",
            params![root.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, 'queued', 1, ?3, ?4)",
            params![task.to_string(), root.to_string(), attempt.to_string(), delivery_json],
        )?;
        insert_attempt(&transaction, attempt, task, 1, "queued")?;
        let limits = WatchdogLimits {
            max_turns: Some(definition.policy.runtime.max_turns),
            max_wall_time_secs: Some(definition.policy.runtime.max_wall_time_secs),
            ..WatchdogLimits::default()
        };
        transaction.execute(
            "UPDATE attempt_watchdogs SET limits_json = ?1 WHERE attempt_id = ?2",
            params![serde_json::to_string(&limits)?, attempt.to_string()],
        )?;
        transaction.execute(
            "UPDATE schedule_occurrences SET outcome = 'fired', root_session_id = ?1
             WHERE schedule_id = ?2 AND due_at = ?3",
            params![root.to_string(), schedule_id, due_at.to_rfc3339()],
        )?;
        transaction.execute(
            "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
            params![next_run_at.to_rfc3339(), schedule_id],
        )?;
        transaction.commit()?;
        Ok(ScheduleOccurrenceResult::Fired)
    }

    /// Persists a non-firing occurrence and advances the schedule as one
    /// transaction, so an offline daemon cannot accumulate duplicate backlog.
    pub fn skip_schedule_occurrence(
        &mut self,
        schedule_id: &str,
        due_at: DateTime<Local>,
        next_run_at: DateTime<Local>,
        outcome: &str,
    ) -> Result<bool, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let claimed = transaction.execute(
            "INSERT INTO schedule_occurrences (schedule_id, due_at, outcome)
             VALUES (?1, ?2, ?3) ON CONFLICT(schedule_id, due_at) DO NOTHING",
            params![schedule_id, due_at.to_rfc3339(), outcome],
        )?;
        if claimed != 0 {
            transaction.execute(
                "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
                params![next_run_at.to_rfc3339(), schedule_id],
            )?;
        }
        transaction.commit()?;
        Ok(claimed != 0)
    }
}
