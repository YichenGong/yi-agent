use tempfile::TempDir;
use yi_agent_core::subagent::task::{
    AttemptId, PermissionDecision, PermissionRequestId, RootSessionId, TaskId,
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
