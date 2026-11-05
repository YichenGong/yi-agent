//! Tests for the durable foreground-root facts.
//!
//! Two groups, on purpose:
//!
//! * **migration/reopen** — what an *existing* installation sees after upgrading. The
//!   dangerous outcome is not a failed migration, it is a successful one that silently
//!   promotes a running root into an unbounded container.
//! * **reducer plumbing** — that the persisted path really delegates to the pure rules
//!   instead of re-implementing a second, subtly different ordering.

use super::*;
use crate::foreground::ForegroundActivation;
use yi_agent_lifecycle::foreground::{
    ActivityPhase, ActivityUpdate, ForegroundFence, RecoveryResult, RootRef,
};

const ATTACHMENT_KEY: &str = "thread-key-1";

/// A root task plus the application attachment a pre-feature build would have written.
fn seed_attached_root(repository: &mut RuntimeRepository) -> (RootSessionId, TaskId, AttemptId) {
    let session = RootSessionId::new();
    let task = TaskId::new();
    let attempt = AttemptId::new();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    repository
        .record_application_root_attachment(
            ATTACHMENT_KEY,
            &session,
            &task,
            "digest",
            "secret",
            "/tmp/workspace",
        )
        .unwrap();
    (session, task, attempt)
}

/// An unattached, scheduled root: the normal execution task.
fn seed_scheduled_root(repository: &mut RuntimeRepository) -> (RootSessionId, TaskId, AttemptId) {
    let session = RootSessionId::new();
    let task = TaskId::new();
    let attempt = AttemptId::new();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    (session, task, attempt)
}

fn fence(
    session: &RootSessionId,
    root: &TaskId,
    attempt: &AttemptId,
    driver_epoch: u64,
) -> ForegroundFence {
    ForegroundFence {
        root: RootRef {
            session_id: session.to_string(),
            root_id: root.to_string(),
            attempt_id: attempt.to_string(),
        },
        generation: "gen-1".into(),
        binding_epoch: 1,
        driver_epoch,
    }
}

fn activity(
    session: &RootSessionId,
    root: &TaskId,
    attempt: &AttemptId,
    seq: u64,
    turn: Option<&str>,
    phase: ActivityPhase,
) -> ActivityUpdate {
    ActivityUpdate {
        fence: fence(session, root, attempt, 1),
        turn_id: turn.map(str::to_owned),
        seq,
        phase,
    }
}

// ---------------------------------------------------------------------------
// Migration and reopen
// ---------------------------------------------------------------------------

#[test]
fn foreground_root_lifecycle_a_legacy_attachment_stays_an_execution_root() {
    // The whole point of the migration: an installation that upgrades must not have
    // its running delegation tree turned into an unbounded container. An attachment
    // proves the root is a foreground *session*; only an authenticated prepare may
    // change its budget policy.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let (application, scheduled) = {
        let mut repository = RuntimeRepository::open(&path).unwrap();
        let (_, application, _) = seed_attached_root(&mut repository);
        let (_, scheduled, _) = seed_scheduled_root(&mut repository);
        (application, scheduled)
    };

    // Reopen twice: a migration that is only correct on the first open is not correct.
    drop(RuntimeRepository::open(&path).unwrap());
    let repository = RuntimeRepository::open(&path).unwrap();

    assert_eq!(
        repository.foreground_class(&application).unwrap(),
        crate::foreground::RootExecutionClass::Execution,
        "a pre-feature attachment is legacy policy, not managed"
    );
    assert_eq!(
        repository.foreground_class(&scheduled).unwrap(),
        crate::foreground::RootExecutionClass::Execution,
        "an unattached scheduled root is a normal execution task"
    );
    assert!(
        repository
            .foreground_activity(&application)
            .unwrap()
            .is_none()
    );
    assert!(
        repository
            .foreground_recovery_result(&application, "not-created")
            .unwrap()
            .is_none()
    );
    // And the policy the watcher obeys follows the classification, not the attachment.
    assert!(repository.root_is_watchdog_governed(&application).unwrap());
}

