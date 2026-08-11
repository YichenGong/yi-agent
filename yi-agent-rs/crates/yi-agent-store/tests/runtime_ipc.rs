use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Mutex};

use futures::{FutureExt, future::BoxFuture};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tempfile::TempDir;
use yi_agent_core::subagent::task::{MessageId, PermissionRequestId, WorkspaceLeaseId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, AgentWorkspaceService, WorkerError, WorkerHandle, WorkerRecoveryContext,
    WorkerStart, WorkerWorkspace,
};
use yi_agent_core::{AttemptId, RootSessionId, TaskId};
use yi_agent_store::ipc::{
    Daemon, IpcRequest, IpcResponse, IpcReviewDecision, SubscriptionFilters, send_request,
    send_request_with_version, subscribe, subscribe_with_filters,
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

fn confirm_cancel(socket: &std::path::Path, task_id: String, recursive: bool) -> IpcResponse {
    let IpcResponse::CancelPreview {
        confirmation_token, ..
    } = send_request(
        socket,
        IpcRequest::PreviewCancel {
            task_id: task_id.clone(),
            recursive,
        },
    )
    .unwrap()
    else {
        panic!("expected a cancel preview");
    };
    send_request(
        socket,
        IpcRequest::ConfirmCancel {
            task_id,
            recursive,
            confirmation_token,
        },
    )
    .unwrap()
}

fn legacy_v6_database() -> PathBuf {
    let directory = TempDir::new().unwrap();
    let database = directory.keep().join("runtime.sqlite");
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 8);
    drop(repository);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP TABLE task_workspaces;
             DROP TABLE application_root_attachments;
             DELETE FROM schema_migrations WHERE version IN (7, 8);",
        )
        .unwrap();
    database
}

fn test_workspace_for_ipc(session: &str, task: &str) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: format!("/tmp/repositories/{session}").into(),
        path: format!("/tmp/repositories/{session}/.worktrees/{task}").into(),
        branch: format!("feat/task-{task}"),
        parent_branch: "main".into(),
        base_commit: "fedcba9876543210fedcba9876543210fedcba98".into(),
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

#[derive(Clone, Default)]
struct ReviewReportingFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

impl AgentWorkerFactory for ReviewReportingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

impl AgentWorkerFactory for ReportingWorkerFactory {
    fn workspace_service(&self) -> Option<Arc<dyn AgentWorkspaceService>> {
        Some(Arc::new(StaticWorkspaceService))
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation);
        *self.handle.lock().unwrap() = Some(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[derive(Clone, Default)]
struct StaticWorkspaceService;

impl AgentWorkspaceService for StaticWorkspaceService {
    fn prepare_root(
        &self,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(test_workspace_for_ipc(
            &root_session_id.to_string(),
            &task_id.to_string(),
        ))
    }

    fn prepare_child(
        &self,
        _parent: &WorkerWorkspace,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(test_workspace_for_ipc(
            &root_session_id.to_string(),
            &task_id.to_string(),
        ))
    }
}

#[derive(Clone)]
struct ApplicationRootFactory {
    workspace_service: Arc<StaticWorkspaceService>,
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}

#[derive(Clone, Default)]
struct TextCompletionFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

impl AgentWorkerFactory for TextCompletionFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn workspace_service(&self) -> Option<Arc<dyn AgentWorkspaceService>> {
        Some(Arc::new(StaticWorkspaceService))
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

impl AgentWorkerFactory for ApplicationRootFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        durable_context()
    }

    fn workspace_service(&self) -> Option<Arc<dyn AgentWorkspaceService>> {
        Some(self.workspace_service.clone())
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        Box::pin(async move { Ok(handle) })
    }
}

fn application_root_daemon(
    directory: &TempDir,
    database: &std::path::Path,
) -> (Daemon, Arc<Mutex<Vec<WorkerStart>>>) {
    let starts = Arc::new(Mutex::new(Vec::new()));
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        database,
        Arc::new(ApplicationRootFactory {
            workspace_service: Arc::new(StaticWorkspaceService),
            starts: Arc::clone(&starts),
        }),
    )
    .unwrap();
    (daemon, starts)
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

    assert_eq!(repository.schema_version().unwrap(), 8);
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
        "task_workspaces",
        "application_root_attachments",
    ] {
        assert!(repository.has_table(table).unwrap(), "missing {table}");
    }
}

#[test]
fn v6_database_migrates_to_workspace_and_attachment_tables() {
    let database = legacy_v6_database();
    let repository = RuntimeRepository::open(&database).unwrap();

    assert_eq!(repository.schema_version().unwrap(), 8);
    assert!(repository.has_table("task_workspaces").unwrap());
    assert!(
        repository
            .has_table("application_root_attachments")
            .unwrap()
    );
}

