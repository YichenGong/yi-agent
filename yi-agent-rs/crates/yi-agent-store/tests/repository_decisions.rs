use rusqlite::Connection;
use tempfile::TempDir;
use yi_agent_core::subagent::task::{
    AttemptId, DeliveryReport, IntegrationValidation, MessageId, PermissionDecision,
    PermissionRequestId, RootSessionId, TaskId, TaskWorkspaceMode, WorkspaceLeaseId,
};
use yi_agent_core::subagent::worker::WorkerWorkspace;
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

fn test_workspace(session: &RootSessionId, task: &TaskId) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: format!("/tmp/repositories/{session}").into(),
        path: format!("/tmp/repositories/{session}/.worktrees/{task}").into(),
        branch: format!("feat/task-{task}"),
        parent_branch: "main".into(),
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
    }
}

fn assert_invalid_workspace(error: RepositoryError) {
    assert!(matches!(
        error,
        RepositoryError::InvalidTaskWorkspace { .. }
    ));
}

#[test]
fn task_workspace_round_trips_every_git_identity_field() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);

    repository
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();

    assert_eq!(repository.task_workspace(&task).unwrap(), workspace);
}

#[test]
fn task_workspace_rejects_attempt_from_another_task() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, _attempt) = persisted_root(&mut repository);
    let (_other_session, _other_task, other_attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);

    let error = repository
        .record_task_workspace(&task, &other_attempt, &workspace)
        .unwrap_err();

    assert_invalid_workspace(error);
    assert_eq!(repository.task_workspace_optional(&task).unwrap(), None);
}

#[test]
fn task_workspace_rejects_stale_non_active_attempt() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, stale_attempt) = persisted_root(&mut repository);
    let active_attempt = AttemptId::new();
    repository
        .activate_successor_attempt(
            &task,
            &active_attempt,
            2,
            "queued",
            RuntimeEvent::TaskQueued,
        )
        .unwrap();
    let workspace = test_workspace(&session, &task);

    let error = repository
        .record_task_workspace(&task, &stale_attempt, &workspace)
        .unwrap_err();

    assert_invalid_workspace(error);
    assert_eq!(repository.task_workspace_optional(&task).unwrap(), None);
}

#[test]
fn task_workspace_recording_is_idempotent_for_identical_retry() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);

    repository
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();
    repository
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();

    assert_eq!(repository.task_workspace(&task).unwrap(), workspace);
}

#[test]
fn task_workspace_rejects_conflicting_duplicate_assignment() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);
    repository
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();
    let active_attempt = AttemptId::new();
    repository
        .activate_successor_attempt(
            &task,
            &active_attempt,
            2,
            "queued",
            RuntimeEvent::TaskQueued,
        )
        .unwrap();
    let conflicting = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: workspace.repository_root.clone(),
        path: workspace.path.with_file_name("different-task"),
        branch: format!("{}-conflict", workspace.branch),
        parent_branch: workspace.parent_branch.clone(),
        base_commit: "fedcba9876543210fedcba9876543210fedcba98".into(),
    };

    let error = repository
        .record_task_workspace(&task, &active_attempt, &conflicting)
        .unwrap_err();

    assert_invalid_workspace(error);
    assert_eq!(repository.task_workspace(&task).unwrap(), workspace);
}

#[test]
fn task_workspace_branch_names_are_unique_only_within_a_repository() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, first_task, first_attempt) = persisted_root(&mut repository);
    let (_second_session, second_task, second_attempt) = persisted_root(&mut repository);
    let first_workspace = test_workspace(&session, &first_task);
    let second_workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: "/tmp/other-repository".into(),
        path: format!("/tmp/other-repository/.worktrees/{second_task}").into(),
        branch: first_workspace.branch.clone(),
        parent_branch: first_workspace.parent_branch.clone(),
        base_commit: first_workspace.base_commit.clone(),
    };

    repository
        .record_task_workspace(&first_task, &first_attempt, &first_workspace)
        .unwrap();
    repository
        .record_task_workspace(&second_task, &second_attempt, &second_workspace)
        .unwrap();

    assert_eq!(
        repository.task_workspace(&first_task).unwrap(),
        first_workspace
    );
    assert_eq!(
        repository.task_workspace(&second_task).unwrap(),
        second_workspace
    );
}

#[test]
fn task_workspace_rejects_duplicate_branch_in_the_same_repository() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, first_task, first_attempt) = persisted_root(&mut repository);
    let (_second_session, second_task, second_attempt) = persisted_root(&mut repository);
    let first_workspace = test_workspace(&session, &first_task);
    let conflicting_workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: first_workspace.repository_root.clone(),
        path: first_workspace.path.with_file_name("second-task"),
        branch: first_workspace.branch.clone(),
        parent_branch: first_workspace.parent_branch.clone(),
        base_commit: first_workspace.base_commit.clone(),
    };

    repository
        .record_task_workspace(&first_task, &first_attempt, &first_workspace)
        .unwrap();
    let error = repository
        .record_task_workspace(&second_task, &second_attempt, &conflicting_workspace)
        .unwrap_err();

    assert_invalid_workspace(error);
    assert_eq!(
        repository.task_workspace_optional(&second_task).unwrap(),
        None
    );
}