#[test]
fn foreground_root_lifecycle_a_legacy_row_without_an_attachment_is_corrupt() {
    // A row that claims the foreground role while the attachment it must agree with is
    // gone is a contradiction. Refusing (rather than guessing an exemption) is what
    // stops a hand-edited or half-deleted database from producing an immortal root.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_, root, _) = seed_attached_root(&mut repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Legacy,
            binding_epoch: 0,
            driver_epoch: 0,
            owner_client_id: None,
            admission: AdmissionState::Closed,
            driver_bind_operation_id: None,
        })
        .unwrap();
    repository
        .detach_application_root(
            &repository
                .application_root_attachment(ATTACHMENT_KEY)
                .unwrap()
                .unwrap()
                .root_session_id,
            &root,
        )
        .unwrap();
    // Detaching is legal and keeps the row; deleting the attachment out from under a
    // legacy row is not. Simulate the contradiction directly.
    repository
        .connection
        .execute(
            "DELETE FROM application_root_attachments WHERE root_task_id = ?1",
            params![root.to_string()],
        )
        .unwrap();

    assert_eq!(
        repository.foreground_class(&root).unwrap(),
        crate::foreground::RootExecutionClass::Corrupt
    );
}

#[test]
fn foreground_root_lifecycle_migration_leaves_a_settled_attempt_untouched() {
    // A stall recorded before the upgrade is history. The migration must not rewrite
    // `terminal_json`, or an operator loses the only evidence of why the tree died.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let root = {
        let mut repository = RuntimeRepository::open(&path).unwrap();
        let (_, root, attempt) = seed_attached_root(&mut repository);
        repository
            .transition_task(&root, "stalled", RuntimeEvent::TaskStalled)
            .unwrap();
        repository
            .connection
            .execute(
                "UPDATE attempts SET terminal_json = '{\"reason\":\"pre-existing\"}'
                 WHERE id = ?1",
                params![attempt.to_string()],
            )
            .unwrap();
        root
    };
    let repository = RuntimeRepository::open(&path).unwrap();
    let terminal: Option<String> = repository
        .connection
        .query_row(
            "SELECT terminal_json FROM attempts
             WHERE id = (SELECT active_attempt_id FROM tasks WHERE id = ?1)",
            params![root.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(terminal.as_deref(), Some("{\"reason\":\"pre-existing\"}"));
}

#[test]
fn foreground_root_lifecycle_managed_identity_survives_a_restart_closed() {
    // A `managed` row must not be demoted to `Execution` on restart — that would hand
    // the container back to the worker watchdog, which is the bug this feature exists
    // to fix. But it also must not stay open: no authorization survives the process.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let root = {
        let mut repository = RuntimeRepository::open(&path).unwrap();
        let (_, root, _) = seed_attached_root(&mut repository);
        repository
            .record_foreground_root(&PersistedForegroundRoot {
                root: root.clone(),
                classification: ForegroundClassification::Managed,
                binding_epoch: 3,
                driver_epoch: 2,
                owner_client_id: Some("owner-a".into()),
                admission: AdmissionState::Closed,
                driver_bind_operation_id: Some("op-1".into()),
            })
            .unwrap();
        // Pretend a previous generation had opened it, to prove persistence cannot
        // express that state at all.
        let opened = repository.connection.execute(
            "UPDATE foreground_root_state SET admission_state = 'open' WHERE root_task_id = ?1",
            params![root.to_string()],
        );
        assert!(
            opened.is_err(),
            "the schema must make a persisted 'open' unrepresentable"
        );
        root
    };

    let mut repository = RuntimeRepository::open(&path).unwrap();
    repository.close_foreground_admission().unwrap();
    assert_eq!(
        repository.foreground_class(&root).unwrap(),
        crate::foreground::RootExecutionClass::ForegroundManaged
    );
    let persisted = repository
        .persisted_foreground_root(&root)
        .unwrap()
        .expect("the row survives the restart");
    assert_eq!(persisted.binding_epoch, 3);
    assert_eq!(persisted.driver_epoch, 2);
    assert_eq!(persisted.admission, AdmissionState::Closed);
    assert!(!persisted.admission.admits_business());
}

