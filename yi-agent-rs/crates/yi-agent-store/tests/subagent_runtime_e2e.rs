use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::subagent::task::{DeliveryReport, WorkspaceLeaseId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, AgentWorkspaceService, WorkerError, WorkerHandle, WorkerRecoveryContext,
    WorkerStart, WorkerWorkspace,
};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, IpcReviewDecision, send_request};

#[derive(Clone, Default)]
struct DeliveryFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
    handles: Arc<Mutex<Vec<WorkerHandle>>>,
}

impl AgentWorkspaceService for DeliveryFactory {
    fn prepare_root(
        &self,
        root: &yi_agent_core::RootSessionId,
        task: &yi_agent_core::TaskId,
        _: &yi_agent_core::AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(root, task))
    }
    fn prepare_child(
        &self,
        _: &WorkerWorkspace,
        root: &yi_agent_core::RootSessionId,
        task: &yi_agent_core::TaskId,
        _: &yi_agent_core::AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(root, task))
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

impl AgentWorkerFactory for DeliveryFactory {
    fn workspace_service(&self) -> Option<Arc<dyn AgentWorkspaceService>> {
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
fn delivery_review_survives_restart_without_restarting_worker() {
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
            session_id,
            parent_task_id: root_task_id,
            capability: message_capability,
            objective: "deliver".into(),
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
    for _ in 0..100 {
        if matches!(send_request(daemon.socket_path(), IpcRequest::InspectTask { task_id: task_id.clone() }).unwrap(), IpcResponse::TaskDetail(ref detail) if detail.state == "awaiting_parent_review")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let IpcResponse::ReviewPreview {
        confirmation_token, ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewReview {
            task_id: task_id.clone(),
            decision: IpcReviewDecision::Reject {
                reason: "test rejection".into(),
            },
        },
    )
    .unwrap()
    else {
        panic!()
    };
    let review_response = send_request(
        daemon.socket_path(),
        IpcRequest::ConfirmReview {
            task_id: task_id.clone(),
            decision: IpcReviewDecision::Reject {
                reason: "test rejection".into(),
            },
            confirmation_token,
        },
    )
    .unwrap();
    assert!(
        matches!(review_response, IpcResponse::ReviewRejected),
        "expected rejected review, got {review_response:?}"
    );
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
    assert!(
        matches!(send_request(restarted.socket_path(), IpcRequest::InspectTask { task_id: task_id.clone() }).unwrap(), IpcResponse::TaskDetail(detail) if detail.state == "blocked")
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
