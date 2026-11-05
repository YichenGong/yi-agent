//! repository 的跨模块共享辅助函数。
//!
//! 这些函数原先内联在 `repository/mod.rs`，被多个域子模块（watchdog / mailbox /
//! permission / trace / task / schedule）共同调用。搬到这里是为了让 `mod.rs`
//! 只剩类型定义、`open`、`migrate` 与模块声明，同时保持每个函数体逐字不变。
//!
//! 可见性为 `pub(super)`：只有 repository 这一层用得到，不外泄到 crate 外。

use super::*;

pub(super) fn append_event(
    transaction: &Transaction<'_>,
    task: &TaskId,
    event: RuntimeEvent,
) -> Result<i64, RepositoryError> {
    append_event_with_payload(transaction, task, event, "{}")
}

pub(super) fn append_event_with_payload(
    transaction: &Transaction<'_>,
    task: &TaskId,
    event: RuntimeEvent,
    payload_json: &str,
) -> Result<i64, RepositoryError> {
    append_event_with_actor_payload(
        transaction,
        task,
        event,
        r#"{"kind":"runtime"}"#,
        payload_json,
    )
}

pub(super) fn append_event_with_actor_payload(
    transaction: &Transaction<'_>,
    task: &TaskId,
    event: RuntimeEvent,
    actor_json: &str,
    payload_json: &str,
) -> Result<i64, RepositoryError> {
    let inserted = transaction.execute(
        "INSERT INTO events (session_id, task_id, actor_json, kind, payload_json)
         SELECT root_session_id, id, ?1, ?2, ?3
         FROM tasks WHERE id = ?4",
        params![actor_json, event.name(), payload_json, task.to_string()],
    )?;
    if inserted == 0 {
        return Err(RepositoryError::TaskNotFound {
            task: task.to_string(),
        });
    }
    Ok(transaction.last_insert_rowid())
}

pub(super) fn insert_attempt(
    transaction: &Transaction<'_>,
    attempt: &AttemptId,
    task: &TaskId,
    number: u32,
    state: &str,
) -> Result<(), RepositoryError> {
    transaction.execute(
        "INSERT INTO attempts (id, task_id, number, state, budget_json, usage_json)
         VALUES (?1, ?2, ?3, ?4, '{}', '{}')",
        params![attempt.to_string(), task.to_string(), number, state],
    )?;
    let limits = WatchdogLimits::default();
    transaction.execute(
        "INSERT INTO attempt_watchdogs (
            attempt_id, task_id, limits_json, usage_json, last_meaningful_event_id,
            last_meaningful_at, resource_wait_key, resource_wait_started_at
         ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, NULL, NULL)",
        params![
            attempt.to_string(),
            task.to_string(),
            serde_json::to_string(&limits)?,
            serde_json::to_string(&WatchdogUsage::default())?,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub(super) fn validate_recovery_context(
    context: &WorkerRecoveryContext,
) -> Result<(), RepositoryError> {
    let workspace = context
        .workspace_lease_id
        .as_deref()
        .filter(|lease| lease.starts_with("workspace:") && lease.len() > "workspace:".len())
        .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
            reason: "workspace lease is missing".into(),
        })?;
    // A read-only worker runs in place, so there is no worktree to recover into.
    // The checkpoint kind is the explicit permission: it is never inferred from
    // an absent lease, because a coding worker that lost its lease would then be
    // admitted as if it were read-only.
    let checkpoint: serde_json::Value =
        serde_json::from_str(&context.checkpoint_json).map_err(|error| {
            RepositoryError::InvalidWorkerRecoveryContext {
                reason: format!("checkpoint evidence is invalid JSON: {error}"),
            }
        })?;
    let is_read_only =
        checkpoint.get("kind").and_then(serde_json::Value::as_str) == Some("read_only_checkpoint");
    if !is_read_only {
        let worktree = context
            .worktree_lease
            .as_deref()
            .filter(|lease| lease.starts_with("worktree:") && lease.len() > "worktree:".len())
            .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
                reason: "worktree lease is missing".into(),
            })?;
        if worktree == "worktree:" {
            return Err(RepositoryError::InvalidWorkerRecoveryContext {
                reason: "resource lease key is empty".into(),
            });
        }
    }
    if workspace == "workspace:" {
        return Err(RepositoryError::InvalidWorkerRecoveryContext {
            reason: "resource lease key is empty".into(),
        });
    }
    serde_json::from_str::<serde_json::Value>(&context.tool_state_json).map_err(|error| {
        RepositoryError::InvalidWorkerRecoveryContext {
            reason: format!("tool-state evidence is invalid JSON: {error}"),
        }
    })?;
    Ok(())
}