#[test]
fn application_root_attach_is_idempotent_and_returns_the_same_workspace() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);

    let first = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-start-1".into(),
        },
    )
    .unwrap();
    let second = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-start-1".into(),
        },
    )
    .unwrap();

    let IpcResponse::ApplicationRootAttached {
        session_id: first_session,
        root_task_id: first_root,
        message_capability: first_capability,
        workspace: first_workspace,
    } = first
    else {
        panic!("expected application root attachment");
    };
    let IpcResponse::ApplicationRootAttached {
        session_id: second_session,
        root_task_id: second_root,
        message_capability: second_capability,
        workspace: second_workspace,
    } = second
    else {
        panic!("expected idempotent application root attachment");
    };

    assert_eq!(first_session, second_session);
    assert_eq!(first_root, second_root);
    assert_eq!(first_capability, second_capability);
    assert_eq!(first_workspace, second_workspace);
    assert!(!first_capability.is_empty());
}

#[test]
fn application_root_delegation_rejects_a_capability_from_another_attached_root() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);

    let IpcResponse::ApplicationRootAttached {
        session_id: first_session,
        root_task_id: first_root,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-a".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected first attachment");
    };
    let IpcResponse::ApplicationRootAttached {
        message_capability: second_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-b".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected second attachment");
    };

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::SpawnApplicationChild {
                session_id: first_session,
                parent_task_id: first_root,
                capability: second_capability,
                objective: "inspect the parser".into(),
            },
        )
        .unwrap(),
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            message: None,
        }
    );
}

#[test]
fn application_root_capability_is_not_derived_from_idempotency_key() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        message_capability, ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-random-capability".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected application root attachment");
    };
    let deterministic = format!(
        "app-root-{}",
        sha256_hex("application-root:tui-random-capability")
    );
    assert_ne!(message_capability, deterministic);
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn concurrent_application_root_attach_reuses_one_durable_root() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let socket = daemon.socket_path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let socket = socket.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                send_request(
                    &socket,
                    IpcRequest::AttachApplicationRoot {
                        idempotency_key: "tui-concurrent".into(),
                    },
                )
                .unwrap()
            })
        })
        .collect();
    let responses: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    let mut attached = Vec::new();
    for response in responses {
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            workspace,
            ..
        } = response
        else {
            panic!("expected application root attachment");
        };
        attached.push((session_id, root_task_id, workspace.path));
    }
    assert_eq!(attached[0], attached[1]);

    let connection = Connection::open(&database).unwrap();
    let attachment_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM application_root_attachments WHERE idempotency_key = 'tui-concurrent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let root_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE parent_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attachment_count, 1);
    assert_eq!(root_count, 1);
}

#[test]
fn application_root_activate_marks_the_foreground_root_running_without_starting_a_worker() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-activate".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected application root attachment");
    };

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id,
                root_task_id: root_task_id.clone(),
                capability: message_capability,
                objective: "build the TUI MVP".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    );

    assert!(starts.lock().unwrap().is_empty());
    let task_id: TaskId = root_task_id.parse().unwrap();
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&task_id).unwrap(), "running");
    let detail = repository.task_detail(&task_id).unwrap();
    let delivery: Value = serde_json::from_str(&detail.delivery_json).unwrap();
    assert_eq!(delivery["objective"], "build the TUI MVP");
}

#[test]
fn concurrent_application_root_activation_records_one_first_objective() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-concurrent-activate".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    let socket = daemon.socket_path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["first objective", "second objective"]
        .into_iter()
        .map(|objective| {
            let socket = socket.clone();
            let barrier = Arc::clone(&barrier);
            let session_id = session_id.clone();
            let root_task_id = root_task_id.clone();
            let message_capability = message_capability.clone();
            std::thread::spawn(move || {
                barrier.wait();
                send_request(
                    &socket,
                    IpcRequest::ActivateApplicationRoot {
                        session_id,
                        root_task_id,
                        capability: message_capability,
                        objective: objective.into(),
                    },
                )
                .unwrap()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(
            handle.join().unwrap(),
            IpcResponse::ApplicationRootActivated
        );
    }

    let connection = Connection::open(&database).unwrap();
    let started_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND kind = 'task_started'",
            [root_task_id.clone()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(started_events, 1);

    let detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&root_task_id.parse().unwrap())
        .unwrap();
    let delivery: Value = serde_json::from_str(&detail.delivery_json).unwrap();
    assert!(matches!(
        delivery["objective"].as_str(),
        Some("first objective" | "second objective")
    ));
}

#[test]
fn application_root_activation_keeps_the_first_objective() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-first-objective".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected application root attachment");
    };

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                objective: "first objective".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    );
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id,
                root_task_id: root_task_id.clone(),
                capability: message_capability,
                objective: "second objective".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    );

    let task_id: TaskId = root_task_id.parse().unwrap();
    let detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&task_id)
        .unwrap();
    let delivery: Value = serde_json::from_str(&detail.delivery_json).unwrap();
    assert_eq!(delivery["objective"], "first objective");
}

#[test]
fn application_root_detach_requires_capability_and_does_not_complete_root() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-detach".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected application root attachment");
    };

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DetachApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: "wrong".into(),
            },
        )
        .unwrap(),
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            message: None,
        }
    );
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DetachApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability,
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootDetached
    );

    let task_id: TaskId = root_task_id.parse().unwrap();
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_ne!(repository.task_state(&task_id).unwrap(), "completed");
    assert_eq!(
        repository
            .application_root_attachment("tui-detach")
            .unwrap()
            .unwrap()
            .state,
        "detached"
    );
}

