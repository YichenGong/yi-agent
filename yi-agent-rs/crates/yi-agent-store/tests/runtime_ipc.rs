use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use futures::{FutureExt, future::BoxFuture};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;
use yi_agent_core::subagent::task::MessageId;
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
};
use yi_agent_core::{AttemptId, RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, SubscriptionFilters, send_request, send_request_with_version,
    subscribe, subscribe_with_filters,
};
use yi_agent_store::repository::{
    RepositoryError, RuntimeCursorState, RuntimeEvent, RuntimeRepository,
};

struct RecordingWorkerFactory;

fn durable_context() -> WorkerRecoveryContext {
    WorkerRecoveryContext {
        workspace_lease_id: Some("workspace:test".into()),
        worktree_lease: Some("worktree:test".into()),
        checkpoint_json: r#"{"git_head":"test","git_status":""}"#.into(),
        tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
    }
}

impl AgentWorkerFactory for RecordingWorkerFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

#[derive(Clone, Default)]
struct StartCountingFactory {
    starts: Arc<Mutex<usize>>,
}

impl AgentWorkerFactory for StartCountingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        *self.starts.lock().unwrap() += 1;
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

#[derive(Clone)]
struct ReportingWorkerFactory {
    handle: Arc<Mutex<Option<WorkerHandle>>>,
}

impl AgentWorkerFactory for ReportingWorkerFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation);
        *self.handle.lock().unwrap() = Some(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[test]
fn task_snapshot_and_event_log_replay_from_cursor() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();

    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskQueued)
        .unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskStarted)
        .unwrap();

    assert_eq!(repository.task_state(&task).unwrap(), "queued");
    assert_eq!(repository.events_after(0).unwrap().len(), 2);
    assert_eq!(repository.events_after(1).unwrap().len(), 1);
}

#[test]
fn opening_runtime_store_migrates_the_complete_runtime_schema() {
    let directory = TempDir::new().unwrap();
    let repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();

    assert_eq!(repository.schema_version().unwrap(), 6);
    for table in [
        "sessions",
        "tasks",
        "attempts",
        "contracts",
        "contract_amendments",
        "mailbox_messages",
        "resource_leases",
        "deliveries",
        "reviews",
        "permission_requests",
        "schedules",
        "attempt_watchdogs",
        "events",
        "runtime_metadata",
        "resource_admission_cursors",
    ] {
        assert!(repository.has_table(table).unwrap(), "missing {table}");
    }
}

#[test]
fn schedule_ipc_validates_creates_lists_and_deletes() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::CreateSchedule {
                cron: "0 0 0 * * *".into(),
                objective: "bad cron".into(),
            },
        )
        .unwrap(),
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::Validation
        }
    ));
    let IpcResponse::ScheduleCreated { schedule_id } = send_request(
        daemon.socket_path(),
        IpcRequest::CreateSchedule {
            cron: "0 9 * * 1-5".into(),
            objective: "Write the weekly report".into(),
        },
    )
    .unwrap() else {
        panic!("expected schedule creation");
    };
    let IpcResponse::Schedules { schedules } =
        send_request(daemon.socket_path(), IpcRequest::ListSchedules).unwrap()
    else {
        panic!("expected schedule listing");
    };
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].objective, "Write the weekly report");
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DeleteSchedule { schedule_id },
        )
        .unwrap(),
        IpcResponse::ScheduleDeleted
    ));
}

