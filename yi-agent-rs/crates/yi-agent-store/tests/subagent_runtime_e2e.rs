use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::subagent::task::{DeliveryReport, TaskId, WorkspaceLeaseId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider, WorkerWorkspaceRegistry,
};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};

#[derive(Clone, Default)]
struct DeliveryFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

impl WorkerWorkspaceProvider for DeliveryFactory {
    fn in_place_workspace(
        &self,
        root: &yi_agent_core::RootSessionId,
        task: &yi_agent_core::TaskId,
        _: &yi_agent_core::AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(root, task))
    }
    fn workspace_in(
        &self,
        task_id: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(&yi_agent_core::RootSessionId::new(), task_id))
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let mut position = parent
            .cloned()
            .unwrap_or_else(|| workspace(&yi_agent_core::RootSessionId::new(), task_id));
        position.branch = String::new();
        position.parent_branch = String::new();
        position.base_commit = String::new();
        position.lease_id = WorkspaceLeaseId::new();
        Ok(position)
    }
}

fn workspace(root: &yi_agent_core::RootSessionId, task: &yi_agent_core::TaskId) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: format!("/tmp/{root}").into(),
        path: format!("/tmp/{root}/{task}").into(),
        branch: format!("child-{task}"),
        parent_branch: "main".into(),
        base_commit: "fedcba9876543210fedcba9876543210fedcba98".into(),
    }
}

impl WorkerWorkspaceRegistry for DeliveryFactory {
    fn register_prepared(&self, _workspace: &WorkerWorkspace) {}

    fn prepared_workspace_for_workdir(
        &self,
        _workdir: &std::path::Path,
    ) -> Option<WorkerWorkspace> {
        None
    }

    fn inspect_delivery(&self, workspace: &WorkerWorkspace) -> Result<DeliveryReport, WorkerError> {
        Ok(DeliveryReport::coding(
            "deadbeef",
            "main",
            workspace.lease_id.clone(),
            "verified",
        ))
    }
}

impl AgentWorkerFactory for DeliveryFactory {
    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(self.clone()))
    }

    fn worker_workspace_registry(&self) -> Option<Arc<dyn WorkerWorkspaceRegistry>> {
        Some(Arc::new(self.clone()))
    }
    fn recovery_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:test".into()),
            worktree_lease: Some("worktree:test".into()),
            checkpoint_json: "{}".into(),
            tool_state_json: "{}".into(),
        }
    }
    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        self.handles.lock().unwrap().push(handle.clone());
        Box::pin(async move { Ok(handle) })
    }
}

#[test]
fn concurrent_cancel_confirmation_has_one_winner() {
    let (_directory, daemon, factory, session_id, root_task_id, capability) = active_runtime();
    let task_id = spawn_child(
        &daemon,
        &session_id,
        &root_task_id,
        &capability,
        "cancel concurrently",
    );
    let IpcResponse::CancelPreview {
        confirmation_token, ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewCancel {
            task_id: task_id.clone(),
            recursive: false,
        },
    )
    .unwrap()
    else {
        panic!("expected cancellation preview");
    };

    let barrier = Arc::new(Barrier::new(2));
    let responses = thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            send_request(
                daemon.socket_path(),
                IpcRequest::ConfirmCancel {
                    task_id: task_id.clone(),
                    recursive: false,
                    confirmation_token: confirmation_token.clone(),
                },
            )
            .unwrap()
        });
        let second = scope.spawn(|| {
            barrier.wait();
            send_request(
                daemon.socket_path(),
                IpcRequest::ConfirmCancel {
                    task_id: task_id.clone(),
                    recursive: false,
                    confirmation_token: confirmation_token.clone(),
                },
            )
            .unwrap()
        });
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, IpcResponse::TaskCancelled))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response, IpcResponse::Error { .. }))
            .count(),
        1
    );
    assert_eq!(factory.handles.lock().unwrap().len(), 1);
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::InspectTask {
                task_id: task_id.clone()
            }
        )
        .unwrap(),
        IpcResponse::TaskDetail(detail) if detail.state == "cancelled"
    ));
    let IpcResponse::TaskEvents { events } = send_request(
        daemon.socket_path(),
        IpcRequest::ReadTaskEvents {
            task_id,
            after_event_id: None,
        },
    )
    .unwrap() else {
        panic!("expected task events");
    };
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "task_cancelled")
            .count(),
        1
    );
}