#[test]
fn application_root_can_spawn_and_send_message_to_its_child() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-send".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    let IpcResponse::TaskSpawned { task_id: child } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "child task".into(),
        },
    )
    .unwrap() else {
        panic!("expected child spawn");
    };

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::SendApplicationMessage {
                session_id,
                sender_task_id: root_task_id.clone(),
                capability: message_capability,
                recipient_task_id: child.clone(),
                message: "please continue".into(),
            },
        )
        .unwrap(),
        IpcResponse::MessageQueued
    );

    let child_id: TaskId = child.parse().unwrap();
    let messages = RuntimeRepository::open(&database)
        .unwrap()
        .mailbox_messages_for_task(&child_id)
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].sender_task_id,
        Some(root_task_id.parse().unwrap())
    );
}

#[test]
fn application_root_can_spawn_multiple_direct_children() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-multiple-spawn".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };

    let first = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "fast child".into(),
        },
    )
    .unwrap();
    let second = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id,
            parent_task_id: root_task_id,
            capability: message_capability,
            objective: "slow child".into(),
        },
    )
    .unwrap();

    assert!(matches!(first, IpcResponse::TaskSpawned { .. }));
    assert!(matches!(second, IpcResponse::TaskSpawned { .. }));
}

#[test]
fn application_root_can_spawn_second_child_while_first_is_running() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-second-spawn-while-running".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };

    let IpcResponse::TaskSpawned {
        task_id: first_child,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "fast child".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected first child spawn");
    };
    assert_eq!(starts.lock().unwrap().len(), 1);

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id,
            parent_task_id: root_task_id,
            capability: message_capability,
            objective: "slow child".into(),
        },
    )
    .unwrap();

    let IpcResponse::TaskSpawned {
        task_id: second_child,
    } = response
    else {
        panic!("second spawn should be queued instead of rejected, got {response:?}");
    };
    assert_ne!(first_child, second_child);
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&second_child.parse().unwrap())
            .unwrap(),
        "running"
    );
}

#[test]
fn application_root_rejects_more_than_four_direct_children() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-direct-child-limit".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };

    for index in 0..4 {
        assert!(matches!(
            send_request(
                daemon.socket_path(),
                IpcRequest::SpawnApplicationChild {
                    session_id: session_id.clone(),
                    parent_task_id: root_task_id.clone(),
                    capability: message_capability.clone(),
                    objective: format!("child {index}"),
                },
            )
            .unwrap(),
            IpcResponse::TaskSpawned { .. }
        ));
    }

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id,
            parent_task_id: root_task_id,
            capability: message_capability,
            objective: "fifth child".into(),
        },
    )
    .unwrap();

    let IpcResponse::Error { code, message } = response else {
        panic!("expected direct child limit error, got {response:?}");
    };
    assert_eq!(code, yi_agent_store::ipc::IpcErrorCode::InvalidState);
    assert!(message.unwrap().contains("at most four direct children"));
}

#[test]
fn application_root_reuses_direct_child_slots_after_terminal_reports() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-terminal-slot-reuse".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };

    for index in 0..4 {
        assert!(matches!(
            send_request(
                daemon.socket_path(),
                IpcRequest::SpawnApplicationChild {
                    session_id: session_id.clone(),
                    parent_task_id: root_task_id.clone(),
                    capability: message_capability.clone(),
                    objective: format!("historical child {index}"),
                },
            )
            .unwrap(),
            IpcResponse::TaskSpawned { .. }
        ));
        factory.handles.lock().unwrap()[index].report_completed(format!("done {index}"));
    }
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::WaitAgent {
                session_id: session_id.clone(),
                caller_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                mode: "all".into(),
                timeout_ms: Some(1_000),
            },
        )
        .unwrap(),
        IpcResponse::WaitCompleted { status, .. } if status == "completed"
    ));

    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::SpawnApplicationChild {
                session_id,
                parent_task_id: root_task_id,
                capability: message_capability,
                objective: "new child after historical completions".into(),
            },
        )
        .unwrap(),
        IpcResponse::TaskSpawned { .. }
    ));
}

#[test]
fn detached_paused_application_root_can_reattach_activate_and_spawn() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-paused-reattach".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                objective: "first prompt".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    );
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DetachApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootDetached
    );
    let IpcResponse::ApplicationRootAttached {
        session_id: reattached_session,
        root_task_id: reattached_root,
        message_capability: reattached_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-paused-reattach".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected reattachment");
    };

    assert_eq!(reattached_session, session_id);
    assert_eq!(reattached_root, root_task_id);
    assert_eq!(reattached_capability, message_capability);
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: reattached_session.clone(),
                root_task_id: reattached_root.clone(),
                capability: reattached_capability.clone(),
                objective: "second prompt should not be dropped".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    );
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::SpawnApplicationChild {
                session_id: reattached_session,
                parent_task_id: reattached_root.clone(),
                capability: reattached_capability,
                objective: "after paused reattach".into(),
            },
        )
        .unwrap(),
        IpcResponse::TaskSpawned { .. }
    ));
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&reattached_root.parse().unwrap())
            .unwrap(),
        "running"
    );
}

