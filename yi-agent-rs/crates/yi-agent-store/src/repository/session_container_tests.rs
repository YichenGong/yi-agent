//! Session-container migration and CAS behaviour.
//!
//! The dangerous outcomes are silent ones: a migration that promotes an
//! incoherent attachment, or a CAS retry that turns a lost response into a second
//! grant. Both are pinned here rather than left to a later end-to-end test.

use super::*;
use yi_agent_lifecycle::session_delegation::{SessionFailureCode, SessionRef};

fn seed_legacy_v18(
    repository: &mut RuntimeRepository,
    key: &str,
) -> (RootSessionId, TaskId, AttemptId) {
    let session = RootSessionId::new();
    let root = TaskId::new();
    let attempt = AttemptId::new();
    repository
        .create_task_with_attempt(&root, &session, &attempt, 1, "stalled")
        .unwrap();
    repository
        .record_application_root_attachment(
            key,
            &session,
            &root,
            "old-digest",
            "old-secret",
            "/tmp/workspace",
        )
        .unwrap();
    (session, root, attempt)
}

#[test]
fn session_delegation_lifecycle_migration_preserves_terminal_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let (session, root, attempt) = {
        let mut repository = RuntimeRepository::open(&path).unwrap();
        let (session, root, attempt) = seed_legacy_v18(&mut repository, "thread:old");
        // Rebuild the exact pre-v19 shape: no role column, no container tables.
        downgrade_to_v18(&mut repository);
        (session, root, attempt)
    };
    for _ in 0..2 {
        let repository = RuntimeRepository::open(&path).unwrap();
        assert_eq!(
            repository.task_role(&root).unwrap(),
            yi_agent_core::subagent::task::TaskRole::SessionContainer
        );
        assert_eq!(
            repository
                .connection
                .query_row(
                    "SELECT state_json FROM tasks WHERE id = ?1",
                    params![root.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "stalled"
        );
        assert_eq!(
            repository
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM attempts WHERE task_id = ?1",
                    params![root.to_string()],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        let container = repository.session_container(&root).unwrap().unwrap();
        assert!(!container.closed);
        assert_eq!(container.root.session_id, session.to_string());
        assert_eq!(container.binding_epoch, 0);
        assert_eq!(container.driver_epoch, 0);
        assert_eq!(
            repository
                .connection
                .query_row(
                    "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                    params![root.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            attempt.to_string()
        );
    }
}

#[test]
fn session_delegation_lifecycle_migration_rejects_corrupt_attachment() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();

    // A coherent root, so the test proves valid rows still promote.
    let (_good_session, good_root, _) = seed_legacy_v18(&mut repository, "thread:good");

    // Two thread keys claiming one root.
    let (shared_session, shared_root, _) = seed_legacy_v18(&mut repository, "thread:shared");
    repository
        .record_application_root_attachment(
            "thread:second",
            &shared_session,
            &shared_root,
            "d",
            "s",
            "/tmp/workspace",
        )
        .unwrap();

    // Attachment whose session does not match the task's session.
    let (_, mismatched_root, _) = seed_legacy_v18(&mut repository, "thread:base-mismatch");
    let other_session = RootSessionId::new();
    repository
        .record_application_root_attachment(
            "thread:mismatch",
            &other_session,
            &mismatched_root,
            "d",
            "s",
            "/tmp/workspace",
        )
        .unwrap();

    // Attachment pointing at a child task.
    let (child_session, _, _) = seed_legacy_v18(&mut repository, "thread:child-base");
    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    repository
        .create_child_task_with_attempt(
            &child,
            &child_session,
            &good_root,
            1,
            &child_attempt,
            1,
            "running",
        )
        .unwrap();
    repository
        .record_application_root_attachment(
            "thread:child",
            &child_session,
            &child,
            "d",
            "s",
            "/tmp/workspace",
        )
        .unwrap();

    downgrade_to_v18(&mut repository);
    drop(repository);

    let repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    assert_eq!(
        repository.task_role(&good_root).unwrap(),
        yi_agent_core::subagent::task::TaskRole::SessionContainer
    );
    for (key, root) in [
        ("thread:second", &shared_root),
        ("thread:mismatch", &mismatched_root),
        ("thread:child", &child),
    ] {
        assert!(
            repository.session_migration_error(key).unwrap().is_some(),
            "{key} must be diagnosed, not silently promoted"
        );
        assert_eq!(
            repository.task_role(root).unwrap(),
            yi_agent_core::subagent::task::TaskRole::Execution,
            "{key} must not be promoted"
        );
        assert!(repository.session_container(root).unwrap().is_none());
    }
    // The valid attachment is untouched by its corrupt neighbours.
    assert!(
        repository
            .session_migration_error("thread:good")
            .unwrap()
            .is_none()
    );
}

#[test]
fn session_delegation_lifecycle_binding_cas_and_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, _) = seed_legacy_v18(&mut repository, "thread:cas");
    let root_ref = SessionRef {
        session_id: session.to_string(),
        root_id: root.to_string(),
    };
    let commit = SessionBindingCommit {
        root: root_ref.clone(),
        expected_binding_epoch: 0,
        operation_id: "op-1".into(),
        request_digest: "digest-1".into(),
        owner_run_id: "host-run-1".into(),
        generation: "gen-1".into(),
        resume_digest: Some("resume-1".into()),
        consume_legacy_upgrade: false,
        retired_executions: Vec::new(),
    };
    let first = repository.commit_session_binding(&commit).unwrap();
    assert_eq!(first.binding_epoch, 1);
    assert_eq!(first.driver_epoch, 1);
    assert_eq!(first.owner_run_id.as_deref(), Some("host-run-1"));

    // Same operation and digest: original epochs, no extra increment.
    let retry = repository.commit_session_binding(&commit).unwrap();
    assert_eq!(retry.binding_epoch, 1);
    assert_eq!(retry.driver_epoch, 1);

    // Same operation id, different request: refuse rather than mint again.
    let mut conflicting = commit.clone();
    conflicting.request_digest = "digest-2".into();
    match repository.commit_session_binding(&conflicting) {
        Err(RepositoryError::SessionDelegation(failure)) => {
            assert_eq!(failure.code, SessionFailureCode::OwnerConflict)
        }
        other => panic!("expected OwnerConflict, got {other:?}"),
    }

    // A fresh operation against the stale epoch loses the CAS.
    let mut stale = commit.clone();
    stale.operation_id = "op-2".into();
    stale.request_digest = "digest-3".into();
    match repository.commit_session_binding(&stale) {
        Err(RepositoryError::SessionDelegation(failure)) => {
            assert_eq!(failure.code, SessionFailureCode::StaleBinding)
        }
        other => panic!("expected StaleBinding, got {other:?}"),
    }
    assert_eq!(
        repository
            .session_container(&root)
            .unwrap()
            .unwrap()
            .binding_epoch,
        1
    );

    // Close is a tombstone: repeated close is idempotent and stays closed.
    repository.close_session_container(&root).unwrap();
    repository.close_session_container(&root).unwrap();
    assert!(repository.session_container(&root).unwrap().unwrap().closed);
}

