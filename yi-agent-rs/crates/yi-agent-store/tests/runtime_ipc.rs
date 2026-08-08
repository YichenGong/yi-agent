use std::sync::Arc;

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::subagent::worker::{AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart};
use yi_agent_core::{AttemptId, RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, send_request, send_request_with_version, subscribe,
};
use yi_agent_store::repository::{RepositoryError, RuntimeEvent, RuntimeRepository};

struct RecordingWorkerFactory;

impl AgentWorkerFactory for RecordingWorkerFactory {
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
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

    assert_eq!(repository.schema_version().unwrap(), 1);
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
    ] {
        assert!(repository.has_table(table).unwrap(), "missing {table}");
    }
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
        IpcRequest::SubscribeEvents { after_event_id: 1 },
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
        IpcRequest::SubscribeEvents { after_event_id: 0 },
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
fn daemon_routes_adjacent_task_messages_and_persists_the_mailbox_record() {
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
            recipient_task_id: child_task_id,
            message: "Please report the changed files.".into(),
        },
    )
    .unwrap();

    assert!(matches!(response, IpcResponse::MessageDelivered));
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.mailbox_message_count().unwrap(), 1);
    assert!(
        repository
            .event_records_after(0)
            .unwrap()
            .iter()
            .any(|event| event.event == RuntimeEvent::MailboxMessageDelivered)
    );
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