#[test]
fn detached_application_root_can_be_reattached_with_the_same_key() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-reattach".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DetachApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootDetached
    );

    let IpcResponse::ApplicationRootAttached {
        session_id: reattached_session,
        root_task_id: reattached_root,
        message_capability: reattached_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-reattach".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected reattachment");
    };

    assert_eq!(reattached_session, session_id);
    assert_eq!(reattached_root, root_task_id);
    assert_eq!(reattached_capability, message_capability);
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::SpawnApplicationChild {
                session_id,
                parent_task_id: root_task_id,
                capability: reattached_capability,
                objective: "after reattach".into(),
            },
        )
        .unwrap(),
        IpcResponse::TaskSpawned { .. }
    ));
}

#[test]
fn attached_application_root_can_be_reused_after_daemon_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let runtime = directory.path().join("runtime");
    let starts = Arc::new(Mutex::new(Vec::new()));
    {
        let daemon = Daemon::start_with_factory(
            &runtime,
            &database,
            Arc::new(ApplicationRootFactory {
                workspace_service: Arc::new(StaticWorkspaceService),
                starts: Arc::clone(&starts),
            }),
        )
        .unwrap();
        let _ = send_request(
            daemon.socket_path(),
            IpcRequest::AttachApplicationRoot {
                idempotency_key: "tui-restart".into(),
            },
        )
        .unwrap();
    }

    let daemon = Daemon::start_with_factory(
        &runtime,
        &database,
        Arc::new(ApplicationRootFactory {
            workspace_service: Arc::new(StaticWorkspaceService),
            starts,
        }),
    )
    .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-restart".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected restarted attachment");
    };

    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::SpawnApplicationChild {
                session_id,
                parent_task_id: root_task_id,
                capability: message_capability,
                objective: "after restart".into(),
            },
        )
        .unwrap(),
        IpcResponse::TaskSpawned { .. }
    ));
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
            code: yi_agent_store::ipc::IpcErrorCode::Validation,
            message: None,
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
    assert_eq!(repository.schema_version().unwrap(), 8);
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
fn daemon_lists_compact_task_summaries() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let root = RootSessionId::new();
    let task = TaskId::new();
    repository.create_task(&task, &root, "queued").unwrap();
    let other_root = RootSessionId::new();
    let completed_task = TaskId::new();
    repository
        .create_task(&completed_task, &other_root, "completed_no_changes")
        .unwrap();

    let IpcResponse::TaskSummaries { tasks } = send_request(
        daemon.socket_path(),
        IpcRequest::ListTaskSummaries {
            session_id: Some(root.to_string()),
            active_only: false,
        },
    )
    .unwrap() else {
        panic!("expected task summaries");
    };

    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].task_id, task.to_string());
    assert_eq!(tasks[0].state, "queued");
    assert!(tasks[0].is_root);

    let IpcResponse::TaskSummaries { tasks } = send_request(
        daemon.socket_path(),
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: true,
        },
    )
    .unwrap() else {
        panic!("expected active task summaries");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].task_id, task.to_string());
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

    let response = confirm_cancel(daemon.socket_path(), root_task_id, true);
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
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child task");
    };
    let response = confirm_cancel(daemon.socket_path(), child_task_id.clone(), false);
    assert!(matches!(response, IpcResponse::TaskCancelled));

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::WaitAgent {
                session_id: session_id.clone(),
                caller_task_id: root_task_id.clone(),
                capability: "wrong".into(),
                mode: "all".into(),
                timeout_ms: None,
            },
        )
        .unwrap(),
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            message: None,
        }
    );

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: message_capability,
            mode: "all".into(),
            timeout_ms: None,
        },
    )
    .unwrap();

    assert!(matches!(
        response,
        IpcResponse::WaitCompleted { status, children, .. }
            if status == "completed" && children == vec![child_task_id]
    ));
}

#[test]
fn daemon_wait_agent_times_out_instead_of_waiting_forever() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory).unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-timeout".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "Inspect child behavior slowly".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child task");
    };

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: message_capability,
            mode: "all".into(),
            timeout_ms: Some(10),
        },
    )
    .unwrap();

    let IpcResponse::WaitCompleted {
        status,
        children,
        reports,
    } = response
    else {
        panic!("expected bounded wait response");
    };
    assert_eq!(status, "timeout");
    assert_eq!(children, vec![task_id]);
    assert!(reports.is_empty());
}

#[test]
fn daemon_wait_agent_timeout_returns_partial_completed_reports() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-partial-timeout".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned {
        task_id: completed_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "finish first".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected first child");
    };
    let IpcResponse::TaskSpawned {
        task_id: pending_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "stay pending".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected second child");
    };
    factory.handles.lock().unwrap()[0].report_completed("partial result");

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: message_capability,
            mode: "all".into(),
            timeout_ms: Some(10),
        },
    )
    .unwrap();

    let IpcResponse::WaitCompleted {
        status,
        children,
        reports,
    } = response
    else {
        panic!("expected bounded wait response");
    };
    assert_eq!(status, "timeout");
    assert_eq!(children, vec![completed_id.clone(), pending_id]);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].task_id, completed_id);
    assert_eq!(reports[0].state, "completed_no_changes");
    assert_eq!(reports[0].report.as_deref(), Some("partial result"));
}

