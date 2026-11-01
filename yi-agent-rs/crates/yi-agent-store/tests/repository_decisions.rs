use rusqlite::Connection;
use tempfile::TempDir;
use yi_agent_core::subagent::task::{
    AttemptId, DeliveryReport, MessageId, PermissionDecision, PermissionRequestId, RootSessionId,
    TaskId, WorkspaceLeaseId,
};
use yi_agent_store::repository::{RuntimeEvent, RuntimeRepository};

#[test]
fn permission_resolution_is_an_atomic_audited_task_transition() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let task = TaskId::new();
    let attempt = AttemptId::new();
    let request = PermissionRequestId::new();
    let session = RootSessionId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();

    repository
        .request_permission(
            &task,
            &attempt,
            &request,
            r#"{"tool":"network","reason":"download dependency"}"#,
        )
        .unwrap();
    assert_eq!(
        repository.task_state(&task).unwrap(),
        "waiting_for_permission"
    );

    repository
        .resolve_permission(
            &request,
            PermissionDecision::Allow,
            r#"{"kind":"user","id":"local-user"}"#,
        )
        .unwrap();

    assert_eq!(repository.task_state(&task).unwrap(), "queued");
    assert_eq!(
        repository.permission_request_state(&request).unwrap(),
        "allowed"
    );
    let events = repository.event_records_for_task_after(&task, 0).unwrap();
    assert_eq!(
        events.iter().map(|event| event.event).collect::<Vec<_>>(),
        vec![
            RuntimeEvent::PermissionRequested,
            RuntimeEvent::PermissionResolved
        ]
    );
    assert!(events[1].payload_json.contains("local-user"));
}

/// 交付落库即 `completed`，不再是 awaiting_parent_review。
#[test]
fn record_delivery_commits_the_task_as_completed() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let parent = TaskId::new();
    let parent_attempt = AttemptId::new();
    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    let delivery = DeliveryReport::coding(
        "deadbeef",
        "main",
        WorkspaceLeaseId::new(),
        "cargo test -p child",
    );
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&parent, &session, &parent_attempt, 1, "running")
        .unwrap();
    repository
        .create_child_task_with_attempt(&child, &session, &parent, 1, &child_attempt, 1, "running")
        .unwrap();

    repository
        .record_delivery(&child, &child_attempt, &delivery)
        .unwrap();

    assert_eq!(repository.task_state(&child).unwrap(), "completed");
    assert_eq!(
        repository.attempt_state(&child_attempt).unwrap(),
        "completed"
    );
    // 交付证据照旧保留：direct parent 的 mailbox 通知与 deliveries 行都还在，
    // 只是不再有一个等待审批的中间态。
    let mailbox = repository.mailbox_messages_for_task(&parent).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].sender_task_id, Some(child.clone()));
    assert_eq!(mailbox[0].kind, "completed");
    assert!(mailbox[0].payload_json.contains(&delivery.id.to_string()));
    assert!(
        repository
            .event_records_for_task_after(&child, 0)
            .unwrap()
            .iter()
            .any(|event| event.event == RuntimeEvent::TaskDelivered)
    );
}

