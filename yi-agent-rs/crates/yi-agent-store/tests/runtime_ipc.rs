use tempfile::TempDir;
use yi_agent_core::{RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, send_request, send_request_with_version,
};
use yi_agent_store::repository::{RepositoryError, RuntimeEvent, RuntimeRepository};

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