#[test]
fn opening_a_version_one_store_adds_replay_metadata_without_rewriting_history() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY, project_root TEXT NOT NULL, state TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, config_json TEXT NOT NULL
            );
            CREATE TABLE tasks (
                id TEXT PRIMARY KEY, root_session_id TEXT NOT NULL REFERENCES sessions(id),
                parent_id TEXT REFERENCES tasks(id), depth INTEGER NOT NULL, state_json TEXT NOT NULL,
                contract_version INTEGER NOT NULL, active_attempt_id TEXT NOT NULL,
                delivery_json TEXT NOT NULL, workspace_lease_id TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id), task_id TEXT REFERENCES tasks(id),
                attempt_id TEXT, actor_json TEXT NOT NULL, kind TEXT NOT NULL,
                payload_json TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            INSERT INTO schema_migrations (version) VALUES (1);",
        )
        .unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    connection
        .execute(
            "INSERT INTO sessions (id, project_root, state, config_json) VALUES (?1, '', 'active', '{}')",
            rusqlite::params![root.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO tasks
             (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json)
             VALUES (?1, ?2, NULL, 0, 'running', 1, '', '{}')",
            rusqlite::params![task.to_string(), root.to_string()],
        )
        .unwrap();
    for (event_id, kind) in [(3, "task_queued"), (7, "task_started")] {
        connection
            .execute(
                "INSERT INTO events
                 (id, session_id, task_id, actor_json, kind, payload_json)
                 VALUES (?1, ?2, ?3, '{\"kind\":\"runtime\"}', ?4, '{}')",
                rusqlite::params![event_id, root.to_string(), task.to_string(), kind],
            )
            .unwrap();
    }
    drop(connection);

    let mut repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 6);
    assert!(repository.has_table("attempt_watchdogs").unwrap());
    assert!(repository.has_table("runtime_metadata").unwrap());
    assert_eq!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .map(|event| event.id)
            .collect::<Vec<_>>(),
        vec![3, 7]
    );

    let replayable = repository.subscription_snapshot(1).unwrap();
    assert_eq!(replayable.cursor_state, RuntimeCursorState::Replayable);
    assert_eq!(
        replayable
            .events
            .iter()
            .map(|event| event.id)
            .collect::<Vec<_>>(),
        vec![3, 7]
    );
    repository.advance_event_replay_floor_through(3).unwrap();
    assert_eq!(
        repository.subscription_snapshot(2).unwrap().cursor_state,
        RuntimeCursorState::Expired {
            oldest_replayable_event_id: 4,
        }
    );
    let boundary = repository.subscription_snapshot(3).unwrap();
    assert_eq!(boundary.cursor_state, RuntimeCursorState::Replayable);
    assert_eq!(
        boundary
            .events
            .iter()
            .map(|event| event.id)
            .collect::<Vec<_>>(),
        vec![7]
    );
}

#[test]
fn transition_updates_task_snapshot_and_event_journal_together() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();

    let event_id = repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();

    assert!(event_id > 0);
    assert_eq!(repository.task_state(&task).unwrap(), "running");
    let events = repository.event_records_after(0).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, event_id);
    assert_eq!(events[0].event, RuntimeEvent::TaskStarted);
}

#[test]
fn consuming_an_external_override_marks_its_mailbox_row_and_appends_an_audit_event() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    let message_id = MessageId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .record_user_override_message_with_id(&message_id, &task, "continue with the fix")
        .unwrap();

    assert_eq!(
        repository
            .mailbox_message_delivered_at(&message_id)
            .unwrap(),
        None
    );
    repository
        .mark_user_override_consumed(&task, &message_id)
        .unwrap();

    assert!(
        repository
            .mailbox_message_delivered_at(&message_id)
            .unwrap()
            .is_some()
    );
    assert!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|record| record.event == RuntimeEvent::MailboxMessageConsumed)
    );
}

#[test]
fn subscription_snapshot_uses_one_high_water_boundary_for_replay() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskQueued)
        .unwrap();
    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();

    let snapshot = repository.subscription_snapshot(1).unwrap();
    assert_eq!(snapshot.high_water_event_id, 2);
    assert_eq!(snapshot.tasks[0].state, "running");
    assert_eq!(snapshot.events.len(), 1);
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| event.id <= snapshot.high_water_event_id)
    );
}

#[test]
fn fresh_cursor_gets_current_replacement_snapshot_without_event_history() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();

    let snapshot = repository.subscription_snapshot(0).unwrap();
    assert_eq!(snapshot.cursor_state, RuntimeCursorState::Fresh);
    assert_eq!(snapshot.tasks[0].state, "running");
    assert_eq!(snapshot.high_water_event_id, 1);
    assert!(snapshot.events.is_empty());
}

#[test]
fn expired_cursor_gets_replacement_snapshot_then_ordered_events_after_boundary() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskQueued)
        .unwrap();
    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let boundary = repository
        .transition_task(&task, "paused", RuntimeEvent::TaskPaused)
        .unwrap();
    repository.advance_event_replay_floor_through(1).unwrap();

    assert_eq!(repository.event_records_after(0).unwrap().len(), 3);

    let snapshot = repository.subscription_snapshot(0).unwrap();
    assert_eq!(
        snapshot.cursor_state,
        RuntimeCursorState::Expired {
            oldest_replayable_event_id: 2,
        }
    );
    assert_eq!(snapshot.high_water_event_id, boundary);
    assert_eq!(snapshot.tasks[0].state, "paused");
    assert!(snapshot.events.is_empty());
    drop(repository);

    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut subscription = subscribe(daemon.socket_path(), 0).unwrap();
    let IpcResponse::Subscription(snapshot) = subscription.next_response().unwrap() else {
        panic!("expected replacement snapshot");
    };
    assert_eq!(snapshot.high_water_event_id, boundary);
    assert_eq!(snapshot.tasks[0].state, "paused");
    assert!(snapshot.events.is_empty());

    let mut repository = RuntimeRepository::open(&database).unwrap();
    let first = repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let second = repository
        .transition_task(&task, "cancelled", RuntimeEvent::TaskCancelled)
        .unwrap();

    let IpcResponse::Event(first_event) = subscription.next_response().unwrap() else {
        panic!("expected first live event");
    };
    let IpcResponse::Event(second_event) = subscription.next_response().unwrap() else {
        panic!("expected second live event");
    };
    assert_eq!(
        [first_event.event_id, second_event.event_id],
        [first, second]
    );
}