/// v13 库里的 awaiting_parent_review 行迁移后变 completed，
/// 且旧 review_* 事件 kind 被改写，避免 parse 报未知 kind。
#[test]
fn migration_v14_folds_review_state_into_completed() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let parent = TaskId::new();
    let parent_attempt = AttemptId::new();
    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&parent, &session, &parent_attempt, 1, "completed")
        .unwrap();
    repository
        .create_child_task_with_attempt(
            &child,
            &session,
            &parent,
            1,
            &child_attempt,
            1,
            "completed",
        )
        .unwrap();
    drop(repository);

    // 退回 v13 并写入 review 时代的残留行：一条父审等待中的 task/attempt、
    // 一条 review_accepted 事件、一条 rework 邮件。重新 open 必须触发 v14。
    let connection = Connection::open(&database).unwrap();
    connection
        .execute("DELETE FROM schema_migrations WHERE version >= 14", [])
        .unwrap();
    connection
        .execute(
            "UPDATE tasks SET state_json = 'awaiting_parent_review' WHERE id = ?1",
            [child.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE attempts SET state = 'awaiting_parent_review' WHERE id = ?1",
            [child_attempt.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO events (session_id, task_id, actor_json, kind, payload_json)
             VALUES (?1, ?2, ?3, 'review_accepted', '{}')",
            rusqlite::params![
                session.to_string(),
                child.to_string(),
                r#"{"kind":"runtime"}"#
            ],
        )
        .unwrap();
    let stale_message = MessageId::new();
    connection
        .execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
             VALUES (?1, ?2, ?3, 'rework', 1, ?4)",
            rusqlite::params![
                stale_message.to_string(),
                child.to_string(),
                parent.to_string(),
                r#"{"feedback":"stale"}"#
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
             VALUES (?1, ?2, ?3, 'review_rejected', 1, ?4)",
            rusqlite::params![
                MessageId::new().to_string(),
                child.to_string(),
                parent.to_string(),
                r#"{"reason":"stale"}"#
            ],
        )
        .unwrap();
    // 父审通知：kind 是仍在用的 'user_override'，只有 payload 里的
    // 'user_review_override' 暴露它的 review 出身。
    let review_notification = MessageId::new();
    connection
        .execute(
            "INSERT INTO mailbox_messages
             (id, recipient_task_id, sender_task_id, kind, priority, payload_json)
             VALUES (?1, ?2, NULL, 'user_override', 1, ?3)",
            rusqlite::params![
                review_notification.to_string(),
                parent.to_string(),
                r#"{"message":"stale review notification","kind":"user_review_override"}"#
            ],
        )
        .unwrap();
    drop(connection);

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 15);
    assert_eq!(repository.task_state(&child).unwrap(), "completed");
    assert_eq!(
        repository.attempt_state(&child_attempt).unwrap(),
        "completed"
    );

    // ReadTaskEvents 会逐个 parse kind：残留的 review_* kind 若未改写，
    // 这里会直接以 UnknownEventKind 失败。
    let events = repository.event_records_for_task_after(&child, 0).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event == RuntimeEvent::TaskCompleted),
        "the rewritten review event must be readable as task_completed"
    );

    let connection = Connection::open(&database).unwrap();
    let review_kind_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE kind IN ('review_approved', 'review_accepted',
                            'review_rework', 'review_rejected')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(review_kind_rows, 0, "no review event kind may survive");
    let review_mailbox_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM mailbox_messages
             WHERE kind IN ('rework', 'review_rejected')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        review_mailbox_rows, 0,
        "review-only mailbox rows cannot be replayed once their kinds are gone"
    );
    let review_notification_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM mailbox_messages
             WHERE kind = 'user_override'
               AND json_extract(payload_json, '$.kind') = 'user_review_override'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        review_notification_rows, 0,
        "parent-facing review notifications must not replay as user instructions"
    );
}

#[test]
fn a_fresh_database_has_no_task_workspaces_table() {
    let directory = TempDir::new().unwrap();
    let repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();

    assert!(
        !repository.has_table("task_workspaces").unwrap(),
        "a fresh schema must not create the retired task_workspaces table"
    );
}

