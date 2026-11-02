use super::*;

impl RuntimeRepository {
    pub fn task_snapshots(&self) -> Result<Vec<PersistedTask>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.id, tasks.state_json
             FROM tasks
             ORDER BY tasks.created_at, tasks.id",
        )?;
        statement
            .query_map([], |row| {
                Ok(PersistedTask {
                    task_id: row.get(0)?,
                    state: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, rusqlite::Error>>()
            .map_err(RepositoryError::from)
    }

    pub fn queued_task_count(&self) -> Result<usize, RepositoryError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM tasks WHERE state_json = 'queued' AND parent_id IS NOT NULL",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn task_detail(&self, task: &TaskId) -> Result<PersistedTaskDetail, RepositoryError> {
        let detail = self
            .connection
            .query_row(
                "SELECT tasks.id, tasks.root_session_id, tasks.parent_id, tasks.depth,
                        tasks.state_json, tasks.delivery_json, attempts.terminal_json
                 FROM tasks
                 JOIN attempts ON attempts.id = tasks.active_attempt_id
                 WHERE tasks.id = ?1",
                params![task.to_string()],
                |row| {
                    Ok(PersistedTaskDetail {
                        task_id: row.get(0)?,
                        session_id: row.get(1)?,
                        parent_task_id: row.get(2)?,
                        depth: row.get(3)?,
                        state: row.get(4)?,
                        delivery_json: row.get(5)?,
                        terminal_json: row.get(6)?,
                    })
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => RepositoryError::TaskNotFound {
                    task: task.to_string(),
                },
                error => RepositoryError::Sql(error),
            })?;
        Ok(detail)
    }

    pub fn task_summaries(
        &self,
        root_session_id: Option<&RootSessionId>,
        active_only: bool,
    ) -> Result<Vec<PersistedTaskSummary>, RepositoryError> {
        let (sql, parameters): (&str, Vec<String>) = match (root_session_id, active_only) {
            (Some(session), false) => (
                "SELECT id, state_json, parent_id IS NULL, parent_id, thread_id FROM tasks
                 WHERE root_session_id = ?1 ORDER BY created_at, id",
                vec![session.to_string()],
            ),
            (None, true) => (
                "SELECT id, state_json, parent_id IS NULL, parent_id, thread_id FROM tasks
                 WHERE state_json NOT IN ('completed', 'completed_no_changes', 'failed', 'cancelled')
                 ORDER BY created_at, id",
                Vec::new(),
            ),
            (None, false) => (
                "SELECT id, state_json, parent_id IS NULL, parent_id, thread_id FROM tasks ORDER BY created_at, id",
                Vec::new(),
            ),
            (Some(session), true) => (
                "SELECT id, state_json, parent_id IS NULL, parent_id, thread_id FROM tasks
                 WHERE root_session_id = ?1
                   AND state_json NOT IN ('completed', 'completed_no_changes', 'failed', 'cancelled')
                 ORDER BY created_at, id",
                vec![session.to_string()],
            ),
        };
        let mut statement = self.connection.prepare(sql)?;
        let mut summaries = statement
            .query_map(rusqlite::params_from_iter(parameters), |row| {
                Ok(PersistedTaskSummary {
                    task_id: row.get(0)?,
                    state: row.get(1)?,
                    is_root: row.get(2)?,
                    parent_task_id: row.get(3)?,
                    thread_id: row.get(4)?,
                    usage: crate::ipc::IpcTaskUsage::default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::Sql)?;

        // 用量按 task 聚合后挂上。单开一次查询而不是写进上面的 SQL：分项存在
        // `attempt_watchdogs.usage_json`（JSON 文本）里，SQL 求和不了，只能在
        // Rust 侧解析；一次批量取回避免 N+1。
        let ids: Vec<String> = summaries.iter().map(|s| s.task_id.clone()).collect();
        let usages = self.attempt_usages_for_tasks(&ids)?;
        for summary in &mut summaries {
            if let Some(usage) = usages.get(&summary.task_id) {
                summary.usage = *usage;
            }
        }
        Ok(summaries)
    }

    /// 按 task 聚合其所有 attempt 的用量分项。
    ///
    /// 一个 task 可以有多次 attempt（重试），用量记在 attempt 上，所以必须 `SUM`
    /// 而不是取最后一次——重试烧掉的 token 同样是真实开销。
    fn attempt_usages_for_tasks(
        &self,
        task_ids: &[String],
    ) -> Result<std::collections::HashMap<String, crate::ipc::IpcTaskUsage>, RepositoryError> {
        let mut totals: std::collections::HashMap<String, crate::ipc::IpcTaskUsage> =
            std::collections::HashMap::new();
        if task_ids.is_empty() {
            return Ok(totals);
        }
        // `IN (?1, ?2, ...)` 的占位符按需生成：sqlite 不支持用单个参数展开数组。
        let placeholders = (1..=task_ids.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT task_id, usage_json FROM attempt_watchdogs WHERE task_id IN ({placeholders})"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(task_ids.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::Sql)?;
        for (task_id, usage_json) in rows {
            // 解析失败的 attempt 跳过而不是整个查询失败：一行坏数据不该让整个
            // 子任务列表消失（列表本身还有状态等其它信息要展示）。
            let Ok(usage) = serde_json::from_str::<WatchdogUsage>(&usage_json) else {
                continue;
            };
            let entry = totals.entry(task_id).or_default();
            entry.input_tokens = entry.input_tokens.saturating_add(usage.input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(usage.output_tokens);
            entry.cache_read_tokens = entry
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
        }
        Ok(totals)
    }

    /// Returns a task and, when requested, all descendants in stable tree order.
    pub fn task_tree_ids(
        &self,
        task: &TaskId,
        recursive: bool,
    ) -> Result<Vec<TaskId>, RepositoryError> {
        self.task_detail(task)?;
        let mut statement = self.connection.prepare(
            "WITH RECURSIVE affected(id, tree_depth) AS (
                SELECT id, 0 FROM tasks WHERE id = ?1
                UNION ALL
                SELECT tasks.id, affected.tree_depth + 1
                FROM tasks JOIN affected ON tasks.parent_id = affected.id
                WHERE ?2
             )
             SELECT id FROM affected ORDER BY tree_depth, id",
        )?;
        statement
            .query_map(params![task.to_string(), recursive], |row| {
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

    /// Resolves every durable effect a cancellation confirmation describes.
    /// The stable ordering makes it safe to bind the preview to its action.
    pub fn cancel_scope(
        &self,
        task: &TaskId,
        recursive: bool,
    ) -> Result<PersistedCancelScope, RepositoryError> {
        let task_ids = self.task_tree_ids(task, recursive)?;
        let mut active_leases = Vec::new();
        let mut unmerged_deliveries = Vec::new();
        for task_id in &task_ids {
            let mut leases = self.connection.prepare(
                "SELECT id, resource_key FROM resource_leases
                 WHERE task_id = ?1 AND state = 'active' ORDER BY resource_key, id",
            )?;
            active_leases.extend(
                leases
                    .query_map([task_id.to_string()], |row| {
                        Ok(PersistedActiveLease {
                            lease_id: row.get(0)?,
                            task_id: task_id.clone(),
                            resource_key: row.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let mut deliveries = self.connection.prepare(
                // 父审下线后不再写 reviews，所有交付行都尚未被合并，
                // 因此取消预览一律把它们当作待合并交付披露。
                "SELECT deliveries.id, deliveries.payload_json FROM deliveries
                 WHERE deliveries.task_id = ?1
                 ORDER BY deliveries.created_at, deliveries.id",
            )?;
            unmerged_deliveries.extend(
                deliveries
                    .query_map([task_id.to_string()], |row| {
                        Ok(PersistedUnmergedDelivery {
                            delivery_id: row.get(0)?,
                            task_id: task_id.clone(),
                            payload_json: row.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(PersistedCancelScope {
            task_ids,
            active_leases,
            unmerged_deliveries,
        })
    }

    /// Read task snapshots, a high-water mark, and replay in one SQLite read transaction.
    pub fn subscription_snapshot(
        &mut self,
        after_event_id: i64,
    ) -> Result<RuntimeSubscriptionSnapshot, RepositoryError> {
        let transaction = self.connection.transaction()?;
        let high_water_event_id: i64 = transaction.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'events'), 0)",
            [],
            |row| row.get(0),
        )?;
        let oldest_replayable_event_id: i64 = transaction.query_row(
            "SELECT value FROM runtime_metadata WHERE key = 'event_replay_floor'",
            [],
            |row| row.get(0),
        )?;
        let cursor_state = if after_event_id < oldest_replayable_event_id.saturating_sub(1)
            || after_event_id > high_water_event_id
        {
            RuntimeCursorState::Expired {
                oldest_replayable_event_id,
            }
        } else if after_event_id == 0 {
            RuntimeCursorState::Fresh
        } else {
            RuntimeCursorState::Replayable
        };
        let tasks = {
            let mut statement = transaction.prepare(
                "SELECT tasks.id, tasks.state_json
                     FROM tasks
                     ORDER BY tasks.created_at, tasks.id",
            )?;
            statement
                .query_map([], |row| {
                    Ok(PersistedTask {
                        task_id: row.get(0)?,
                        state: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, rusqlite::Error>>()
                .map_err(RepositoryError::from)?
        };
        let events = if matches!(cursor_state, RuntimeCursorState::Replayable) {
            let mut statement = transaction.prepare(
                "SELECT id, task_id, kind, payload_json FROM events WHERE id > ?1 AND id <= ?2 ORDER BY id",
            )?;
            statement
                .query_map(params![after_event_id, high_water_event_id], |row| {
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
                        task_id: task_id.parse().map_err(|_| {
                            RepositoryError::UnknownEventKind {
                                kind: format!("invalid task ID in store: {task_id}"),
                            }
                        })?,
                        event: RuntimeEvent::parse(kind)?,
                        payload_json,
                    })
                })
                .collect::<Result<Vec<_>, RepositoryError>>()?
        } else {
            Vec::new()
        };
        transaction.commit()?;
        Ok(RuntimeSubscriptionSnapshot {
            high_water_event_id,
            cursor_state,
            tasks,
            events,
        })
    }
}