#[test]
fn task_workspace_maps_insert_constraint_race_to_domain_error() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, attempt) = persisted_root(&mut repository);
    let (_other_session, other_task, other_attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);
    drop(repository);
    let injected_lease = WorkspaceLeaseId::new();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER inject_workspace_path_conflict
             BEFORE INSERT ON task_workspaces
             WHEN NEW.task_id = '{task}'
             BEGIN
                INSERT INTO task_workspaces (
                    task_id, attempt_id, lease_id, repository_root, path,
                    branch, parent_branch, base_commit
                ) VALUES (
                    '{other_task}', '{other_attempt}', '{injected_lease}',
                    '/tmp/injected-repository', NEW.path, 'feat/injected',
                    'main', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
                );
             END;"
        ))
        .unwrap();
    drop(connection);
    let mut repository = RuntimeRepository::open(&database).unwrap();

    let error = repository
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap_err();

    assert_invalid_workspace(error);
    assert_eq!(repository.task_workspace_optional(&task).unwrap(), None);
    assert_eq!(
        repository.task_workspace_optional(&other_task).unwrap(),
        None
    );
}

#[test]
fn task_workspace_rejects_empty_persisted_git_identity_fields() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (_session, task, attempt) = persisted_root(&mut repository);
    drop(repository);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO task_workspaces (
                task_id, attempt_id, lease_id, repository_root, path,
                branch, parent_branch, base_commit
             ) VALUES (?1, ?2, ?3, '', '', '', '', '')",
            rusqlite::params![
                task.to_string(),
                attempt.to_string(),
                WorkspaceLeaseId::new().to_string(),
            ],
        )
        .unwrap();
    drop(connection);
    let repository = RuntimeRepository::open(&database).unwrap();

    assert!(matches!(
        repository.task_workspace_optional(&task),
        Err(RepositoryError::InvalidTaskWorkspace { .. })
    ));
}

#[test]
fn delete_task_workspace_removes_the_row_and_is_idempotent() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);

    repository
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();
    assert!(repository.task_workspace_optional(&task).unwrap().is_some());

    repository.delete_task_workspace(&task).unwrap();
    assert!(repository.task_workspace_optional(&task).unwrap().is_none());

    // Second delete is a no-op, not an error.
    repository.delete_task_workspace(&task).unwrap();
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
fn reclaim_candidates_are_deepest_first_and_carry_their_workspace() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let session = RootSessionId::new();
    let root = TaskId::new();
    let child = TaskId::new();
    let root_attempt = AttemptId::new();
    let child_attempt = AttemptId::new();
    repository
        .create_task_with_attempt_and_objective(
            &root,
            &session,
            &root_attempt,
            1,
            "completed",
            "root objective",
            TaskWorkspaceMode::Coding,
        )
        .unwrap();
    repository
        .create_child_task_with_attempt(&child, &session, &root, 1, &child_attempt, 1, "completed")
        .unwrap();
    repository
        .record_task_workspace(&root, &root_attempt, &test_workspace(&session, &root))
        .unwrap();
    let mut child_workspace = test_workspace(&session, &child);
    child_workspace.branch = "feat/child".into();
    repository
        .record_task_workspace(&child, &child_attempt, &child_workspace)
        .unwrap();

    let candidates = repository.reclaim_candidates(&session).unwrap();

    assert_eq!(candidates.len(), 2, "both tasks carry a workspace row");
    assert_eq!(
        candidates[0].task_id,
        child.to_string(),
        "the deeper task is listed first so reclaim runs child before parent"
    );
    assert_eq!(candidates[0].depth, 1);
    assert_eq!(candidates[1].task_id, root.to_string());
    assert_eq!(candidates[1].depth, 0);
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.workspace.is_some()),
        "workspace is joined in"
    );
}

#[test]
fn reclaim_candidates_exclude_tasks_without_a_workspace_row() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let session = RootSessionId::new();
    let task = TaskId::new();
    repository
        .create_task_with_attempt_and_objective(
            &task,
            &session,
            &AttemptId::new(),
            1,
            "completed",
            "objective",
            TaskWorkspaceMode::ReadOnly,
        )
        .unwrap();

    assert!(
        repository.reclaim_candidates(&session).unwrap().is_empty(),
        "a read-only task owns no worktree and is not a candidate"
    );
}