/// Rebuilds the v18 shape so `open()` really runs the v19 migration.
fn downgrade_to_v18(repository: &mut RuntimeRepository) {
    repository
        .connection
        .execute_batch(
            "DROP TABLE session_host_executions;
             DROP TABLE session_migration_errors;
             DROP TABLE session_binding_operations;
             DROP TABLE session_containers;
             ALTER TABLE tasks DROP COLUMN role;
             DELETE FROM schema_migrations WHERE version = 19;",
        )
        .unwrap();
}

#[test]
fn session_delegation_lifecycle_container_is_absent_from_worker_watchdog() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_session, root, attempt) = seed_legacy_v18(&mut repository, "thread:watchdog");
    // The attachment path already wrote the active attempt's watchdog row; make
    // the task look exactly like a quiet running worker (309s, no progress).
    repository
        .connection
        .execute(
            "UPDATE tasks SET state_json = 'running' WHERE id = ?1",
            params![root.to_string()],
        )
        .unwrap();
    repository
        .connection
        .execute(
            "UPDATE attempt_watchdogs
             SET last_meaningful_at = datetime('now', '-309 seconds'),
                 last_meaningful_event_id = NULL
             WHERE attempt_id = ?1",
            params![attempt.to_string()],
        )
        .unwrap();

    let candidates = repository.active_attempt_watchdogs().unwrap();
    assert!(
        candidates.iter().all(|task| task.task_id != root),
        "a session container must never be a worker-budget candidate"
    );

    // The commit side must reach the same verdict from a stale candidate.
    let outcome = repository
        .record_watchdog_terminal_with_policy(
            &root,
            &attempt,
            WatchdogTerminal::Stalled,
            &WatchdogEvidence {
                last_meaningful_event_id: None,
                last_meaningful_at: chrono::Utc::now(),
                elapsed_secs: 309,
                current_wait: None,
            },
        )
        .unwrap();
    assert!(
        matches!(outcome, WatchdogTerminalOutcome::ForegroundContainer),
        "a stale candidate must not settle the container: {outcome:?}"
    );
    assert_eq!(
        repository
            .connection
            .query_row(
                "SELECT state_json FROM tasks WHERE id = ?1",
                params![root.to_string()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "running",
        "a refusal must not rewrite the historical row"
    );

    // An execution in the same database is still an ordinary candidate, so the
    // exclusion is about the role and not about the whole table.
    let execution_session = RootSessionId::new();
    let execution = TaskId::new();
    let execution_attempt = AttemptId::new();
    repository
        .create_task_with_attempt(
            &execution,
            &execution_session,
            &execution_attempt,
            1,
            "running",
        )
        .unwrap();
    repository
        .connection
        .execute(
            "UPDATE attempt_watchdogs
             SET last_meaningful_at = datetime('now', '-309 seconds'),
                 last_meaningful_event_id = NULL
             WHERE attempt_id = ?1",
            params![execution_attempt.to_string()],
        )
        .unwrap();
    assert!(
        repository
            .active_attempt_watchdogs()
            .unwrap()
            .iter()
            .any(|task| task.task_id == execution),
        "a running execution must still be a watchdog candidate"
    );
}