#[test]
fn daemon_wait_any_returns_only_terminal_child_reports() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-any-terminal-only".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned {
        task_id: completed_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "finish first".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected first child");
    };
    let IpcResponse::TaskSpawned {
        task_id: _pending_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "stay pending".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected second child");
    };
    factory.handles.lock().unwrap()[0].report_completed("first result");

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: message_capability,
            mode: "any".into(),
            timeout_ms: None,
        },
    )
    .unwrap();

    let IpcResponse::WaitCompleted {
        status,
        children,
        reports,
    } = response
    else {
        panic!("expected wait completion");
    };
    assert_eq!(status, "completed");
    assert_eq!(children, vec![completed_id.clone()]);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].task_id, completed_id);
    assert_eq!(reports[0].state, "completed_no_changes");
    assert_eq!(reports[0].report.as_deref(), Some("first result"));
}

#[test]
fn daemon_wait_completed_report_wakes_before_timeout() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-completed-wake".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "reply quickly".into(),
        },
    )
    .unwrap() else {
        panic!("expected child task");
    };

    let socket = daemon.socket_path().to_path_buf();
    let (wait_tx, wait_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let response = send_request(
            &socket,
            IpcRequest::WaitAgent {
                session_id,
                caller_task_id: root_task_id,
                capability: message_capability,
                mode: "all".into(),
                timeout_ms: Some(1_000),
            },
        );
        wait_tx.send(response).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(30));
    factory.handles.lock().unwrap()[0].report_completed("hello from subagent");

    let response = wait_rx
        .recv_timeout(std::time::Duration::from_millis(500))
        .expect("completed report should wake wait_agent before timeout")
        .unwrap();
    let IpcResponse::WaitCompleted {
        status,
        children,
        reports,
    } = response
    else {
        panic!("expected wait completion");
    };
    assert_eq!(status, "completed");
    assert_eq!(children, vec![task_id.clone()]);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].task_id, task_id);
    assert_eq!(reports[0].report.as_deref(), Some("hello from subagent"));
}

#[test]
fn daemon_wait_timeout_does_not_bypass_application_capability() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory).unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-timeout-auth".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned { .. } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability,
            objective: "stay pending".into(),
        },
    )
    .unwrap() else {
        panic!("expected child task");
    };

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: "wrong".into(),
            mode: "all".into(),
            timeout_ms: Some(0),
        },
    )
    .unwrap();

    assert_eq!(
        response,
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            message: None,
        }
    );
}

#[test]
fn daemon_bounded_wait_keeps_other_ipc_clients_responsive() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory).unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-concurrent-client".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "stay pending".into(),
        },
    )
    .unwrap() else {
        panic!("expected child task");
    };

    let socket = daemon.socket_path().to_path_buf();
    let (wait_tx, wait_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let response = send_request(
            &socket,
            IpcRequest::WaitAgent {
                session_id,
                caller_task_id: root_task_id,
                capability: message_capability,
                mode: "all".into(),
                timeout_ms: Some(200),
            },
        );
        wait_tx.send(response).unwrap();
    });
    std::thread::sleep(std::time::Duration::from_millis(30));

    let IpcResponse::Status { .. } =
        send_request(daemon.socket_path(), IpcRequest::Status).unwrap()
    else {
        panic!("status request should complete while another client is waiting");
    };

    let response = wait_rx
        .recv_timeout(std::time::Duration::from_millis(500))
        .expect("bounded wait should eventually return")
        .unwrap();
    assert!(matches!(
        response,
        IpcResponse::WaitCompleted {
            status,
            children,
            reports,
        } if status == "timeout" && children == vec![task_id] && reports.is_empty()
    ));
}

#[test]
fn daemon_wait_agent_keeps_completed_child_reports_after_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let runtime_dir = directory.path().join("runtime");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon = Daemon::start_with_factory(&runtime_dir, &database, factory.clone()).unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-report-restart".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child task");
    };
    factory.handles.lock().unwrap()[0].report_completed("sub-agent 正常完成，结果可读");

    let IpcResponse::WaitCompleted { reports, .. } = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id: session_id.clone(),
            caller_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            mode: "all".into(),
            timeout_ms: None,
        },
    )
    .unwrap() else {
        panic!("expected initial wait completion");
    };
    assert_eq!(
        reports[0].report.as_deref(),
        Some("sub-agent 正常完成，结果可读")
    );
    drop(daemon);

    let restarted = Daemon::start_with_factory(&runtime_dir, &database, factory).unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        restarted.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-report-restart".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected reattached root after restart");
    };
    let response = send_request(
        restarted.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: message_capability,
            mode: "all".into(),
            timeout_ms: None,
        },
    )
    .unwrap();

    let IpcResponse::WaitCompleted {
        status,
        children,
        reports,
    } = response
    else {
        panic!("expected wait completion after restart, got {response:?}");
    };
    assert_eq!(status, "completed");
    assert_eq!(children, vec![task_id.clone()]);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].task_id, task_id);
    assert_eq!(reports[0].state, "completed_no_changes");
    assert_eq!(
        reports[0].report.as_deref(),
        Some("sub-agent 正常完成，结果可读")
    );
}