#[test]
fn foreground_root_lifecycle_startup_leaves_pending_and_managed_roots_alone_when_enabled() {
    // `pending`/`managed` must not be parked in `recovery_required`: one never ran,
    // the other is the user's live conversation. `legacy` keeps the historical sweep
    // because it keeps the historical budget policy.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let (pending, managed, legacy, scheduled) = {
        let mut repository = RuntimeRepository::open(&path).unwrap();
        let (_, pending, _) = seed_scheduled_root(&mut repository);
        let (_, managed, _) = seed_attached_root(&mut repository);
        let (_, legacy, _) = seed_scheduled_root(&mut repository);
        let (_, legacy_attempt, _) = (0, AttemptId::new(), 0);
        let _ = legacy_attempt;
        let (session, scheduled, _) = seed_scheduled_root(&mut repository);
        let _ = session;
        // `legacy` needs an attachment so its row is not corrupt.
        let legacy_session = RootSessionId::new();
        repository
            .record_application_root_attachment(
                "legacy-key",
                &legacy_session,
                &legacy,
                "digest",
                "secret",
                "/tmp/workspace",
            )
            .unwrap();
        for (root, classification) in [
            (pending.clone(), ForegroundClassification::Pending),
            (managed.clone(), ForegroundClassification::Managed),
            (legacy.clone(), ForegroundClassification::Legacy),
        ] {
            repository
                .record_foreground_root(&PersistedForegroundRoot {
                    root: root.clone(),
                    classification,
                    binding_epoch: 1,
                    driver_epoch: 1,
                    owner_client_id: None,
                    admission: AdmissionState::Closed,
                    driver_bind_operation_id: None,
                })
                .unwrap();
        }
        (pending, managed, legacy, scheduled)
    };

    let mut repository = RuntimeRepository::open(&path).unwrap();
    let recovered = repository
        .recover_inflight_tasks_with_activation(ForegroundActivation::Enabled)
        .unwrap();
    // Only the legacy and the unattached scheduled root are swept; the excluded count
    // is therefore two fewer than the total.
    assert_eq!(recovered, 2);

    let state = |repository: &RuntimeRepository, task: &TaskId| -> String {
        repository
            .connection
            .query_row(
                "SELECT state_json FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(state(&repository, &pending), "running");
    assert_eq!(state(&repository, &managed), "running");
    assert_eq!(state(&repository, &legacy), "recovery_required");
    assert_eq!(state(&repository, &scheduled), "recovery_required");
}

#[test]
fn foreground_root_lifecycle_disabled_startup_is_the_historical_sweep() {
    // The production default until Task 11. Every live root is parked, including one
    // that carries a managed row: with the feature off, nothing about classification
    // may change behaviour.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.sqlite");
    let (managed_a, managed_b) = {
        let mut repository = RuntimeRepository::open(&path).unwrap();
        let (_, managed_a, _) = seed_attached_root(&mut repository);
        let (_, managed_b, _) = seed_scheduled_root(&mut repository);
        for root in [managed_a.clone(), managed_b.clone()] {
            repository
                .record_foreground_root(&PersistedForegroundRoot {
                    root,
                    classification: ForegroundClassification::Managed,
                    binding_epoch: 1,
                    driver_epoch: 1,
                    owner_client_id: None,
                    admission: AdmissionState::Closed,
                    driver_bind_operation_id: None,
                })
                .unwrap();
        }
        (managed_a, managed_b)
    };
    let mut repository = RuntimeRepository::open(&path).unwrap();
    assert_eq!(repository.recover_inflight_tasks().unwrap(), 2);
    for root in [managed_a, managed_b] {
        let state: String = repository
            .connection
            .query_row(
                "SELECT state_json FROM tasks WHERE id = ?1",
                params![root.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "recovery_required");
    }
}

#[test]
fn foreground_root_lifecycle_startup_cancels_an_abandoned_pending_reservation() {
    // A prepare that never completed leaves nothing that can resume it: the operation
    // id and deadline died with the process. Parking it would create a root that
    // refuses business forever.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_, root, _) = seed_scheduled_root(&mut repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Pending,
            binding_epoch: 0,
            driver_epoch: 0,
            owner_client_id: None,
            admission: AdmissionState::Closed,
            driver_bind_operation_id: Some("abandoned-op".into()),
        })
        .unwrap();

    let cancelled = repository.cancel_pending_foreground_roots().unwrap();
    assert_eq!(cancelled, vec![root.clone()]);

    let (state, terminal): (String, Option<String>) = repository
        .connection
        .query_row(
            "SELECT tasks.state_json, attempts.terminal_json
             FROM tasks JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.id = ?1",
            params![root.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "cancelled");
    assert!(
        terminal
            .unwrap()
            .contains("pending_foreground_root_abandoned"),
        "the cancellation must say why, or the user cannot tell it from a real failure"
    );
}

#[test]
fn foreground_root_lifecycle_activity_read_projects_a_reloaded_running_turn_as_unknown() {
    // After a restart the daemon cannot know whether that turn is still alive. Showing
    // `idle` would let the host start a second turn inside a live one.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_scheduled_root(&mut repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Managed,
            binding_epoch: 1,
            driver_epoch: 1,
            owner_client_id: None,
            admission: AdmissionState::Closed,
            driver_bind_operation_id: None,
        })
        .unwrap();
    repository
        .apply_foreground_activity(&activity(
            &session,
            &root,
            &attempt,
            1,
            Some("turn-1"),
            ActivityPhase::Running,
        ))
        .unwrap();

    let snapshot = repository.foreground_activity(&root).unwrap().unwrap();
    assert_eq!(snapshot.update.phase, ActivityPhase::Unknown);
    assert_eq!(snapshot.update.turn_id.as_deref(), Some("turn-1"));
    // The stored row is untouched: it is diagnostic evidence of what was last claimed.
    let stored: String = repository
        .connection
        .query_row(
            "SELECT activity_json FROM foreground_root_state WHERE root_task_id = ?1",
            params![root.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stored.contains("\"running\""));
}