#[test]
fn startup_recovery_marks_live_tasks_without_replaying_work() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "running").unwrap();

    assert_eq!(repository.recover_inflight_tasks().unwrap(), 1);
    assert_eq!(repository.task_state(&task).unwrap(), "recovery_required");
    assert_eq!(
        repository.event_records_after(0).unwrap()[0].event,
        RuntimeEvent::TaskRecoveryRequired
    );
}

#[test]
fn startup_recovery_marks_live_attempts_with_their_parent_tasks() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    let attempt = AttemptId::new();
    repository.create_task(&task, &root, "running").unwrap();
    repository
        .create_attempt(&attempt, &task, 1, "running")
        .unwrap();

    repository.recover_inflight_tasks().unwrap();
    assert_eq!(repository.task_state(&task).unwrap(), "recovery_required");
    assert_eq!(
        repository.attempt_state(&attempt).unwrap(),
        "recovery_required"
    );
}

#[test]
fn daemon_start_publishes_one_runtime_recovered_event_for_reconciled_work() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let session = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &session, "running").unwrap();
    drop(repository);

    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let events = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == RuntimeEvent::RuntimeRecovered)
            .count(),
        1
    );
    let summary = events
        .iter()
        .find(|event| event.event == RuntimeEvent::RuntimeRecovered)
        .unwrap();
    assert_eq!(
        summary.payload_json,
        r#"{"recovered_attempts":1,"recovered_tasks":1,"released_process_leases":0,"retained_workspace_worktree_leases":0}"#
    );
    let recovery_required_id = events
        .iter()
        .find(|event| event.event == RuntimeEvent::TaskRecoveryRequired)
        .unwrap()
        .id;
    let mut subscription = subscribe(daemon.socket_path(), recovery_required_id).unwrap();
    let IpcResponse::Subscription(snapshot) = subscription.next_response().unwrap() else {
        panic!("expected recovery replay snapshot");
    };
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].kind, "runtime_recovered");
    assert_eq!(snapshot.events[0].payload_json, summary.payload_json);
    drop(subscription);
    drop(daemon);
}

#[test]
fn startup_recovery_never_starts_a_worker_or_replays_actions() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let session = RootSessionId::new();
    let task = TaskId::new();
    let attempt = AttemptId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository
        .create_task_with_attempt(&task, &session, &attempt, 1, "running")
        .unwrap();
    drop(repository);

    let factory = Arc::new(StartCountingFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    assert_eq!(*factory.starts.lock().unwrap(), 0);
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&task)
            .unwrap(),
        "recovery_required"
    );
    drop(daemon);
}

#[test]
fn startup_recovery_keeps_workspace_and_worktree_leases_for_reconciliation() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let session = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &session, "running").unwrap();
    drop(repository);

    let connection = Connection::open(&database).unwrap();
    for (id, resource_key) in [
        ("process", "process:resident"),
        ("tool", "tool:rate"),
        ("worktree", "worktree:checkout"),
        ("workspace", "workspace:project"),
    ] {
        connection
            .execute(
                "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
                 VALUES (?1, ?2, ?3, 'exclusive', 1, 'active')",
                params![id, task.to_string(), resource_key],
            )
            .unwrap();
    }
    drop(connection);

    RuntimeRepository::open(&database)
        .unwrap()
        .recover_inflight_tasks()
        .unwrap();

    let connection = Connection::open(&database).unwrap();
    for (id, expected) in [
        ("process", "released"),
        ("tool", "released"),
        ("worktree", "active"),
        ("workspace", "active"),
    ] {
        let state: String = connection
            .query_row(
                "SELECT state FROM resource_leases WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, expected, "lease {id}");
    }
}

#[test]
fn transition_of_an_unknown_task_does_not_create_an_orphan_event() {
    let directory = TempDir::new().unwrap();
    let mut repository = RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let unknown = TaskId::new();

    assert!(matches!(
        repository.transition_task(&unknown, "running", RuntimeEvent::TaskStarted),
        Err(RepositoryError::TaskNotFound { .. })
    ));
    assert!(repository.event_records_after(0).unwrap().is_empty());
}