#[test]
fn daemon_wait_agent_returns_completed_child_reports() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(TextCompletionFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-wait-report".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "Inspect child behavior".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child task");
    };
    factory.handles.lock().unwrap()[0].report_completed("sub-agent 正常完成，结果可读");

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::WaitAgent {
            session_id,
            caller_task_id: root_task_id,
            capability: message_capability,
            mode: "all".into(),
            timeout_ms: None,
        },
    )
    .unwrap();

    let IpcResponse::WaitCompleted {
        status,
        children,
        reports,
    } = response
    else {
        panic!("expected wait completion");
    };
    assert_eq!(status, "completed");
    assert_eq!(children, vec![task_id.clone()]);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].task_id, task_id);
    assert_eq!(reports[0].state, "completed_no_changes");
    assert_eq!(
        reports[0].report.as_deref(),
        Some("sub-agent 正常完成，结果可读")
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&reports[0].task_id.parse().unwrap())
            .unwrap(),
        "completed_no_changes"
    );
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
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-worker-wait".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected an attached root");
    };
    let IpcResponse::TaskSpawned {
        task_id: child_task_id,
    } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
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
                capability: message_capability,
                mode: "all".into(),
                timeout_ms: None,
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
fn inspect_task_includes_the_authoritative_recorded_workspace() {
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
            parent_task_id: root_task_id,
            objective: "Inspect workspace assignment".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child");
    };
    let task: TaskId = task_id.parse().unwrap();
    let attempt = RuntimeRepository::open(&database)
        .unwrap()
        .active_attempt_id(&task)
        .unwrap();
    let workspace = test_workspace_for_ipc(&session_id, &task_id);
    RuntimeRepository::open(&database)
        .unwrap()
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();

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
    assert_eq!(detail.workspace, Some(workspace));
}

#[test]
fn inspect_task_omits_workspace_for_unassigned_old_tasks() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };

    let IpcResponse::TaskDetail(detail) = send_request(
        daemon.socket_path(),
        IpcRequest::InspectTask {
            task_id: root_task_id,
        },
    )
    .unwrap() else {
        panic!("expected task detail");
    };

    assert_eq!(detail.workspace, None);
}

#[test]
fn subscription_snapshot_omits_workspace_for_unassigned_old_tasks() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
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

    let task = snapshot
        .tasks
        .iter()
        .find(|task| task.task_id == root_task_id)
        .expect("snapshot contains the unassigned root task");
    assert_eq!(task.workspace, None);
}

#[test]
fn subscription_snapshot_includes_recorded_task_workspace() {
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
            parent_task_id: root_task_id,
            objective: "Publish workspace assignment".into(),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child");
    };
    let task: TaskId = task_id.parse().unwrap();
    let attempt = RuntimeRepository::open(&database)
        .unwrap()
        .active_attempt_id(&task)
        .unwrap();
    let workspace = test_workspace_for_ipc(&session_id, &task_id);
    RuntimeRepository::open(&database)
        .unwrap()
        .record_task_workspace(&task, &attempt, &workspace)
        .unwrap();

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
            .any(|task| task.task_id == task_id && task.workspace == Some(workspace.clone()))
    );
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
            code: yi_agent_store::ipc::IpcErrorCode::ConfirmationRequired,
            message: None,
        }
    ));
    let detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&child_task_id.parse().unwrap())
        .unwrap();
    assert_eq!(detail.state, "cancelled");
}

#[test]
fn cancel_preview_includes_active_worktrees_leases_and_unmerged_deliveries() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let root: TaskId = root_task_id.parse().unwrap();
    let attempt = RuntimeRepository::open(&database)
        .unwrap()
        .active_attempt_id(&root)
        .unwrap();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES ('preview-worktree', ?1, 'worktree:preview', 'exclusive', 1, 'active')",
            [root_task_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES ('preview-cargo', ?1, 'cargo:workspace', 'shared', 1, 'active')",
            [root_task_id.as_str()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO deliveries (id, task_id, attempt_id, payload_json)
             VALUES ('preview-delivery', ?1, ?2, '{\"base\":\"main\",\"head\":\"child\"}')",
            rusqlite::params![root_task_id, attempt.to_string()],
        )
        .unwrap();

    let IpcResponse::CancelPreview {
        worktree_leases,
        active_leases,
        unmerged_deliveries,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewCancel {
            task_id: root.to_string(),
            recursive: false,
        },
    )
    .unwrap()
    else {
        panic!("expected a cancel preview");
    };
    assert_eq!(worktree_leases, vec!["worktree:preview"]);
    assert_eq!(active_leases.len(), 2);
    assert_eq!(active_leases[0].task_id, root.to_string());
    assert_eq!(unmerged_deliveries.len(), 1);
    assert_eq!(unmerged_deliveries[0].delivery_id, "preview-delivery");
    assert!(unmerged_deliveries[0].payload_json.contains("child"));
}

