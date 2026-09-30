use rusqlite::Connection;
use tempfile::TempDir;
use yi_agent_core::subagent::task::{
    AttemptId, DeliveryReport, IntegrationValidation, MessageId, PermissionDecision,
    PermissionRequestId, RootSessionId, TaskId, WorkspaceLeaseId,
};
use yi_agent_store::repository::{RepositoryError, RuntimeEvent, RuntimeRepository};

fn persisted_root(repository: &mut RuntimeRepository) -> (RootSessionId, TaskId, AttemptId) {
    let session = RootSessionId::new();
    let task = TaskId::new();
    let attempt = AttemptId::new();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    (session, task, attempt)
}

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

#[test]
fn accepted_review_is_an_atomic_audited_task_transition() {
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
    let integration = IntegrationValidation::passed("cargo test -p parent");
    let actor_json = r#"{"kind":"task","source":"daemon"}"#;
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

    repository
        .accept_delivery_review(&child, &delivery.id, &parent, &integration, actor_json)
        .unwrap();

    assert_eq!(repository.task_state(&child).unwrap(), "completed");
    assert_eq!(
        repository.attempt_state(&child_attempt).unwrap(),
        "completed"
    );
    let connection = Connection::open(&database).unwrap();
    let (decision, evidence_json, persisted_actor): (String, String, String) = connection
        .query_row(
            "SELECT decision, evidence_json, actor_json FROM reviews WHERE delivery_id = ?1",
            [delivery.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(decision, "accepted");
    assert_eq!(
        serde_json::from_str::<IntegrationValidation>(&evidence_json).unwrap(),
        integration
    );
    assert_eq!(persisted_actor, actor_json);
    assert_eq!(
        repository.mailbox_messages_for_task(&parent).unwrap().len(),
        1
    );
    let events = repository.event_records_for_task_after(&child, 0).unwrap();
    assert_eq!(events.last().unwrap().event, RuntimeEvent::ReviewAccepted);
    assert!(
        events
            .last()
            .unwrap()
            .payload_json
            .contains("cargo test -p parent")
    );
}

#[test]
fn accepted_review_rejects_an_actor_other_than_the_direct_parent() {
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
    let unrelated_actor = TaskId::new();

    let error = repository
        .accept_delivery_review(
            &child,
            &delivery.id,
            &unrelated_actor,
            &IntegrationValidation::passed("cargo test -p parent"),
            r#"{"kind":"task","source":"daemon"}"#,
        )
        .unwrap_err();

    assert!(matches!(error, RepositoryError::ReviewActorMismatch { .. }));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

#[test]
fn accepted_review_rejects_failed_integration_validation() {
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

    let error = repository
        .accept_delivery_review(
            &child,
            &delivery.id,
            &parent,
            &IntegrationValidation::failed("parent suite failed"),
            r#"{"kind":"task","source":"daemon"}"#,
        )
        .unwrap_err();

    assert!(matches!(error, RepositoryError::IntegrationNotValidated));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

#[test]
fn accepted_review_rejects_a_non_current_delivery_from_the_active_attempt() {
    let (directory, mut repository, parent, child, child_attempt, _delivery) = review_fixture();
    let stale_delivery =
        DeliveryReport::coding("cafebabe", "main", WorkspaceLeaseId::new(), "stale checks");
    Connection::open(directory.path().join("runtime.sqlite"))
        .unwrap()
        .execute(
            "INSERT INTO deliveries (id, task_id, attempt_id, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                stale_delivery.id.to_string(),
                child.to_string(),
                child_attempt.to_string(),
                serde_json::to_string(&stale_delivery).unwrap(),
            ],
        )
        .unwrap();

    let error = repository
        .accept_delivery_review(
            &child,
            &stale_delivery.id,
            &parent,
            &IntegrationValidation::passed("cargo test -p parent"),
            r#"{"kind":"task","source":"daemon"}"#,
        )
        .unwrap_err();

    assert!(matches!(
        error,
        RepositoryError::DeliveryNotAwaitingReview { .. }
    ));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

#[test]
fn rework_review_atomically_creates_a_successor_attempt_and_feedback() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let parent = TaskId::new();
    let parent_attempt = AttemptId::new();
    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    let successor_attempt = AttemptId::new();
    let feedback_message = MessageId::new();
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

    repository
        .rework_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "rebase on the updated parser contract",
            &feedback_message,
            &successor_attempt,
            2,
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap();

    assert_eq!(repository.task_state(&child).unwrap(), "queued");
    assert_eq!(
        repository.active_attempt_id(&child).unwrap(),
        successor_attempt
    );
    assert_eq!(
        repository.attempt_state(&child_attempt).unwrap(),
        "rework_requested"
    );
    assert_eq!(
        repository.attempt_state(&successor_attempt).unwrap(),
        "queued"
    );
    let mailbox = repository.mailbox_messages_for_task(&child).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].sender_task_id, Some(parent));
    assert_eq!(mailbox[0].kind, "rework");
    assert!(mailbox[0].payload_json.contains("updated parser contract"));
    let connection = Connection::open(&database).unwrap();
    let decision: String = connection
        .query_row(
            "SELECT decision FROM reviews WHERE delivery_id = ?1",
            [delivery.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decision, "rework");
    let events = repository.event_records_for_task_after(&child, 0).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event == RuntimeEvent::ReviewRework)
    );
    assert!(
        events
            .iter()
            .any(|event| event.event == RuntimeEvent::MailboxMessageQueued)
    );
}