/// Deletes a task's oldest trace rows until it retains at most
/// [`TRACE_MAX_ROWS_PER_TASK`].
///
/// `id` is the insertion order (`AUTOINCREMENT`), so the 2000th newest id is the
/// watermark: everything strictly below it is the tail the ceiling drops. When
/// the trace is at or under the ceiling the inner query lands on the oldest
/// retained row (or nothing), and nothing is deleted. The caller owns the
/// transaction so the trim and the insert it belongs to commit together.
pub(super) fn trim_trace_rows(
    transaction: &Transaction<'_>,
    task: &TaskId,
) -> Result<(), RepositoryError> {
    transaction.execute(
        "DELETE FROM task_trace_events
         WHERE task_id = ?1 AND id < (
             SELECT id FROM task_trace_events WHERE task_id = ?1
             ORDER BY id DESC LIMIT 1 OFFSET ?2
         )",
        params![task.to_string(), TRACE_MAX_ROWS_PER_TASK as i64 - 1],
    )?;
    Ok(())
}

pub(super) fn migrate(connection: &Connection) -> Result<(), RepositoryError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);",
    )?;
    let current_version: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    // `>=` used to be the whole gate, which silently treated a *newer* database
    // as up to date: written by a build we do not understand, and then written to
    // by us. A future schema must be an explicit refusal; equality is the only
    // success. `SchemaIncompatible` maps to `InvalidState`, so callers surface it
    // as a user-visible "upgrade (or stop) the other build" rather than an
    // internal error.
    if current_version > LATEST_SCHEMA_VERSION {
        return Err(RepositoryError::SchemaIncompatible {
            reason: format!(
                "database schema v{current_version} was written by a newer build \
                 (this build supports up to v{LATEST_SCHEMA_VERSION})"
            ),
        });
    }
    if current_version == LATEST_SCHEMA_VERSION {
        return Ok(());
    }
    if current_version < 1 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY, project_root TEXT NOT NULL, state TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            config_json TEXT NOT NULL
        );
        CREATE TABLE tasks (
            id TEXT PRIMARY KEY, root_session_id TEXT NOT NULL REFERENCES sessions(id),
            parent_id TEXT REFERENCES tasks(id), depth INTEGER NOT NULL, state_json TEXT NOT NULL,
            contract_version INTEGER NOT NULL, active_attempt_id TEXT NOT NULL, delivery_json TEXT NOT NULL,
            workspace_lease_id TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE attempts (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), number INTEGER NOT NULL,
            state TEXT NOT NULL, checkpoint_json TEXT, budget_json TEXT NOT NULL, usage_json TEXT NOT NULL,
            started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, ended_at TEXT, terminal_json TEXT,
            UNIQUE(task_id, number)
        );
        CREATE TABLE contracts (
            task_id TEXT NOT NULL REFERENCES tasks(id), version INTEGER NOT NULL, payload_json TEXT NOT NULL,
            digest TEXT NOT NULL, created_event_id INTEGER NOT NULL, PRIMARY KEY(task_id, version)
        );
        CREATE TABLE contract_amendments (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), from_version INTEGER NOT NULL,
            to_version INTEGER NOT NULL, payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE mailbox_messages (
            id TEXT PRIMARY KEY, recipient_task_id TEXT NOT NULL REFERENCES tasks(id), sender_task_id TEXT,
            kind TEXT NOT NULL, priority INTEGER NOT NULL, correlation_id TEXT, causation_id TEXT,
            payload_json TEXT NOT NULL, coalesced_count INTEGER NOT NULL DEFAULT 0, delivered_at TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE resource_leases (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), resource_key TEXT NOT NULL,
            mode TEXT NOT NULL, units INTEGER NOT NULL, state TEXT NOT NULL,
            acquired_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, released_at TEXT
        );
        CREATE TABLE deliveries (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), attempt_id TEXT NOT NULL,
            payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE reviews (
            id TEXT PRIMARY KEY, delivery_id TEXT NOT NULL REFERENCES deliveries(id), actor_json TEXT NOT NULL,
            decision TEXT NOT NULL, evidence_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE permission_requests (
            id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), payload_json TEXT NOT NULL,
            state TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, decided_at TEXT
        );
        CREATE TABLE schedules (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), definition_json TEXT NOT NULL,
            state TEXT NOT NULL, next_run_at TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE events (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL REFERENCES sessions(id),
            task_id TEXT REFERENCES tasks(id), attempt_id TEXT, actor_json TEXT NOT NULL, kind TEXT NOT NULL,
            payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX tasks_root_state_idx ON tasks(root_session_id, state_json);
        CREATE INDEX mailbox_recipient_idx ON mailbox_messages(recipient_task_id, delivered_at, priority);
        CREATE INDEX leases_resource_idx ON resource_leases(resource_key, state);
        CREATE INDEX events_session_id_idx ON events(session_id, id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
        transaction.commit()?;
    }
    if current_version < 2 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE runtime_metadata (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            INSERT INTO runtime_metadata (key, value) VALUES ('event_replay_floor', 1);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (2)", [])?;
        transaction.commit()?;
    }
    if current_version < 3 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE resource_admission_cursors (
                resource_key TEXT PRIMARY KEY,
                root_session_id TEXT,
                parent_task_id TEXT,
                sequence INTEGER NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX admission_cursor_root_idx
                ON resource_admission_cursors(root_session_id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (3)", [])?;
        transaction.commit()?;
    }
    if current_version < 4 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE attempt_watchdogs (
                attempt_id TEXT PRIMARY KEY REFERENCES attempts(id),
                task_id TEXT NOT NULL REFERENCES tasks(id),
                limits_json TEXT NOT NULL,
                usage_json TEXT NOT NULL,
                last_meaningful_event_id INTEGER,
                last_meaningful_at TEXT NOT NULL,
                resource_wait_key TEXT,
                resource_wait_started_at TEXT
            );
            CREATE INDEX attempt_watchdogs_task_idx ON attempt_watchdogs(task_id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (4)", [])?;
        transaction.commit()?;
    }
    if current_version < 5 {
        let transaction = connection.unchecked_transaction()?;
        // Schedules formerly referenced one session, but each fire now owns a
        // new root. Rebuild the unused early table without that false link.
        let schedules_exist = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schedules')",
                [],
                |row| row.get::<_, bool>(0),
            )?;
        if schedules_exist {
            transaction.execute_batch(
                "ALTER TABLE schedules RENAME TO schedules_legacy;
             CREATE TABLE schedules (
                id TEXT PRIMARY KEY,
                definition_json TEXT NOT NULL,
                state TEXT NOT NULL,
                next_run_at TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             INSERT INTO schedules (id, definition_json, state, next_run_at, created_at)
             SELECT id, definition_json, state, COALESCE(next_run_at, CURRENT_TIMESTAMP), created_at
             FROM schedules_legacy;
             DROP TABLE schedules_legacy;
             CREATE TABLE schedule_occurrences (
                schedule_id TEXT NOT NULL REFERENCES schedules(id),
                due_at TEXT NOT NULL,
                outcome TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(schedule_id, due_at)
             );
             CREATE INDEX schedule_next_run_idx ON schedules(state, next_run_at);",
            )?;
        } else {
            transaction.execute_batch(
                "CREATE TABLE schedules (
                    id TEXT PRIMARY KEY,
                    definition_json TEXT NOT NULL,
                    state TEXT NOT NULL,
                    next_run_at TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                 );
             CREATE TABLE schedule_occurrences (
                schedule_id TEXT NOT NULL REFERENCES schedules(id),
                due_at TEXT NOT NULL,
                outcome TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(schedule_id, due_at)
             );
             CREATE INDEX schedule_next_run_idx ON schedules(state, next_run_at);",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (5)", [])?;
        transaction.commit()?;
    }
    if current_version < 6 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "ALTER TABLE schedule_occurrences
             ADD COLUMN root_session_id TEXT REFERENCES sessions(id);",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (6)", [])?;
        transaction.commit()?;
    }
    if current_version < 7 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE application_root_attachments (
                idempotency_key TEXT PRIMARY KEY,
                root_session_id TEXT NOT NULL,
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                capability_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                detached_at TEXT
             );",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (7)", [])?;
        transaction.commit()?;
    }

    if current_version < 8 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('application_root_attachments')
                WHERE name = 'capability_secret'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            transaction.execute_batch(
                "ALTER TABLE application_root_attachments ADD COLUMN capability_secret TEXT;",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (8)", [])?;
        transaction.commit()?;
    }

    if current_version < 9 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'workspace_mode'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Existing rows own worktrees, so backfill them as coding. New
            // inserts always specify the mode explicitly.
            transaction.execute_batch(
                "ALTER TABLE tasks ADD COLUMN workspace_mode TEXT NOT NULL DEFAULT 'coding';",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (9)", [])?;
        transaction.commit()?;
    }

    if current_version < 10 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'model'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Null means the child inherits its parent's model.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN model TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (10)", [])?;
        transaction.commit()?;
    }

    if current_version < 11 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('application_root_attachments')
                WHERE name = 'workspace_root'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // A read-only application root keeps no `task_workspaces` row, so its
            // project workspace is recorded here instead.
            transaction.execute_batch(
                "ALTER TABLE application_root_attachments ADD COLUMN workspace_root TEXT;",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (11)", [])?;
        transaction.commit()?;
    }

    if current_version < 12 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'inherited_sandbox'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // NULL means "no inheritance recorded": the worker factory falls
            // back to its configured sandbox, preserving pre-change behavior.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN inherited_sandbox TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (12)", [])?;
        transaction.commit()?;
    }

    // v13 rather than a widened v12: a database that already recorded v12 for
    // the inherited sandbox would never re-run v12, so folding the trace table
    // into it would leave those installations without `task_trace_events`.
    if current_version < 13 {
        let transaction = connection.unchecked_transaction()?;
        // A worker's trace lives apart from the audit `events` table: it is
        // high-volume, prunable observation data, not durable history.
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS task_trace_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES tasks(id),
                kind TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX IF NOT EXISTS task_trace_task_id_idx ON task_trace_events(task_id, id);",
        )?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'thread_id'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Null means the task belongs to no desktop conversation, which is
            // every task that predates this column.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN thread_id TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (13)", [])?;
        transaction.commit()?;
    }

    // v14：父审状态下线。历史行改写为 `completed`，并清掉 review-only 的
    // mailbox 消息与事件 kind——否则删除枚举后 `RuntimeEvent::parse` 会对
    // 残留 kind 直接报 UnknownEventKind，让 ReadTaskEvents 整条读取失败。
    // 父审通知行 kind 是仍在用的 'user_override'，靠 payload 里的
    // 'user_review_override' 区分；它们同样是 review 时代产物，必须一并清掉，
    // 否则重启后会被当成真的用户指令重放给父级。
    if current_version < 14 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "UPDATE tasks SET state_json = 'completed'
                 WHERE state_json = 'awaiting_parent_review';
             UPDATE attempts SET state = 'completed'
                 WHERE state = 'awaiting_parent_review';
             DELETE FROM mailbox_messages
                 WHERE kind IN ('rework', 'review_rejected')
                    OR (kind = 'user_override'
                        AND sender_task_id IS NULL
                        AND json_extract(payload_json, '$.kind') = 'user_review_override');
             UPDATE events SET kind = 'task_completed'
                 WHERE kind IN ('review_approved', 'review_accepted',
                                'review_rework', 'review_rejected');",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (14)", [])?;
        transaction.commit()?;
    }

    // v15：根会话的关联根。会话创建时快照进根任务行，子任务按父链继承。
    if current_version < 15 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'extra_roots'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // JSON array of canonical paths. NULL means "no linked roots": the
            // worker falls back to its single-root behavior, so pre-change rows
            // keep today's semantics.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN extra_roots TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (15)", [])?;
        transaction.commit()?;
    }

    // v16：root 的最近所有者与退出原因。租约/所有权本身住在内存里（daemon 重启
    // 即新生代），这张表只回答「这个会话上一次由谁 attach、为什么结束」——用于
    // 重启后向用户解释，以及阻止把已结束的 root 当成从没绑定过。
    //
    // ALTER TABLE 在 SQLite 里不可回滚，所以每一步都先用 `pragma_table_info`
    // 探测再改，事务只包住 DDL + 版本行。中途失败时版本行不落，下次从 v15 重来，
    // 探测让已存在的列/表不会被重复创建。
    if current_version < 16 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS root_owners (
                root TEXT PRIMARY KEY,
                owner TEXT NOT NULL,
                generation TEXT NOT NULL,
                ended_reason TEXT,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (16)", [])?;
        transaction.commit()?;
    }

    // v17：前台 root 生命周期的事实表。
    //
    // 三张表分别回答三个不同的问题，刻意不合并：
    //   * `foreground_root_state`：这个 root 是谁的、什么分类、准入是否开着、活动快照是什么。
    //     一行一个 root，`binding_epoch`/`driver_epoch` 是准入栅栏，必须持久化——重启后
    //     旧代的报告不能落在一个新代上。
    //   * `foreground_recovery_operations`：恢复操作的幂等记录。`request_fingerprint` 绑定
    //     稳定 root/session、原 attempt 和动作；`CHECK((phase='committed') = (result_json
    //     IS NOT NULL))` 让"已提交但没有结果"在数据库层面不可能存在。
    //   * `foreground_stop_evidence`：停止证明，key 到 (root, attempt, execution)。
    //     只记证据不记权限，恢复结果里也不带任何秘密。
    //
    // 迁移本身**不**给任何 root 写入分类行：旧 attachment 保留原样，`foreground_class`
    // 读时按 legacy 策略解释（见 `repository::foreground`）。已存在的历史行也不会被
    // 回填成 managed——那需要一个通过认证的 prepare 加必要停止边界，迁移做不到也不该假装做到。
    if current_version < 17 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS foreground_root_state (
                root_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
                binding_epoch INTEGER NOT NULL CHECK(binding_epoch >= 0),
                driver_epoch INTEGER NOT NULL CHECK(driver_epoch >= 0),
                owner_client_id TEXT,
                admission_state TEXT NOT NULL CHECK(admission_state IN ('closed', 'frozen')),
                driver_bind_operation_id TEXT,
                activity_json TEXT,
                classification TEXT NOT NULL CHECK(classification IN ('legacy', 'pending', 'managed'))
            );
            CREATE TABLE IF NOT EXISTS foreground_recovery_operations (
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                operation_id TEXT NOT NULL,
                expected_attempt_id TEXT NOT NULL REFERENCES attempts(id),
                request_fingerprint TEXT NOT NULL,
                original_request_json TEXT NOT NULL,
                phase TEXT NOT NULL CHECK(phase IN ('preparing','needs_preview','blocked','ready','committed')),
                impact_json TEXT,
                result_json TEXT,
                PRIMARY KEY(root_task_id, operation_id),
                CHECK((phase = 'committed') = (result_json IS NOT NULL))
            );
            CREATE TABLE IF NOT EXISTS foreground_stop_evidence (
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                attempt_id TEXT NOT NULL REFERENCES attempts(id),
                execution_id TEXT NOT NULL,
                evidence_json TEXT NOT NULL,
                PRIMARY KEY(root_task_id, attempt_id, execution_id)
            );",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (17)", [])?;
        transaction.commit()?;
    }

    // v18：一次冻结对应的**封闭执行集合**。
    //
    // 停止证明的正确性取决于"证明的是同一批执行"。若集合只存在于内存，重启后 daemon
    // 只剩一组证据，却不知道它们原本该覆盖谁——那样"集合里每个执行都有证明"就退化成
    // "有证据的执行都停了"，冻结后新准入的东西会被静默漏掉。
    //
    // `foreground_freeze`：每个 root 当前那一次冻结，`execution_set_version` 单调递增，
    // 旧证据只能匹配自己那一版。`foreground_frozen_executions`：集合成员，带不可复用的
    // execution identity 与所属 attempt。二者与 v17 的 `foreground_stop_evidence`
    // 一起，让"冻结、证明、核验"三步都能在重启后重放。
    if current_version < 18 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS foreground_freeze (
                root_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
                execution_set_version INTEGER NOT NULL CHECK(execution_set_version > 0),
                frozen_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS foreground_frozen_executions (
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                execution_set_version INTEGER NOT NULL,
                execution_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN ('driver', 'worker')),
                PRIMARY KEY(root_task_id, execution_set_version, execution_id)
            );",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (18)", [])?;
        transaction.commit()?;
    }

    // v19：会话容器角色与委派事实。
    //
    // 关键约束是**角色必须是持久字段**，不能从 `parent_id IS NULL`、cwd、thread
    // 字符串或 watchdog 分类推导：那些判据要么把 worker 误当容器，要么让历史
    // `stalled` 永久封禁一个仍然开放的会话。
    //
    // 迁移只做两件事：回填已验证的 application root 为 `session_container`，
    // 并为它们建一条**准入**记录（`closed = 0`）。`closed = 0` 不表示准入此刻是
    // 开放的——开放要当前身份 + 绑定/turn 栅栏 + 停止边界，迁移一概不猜。
    // 尤其不能为升级批量写 tombstone：历史 `cancelled` 无法区分预算级联与用户
    // 关闭，把它当永久关闭会把合法会话锁死。
    //
    // 损坏的 attachment（同 root 多线程 key、root/session 不匹配、指向 child、
    // 非 root 深度、session 不存在）只记进 `session_migration_errors`，合法项照常
    // 迁移，其余任务保留 `execution` 默认值。不修改 attempt/event/terminal 证据。
    if current_version < 19 {
        let transaction = connection.unchecked_transaction()?;
        // `ALTER TABLE` cannot be rolled back with the version row in every crash
        // window (the same hazard v16 documents), so probe before adding: a
        // replay must succeed instead of failing on "duplicate column name".
        let has_role: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'role')",
            [],
            |row| row.get(0),
        )?;
        if !has_role {
            transaction.execute_batch(
                "ALTER TABLE tasks ADD COLUMN role TEXT NOT NULL DEFAULT 'execution'
                    CHECK(role IN ('execution','session_container'));",
            )?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_containers (
                root_task_id TEXT PRIMARY KEY REFERENCES tasks(id),
                root_session_id TEXT NOT NULL,
                closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1)),
                binding_epoch INTEGER NOT NULL DEFAULT 0 CHECK(binding_epoch >= 0),
                driver_epoch INTEGER NOT NULL DEFAULT 0 CHECK(driver_epoch >= 0),
                owner_run_id TEXT,
                resume_digest TEXT,
                legacy_upgrade_consumed INTEGER NOT NULL DEFAULT 0
                    CHECK(legacy_upgrade_consumed IN (0,1))
            );
            CREATE TABLE IF NOT EXISTS session_binding_operations (
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                operation_id TEXT NOT NULL,
                request_digest TEXT NOT NULL,
                generation TEXT NOT NULL,
                binding_epoch INTEGER NOT NULL,
                driver_epoch INTEGER NOT NULL,
                PRIMARY KEY(root_task_id, operation_id)
            );
            CREATE TABLE IF NOT EXISTS session_migration_errors (
                attachment_key TEXT PRIMARY KEY,
                reason TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS session_host_executions (
                execution_id TEXT PRIMARY KEY,
                root_task_id TEXT NOT NULL REFERENCES tasks(id),
                generation TEXT NOT NULL,
                binding_epoch INTEGER NOT NULL,
                driver_epoch INTEGER NOT NULL,
                turn_id TEXT NOT NULL,
                identity_json TEXT NOT NULL,
                phase TEXT NOT NULL CHECK(phase IN ('starting','running','unknown','stopped')),
                evidence_json TEXT,
                execution_set_version INTEGER NOT NULL
            );",
        )?;
        backfill_session_container_roles(&transaction)?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (19)", [])?;
        transaction.commit()?;
    }
    Ok(())
}