/// 交付落库必须同时释放该任务持有的全部 admission 租约。
///
/// `record_delivery` 把任务置为 `completed` 并结束 attempt，但它曾经不碰
/// `resource_leases`：`worker-worktree-context-*` 与 `worker-workspace-context-*`
/// 两行 `exclusive` 租约因此永远停在 `active`。生产库里由此积压了 486 条挂在
/// 终态子任务上的 `active` 租约（最新一条是两周前获取的）。
///
/// 这个测试钉住「交付即释放」：只要 `record_delivery` 重新漏掉释放，它就红。
#[test]
fn record_delivery_releases_the_workers_admission_leases() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let parent = TaskId::new();
    let parent_attempt = AttemptId::new();
    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    let workspace_lease = format!("workspace:{}", directory.path().join("ws").display());
    let worktree_lease = format!("worktree:{}", directory.path().join("wt").display());
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&parent, &session, &parent_attempt, 1, "running")
        .unwrap();
    repository
        .create_child_task_with_attempt(&child, &session, &parent, 1, &child_attempt, 1, "running")
        .unwrap();
    // 模拟 `start_worker` 为 coding 子任务记录终止边界时落下的两行 exclusive 租约。
    repository
        .transition_task_and_attempt_with_recovery_context(
            &child,
            &child_attempt,
            "running",
            RuntimeEvent::TaskStarted,
            &yi_agent_core::subagent::worker::WorkerRecoveryContext {
                workspace_lease_id: Some(workspace_lease.clone()),
                worktree_lease: Some(worktree_lease.clone()),
                checkpoint_json: r#"{"kind":"coding_checkpoint"}"#.into(),
                tool_state_json: "{}".into(),
            },
        )
        .unwrap();
    assert!(
        repository
            .has_active_lease_prefix(&child, "workspace:")
            .unwrap(),
        "precondition: the worker holds a workspace lease while running"
    );

    let delivery = DeliveryReport::coding(
        "deadbeef",
        "main",
        WorkspaceLeaseId::new(),
        "cargo test -p child",
    );
    repository
        .record_delivery(&child, &child_attempt, &delivery)
        .unwrap();

    assert_eq!(repository.task_state(&child).unwrap(), "completed");
    assert!(
        !repository
            .has_active_lease_prefix(&child, "workspace:")
            .unwrap(),
        "a delivered child must not keep holding its workspace lease"
    );
    assert!(
        !repository
            .has_active_lease_prefix(&child, "worktree:")
            .unwrap(),
        "a delivered child must not keep holding its worktree lease"
    );
}

/// 走普通终态路径（没有交付证据）的 `completed` 也必须释放租约。
///
/// `transition_task_and_attempt` 原来只在 `blocked | cancelled | failed` 时
/// 释放，`completed` 被漏掉了。而 reducer 把「没有交付证据的 Completed」
/// （例如终端回放）恰好送到这条路径，于是那条路上每个子任务又留下两行
/// `exclusive` 租约。终态就是终态：这条边界不该按状态名逐个列举。
#[test]
fn completing_without_a_delivery_also_releases_admission_leases() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let parent = TaskId::new();
    let parent_attempt = AttemptId::new();
    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    let workspace_lease = format!("workspace:{}", directory.path().join("ws").display());
    let worktree_lease = format!("worktree:{}", directory.path().join("wt").display());
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&parent, &session, &parent_attempt, 1, "running")
        .unwrap();
    repository
        .create_child_task_with_attempt(&child, &session, &parent, 1, &child_attempt, 1, "running")
        .unwrap();
    repository
        .transition_task_and_attempt_with_recovery_context(
            &child,
            &child_attempt,
            "running",
            RuntimeEvent::TaskStarted,
            &yi_agent_core::subagent::worker::WorkerRecoveryContext {
                workspace_lease_id: Some(workspace_lease),
                worktree_lease: Some(worktree_lease),
                checkpoint_json: r#"{"kind":"coding_checkpoint"}"#.into(),
                tool_state_json: "{}".into(),
            },
        )
        .unwrap();
    assert!(
        repository
            .has_active_lease_prefix(&child, "workspace:")
            .unwrap(),
        "precondition: the worker holds a workspace lease while running"
    );

    repository
        .transition_task_and_attempt(
            &child,
            &child_attempt,
            "completed",
            RuntimeEvent::TaskCompleted,
        )
        .unwrap();

    assert_eq!(repository.task_state(&child).unwrap(), "completed");
    assert!(
        !repository
            .has_active_lease_prefix(&child, "workspace:")
            .unwrap(),
        "a completed child must not keep holding its workspace lease"
    );
    assert!(
        !repository
            .has_active_lease_prefix(&child, "worktree:")
            .unwrap(),
        "a completed child must not keep holding its worktree lease"
    );
}