#[test]
fn foreground_root_lifecycle_duplicate_activity_does_not_refresh_changed_at() {
    // Replaying a report must not make an old statement look new.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_scheduled_root(&mut repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Managed,
            binding_epoch: 1,
            driver_epoch: 1,
            owner_client_id: None,
            admission: AdmissionState::Closed,
            driver_bind_operation_id: None,
        })
        .unwrap();
    let update = activity(
        &session,
        &root,
        &attempt,
        1,
        Some("turn-1"),
        ActivityPhase::Running,
    );
    let first = repository.apply_foreground_activity(&update).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let replayed = repository.apply_foreground_activity(&update).unwrap();
    assert_eq!(first.changed_at, replayed.changed_at);
}

#[test]
fn foreground_root_lifecycle_stale_activity_is_not_written() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_scheduled_root(&mut repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Managed,
            binding_epoch: 1,
            driver_epoch: 1,
            owner_client_id: None,
            admission: AdmissionState::Closed,
            driver_bind_operation_id: None,
        })
        .unwrap();
    let newest = activity(
        &session,
        &root,
        &attempt,
        5,
        Some("turn-1"),
        ActivityPhase::Running,
    );
    repository.apply_foreground_activity(&newest).unwrap();
    let older = activity(
        &session,
        &root,
        &attempt,
        4,
        Some("turn-1"),
        ActivityPhase::WaitingForChildren,
    );
    let committed = repository.apply_foreground_activity(&older).unwrap();
    assert_eq!(committed.update, newest);
}

#[test]
fn foreground_root_lifecycle_late_finish_of_an_old_turn_is_refused_by_the_store() {
    // The pure reducer already rejects this; the persisted path must not have its own
    // opinion. A store that accepted it would close a live conversation from a stale
    // message that merely arrived last.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_scheduled_root(&mut repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Managed,
            binding_epoch: 1,
            driver_epoch: 1,
            owner_client_id: None,
            admission: AdmissionState::Closed,
            driver_bind_operation_id: None,
        })
        .unwrap();
    repository
        .apply_foreground_activity(&activity(
            &session,
            &root,
            &attempt,
            5,
            Some("new-turn"),
            ActivityPhase::Running,
        ))
        .unwrap();
    let late = activity(
        &session,
        &root,
        &attempt,
        6,
        Some("old-turn"),
        ActivityPhase::Idle,
    );
    let error = repository.apply_foreground_activity(&late).unwrap_err();
    assert!(matches!(
        error,
        RepositoryError::Foreground(crate::foreground::ForegroundError::InvalidActivity)
    ));
    let snapshot = repository.foreground_activity(&root).unwrap().unwrap();
    assert_eq!(snapshot.update.turn_id.as_deref(), Some("new-turn"));
    assert_eq!(snapshot.update.seq, 5);
}