/// Promotes exactly the application roots whose persisted attachment is coherent.
///
/// The validation runs inside the migration transaction so a crash cannot leave a
/// task marked as a container without its record (or the reverse). Anything that
/// does not validate is *diagnosed*, never repaired: guessing ownership here is
/// how a session silently gets handed to the wrong thread.
fn backfill_session_container_roles(transaction: &Transaction<'_>) -> Result<(), RepositoryError> {
    let mut statement = transaction.prepare(
        "SELECT a.idempotency_key, a.root_session_id, a.root_task_id, a.state,
                t.root_session_id, t.parent_id, t.depth
         FROM application_root_attachments a
         LEFT JOIN tasks t ON t.id = a.root_task_id
         ORDER BY a.idempotency_key",
    )?;
    let rows: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    )> = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    // One root may not be claimed by two thread keys: the second key would be a
    // public alias able to locate (and later try to resume) someone's session.
    let mut by_root: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (key, _, root, _, _, _, _) in &rows {
        by_root.entry(root.as_str()).or_default().push(key.as_str());
    }

    for (key, session, root, state, task_session, parent, depth) in &rows {
        let reason = if task_session.is_none() {
            Some("attachment references a task that does not exist".to_string())
        } else if task_session.as_deref() != Some(session.as_str()) {
            Some(format!(
                "attachment session {session} does not match task session {}",
                task_session.as_deref().unwrap_or("<none>")
            ))
        } else if parent.is_some() || *depth != Some(0) {
            Some("attachment points at a non-root task".to_string())
        } else if !matches!(state.as_str(), "attached" | "detached") {
            Some(format!("attachment has unknown state {state}"))
        } else {
            by_root
                .get(root.as_str())
                .filter(|keys| keys.len() > 1)
                .map(|keys| format!("root is claimed by multiple thread keys: {keys:?}"))
        };
        if let Some(reason) = reason {
            transaction.execute(
                "INSERT INTO session_migration_errors (attachment_key, reason)
                 VALUES (?1, ?2)
                 ON CONFLICT(attachment_key) DO UPDATE SET reason = excluded.reason",
                params![key, reason],
            )?;
            continue;
        }
        transaction.execute(
            "UPDATE tasks SET role = 'session_container' WHERE id = ?1",
            params![root],
        )?;
        // `closed = 0` is *admission-closed-until-proven*: it records that the
        // migration did not observe a close, not that this host may delegate now.
        transaction.execute(
            "INSERT INTO session_containers (root_task_id, root_session_id, closed)
             VALUES (?1, ?2, 0)
             ON CONFLICT(root_task_id) DO NOTHING",
            params![root, session],
        )?;
    }
    Ok(())
}
