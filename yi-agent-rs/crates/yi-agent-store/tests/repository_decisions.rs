use tempfile::TempDir;
use yi_agent_core::subagent::task::{
    AttemptId, DeliveryReport, PermissionDecision, PermissionRequestId, RootSessionId, TaskId,
    WorkspaceLeaseId,
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

#[test]
fn delivery_is_atomically_persisted_for_direct_parent_review() {
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
        .record_delivery_for_review(&child, &child_attempt, &delivery)
        .unwrap();

    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
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