#[test]
fn foreground_root_lifecycle_recovery_result_is_stored_and_read_back() {
    // `CHECK((phase='committed') = (result_json IS NOT NULL))` makes "committed but no
    // result" impossible, so the read path only has to distinguish committed from not.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_scheduled_root(&mut repository);
    let result = RecoveryResult {
        operation_id: "op-1".into(),
        previous: RootRef {
            session_id: session.to_string(),
            root_id: root.to_string(),
            attempt_id: attempt.to_string(),
        },
        successor: RootRef {
            session_id: session.to_string(),
            root_id: root.to_string(),
            attempt_id: AttemptId::new().to_string(),
        },
        binding_epoch: 2,
        driver_epoch: 0,
    };
    let committed = repository.connection.execute(
        "INSERT INTO foreground_recovery_operations
            (root_task_id, operation_id, expected_attempt_id, request_fingerprint,
             original_request_json, phase, impact_json, result_json)
         VALUES (?1, 'op-1', ?2, 'fingerprint', '{}', 'committed', NULL, ?3)",
        params![
            root.to_string(),
            attempt.to_string(),
            serde_json::to_string(&result).unwrap()
        ],
    );
    assert!(committed.is_ok());

    let read_back = repository
        .foreground_recovery_result(&root, "op-1")
        .unwrap()
        .expect("the committed receipt is durable");
    assert_eq!(read_back, result);
    assert_eq!(
        repository
            .foreground_recovery_phase(&root, "op-1")
            .unwrap()
            .as_deref(),
        Some("committed")
    );
    assert!(
        repository
            .foreground_recovery_result(&root, "op-2")
            .unwrap()
            .is_none()
    );

    // And the CHECK really is enforced: a committed row must carry a result.
    let broken = repository.connection.execute(
        "INSERT INTO foreground_recovery_operations
            (root_task_id, operation_id, expected_attempt_id, request_fingerprint,
             original_request_json, phase, impact_json, result_json)
         VALUES (?1, 'op-3', ?2, 'fingerprint', '{}', 'committed', NULL, NULL)",
        params![root.to_string(), attempt.to_string()],
    );
    assert!(
        broken.is_err(),
        "committed without a result must violate the schema's CHECK"
    );
}

#[test]
fn foreground_root_lifecycle_uncommitted_operations_need_a_fresh_preview_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_session, root, attempt) = seed_scheduled_root(&mut repository);
    for (operation, phase) in [("op-preparing", "preparing"), ("op-ready", "ready")] {
        repository
            .connection
            .execute(
                "INSERT INTO foreground_recovery_operations
                    (root_task_id, operation_id, expected_attempt_id, request_fingerprint,
                     original_request_json, phase, impact_json, result_json)
                 VALUES (?1, ?2, ?3, 'fingerprint', '{}', ?4, NULL, NULL)",
                params![root.to_string(), operation, attempt.to_string(), phase],
            )
            .unwrap();
    }
    assert_eq!(
        repository.expire_uncommitted_recovery_operations().unwrap(),
        2
    );
    for operation in ["op-preparing", "op-ready"] {
        assert_eq!(
            repository
                .foreground_recovery_phase(&root, operation)
                .unwrap()
                .as_deref(),
            Some("needs_preview"),
            "the token lived in the dead process, so the operation cannot stay ready"
        );
        assert!(
            repository
                .foreground_recovery_result(&root, operation)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn foreground_root_lifecycle_no_capability_is_written_to_the_new_tables() {
    // A regression guard with teeth: if someone stores a secret here, this fails on the
    // column-name check rather than on a manual audit months later.
    let directory = tempfile::tempdir().unwrap();
    let repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    for table in [
        "foreground_root_state",
        "foreground_recovery_operations",
        "foreground_stop_evidence",
    ] {
        let mut statement = repository
            .connection
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap();
        let columns: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for column in &columns {
            let lowered = column.to_ascii_lowercase();
            assert!(
                !lowered.contains("capability")
                    && !lowered.contains("secret")
                    && !lowered.contains("token"),
                "{table}.{column} looks like it stores a secret; capabilities belong in the \
                 lifecycle grant/lease tables or in memory"
            );
        }
    }
}

/// A managed root an owner may bind a driver to, with the given driver epoch.
fn seed_managed_root(
    repository: &mut RuntimeRepository,
    driver_epoch: u64,
) -> (RootSessionId, TaskId, AttemptId) {
    let (session, root, attempt) = seed_attached_root(repository);
    repository
        .record_foreground_root(&PersistedForegroundRoot {
            root: root.clone(),
            classification: ForegroundClassification::Managed,
            binding_epoch: 1,
            driver_epoch,
            owner_client_id: Some("owner-a".into()),
            admission: AdmissionState::Closed,
            driver_bind_operation_id: None,
        })
        .unwrap();
    (session, root, attempt)
}

#[test]
fn foreground_root_lifecycle_the_same_bind_operation_replays_the_same_epoch() {
    // The transport can lose an answer. Re-minting on retry would invalidate the
    // driver that is actually running, so the operation id has to be the identity.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_managed_root(&mut repository, 0);

    let first = repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap();
    let replay = repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap();
    assert_eq!(first, replay, "a replay must not mint a second epoch");
    assert_eq!(first.driver_epoch, 1);
    assert_eq!(first.last_seq, 0);
    assert_eq!(first.attempt, attempt.to_string());
    assert_eq!(
        first.session,
        session.to_string(),
        "the record must carry the root's own session, so a caller can build the fence"
    );
    assert_eq!(replay.last_seq, 0);
}

#[test]
fn foreground_root_lifecycle_a_second_bind_without_a_stop_proof_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_session, root, _) = seed_managed_root(&mut repository, 0);
    repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap();

    let refused = repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-2")
        .unwrap_err();
    assert!(
        matches!(
            refused,
            RepositoryError::Foreground(ForegroundError::StopUnproven(_))
        ),
        "an unproven takeover must say so; a generic conflict would hide it, got {refused:?}"
    );
    // The refusal must not have advanced the epoch: the live driver keeps its rights.
    let persisted = repository
        .persisted_foreground_root(&root)
        .unwrap()
        .unwrap();
    assert_eq!(persisted.driver_epoch, 1);
    assert_eq!(
        persisted.driver_bind_operation_id.as_deref(),
        Some("bind-1")
    );
}