#[test]
fn rework_review_requires_non_empty_feedback() {
    let (_directory, mut repository, parent, child, _child_attempt, delivery) = review_fixture();

    let error = repository
        .rework_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "   ",
            &MessageId::new(),
            &AttemptId::new(),
            2,
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap_err();

    assert!(matches!(error, RepositoryError::ReviewFeedbackRequired));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

#[test]
fn rework_review_rejects_an_actor_other_than_the_direct_parent() {
    let (_directory, mut repository, _parent, child, _child_attempt, delivery) = review_fixture();

    let error = repository
        .rework_delivery_review(
            &child,
            &delivery.id,
            &TaskId::new(),
            "please update the parser",
            &MessageId::new(),
            &AttemptId::new(),
            2,
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap_err();

    assert!(matches!(error, RepositoryError::ReviewActorMismatch { .. }));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

#[test]
fn rework_review_rejects_a_non_reviewing_attempt_without_advancing_the_task() {
    let (directory, mut repository, parent, child, child_attempt, delivery) = review_fixture();
    Connection::open(directory.path().join("runtime.sqlite"))
        .unwrap()
        .execute(
            "UPDATE attempts SET state = 'running' WHERE id = ?1",
            [child_attempt.to_string()],
        )
        .unwrap();

    let error = repository
        .rework_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "please update the parser",
            &MessageId::new(),
            &AttemptId::new(),
            2,
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        RepositoryError::DeliveryNotAwaitingReview { .. }
    ));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
    assert_eq!(repository.attempt_state(&child_attempt).unwrap(), "running");
}

#[test]
fn rejected_review_is_an_atomic_audited_terminal_transition() {
    let (directory, mut repository, parent, child, child_attempt, delivery) = review_fixture();
    let reason_message = MessageId::new();

    repository
        .reject_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "verification evidence does not cover the regression",
            &reason_message,
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap();

    assert_eq!(repository.task_state(&child).unwrap(), "blocked");
    assert_eq!(repository.attempt_state(&child_attempt).unwrap(), "blocked");
    assert!(
        repository
            .attempt_ended_at(&child_attempt)
            .unwrap()
            .is_some()
    );
    let mailbox = repository.mailbox_messages_for_task(&child).unwrap();
    assert_eq!(mailbox.len(), 1);
    assert_eq!(mailbox[0].sender_task_id, Some(parent));
    assert_eq!(mailbox[0].kind, "review_rejected");
    assert!(mailbox[0].payload_json.contains("regression"));
    let connection = Connection::open(directory.path().join("runtime.sqlite")).unwrap();
    let decision: String = connection
        .query_row(
            "SELECT decision FROM reviews WHERE delivery_id = ?1",
            [delivery.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(decision, "rejected");
    let events = repository.event_records_for_task_after(&child, 0).unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.event == RuntimeEvent::ReviewRejected)
    );
}

#[test]
fn rejected_review_requires_a_non_empty_reason() {
    let (_directory, mut repository, parent, child, _child_attempt, delivery) = review_fixture();

    let error = repository
        .reject_delivery_review(
            &child,
            &delivery.id,
            &parent,
            "\t",
            &MessageId::new(),
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap_err();

    assert!(matches!(error, RepositoryError::ReviewReasonRequired));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

#[test]
fn rejected_review_rejects_an_actor_other_than_the_direct_parent() {
    let (_directory, mut repository, _parent, child, _child_attempt, delivery) = review_fixture();

    let error = repository
        .reject_delivery_review(
            &child,
            &delivery.id,
            &TaskId::new(),
            "verification is incomplete",
            &MessageId::new(),
            r#"{"kind":"task","source":"daemon"}"#,
            &MessageId::new(),
        )
        .unwrap_err();

    assert!(matches!(error, RepositoryError::ReviewActorMismatch { .. }));
    assert_eq!(
        repository.task_state(&child).unwrap(),
        "awaiting_parent_review"
    );
}

fn review_fixture() -> (
    TempDir,
    RuntimeRepository,
    TaskId,
    TaskId,
    AttemptId,
    DeliveryReport,
) {
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
    (
        directory,
        repository,
        parent,
        child,
        child_attempt,
        delivery,
    )
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