fn active_runtime() -> (
    TempDir,
    Daemon,
    Arc<DeliveryFactory>,
    String,
    String,
    String,
) {
    let directory = TempDir::new().unwrap();
    let factory = Arc::new(DeliveryFactory::default());
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        directory.path().join("runtime.sqlite"),
        factory.clone(),
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
            idempotency_key: "runtime-e2e-active".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
        },
    )
    .unwrap()
    else {
        panic!("expected attached application root");
    };
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                objective: "run lifecycle test".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    ));
    (
        directory,
        daemon,
        factory,
        session_id,
        root_task_id,
        message_capability,
    )
}

fn spawn_child(
    daemon: &Daemon,
    session_id: &str,
    root_task_id: &str,
    capability: &str,
    objective: &str,
) -> String {
    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id: session_id.into(),
            parent_task_id: root_task_id.into(),
            capability: capability.into(),
            objective: objective.into(),
            mode: Some("coding".into()),
            model: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .unwrap();
    let IpcResponse::TaskSpawned { task_id } = response else {
        panic!("expected spawned child, got {response:?}");
    };
    task_id
}

#[test]
fn delivery_survives_restart_without_restarting_worker() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("runtime.sqlite");
    let runtime = dir.path().join("runtime");
    let factory = Arc::new(DeliveryFactory::default());
    let daemon = Daemon::start_with_factory(&runtime, &database, factory.clone()).unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "runtime-e2e-restart".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
        },
    )
    .unwrap()
    else {
        panic!("expected attached application root");
    };
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                objective: "run lifecycle test".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    ));
    let response = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id,
            parent_task_id: root_task_id,
            capability: message_capability,
            objective: "deliver".into(),
            mode: Some("coding".into()),
            model: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .unwrap();
    let IpcResponse::TaskSpawned { task_id } = response else {
        panic!("expected spawned child, got {response:?}");
    };
    let started = factory.starts.lock().unwrap()[0].clone();
    factory.handles.lock().unwrap()[0].report_delivery(DeliveryReport::coding(
        "deadbeef",
        "main",
        started.workspace_lease_id.unwrap(),
        "verified",
    ));
    // 交付即完成：没有 awaiting_parent_review 中间态可以等待。
    let mut completed = false;
    for _ in 0..100 {
        if matches!(send_request(daemon.socket_path(), IpcRequest::InspectTask { task_id: task_id.clone() }).unwrap(), IpcResponse::TaskDetail(ref detail) if detail.state == "completed")
        {
            completed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(completed, "a delivered child must reach completed");

    let starts = factory.starts.lock().unwrap().len();
    let before = send_request(
        daemon.socket_path(),
        IpcRequest::ReadTaskEvents {
            task_id: task_id.clone(),
            after_event_id: None,
        },
    )
    .unwrap();
    drop(daemon);
    let restarted = Daemon::start_with_factory(&runtime, &database, factory.clone()).unwrap();
    assert_eq!(factory.starts.lock().unwrap().len(), starts);
    // 重启重新水合终态交付行后，任务仍是 completed，事件流逐字不变
    //（旧 review_* kind 若未在 v14 里改写，ReadTaskEvents 会直接解析失败）。
    assert!(
        matches!(send_request(restarted.socket_path(), IpcRequest::InspectTask { task_id: task_id.clone() }).unwrap(), IpcResponse::TaskDetail(ref detail) if detail.state == "completed")
    );
    assert_eq!(
        send_request(
            restarted.socket_path(),
            IpcRequest::ReadTaskEvents {
                task_id,
                after_event_id: None
            }
        )
        .unwrap(),
        before
    );
}