/// 启动清扫：已结算任务持有的租约必须被回收，恢复态任务的租约必须保留。
///
/// 这是修复前泄漏的兜底。`record_delivery` 与普通 `completed` 两条路径现在
/// 都会当场释放，但历史库里已经积压了 486 条挂在终态子任务上的租约，而且
/// 未来任何新的终态投递路径都可能再漏一次。启动时扫一遍是幂等的安全网。
///
/// 恢复态是刻意排除的：`recovery_required` 是「停着但可恢复」，它的
/// workspace/worktree 租约正是恢复要用的东西，扫掉就等于毁掉恢复凭据。
#[test]
fn boot_sweep_releases_settled_leases_and_spares_recovery_ones() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let session = RootSessionId::new();
    let root = TaskId::new();
    let root_attempt = AttemptId::new();
    repository
        .create_task_with_attempt(&root, &session, &root_attempt, 1, "running")
        .unwrap();

    // 一个已结算子任务，仍握着两行 exclusive 租约（修复前的泄漏形态）。
    let settled = TaskId::new();
    let settled_attempt = AttemptId::new();
    repository
        .create_child_task_with_attempt(
            &settled,
            &session,
            &root,
            1,
            &settled_attempt,
            1,
            "running",
        )
        .unwrap();
    repository
        .transition_task_and_attempt_with_recovery_context(
            &settled,
            &settled_attempt,
            "running",
            RuntimeEvent::TaskStarted,
            &yi_agent_core::subagent::worker::WorkerRecoveryContext {
                workspace_lease_id: Some(format!(
                    "workspace:{}",
                    directory.path().join("ws-settled").display()
                )),
                worktree_lease: Some(format!(
                    "worktree:{}",
                    directory.path().join("wt-settled").display()
                )),
                checkpoint_json: r#"{"kind":"coding_checkpoint"}"#.into(),
                tool_state_json: "{}".into(),
            },
        )
        .unwrap();
    repository
        .transition_task(&settled, "completed", RuntimeEvent::TaskCompleted)
        .unwrap();

    // 一个停在恢复态的子任务，租约是它恢复时的凭据。
    let parked = TaskId::new();
    let parked_attempt = AttemptId::new();
    repository
        .create_child_task_with_attempt(&parked, &session, &root, 1, &parked_attempt, 1, "running")
        .unwrap();
    repository
        .transition_task_and_attempt_with_recovery_context(
            &parked,
            &parked_attempt,
            "running",
            RuntimeEvent::TaskStarted,
            &yi_agent_core::subagent::worker::WorkerRecoveryContext {
                workspace_lease_id: Some(format!(
                    "workspace:{}",
                    directory.path().join("ws-parked").display()
                )),
                worktree_lease: Some(format!(
                    "worktree:{}",
                    directory.path().join("wt-parked").display()
                )),
                checkpoint_json: r#"{"kind":"coding_checkpoint"}"#.into(),
                tool_state_json: r#"{"step":3}"#.into(),
            },
        )
        .unwrap();
    repository
        .transition_task_to_recovery_required_releasing_process_leases(
            &parked,
            &parked_attempt,
            r#"{"reason":"daemon_restart"}"#,
        )
        .unwrap();
    assert!(
        repository
            .has_active_lease_prefix(&parked, "workspace:")
            .unwrap()
    );

    let released = repository.release_leases_of_settled_tasks().unwrap();
    assert!(
        released >= 2,
        "the settled task's workspace and worktree leases must be released, got {released}"
    );
    assert!(
        !repository
            .has_active_lease_prefix(&settled, "workspace:")
            .unwrap()
    );
    assert!(
        !repository
            .has_active_lease_prefix(&settled, "worktree:")
            .unwrap()
    );
    assert!(
        repository
            .has_active_lease_prefix(&parked, "workspace:")
            .unwrap(),
        "a resumable task must keep the lease its recovery needs"
    );
    assert!(
        repository
            .has_active_lease_prefix(&parked, "worktree:")
            .unwrap(),
        "a resumable task must keep the lease its recovery needs"
    );

    // 幂等：再扫一次没有东西可释放。
    assert_eq!(repository.release_leases_of_settled_tasks().unwrap(), 0);
}