#[test]
fn second_client_receives_a_snapshot_and_events_after_its_cursor() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    repository
        .append_event(&task, RuntimeEvent::TaskQueued)
        .unwrap();
    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SubscribeEvents {
            after_event_id: 1,
            filters: SubscriptionFilters::default(),
        },
    )
    .unwrap();
    let IpcResponse::Subscription(snapshot) = response else {
        panic!("expected subscription snapshot");
    };
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.tasks[0].task_id, task.to_string());
    assert_eq!(snapshot.tasks[0].state, "running");
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].event_id, 2);
    assert!(snapshot.high_water_event_id >= snapshot.events[0].event_id);
}

#[test]
fn daemon_rejects_second_instance_and_reports_protocol_mismatch() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(&runtime, &database).unwrap();

    assert!(Daemon::start(&runtime, &database).is_err());
    let response = send_request_with_version(daemon.socket_path(), 2, IpcRequest::Status).unwrap();
    assert!(matches!(response, IpcResponse::UnsupportedProtocol { .. }));
}

#[test]
fn raw_ipc_replies_are_versioned_and_echo_the_request_id() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    for (request_id, protocol_version, command, expected_type, expected_error_code) in [
        (
            "status-request",
            1,
            json!({"type": "Status"}),
            "Status",
            None,
        ),
        (
            "version-request",
            99,
            json!({"type": "Status"}),
            "UnsupportedProtocol",
            None,
        ),
        (
            "missing-task-request",
            1,
            json!({"type": "InspectTask", "task_id": TaskId::new().to_string()}),
            "Error",
            Some("not_found"),
        ),
        (
            "invalid-task-request",
            1,
            json!({"type": "InspectTask", "task_id": "not-a-task-id"}),
            "Error",
            Some("validation"),
        ),
    ] {
        let response = raw_request(
            daemon.socket_path(),
            json!({
                "protocol_version": protocol_version,
                "request_id": request_id,
                "command": command,
            }),
        );
        assert_eq!(response["protocol_version"], 1);
        assert_eq!(response["request_id"], request_id);
        assert_eq!(response["result"]["type"], expected_type);
        assert!(
            response.get("Status").is_none(),
            "response must not be bare"
        );

        if let Some(expected_error_code) = expected_error_code {
            assert_eq!(response["result"]["code"], expected_error_code);
        }
    }

    let malformed = raw_request_text(daemon.socket_path(), "{malformed");
    assert_eq!(malformed["protocol_version"], 1);
    assert_eq!(malformed["request_id"], "");
    assert_eq!(malformed["result"]["type"], "Error");
    assert_eq!(malformed["result"]["code"], "validation");
}

#[test]
fn spawning_from_an_unknown_parent_returns_not_found() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { session_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };

    let response = raw_request(
        daemon.socket_path(),
        json!({
            "protocol_version": 1,
            "request_id": "missing-parent",
            "command": {
                "type": "SpawnChild",
                "session_id": session_id,
                "parent_task_id": TaskId::new().to_string(),
                "objective": "must not spawn"
            },
        }),
    );

    assert_eq!(response["result"]["type"], "Error");
    assert_eq!(response["result"]["code"], "not_found");
}

#[test]
fn oversized_request_echoes_an_id_available_in_its_bounded_prefix() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let oversized = format!(
        r#"{{"protocol_version":1,"request_id":"oversized-request","command":{{"type":"Status"}},"padding":"{}"}}"#,
        "x".repeat(1024 * 1024)
    );

    let response = raw_request_text_allowing_peer_close(daemon.socket_path(), &oversized);
    assert_eq!(response["protocol_version"], 1);
    assert_eq!(response["request_id"], "oversized-request");
    assert_eq!(response["result"]["type"], "Error");
    assert_eq!(response["result"]["code"], "validation");
}

#[test]
fn subscription_initialization_failure_returns_an_internal_error_frame() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    std::fs::remove_file(&database).unwrap();
    std::fs::create_dir(&database).unwrap();

    let response = raw_request(
        daemon.socket_path(),
        json!({
            "protocol_version": 1,
            "request_id": "failed-subscription",
            "command": {"type": "SubscribeEvents", "after_event_id": 0},
        }),
    );
    assert_eq!(response["protocol_version"], 1);
    assert_eq!(response["request_id"], "failed-subscription");
    assert_eq!(response["result"]["type"], "Error");
    assert_eq!(response["result"]["code"], "internal");
}