#[test]
fn foreground_root_lifecycle_another_owner_cannot_bind_the_same_root() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_session, root, _) = seed_managed_root(&mut repository, 0);
    let error = repository
        .bind_foreground_driver(&root, "owner-b", 1, "bind-1")
        .unwrap_err();
    assert!(matches!(
        error,
        RepositoryError::Foreground(ForegroundError::AuthorityDenied)
    ));
}

#[test]
fn foreground_root_lifecycle_a_bind_with_a_stale_binding_epoch_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_session, root, _) = seed_managed_root(&mut repository, 0);
    let error = repository
        .bind_foreground_driver(&root, "owner-a", 7, "bind-1")
        .unwrap_err();
    assert!(
        matches!(
            error,
            RepositoryError::Foreground(ForegroundError::StaleAttempt)
        ),
        "a bind from another binding epoch is a stale attempt, got {error:?}"
    );
}

#[test]
fn foreground_root_lifecycle_a_frozen_root_refuses_a_driver_bind() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (_session, root, _) = seed_managed_root(&mut repository, 0);
    repository
        .connection
        .execute(
            "UPDATE foreground_root_state SET admission_state = 'frozen' WHERE root_task_id = ?1",
            params![root.to_string()],
        )
        .unwrap();
    let error = repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap_err();
    assert!(matches!(
        error,
        RepositoryError::Foreground(ForegroundError::AdmissionClosed)
    ));
}

#[test]
fn foreground_root_lifecycle_activity_for_an_older_driver_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_managed_root(&mut repository, 0);
    repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap();
    // The committed activity names driver epoch 1; a report from epoch 0 is a
    // superseded driver, which is a different refusal from a superseded attempt.
    let mut update = activity(
        &session,
        &root,
        &attempt,
        1,
        Some("turn-1"),
        ActivityPhase::Running,
    );
    update.fence.driver_epoch = 0;
    let error = repository.apply_foreground_activity(&update).unwrap_err();
    assert!(
        matches!(
            error,
            RepositoryError::Foreground(ForegroundError::StaleDriver)
        ),
        "got {error:?}"
    );
}

#[test]
fn foreground_root_lifecycle_activity_after_a_bind_continues_from_the_committed_seq() {
    // The daemon tells the driver `last_seq`; the driver's first report is +1. A
    // restart must therefore never require the client to guess zero.
    let directory = tempfile::tempdir().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let (session, root, attempt) = seed_managed_root(&mut repository, 0);
    repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap();
    for (seq, turn, phase) in [
        (1, Some("turn-1"), ActivityPhase::Running),
        (2, Some("turn-1"), ActivityPhase::Idle),
        (3, Some("turn-2"), ActivityPhase::Running),
    ] {
        repository
            .apply_foreground_activity(&activity(&session, &root, &attempt, seq, turn, phase))
            .unwrap();
    }
    let rebound = repository
        .bind_foreground_driver(&root, "owner-a", 1, "bind-1")
        .unwrap();
    assert_eq!(
        rebound.last_seq, 3,
        "a replayed bind answers with the committed sequence, not zero"
    );
}