#[test]
fn cancel_confirmation_rejects_a_preview_when_its_active_lease_scope_changes() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();
    let IpcResponse::SessionCreated { root_task_id, .. } =
        send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };
    let IpcResponse::CancelPreview {
        confirmation_token, ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewCancel {
            task_id: root_task_id.clone(),
            recursive: false,
        },
    )
    .unwrap()
    else {
        panic!("expected a cancel preview");
    };
    Connection::open(&database)
        .unwrap()
        .execute(
            "INSERT INTO resource_leases (id, task_id, resource_key, mode, units, state)
             VALUES ('new-preview-lease', ?1, 'worktree:changed', 'exclusive', 1, 'active')",
            [root_task_id.as_str()],
        )
        .unwrap();

    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ConfirmCancel {
                task_id: root_task_id,
                recursive: false,
                confirmation_token,
            },
        )
        .unwrap(),
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::ConfirmationRequired,
            message: None,
        }
    ));
}

#[test]
fn resolve_permission_ipc_uses_only_the_daemon_owned_request_identity() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::ResolvePermission {
            request_id: PermissionRequestId::new().to_string(),
            decision: yi_agent_store::ipc::IpcPermissionDecision::Allow,
        },
    )
    .unwrap();
    assert!(matches!(
        response,
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::NotFound,
            message: None,
        }
    ));
}

#[test]
fn review_ipc_accept_records_user_approval_without_completing_integration() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ReviewReportingFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
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
            objective: "Implement the parser".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child");
    };
    let child: TaskId = child_task_id.parse().unwrap();
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "running"
    );
    let workspace = factory.starts.lock().unwrap()[0]
        .workspace_lease_id
        .clone()
        .unwrap();
    factory.handles.lock().unwrap()[0].report_delivery(
        yi_agent_core::subagent::task::DeliveryReport::coding(
            "deadbeef",
            "main",
            workspace,
            "cargo test -p child",
        ),
    );
    for _ in 0..100 {
        if RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap()
            == "awaiting_parent_review"
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );

    let accept_preview: IpcRequest = serde_json::from_value(serde_json::json!({
        "type": "PreviewReview",
        "task_id": child_task_id,
        "decision": { "type": "accept" }
    }))
    .expect("accept has no caller-supplied integration evidence");
    let response = send_request(daemon.socket_path(), accept_preview).unwrap();
    let IpcResponse::ReviewPreview {
        confirmation_token, ..
    } = response
    else {
        panic!("expected a review preview");
    };
    let response = send_request(
        daemon.socket_path(),
        IpcRequest::ConfirmReview {
            task_id: child_task_id.clone(),
            decision: IpcReviewDecision::Accept {},
            confirmation_token,
        },
    )
    .unwrap();

    let review_detail = RuntimeRepository::open(&database)
        .unwrap()
        .task_detail(&child_task_id.parse().unwrap())
        .unwrap();
    assert!(
        serde_json::to_value(&response).unwrap()["type"] == "ReviewApproved",
        "unexpected review response: {response:?}; state={}; delivery={}",
        review_detail.state,
        review_detail.delivery_json
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let (decision, actor_json): (String, String) = connection
        .query_row(
            "SELECT decision, actor_json FROM reviews ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(decision, "approved");
    let actor: serde_json::Value = serde_json::from_str(&actor_json).unwrap();
    assert_eq!(actor["task_id"], root_task_id);
    assert_eq!(actor["initiated_by"]["kind"], "local_user");
    let parent: TaskId = root_task_id.parse().unwrap();
    let parent_mailbox = RuntimeRepository::open(&database)
        .unwrap()
        .mailbox_messages_for_task(&parent)
        .unwrap();
    assert_eq!(parent_mailbox.len(), 2);
    assert!(parent_mailbox[1].payload_json.contains("approved"));
    let encoded = serde_json::to_value(IpcRequest::PreviewReview {
        task_id: child_task_id,
        decision: IpcReviewDecision::Reject {
            reason: "not acceptable".into(),
        },
    })
    .unwrap();
    assert_eq!(encoded["decision"]["type"], "reject");
    assert!(encoded.get("actor").is_none());
    assert!(encoded.get("session_id").is_none());
    assert!(encoded.get("delivery_id").is_none());
}

#[test]
fn legacy_review_request_fails_instead_of_silently_previewing() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ReviewReportingFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let child_task_id = delivered_child_over_ipc(&daemon, &database, &factory);

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::Review {
            task_id: child_task_id,
            decision: IpcReviewDecision::Accept {},
        },
    )
    .unwrap();

    assert!(matches!(
        response,
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::ConfirmationRequired,
            message: None,
        }
    ));
}

#[test]
fn review_ipc_accept_rejects_forged_integration_evidence() {
    let forged = serde_json::from_value::<IpcReviewDecision>(serde_json::json!({
        "type": "accept",
        "integration_evidence": "caller claims merge and tests passed"
    }));

    assert!(forged.is_err());
}

#[test]
fn review_ipc_requires_a_confirmation_token_before_integrating_a_delivery() {
    let command = serde_json::json!({
        "type": "ConfirmReview",
        "task_id": TaskId::new().to_string(),
        "decision": { "type": "accept" },
        "confirmation_token": "not-a-token"
    });

    let request = serde_json::from_value::<IpcRequest>(command).unwrap();
    assert!(matches!(request, IpcRequest::ConfirmReview { .. }));
}