#[test]
fn subscription_frames_are_versioned_and_correlated_to_the_request() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();

    let request_id = "subscription-request";
    let mut stream = UnixStream::connect(daemon.socket_path()).unwrap();
    let frame = json!({
        "protocol_version": 1,
        "request_id": request_id,
        "command": {"type": "SubscribeEvents", "after_event_id": 0},
    });
    writeln!(stream, "{}", serde_json::to_string(&frame).unwrap()).unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);

    let snapshot = raw_response(&mut reader);
    assert_eq!(snapshot["protocol_version"], 1);
    assert_eq!(snapshot["request_id"], request_id);
    assert_eq!(snapshot["result"]["type"], "Subscription");

    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let event = raw_response(&mut reader);
    assert_eq!(event["protocol_version"], 1);
    assert_eq!(event["request_id"], request_id);
    assert_eq!(event["event_id"], 1);
    assert_eq!(event["event"]["type"], "task_started");
    assert_eq!(event["event"]["task_id"], task.to_string());
}

#[test]
fn slow_daemon_subscriber_gets_one_framed_resync_without_affecting_another_client() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let slow_task = TaskId::new();
    let healthy_task = TaskId::new();
    repository.create_task(&slow_task, &root, "queued").unwrap();
    repository
        .create_task(&healthy_task, &root, "queued")
        .unwrap();

    let request_id = "real-slow-subscriber";
    let mut slow_stream = UnixStream::connect(daemon.socket_path()).unwrap();
    set_receive_buffer(&slow_stream, 4 * 1024);
    writeln!(
        slow_stream,
        "{}",
        serde_json::to_string(&json!({
            "protocol_version": 1,
            "request_id": request_id,
            "command": {
                "type": "SubscribeEvents",
                "after_event_id": 0,
                "filters": {"task_ids": [slow_task.to_string()]}
            },
        }))
        .unwrap()
    )
    .unwrap();
    slow_stream.flush().unwrap();
    let mut slow_reader = BufReader::new(slow_stream);
    let snapshot = raw_response(&mut slow_reader);
    assert_eq!(snapshot["result"]["type"], "Subscription");
    slow_reader
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();

    let mut healthy = subscribe_with_filters(
        daemon.socket_path(),
        0,
        SubscriptionFilters {
            task_ids: vec![healthy_task.to_string()],
            kinds: Vec::new(),
        },
    )
    .unwrap();
    assert!(matches!(
        healthy.next_response().unwrap(),
        IpcResponse::Subscription(_)
    ));

    for _ in 0..1_400 {
        repository
            .append_event(&slow_task, RuntimeEvent::TaskStarted)
            .unwrap();
    }
    let healthy_event_id = repository
        .transition_task(&healthy_task, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let IpcResponse::Event(healthy_event) = healthy.next_response().unwrap() else {
        panic!("healthy subscriber did not receive its event");
    };
    assert_eq!(healthy_event.event_id, healthy_event_id);

    std::thread::sleep(std::time::Duration::from_secs(2));
    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        let read = slow_reader.read_line(&mut line).unwrap();
        if read == 0 {
            break;
        }
        frames.push(
            serde_json::from_str::<Value>(&line).expect("every frame must remain valid JSON"),
        );
    }

    assert!(!frames.is_empty());
    assert!(frames.iter().all(|frame| frame["request_id"] == request_id));
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["result"]["type"] == "ResyncRequired")
            .count(),
        1
    );
    assert_eq!(frames.last().unwrap()["result"]["type"], "ResyncRequired");
}

fn set_receive_buffer(stream: &UnixStream, bytes: libc::c_int) {
    // SAFETY: the file descriptor and option pointer are valid for this call.
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            std::ptr::addr_of!(bytes).cast(),
            std::mem::size_of_val(&bytes) as libc::socklen_t,
        )
    };
    assert_eq!(result, 0);
}

fn raw_request(socket_path: &std::path::Path, request: Value) -> Value {
    raw_request_text(socket_path, &serde_json::to_string(&request).unwrap())
}

fn raw_request_text(socket_path: &std::path::Path, request: &str) -> Value {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();
    raw_response(&mut BufReader::new(stream))
}

fn raw_request_text_allowing_peer_close(socket_path: &std::path::Path, request: &str) -> Value {
    let mut stream = UnixStream::connect(socket_path).unwrap();
    let _ = writeln!(stream, "{request}");
    let _ = stream.flush();
    raw_response(&mut BufReader::new(stream))
}

fn raw_response(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn manually_stopped_daemon_releases_its_socket_and_instance_lock() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let database = directory.path().join("runtime.sqlite");
    let mut daemon = Daemon::start(&runtime, &database).unwrap();

    daemon.stop().unwrap();
    assert!(Daemon::start(&runtime, &database).is_ok());
}

#[test]
fn daemon_reclaims_a_stale_lock_when_no_socket_is_listening() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(runtime.join("runtime.lock"), "999999\n").unwrap();
    let database = directory.path().join("runtime.sqlite");

    assert!(Daemon::start(&runtime, &database).is_ok());
}

