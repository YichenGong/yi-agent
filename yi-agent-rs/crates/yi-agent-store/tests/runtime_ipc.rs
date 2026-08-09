use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tempfile::TempDir;
use yi_agent_core::subagent::task::MessageId;
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_core::{AttemptId, RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, SubscriptionFilters, send_request, send_request_with_version,
    subscribe, subscribe_with_filters,
};
use yi_agent_store::repository::{
    RepositoryError, RuntimeCursorState, RuntimeEvent, RuntimeRepository,
};

struct RecordingWorkerFactory;

impl AgentWorkerFactory for RecordingWorkerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

#[derive(Clone)]
struct ReportingWorkerFactory {
    handle: Arc<Mutex<Option<WorkerHandle>>>,
}

impl AgentWorkerFactory for ReportingWorkerFactory {
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

    assert_eq!(repository.schema_version().unwrap(), 2);
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
        "events",
        "runtime_metadata",
    ] {
        assert!(repository.has_table(table).unwrap(), "missing {table}");
    }
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
            INSERT INTO schema_migrations (version) VALUES (1);",
        )
        .unwrap();
    drop(connection);

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 2);
    assert!(repository.has_table("runtime_metadata").unwrap());
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
