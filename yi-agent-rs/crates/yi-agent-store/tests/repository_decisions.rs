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
        .execute("DELETE FROM schema_migrations WHERE version = 14", [])
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
    drop(connection);

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 14);
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