#[test]
fn daemon_stop_request_releases_the_runtime_for_a_future_manual_start() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(&runtime, &database).unwrap();

    let response = send_request(daemon.socket_path(), IpcRequest::Stop).unwrap();
    assert!(matches!(response, IpcResponse::Stopping));
    for _ in 0..50 {
        if Daemon::start(&runtime, &database).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("daemon stop request did not release runtime lock");
}

#[test]
fn daemon_stop_joins_an_incomplete_client_handler_before_releasing_runtime_files() {
    let directory = TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let database = directory.path().join("runtime.sqlite");
    let mut daemon = Daemon::start(&runtime, &database).unwrap();
    let mut client = UnixStream::connect(daemon.socket_path()).unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    // Give the listener an opportunity to hand this incomplete frame to a
    // client handler that is blocked in its framed read.
    std::thread::sleep(std::time::Duration::from_millis(30));

    daemon.stop().unwrap();

    let mut byte = [0_u8; 1];
    assert_eq!(client.read(&mut byte).unwrap(), 0, "handler must be joined");
    assert!(!runtime.join("runtime.sock").exists());
    assert!(!runtime.join("runtime.lock").exists());
}

#[test]
fn subscription_connection_receives_events_persisted_after_its_snapshot() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();

    let mut subscription = subscribe(daemon.socket_path(), 0).unwrap();
    assert!(matches!(
        subscription.next_response().unwrap(),
        IpcResponse::Subscription(_)
    ));
    repository
        .transition_task(&task, "running", RuntimeEvent::TaskStarted)
        .unwrap();

    let IpcResponse::Event(event) = subscription.next_response().unwrap() else {
        panic!("expected live event");
    };
    assert_eq!(event.task_id, task.to_string());
    assert_eq!(event.event_id, 1);
}

#[test]
fn subscription_filters_apply_to_replay_and_live_events() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let selected = TaskId::new();
    let ignored = TaskId::new();
    repository.create_task(&selected, &root, "queued").unwrap();
    repository.create_task(&ignored, &root, "queued").unwrap();
    repository
        .append_event(&ignored, RuntimeEvent::TaskQueued)
        .unwrap();
    repository
        .append_event(&selected, RuntimeEvent::TaskQueued)
        .unwrap();
    let replayed = repository.latest_event_id().unwrap();

    let mut subscription = subscribe_with_filters(
        daemon.socket_path(),
        1,
        SubscriptionFilters {
            task_ids: vec![selected.to_string()],
            kinds: vec!["task_queued".into(), "task_started".into()],
        },
    )
    .unwrap();
    let IpcResponse::Subscription(snapshot) = subscription.next_response().unwrap() else {
        panic!("expected filtered snapshot");
    };
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].event_id, replayed);

    repository
        .transition_task(&ignored, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let live = repository
        .transition_task(&selected, "running", RuntimeEvent::TaskStarted)
        .unwrap();
    let IpcResponse::Event(event) = subscription.next_response().unwrap() else {
        panic!("expected filtered live event");
    };
    assert_eq!(event.event_id, live);
    assert_eq!(event.task_id, selected.to_string());
}

#[test]
fn daemon_routes_session_spawn_and_recursive_cancel_to_its_coordinator() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child task");
    };

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::CancelTask {
            session_id,
            task_id: root_task_id,
            recursive: true,
        },
    )
    .unwrap();
    assert!(matches!(response, IpcResponse::TaskCancelled));

    let IpcResponse::Subscription(snapshot) = send_request(
        daemon.socket_path(),
        IpcRequest::SubscribeEvents {
            after_event_id: 0,
            filters: SubscriptionFilters::default(),
        },
    )
    .unwrap() else {
        panic!("expected subscription snapshot");
    };
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.task_id == child_task_id && task.state == "cancelled")
    );
}

#[test]
fn daemon_rejects_unbound_agent_message_requests_without_persisting_them() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child task");
    };

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SendMessage {
            session_id,
            sender_task_id: root_task_id,
            worker_capability: "forged".into(),
            recipient_task_id: child_task_id,
            message: "Please report the changed files.".into(),
        },
    )
    .unwrap();

    assert!(matches!(response, IpcResponse::Error { .. }));
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.mailbox_message_count().unwrap(), 0);
}

#[test]
fn daemon_rejects_messages_to_unrelated_tasks_without_persisting_them() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::SessionCreated {
        root_task_id: unrelated_task_id,
        ..
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a second session");
    };

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SendMessage {
            session_id,
            sender_task_id: root_task_id,
            worker_capability: "forged".into(),
            recipient_task_id: unrelated_task_id,
            message: "This must not cross task trees.".into(),
        },
    )
    .unwrap();

    assert!(matches!(response, IpcResponse::Error { .. }));
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.mailbox_message_count().unwrap(), 0);
}

