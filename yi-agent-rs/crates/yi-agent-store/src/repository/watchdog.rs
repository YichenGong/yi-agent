use super::*;

impl RuntimeRepository {
    /// Stores watchdog facts separately from recovery's tool-state payload.
    /// The upsert lets the daemon persist each meaningful progress or resource
    /// queue transition without creating another attempt.
    #[allow(clippy::too_many_arguments)]
    pub fn save_attempt_watchdog_snapshot(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        limits: &WatchdogLimits,
        usage: &WatchdogUsage,
        last_meaningful_event_id: Option<i64>,
        last_meaningful_at: DateTime<Utc>,
        current_wait: Option<&WatchdogResourceWait>,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts WHERE id = ?1 AND task_id = ?2)",
            params![attempt.to_string(), task.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "INSERT INTO attempt_watchdogs (
                attempt_id, task_id, limits_json, usage_json, last_meaningful_event_id,
                last_meaningful_at, resource_wait_key, resource_wait_started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(attempt_id) DO UPDATE SET
                limits_json = excluded.limits_json,
                usage_json = excluded.usage_json,
                last_meaningful_event_id = excluded.last_meaningful_event_id,
                last_meaningful_at = excluded.last_meaningful_at,
                resource_wait_key = excluded.resource_wait_key,
                resource_wait_started_at = excluded.resource_wait_started_at",
            params![
                attempt.to_string(),
                task.to_string(),
                serde_json::to_string(limits)?,
                serde_json::to_string(usage)?,
                last_meaningful_event_id,
                last_meaningful_at.to_rfc3339(),
                current_wait.map(|wait| wait.resource_key.as_str()),
                current_wait.map(|wait| wait.queued_at.to_rfc3339()),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn attempt_watchdog_snapshot(
        &self,
        attempt: &AttemptId,
    ) -> Result<Option<PersistedAttemptWatchdog>, RepositoryError> {
        let row = self
            .connection
            .query_row(
                "SELECT attempts.started_at, attempt_watchdogs.limits_json,
                        attempt_watchdogs.usage_json, attempt_watchdogs.last_meaningful_event_id,
                        attempt_watchdogs.last_meaningful_at,
                        attempt_watchdogs.resource_wait_key,
                        attempt_watchdogs.resource_wait_started_at
                 FROM attempt_watchdogs JOIN attempts ON attempts.id = attempt_watchdogs.attempt_id
                 WHERE attempt_watchdogs.attempt_id = ?1",
                params![attempt.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(
                started_at,
                limits_json,
                usage_json,
                last_event,
                last_meaningful_at,
                wait_key,
                wait_started_at,
            )| {
                let parse_time = |value: &str| {
                    DateTime::parse_from_rfc3339(value)
                        .map(|time| time.with_timezone(&Utc))
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
                                .map(|time| time.and_utc())
                        })
                        .map_err(|error| RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt.to_string(),
                            reason: format!("invalid timestamp {value:?}: {error}"),
                        })
                };
                Ok(PersistedAttemptWatchdog {
                    limits: serde_json::from_str(&limits_json).map_err(|error| {
                        RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt.to_string(),
                            reason: format!("invalid limits JSON: {error}"),
                        }
                    })?,
                    observation: WatchdogObservation {
                        attempt_started_at: parse_time(&started_at)?,
                        last_meaningful_at: parse_time(&last_meaningful_at)?,
                        resource_wait_started_at: wait_started_at
                            .as_deref()
                            .map(parse_time)
                            .transpose()?,
                        usage: serde_json::from_str(&usage_json).map_err(|error| {
                            RepositoryError::InvalidWatchdogSnapshot {
                                attempt: attempt.to_string(),
                                reason: format!("invalid usage JSON: {error}"),
                            }
                        })?,
                    },
                    last_meaningful_event_id: last_event,
                    current_wait: match (wait_key, wait_started_at) {
                        (Some(resource_key), Some(queued_at)) => Some(WatchdogResourceWait {
                            resource_key,
                            queued_at: parse_time(&queued_at)?,
                        }),
                        (None, None) => None,
                        _ => {
                            return Err(RepositoryError::InvalidWatchdogSnapshot {
                                attempt: attempt.to_string(),
                                reason: "resource wait key and timestamp must both be present"
                                    .into(),
                            });
                        }
                    },
                })
            },
        )
        .transpose()
    }

    /// Atomically adds durable worker usage and, for a non-generated progress
    /// point, records the event ID used by the idle watchdog.
    #[allow(clippy::too_many_arguments)]
    pub fn record_watchdog_progress(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        turn_delta: u32,
        token_delta: u64,
        provider_retry_delta: u16,
        tool_retry_delta: u16,
        meaningful: bool,
        now: DateTime<Utc>,
        usage_delta: WatchdogTokenDelta,
    ) -> Result<(), RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (usage_json, exists): (String, bool) = transaction.query_row(
            "SELECT attempt_watchdogs.usage_json,
                    tasks.active_attempt_id = attempt_watchdogs.attempt_id
             FROM attempt_watchdogs JOIN tasks ON tasks.id = attempt_watchdogs.task_id
             WHERE attempt_watchdogs.task_id = ?1 AND attempt_watchdogs.attempt_id = ?2",
            params![task.to_string(), attempt.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !exists {
            return Ok(());
        }
        let mut usage: WatchdogUsage = serde_json::from_str(&usage_json).map_err(|error| {
            RepositoryError::InvalidWatchdogSnapshot {
                attempt: attempt.to_string(),
                reason: format!("invalid usage JSON: {error}"),
            }
        })?;
        usage.turns = usage.turns.saturating_add(turn_delta);
        usage.tokens = usage.tokens.saturating_add(token_delta);
        // 分项记账。与上面的 `tokens` 是两回事：`tokens` 仍是 input+output 的合计
        // （预算判断依赖它），这三项是给会话级展示按项拆开用的。
        usage.input_tokens = usage.input_tokens.saturating_add(usage_delta.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(usage_delta.output_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(usage_delta.cache_read_tokens);
        usage.provider_retries = usage.provider_retries.saturating_add(provider_retry_delta);
        usage.tool_retries = usage.tool_retries.saturating_add(tool_retry_delta);
        let progress_event_id = meaningful
            .then(|| append_event(&transaction, task, RuntimeEvent::TaskProgress))
            .transpose()?;
        transaction.execute(
            "UPDATE attempt_watchdogs
             SET usage_json = ?1,
                 last_meaningful_event_id = CASE WHEN ?2 THEN ?3 ELSE last_meaningful_event_id END,
                 last_meaningful_at = CASE WHEN ?2 THEN ?4 ELSE last_meaningful_at END
             WHERE task_id = ?5 AND attempt_id = ?6",
            params![
                serde_json::to_string(&usage)?,
                meaningful,
                progress_event_id,
                now.to_rfc3339(),
                task.to_string(),
                attempt.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 读取某次 attempt 的用量快照。上层聚合与测试共用。
    pub fn watchdog_usage(
        &self,
        task: &TaskId,
        attempt: &AttemptId,
    ) -> Result<WatchdogUsage, RepositoryError> {
        let json: String = self.connection.query_row(
            "SELECT usage_json FROM attempt_watchdogs WHERE task_id = ?1 AND attempt_id = ?2",
            params![task.to_string(), attempt.to_string()],
            |row| row.get(0),
        )?;
        serde_json::from_str(&json).map_err(|error| RepositoryError::InvalidWatchdogSnapshot {
            attempt: attempt.to_string(),
            reason: format!("invalid usage JSON: {error}"),
        })
    }

    /// Returns watchdog facts only for the currently active attempt of a task
    /// that remains eligible for a terminal watchdog decision.
    pub fn active_attempt_watchdogs(&self) -> Result<Vec<PersistedWatchdogTask>, RepositoryError> {
        let rows = {
            let mut statement = self.connection.prepare(
                "SELECT tasks.root_session_id, tasks.id, attempt_watchdogs.attempt_id
                 FROM attempt_watchdogs
                 JOIN tasks ON tasks.id = attempt_watchdogs.task_id
                 WHERE tasks.active_attempt_id = attempt_watchdogs.attempt_id
                   AND tasks.state_json IN (
                       'queued', 'running', 'waiting_for_resource',
                       'waiting_for_permission', 'waiting_for_children'
                   )",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        rows.into_iter()
            .map(|(session_id, task_id, attempt_id)| {
                let attempt_id =
                    attempt_id
                        .parse()
                        .map_err(|_| RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt_id.clone(),
                            reason: "invalid attempt ID".into(),
                        })?;
                let snapshot = self
                    .attempt_watchdog_snapshot(&attempt_id)?
                    .ok_or_else(|| RepositoryError::InvalidWatchdogSnapshot {
                        attempt: attempt_id.to_string(),
                        reason: "active watchdog snapshot disappeared".into(),
                    })?;
                Ok(PersistedWatchdogTask {
                    session_id: session_id.parse().map_err(|_| {
                        RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt_id.to_string(),
                            reason: "invalid root session ID".into(),
                        }
                    })?,
                    task_id: task_id.parse().map_err(|_| {
                        RepositoryError::InvalidWatchdogSnapshot {
                            attempt: attempt_id.to_string(),
                            reason: "invalid task ID".into(),
                        }
                    })?,
                    attempt_id,
                    snapshot,
                })
            })
            .collect()
    }

    /// Test/support API for recording the durable facts a resumed worker must
    /// inspect before it can issue new side effects.
    pub fn record_recovery_context(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        workspace_lease: &str,
        worktree_lease: &str,
        checkpoint_json: &str,
        tool_state_json: &str,
    ) -> Result<(), RepositoryError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE tasks SET workspace_lease_id = ?1 WHERE id = ?2",
            params![workspace_lease, task.to_string()],
        )?;
        transaction.execute(
            "UPDATE attempts SET checkpoint_json = ?1, usage_json = ?2 WHERE id = ?3 AND task_id = ?4",
            params![checkpoint_json, tool_state_json, attempt.to_string(), task.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')",
            params![
                format!("recovery-worktree-{}", task),
                task.to_string(),
                worktree_lease
            ],
        )?;
        transaction.execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')",
            params![
                format!("recovery-workspace-{}", task),
                task.to_string(),
                workspace_lease
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Commits a watchdog outcome exactly once for an active attempt. A later
    /// scan sees the terminal task state and becomes a no-op rather than
    /// duplicating the terminal audit event or lease release.
    pub fn record_watchdog_terminal(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        terminal: WatchdogTerminal,
        evidence: &WatchdogEvidence,
    ) -> Result<Option<i64>, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3
               AND state_json IN ('queued', 'running', 'waiting_for_resource',
                                  'waiting_for_permission', 'waiting_for_children')",
            params![terminal.state_name(), task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(None);
        }

        let terminal_json = terminal.terminal_json(evidence)?;
        transaction.execute(
            "UPDATE attempts SET state = ?1, ended_at = CURRENT_TIMESTAMP, terminal_json = ?2
             WHERE id = ?3 AND task_id = ?4",
            params![
                terminal.state_name(),
                terminal_json,
                attempt.to_string(),
                task.to_string()
            ],
        )?;
        transaction.execute(
            "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
             WHERE task_id = ?1 AND state = 'active'",
            params![task.to_string()],
        )?;
        let event_id =
            append_event_with_payload(&transaction, task, terminal.event(), &terminal_json)?;
        transaction.commit()?;
        Ok(Some(event_id))
    }

    /// Activates a successor attempt while returning the task to the queue.
    /// The task's active-attempt pointer and attempt row must commit together
    /// before a worker may receive the new attempt ID.
    pub fn activate_successor_attempt(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        attempt_number: u32,
        state: &str,
        event: RuntimeEvent,
    ) -> Result<i64, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let recovery_context = if state == "recovery_gated" {
            Some(transaction.query_row(
                "SELECT attempts.checkpoint_json, attempts.usage_json
                 FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
                 WHERE tasks.id = ?1",
                params![task.to_string()],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
            )?)
        } else {
            None
        };
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = ?1, active_attempt_id = ?2, updated_at = CURRENT_TIMESTAMP
             WHERE id = ?3",
            params![state, attempt.to_string(), task.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        insert_attempt(&transaction, attempt, task, attempt_number, state)?;
        if let Some((checkpoint_json, tool_state_json)) = recovery_context {
            transaction.execute(
                "UPDATE attempts SET checkpoint_json = ?1, usage_json = ?2 WHERE id = ?3",
                params![checkpoint_json, tool_state_json, attempt.to_string()],
            )?;
        }
        let event_id = append_event(&transaction, task, event)?;
        transaction.commit()?;
        Ok(event_id)
    }

    /// Commits deterministic inspection evidence before a gated successor can
    /// enter ordinary worker admission.
    pub fn attest_recovery_gate(
        &mut self,
        task: &TaskId,
        attempt: &AttemptId,
        attestation: &WorkerRecoveryAttestation,
    ) -> Result<i64, RepositoryError> {
        serde_json::from_str::<serde_json::Value>(&attestation.checkpoint_json)?;
        serde_json::from_str::<serde_json::Value>(&attestation.tool_state_json)?;
        serde_json::from_str::<serde_json::Value>(&attestation.evidence_json)?;
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tasks SET state_json = 'recovery_attested', updated_at = CURRENT_TIMESTAMP
             WHERE id = ?1 AND active_attempt_id = ?2 AND state_json = 'recovery_gated'",
            params![task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'recovery_attested', checkpoint_json = ?1, usage_json = ?2
             WHERE id = ?3 AND task_id = ?4 AND state = 'recovery_gated'",
            params![
                attestation.checkpoint_json,
                attestation.tool_state_json,
                attempt.to_string(),
                task.to_string()
            ],
        )?;
        let event_id = append_event_with_payload(
            &transaction,
            task,
            RuntimeEvent::TaskRecoveryAttested,
            &attestation.evidence_json,
        )?;
        transaction.commit()?;
        Ok(event_id)
    }

    pub fn recover_inflight_tasks(&mut self) -> Result<usize, RepositoryError> {
        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task_ids = {
            let mut statement = transaction.prepare(
                "SELECT id FROM tasks WHERE state_json IN ('running', 'waiting_for_resource', 'waiting_for_permission', 'waiting_for_children')",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        // v2 compatibility rows were allowed to have no active attempt. Give
        // each live task a closed recovery attempt so hydration can resume it.
        for task_id in &task_ids {
            let active_attempt: String = transaction.query_row(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                params![task_id],
                |row| row.get(0),
            )?;
            if active_attempt.is_empty() {
                let task: TaskId =
                    task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?;
                let attempt = AttemptId::new();
                let number: u32 = transaction.query_row(
                    "SELECT COALESCE(MAX(number), 0) + 1 FROM attempts WHERE task_id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "UPDATE tasks SET active_attempt_id = ?1 WHERE id = ?2",
                    params![attempt.to_string(), task_id],
                )?;
                insert_attempt(&transaction, &attempt, &task, number, "running")?;
            }
        }
        let recovered_attempts: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM attempts WHERE state IN ('running', 'waiting_for_resource', 'waiting_for_permission', 'waiting_for_children')",
            [], |row| row.get(0),
        )?;
        for task_id in &task_ids {
            transaction.execute(
                "UPDATE tasks SET state_json = 'recovery_required', updated_at = CURRENT_TIMESTAMP WHERE id = ?1",
                params![task_id],
            )?;
            let task = task_id
                .parse()
                .map_err(|_| RepositoryError::UnknownEventKind {
                    kind: format!("invalid task ID in store: {task_id}"),
                })?;
            append_event(&transaction, &task, RuntimeEvent::TaskRecoveryRequired)?;
        }
        // Events are task-addressable on the v1 IPC surface. One recovered
        // task carries the daemon-level recovery marker so subscribers still
        // observe a single, durable restart boundary without a protocol break.
        if let Some(task_id) = task_ids.first() {
            let task = task_id
                .parse()
                .map_err(|_| RepositoryError::UnknownEventKind {
                    kind: format!("invalid task ID in store: {task_id}"),
                })?;
            let released_process_leases = transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE state = 'active'
                   AND resource_key NOT LIKE 'worktree:%'
                   AND resource_key NOT LIKE 'workspace:%'",
                [],
            )?;
            let retained_workspace_worktree_leases: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM resource_leases WHERE state = 'active'
                 AND (resource_key LIKE 'worktree:%' OR resource_key LIKE 'workspace:%')",
                [],
                |row| row.get(0),
            )?;
            let payload = serde_json::to_string(&serde_json::json!({
                "recovered_tasks": task_ids.len(),
                "recovered_attempts": recovered_attempts,
                "released_process_leases": released_process_leases,
                "retained_workspace_worktree_leases": retained_workspace_worktree_leases,
            }))?;
            append_event_with_payload(
                &transaction,
                &task,
                RuntimeEvent::RuntimeRecovered,
                &payload,
            )?;
        }
        if task_ids.is_empty() {
            transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE state = 'active'
                   AND resource_key NOT LIKE 'worktree:%'
                   AND resource_key NOT LIKE 'workspace:%'",
                [],
            )?;
        }
        transaction.execute(
            "UPDATE attempts SET state = 'recovery_required', ended_at = CURRENT_TIMESTAMP
             WHERE state IN ('running', 'waiting_for_resource', 'waiting_for_permission', 'waiting_for_children')",
            [],
        )?;
        transaction.commit()?;
        Ok(task_ids.len())
    }

    /// Reclaims tasks that can never finish because nothing owns them any more.
    ///
    /// A subagent only makes progress while its owning daemon holds it in
    /// memory. Once that process is gone, `paused`, `queued` and `running` rows
    /// are not merely idle: no code path will ever move them, because
    /// [`Self::recover_inflight_tasks`] only sweeps the `waiting_*` family and
    /// hydration only revives tasks that carry recovery context. They
    /// accumulate forever, and any parent waiting on "all my children finished"
    /// waits forever too.
    ///
    /// Releases leases still held by tasks that can never run again.
    ///
    /// [`Self::reclaim_orphaned_tasks`] covers the other half of the problem: a
    /// task left live by a dead daemon. This covers the settled half. A settled
    /// task (see [`SETTLED_TASK_STATES`]) is unreachable by every scheduling
    /// path, so any `active` lease it still holds is unreleasable garbage, and
    /// `worktree:` / `workspace:` rows are `exclusive`, so they occupy the
    /// admission slot for that workspace forever.
    ///
    /// It runs at daemon boot because that is the only point where the whole
    /// table can be swept without racing live work: nothing is admitted yet, so
    /// every `active` lease on a settled task is provably stale. Recovery states
    /// are skipped — they are parked and resumable, and their lease is what a
    /// resume needs.
    pub fn release_leases_of_settled_tasks(&mut self) -> Result<usize, RepositoryError> {
        let settled = SETTLED_TASK_STATES
            .iter()
            .map(|state| format!("'{state}'"))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(self.connection.execute(
            &format!(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE state = 'active'
                   AND task_id IN (SELECT id FROM tasks WHERE state_json IN ({settled}))"
            ),
            [],
        )?)
    }

    /// A task is an orphan when the application root that owns its session has
    /// no live attachment (`state = 'detached'`), or has no attachment at all
    /// and has not been touched for `grace_seconds` (a root that is mid-boot
    /// has not written its attachment yet, and must not be reaped).
    ///
    /// Reaping is deliberately gentle: the partial report is preserved in the
    /// attempt's `terminal_json`, so a task killed while it was mid-sentence
    /// still yields whatever it had said.
    pub fn reclaim_orphaned_tasks(&mut self, grace_seconds: i64) -> Result<usize, RepositoryError> {
        let reclaimable_state = "state_json NOT IN ('completed', 'completed_no_changes', 'failed', 'cancelled', \
             'blocked', 'stalled', 'timed_out', 'budget_exhausted', 'recovery_required')";
        let orphaned_because_detached = "EXISTS(
                 SELECT 1 FROM application_root_attachments a
                 WHERE a.root_session_id = tasks.root_session_id AND a.state = 'detached')";
        let orphaned_because_unowned = "(
                 NOT EXISTS(
                     SELECT 1 FROM application_root_attachments a
                     WHERE a.root_session_id = tasks.root_session_id)
                 AND tasks.updated_at <= datetime('now', '-' || ?1 || ' seconds'))";
        // `recovery_required` is a holding pen for explicit resume, not a
        // resting state, and it is itself terminal, so a row parked there can
        // never be swept again once its owner is gone. It is abandoned only if
        // it has no lease and no checkpoint or tool state *and* its root is
        // unowned; a row under a live attachment is the session's own root
        // task, which hydration owns and a user may still resume. (The retired
        // `task_workspaces` table is deliberately not consulted: a fresh schema
        // must not have it.)
        let abandoned_recovery = format!(
            "(state_json = 'recovery_required'
                 AND (workspace_lease_id IS NULL OR workspace_lease_id = '')
                 AND NOT EXISTS(
                     SELECT 1 FROM attempts a
                     WHERE a.id = tasks.active_attempt_id
                       AND (COALESCE(a.checkpoint_json, '') <> ''
                            OR COALESCE(a.usage_json, '') NOT IN ('', '{{}}')))
                 AND ({orphaned_because_detached} OR {orphaned_because_unowned}))"
        );

        let transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let orphaned = transaction
            .prepare(&format!(
                "SELECT id, state_json FROM tasks WHERE {reclaimable_state} \
                 AND ({orphaned_because_detached} OR {orphaned_because_unowned})"
            ))?
            .query_map(params![grace_seconds], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let abandoned = transaction
            .prepare(&format!(
                "SELECT id, state_json FROM tasks WHERE {abandoned_recovery}"
            ))?
            .query_map(params![grace_seconds], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut reclaimed = 0usize;
        for (task_id, state) in orphaned.into_iter().chain(abandoned) {
            let task: TaskId = task_id
                .parse()
                .map_err(|_| RepositoryError::UnknownEventKind {
                    kind: format!("invalid task ID in store: {task_id}"),
                })?;
            // Carry the partial report forward: the terminal stamp replaces
            // the attempt's `terminal_json`, and that field is the only
            // surviving trace of what the child had managed to say.
            let report: Option<String> = transaction
                .query_row(
                    "SELECT attempts.terminal_json FROM attempts
                     JOIN tasks ON tasks.active_attempt_id = attempts.id
                     WHERE tasks.id = ?1",
                    params![task_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let terminal_json = match text_completion_report(report.as_deref()) {
                Some(report) => serde_json::to_string(&serde_json::json!({
                    "reason": "reclaimed_orphan",
                    "previous_state": state,
                    "report": report,
                }))?,
                None => serde_json::to_string(&serde_json::json!({
                    "reason": "reclaimed_orphan",
                    "previous_state": state,
                }))?,
            };
            transaction.execute(
                "UPDATE tasks SET state_json = 'cancelled', updated_at = CURRENT_TIMESTAMP
                 WHERE id = ?1",
                params![task_id],
            )?;
            transaction.execute(
                "UPDATE attempts SET state = 'cancelled', ended_at = CURRENT_TIMESTAMP,
                     terminal_json = ?1
                 WHERE id = (SELECT active_attempt_id FROM tasks WHERE id = ?2)",
                params![terminal_json, task_id],
            )?;
            transaction.execute(
                "UPDATE resource_leases SET state = 'released', released_at = CURRENT_TIMESTAMP
                 WHERE task_id = ?1 AND state = 'active'",
                params![task_id],
            )?;
            transaction.execute(
                "UPDATE application_root_attachments SET state = 'detached',
                     detached_at = COALESCE(detached_at, CURRENT_TIMESTAMP)
                 WHERE root_task_id = ?1 AND state = 'attached'",
                params![task_id],
            )?;
            append_event(&transaction, &task, RuntimeEvent::TaskCancelled)?;
            reclaimed += 1;
        }
        transaction.commit()?;
        Ok(reclaimed)
    }

    pub fn recovered_tasks(&self) -> Result<Vec<PersistedRecoveredTask>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.root_session_id, tasks.id, tasks.parent_id, tasks.depth, attempts.id, attempts.number,
                    tasks.workspace_lease_id,
                    (SELECT resource_key FROM resource_leases
                     WHERE task_id = tasks.id AND state = 'active' AND resource_key LIKE 'worktree:%'
                     ORDER BY acquired_at DESC, id DESC LIMIT 1),
                    attempts.checkpoint_json, attempts.usage_json, tasks.state_json, tasks.delivery_json,
                    tasks.workspace_mode, tasks.inherited_sandbox
             FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.state_json IN ('recovery_required', 'recovery_gated', 'recovery_attested')
             ORDER BY tasks.root_session_id, tasks.depth, tasks.created_at, tasks.id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u8>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            })?
            .map(|row| {
                let (
                    session_id,
                    task_id,
                    parent_id,
                    depth,
                    attempt_id,
                    attempt_number,
                    workspace_lease_id,
                    worktree_lease,
                    checkpoint_json,
                    tool_state_json,
                    task_state,
                    delivery_json,
                    workspace_mode,
                    inherited_sandbox,
                ) = row?;
                Ok(PersistedRecoveredTask {
                    session_id: session_id.parse().map_err(|_| {
                        RepositoryError::UnknownEventKind {
                            kind: format!("invalid session ID in store: {session_id}"),
                        }
                    })?,
                    task_id: task_id
                        .parse()
                        .map_err(|_| RepositoryError::UnknownEventKind {
                            kind: format!("invalid task ID in store: {task_id}"),
                        })?,
                    parent_id: parent_id
                        .map(|parent_id| {
                            parent_id
                                .parse()
                                .map_err(|_| RepositoryError::UnknownEventKind {
                                    kind: format!("invalid parent task ID in store: {parent_id}"),
                                })
                        })
                        .transpose()?,
                    depth,
                    attempt_id: attempt_id.parse().map_err(|_| {
                        RepositoryError::UnknownEventKind {
                            kind: format!("invalid attempt ID in store: {attempt_id}"),
                        }
                    })?,
                    attempt_number,
                    workspace_lease_id,
                    worktree_lease,
                    checkpoint_json,
                    tool_state_json,
                    objective: serde_json::from_str::<serde_json::Value>(&delivery_json)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("objective")
                                .and_then(|value| value.as_str())
                                .map(str::to_owned)
                        })
                        .unwrap_or_else(|| {
                            "Recover safely: inspect the recorded worktree before changes.".into()
                        }),
                    recovery_gated: task_state == "recovery_gated",
                    recovery_attested: task_state == "recovery_attested",
                    workspace_mode: ChildWriteMode::parse(&workspace_mode).ok_or_else(|| {
                        RepositoryError::UnknownEventKind {
                            kind: format!("invalid workspace_mode in store: {workspace_mode}"),
                        }
                    })?,
                    inherited_sandbox: inherited_sandbox
                        .as_deref()
                        .map(|value| {
                            InheritedSandbox::parse(value).ok_or_else(|| {
                                RepositoryError::UnknownEventKind {
                                    kind: format!("invalid inherited_sandbox in store: {value}"),
                                }
                            })
                        })
                        .transpose()?,
                })
            })
            .collect()
    }
}