#[test]
fn review_ipc_rejects_caller_supplied_routing_or_actor_identity() {
    for forged_field in ["actor", "session_id", "delivery_id"] {
        let mut command = serde_json::json!({
            "type": "PreviewReview",
            "task_id": TaskId::new().to_string(),
            "decision": { "type": "accept" }
        });
        command[forged_field] = serde_json::json!("caller-controlled");

        assert!(serde_json::from_value::<IpcRequest>(command).is_err());
    }
}

#[test]
fn review_ipc_routes_rework_feedback_into_the_successor_worker() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ReviewReportingFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let child_task_id = delivered_child_over_ipc(&daemon, &database, &factory);

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewReview {
            task_id: child_task_id.clone(),
            decision: IpcReviewDecision::Rework {
                feedback: "rerun the parser regression suite".into(),
            },
        },
    )
    .unwrap();

    let IpcResponse::ReviewPreview {
        confirmation_token, ..
    } = response
    else {
        panic!("expected a review preview");
    };
    let response = send_request(
        daemon.socket_path(),
        IpcRequest::ConfirmReview {
            task_id: child_task_id.clone(),
            decision: IpcReviewDecision::Rework {
                feedback: "rerun the parser regression suite".into(),
            },
            confirmation_token,
        },
    )
    .unwrap();

    assert!(matches!(response, IpcResponse::ReviewReworkRequested));
    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert!(
        starts[1]
            .initial_user_messages
            .iter()
            .any(|message| message.body.contains("parser regression"))
    );
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child_task_id.parse().unwrap())
            .unwrap(),
        "running"
    );
}

#[test]
fn review_ipc_rejects_with_a_required_durable_reason() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ReviewReportingFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let child_task_id = delivered_child_over_ipc(&daemon, &database, &factory);

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewReview {
            task_id: child_task_id.clone(),
            decision: IpcReviewDecision::Reject {
                reason: "missing regression evidence".into(),
            },
        },
    )
    .unwrap();

    let IpcResponse::ReviewPreview {
        confirmation_token, ..
    } = response
    else {
        panic!("expected a review preview");
    };
    let response = send_request(
        daemon.socket_path(),
        IpcRequest::ConfirmReview {
            task_id: child_task_id.clone(),
            decision: IpcReviewDecision::Reject {
                reason: "missing regression evidence".into(),
            },
            confirmation_token,
        },
    )
    .unwrap();

    assert!(matches!(response, IpcResponse::ReviewRejected));
    let child: TaskId = child_task_id.parse().unwrap();
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.task_state(&child).unwrap(), "blocked");
    assert!(
        repository.mailbox_messages_for_task(&child).unwrap()[0]
            .payload_json
            .contains("regression evidence")
    );
}

#[test]
fn review_ipc_rejects_empty_rework_and_rejection_text() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(ReviewReportingFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let child_task_id = delivered_child_over_ipc(&daemon, &database, &factory);

    for decision in [
        IpcReviewDecision::Rework {
            feedback: " ".into(),
        },
        IpcReviewDecision::Reject {
            reason: "\n".into(),
        },
    ] {
        assert!(matches!(
            send_request(
                daemon.socket_path(),
                IpcRequest::PreviewReview {
                    task_id: child_task_id.clone(),
                    decision,
                },
            )
            .unwrap(),
            IpcResponse::Error {
                code: yi_agent_store::ipc::IpcErrorCode::Validation,
                message: None,
            }
        ));
    }
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child_task_id.parse().unwrap())
            .unwrap(),
        "awaiting_parent_review"
    );
}

fn delivered_child_over_ipc(
    daemon: &Daemon,
    database: &std::path::Path,
    factory: &ReviewReportingFactory,
) -> String {
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
            parent_task_id: root_task_id,
            objective: "Implement the parser".into(),
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child");
    };
    let workspace = factory.starts.lock().unwrap()[0]
        .workspace_lease_id
        .clone()
        .unwrap();
    factory.handles.lock().unwrap()[0].report_delivery(
        yi_agent_core::subagent::task::DeliveryReport::coding(
            "deadbeef",
            "main",
            workspace,
            "cargo test -p child",
        ),
    );
    let child: TaskId = child_task_id.parse().unwrap();
    for _ in 0..100 {
        if RuntimeRepository::open(database)
            .unwrap()
            .task_state(&child)
            .unwrap()
            == "awaiting_parent_review"
        {
            return child_task_id;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("delivery did not reach durable parent review");
}

#[test]
fn raw_cancel_request_cannot_bypass_confirmation() {
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

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::CancelTask {
            session_id,
            task_id: root_task_id,
            recursive: false,
        },
    )
    .unwrap();

    assert!(matches!(
        response,
        IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::ConfirmationRequired,
            message: None,
        }
    ));
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
    let response = confirm_cancel(daemon.socket_path(), root_task_id.clone(), false);
    assert!(matches!(response, IpcResponse::TaskCancelled));

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