#[test]
fn daemon_waits_for_the_callers_direct_children_through_the_runtime() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child task");
    };
    send_request(
        daemon.socket_path(),
        IpcRequest::CancelTask {
            session_id: session_id.clone(),
            task_id: child_task_id.clone(),
            recursive: false,
        },
    )
    .unwrap();

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            mode: "all".into(),
        },
    )
    .unwrap();

    assert!(matches!(
        response,
        IpcResponse::WaitCompleted { status, children }
            if status == "completed" && children == vec![child_task_id]
    ));
}

#[test]
fn worker_lifecycle_is_reconciled_without_another_client_request() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = ReportingWorkerFactory {
        handle: Arc::new(Mutex::new(None)),
    };
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        Arc::new(factory.clone()),
    )
    .unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child task");
    };
    send_request(
        daemon.socket_path(),
        IpcRequest::StartWorker {
            session_id: session_id.clone(),
            task_id: child_task_id,
        },
    )
    .unwrap();

    let socket = daemon.socket_path().to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let response = send_request(
            socket,
            IpcRequest::WaitAgent {
                session_id,
                caller_task_id: root_task_id,
                mode: "all".into(),
            },
        );
        done_tx.send(response).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(30));
    factory
        .handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report_failure("worker stopped");

    let response = done_rx
        .recv_timeout(std::time::Duration::from_millis(500))
        .expect("worker completion must wake wait_agent without another client request")
        .unwrap();
    assert!(matches!(response, IpcResponse::WaitCompleted { status, .. } if status == "completed"));
}

#[test]
fn daemon_starts_a_worker_through_its_injected_factory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        Arc::new(RecordingWorkerFactory),
    )
    .unwrap();

    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::StartWorker {
            session_id,
            task_id: root_task_id,
        },
    )
    .unwrap();
    assert!(matches!(response, IpcResponse::TaskStarted));
}

#[test]
fn local_daemon_stop_announces_draining_before_requesting_worker_checkpoints() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ReportingWorkerFactory {
        handle: Arc::new(Mutex::new(None)),
    });
    let mut daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    send_request(
        daemon.socket_path(),
        IpcRequest::StartWorker {
            session_id,
            task_id: root_task_id.clone(),
        },
    )
    .unwrap();

    let reporter = factory.handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        reporter.lock().unwrap().as_ref().unwrap().report_paused();
    });
    daemon.stop().unwrap();

    let events = RuntimeRepository::open(&database)
        .unwrap()
        .event_records_after(0)
        .unwrap();
    let draining = events
        .iter()
        .position(|event| event.event == RuntimeEvent::RuntimeDraining)
        .unwrap();
    let checkpoint = events
        .iter()
        .position(|event| event.event == RuntimeEvent::TaskPauseRequested)
        .unwrap();
    assert!(draining < checkpoint);
    assert!(
        factory
            .handle
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .subscribe_pause()
            .requested()
            .now_or_never()
            .unwrap()
    );
}

#[test]
fn daemon_admits_a_spawned_child_when_an_application_factory_is_available() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        Arc::new(RecordingWorkerFactory),
    )
    .unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id: root_task_id,
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child task");
    };

    let IpcResponse::Subscription(snapshot) = send_request(
        daemon.socket_path(),
        IpcRequest::SubscribeEvents {
            after_event_id: 0,
            filters: SubscriptionFilters::default(),
        },
    )
    .unwrap() else {
        panic!("expected subscription snapshot");
    };
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.task_id == task_id && task.state == "running")
    );
}

#[test]
fn daemon_returns_an_inspectable_task_detail_for_user_intervention() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            objective: "Inspect the target".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child");
    };

    let IpcResponse::TaskDetail(detail) = send_request(
        daemon.socket_path(),
        IpcRequest::InspectTask {
            task_id: task_id.clone(),
        },
    )
    .unwrap() else {
        panic!("expected task detail");
    };
    assert_eq!(detail.task_id, task_id);
    assert_eq!(detail.session_id, session_id);
    assert_eq!(
        detail.parent_task_id.as_deref(),
        Some(root_task_id.as_str())
    );
    assert_eq!(detail.depth, 1);
    assert_eq!(detail.state, "queued");
}

#[test]
fn daemon_reads_ordered_events_for_only_the_requested_task_after_a_cursor() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated {
        root_task_id,
        session_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned {
        task_id: other_task,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id: root_task_id.clone(),
            objective: "Unrelated task".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child");
    };

    let mut repository = RuntimeRepository::open(&database).unwrap();
    let cursor = repository.latest_event_id().unwrap();
    repository
        .append_event(&root_task_id.parse().unwrap(), RuntimeEvent::TaskStarted)
        .unwrap();
    repository
        .append_event(&other_task.parse().unwrap(), RuntimeEvent::TaskQueued)
        .unwrap();
    repository
        .append_event(&root_task_id.parse().unwrap(), RuntimeEvent::TaskProgress)
        .unwrap();
    drop(repository);

    let IpcResponse::TaskEvents { events } = send_request(
        daemon.socket_path(),
        IpcRequest::ReadTaskEvents {
            task_id: root_task_id.clone(),
            after_event_id: Some(cursor),
        },
    )
    .unwrap() else {
        panic!("expected task events");
    };

    assert_eq!(events.len(), 2);
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].event_id < pair[1].event_id)
    );
    assert!(events.iter().all(|event| event.task_id == root_task_id));
    assert_eq!(
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        ["task_started", "task_progress"]
    );
}

#[test]
fn daemon_reads_a_task_mailbox_without_consuming_its_messages() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };

    send_request(
        daemon.socket_path(),
        IpcRequest::SendUserMessage {
            task_id: root_task_id.clone(),
            message: "Prefer a focused status report".into(),
        },
    )
    .unwrap();

    let IpcResponse::TaskMailbox { messages } = send_request(
        daemon.socket_path(),
        IpcRequest::ReadTaskMailbox {
            task_id: root_task_id.clone(),
        },
    )
    .unwrap() else {
        panic!("expected task mailbox");
    };

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].recipient_task_id, root_task_id);
    assert_eq!(messages[0].sender_task_id, None);
    assert_eq!(messages[0].kind, "user_override");
    assert_eq!(
        messages[0].payload_json,
        r#"{"message":"Prefer a focused status report"}"#
    );
    assert_eq!(messages[0].delivered_at, None);

    let IpcResponse::TaskMailbox { messages } = send_request(
        daemon.socket_path(),
        IpcRequest::ReadTaskMailbox {
            task_id: root_task_id,
        },
    )
    .unwrap() else {
        panic!("expected task mailbox");
    };
    assert_eq!(messages.len(), 1, "reading must not consume messages");
}

#[test]
fn daemon_reads_task_delivery_evidence_for_diff_inspection() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };

    let IpcResponse::TaskDiff {
        task_id,
        delivery_json,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::ReadTaskDiff {
            task_id: root_task_id.clone(),
        },
    )
    .unwrap()
    else {
        panic!("expected task delivery evidence");
    };

    assert_eq!(task_id, root_task_id);
    let evidence: Value = serde_json::from_str(&delivery_json).unwrap();
    assert_eq!(
        evidence["objective"],
        "Root session objective not specified."
    );
}

#[test]
fn cancel_confirmation_is_single_use_and_bound_to_the_previewed_task_tree() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id: root_task_id.clone(),
            objective: "Child task".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child");
    };

    let IpcResponse::CancelPreview {
        confirmation_token,
        task_ids,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewCancel {
            task_id: root_task_id.clone(),
            recursive: true,
        },
    )
    .unwrap()
    else {
        panic!("expected a cancel preview");
    };
    assert_eq!(task_ids, vec![root_task_id.clone(), child_task_id.clone()]);

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::ConfirmCancel {
            task_id: root_task_id.clone(),
            recursive: true,
            confirmation_token: confirmation_token.clone(),
        },
    )
    .unwrap();
    assert!(matches!(response, IpcResponse::TaskCancelled));

    let reused = send_request(
        daemon.socket_path(),
        IpcRequest::ConfirmCancel {
            task_id: root_task_id,
            recursive: true,
            confirmation_token,
        },
    )
    .unwrap();
    assert!(matches!(
        reused,
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::ConfirmationRequired
        }
    ));
    let detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&child_task_id.parse().unwrap())
        .unwrap();
    assert_eq!(detail.state, "cancelled");
}

#[test]
fn daemon_retries_a_terminal_task_through_its_control_api() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        Arc::new(RecordingWorkerFactory),
    )
    .unwrap();
    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    send_request(
        daemon.socket_path(),
        IpcRequest::StartWorker {
            session_id: session_id.clone(),
            task_id: root_task_id.clone(),
        },
    )
    .unwrap();
    send_request(
        daemon.socket_path(),
        IpcRequest::CancelTask {
            session_id: session_id.clone(),
            task_id: root_task_id.clone(),
            recursive: false,
        },
    )
    .unwrap();

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::RetryTask {
            session_id,
            task_id: root_task_id.clone(),
        },
    )
    .unwrap();

    assert!(matches!(response, IpcResponse::TaskRetried));
    let detail = send_request(
        daemon.socket_path(),
        IpcRequest::InspectTask {
            task_id: root_task_id,
        },
    )
    .unwrap();
    assert!(matches!(
        detail,
        IpcResponse::TaskDetail(detail) if detail.state == "running"
    ));
}
