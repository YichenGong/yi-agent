use std::sync::{Arc, Barrier, Mutex};
use std::thread;
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

#[test]
fn competing_review_confirmations_have_one_winner() {
    let (_directory, daemon, factory, session_id, root_task_id, capability) = active_runtime();
    let task_id = spawn_child(
        &daemon,
        &session_id,
        &root_task_id,
        &capability,
        "review concurrently",
    );
    report_delivery_and_wait_for_review(&daemon, &factory, &task_id);
    let IpcResponse::ReviewPreview {
        confirmation_token: accept_token,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewReview {
            task_id: task_id.clone(),
            decision: IpcReviewDecision::Accept {},
        },
    )
    .unwrap()
    else {
        panic!("expected accept preview");
    };
    let IpcResponse::ReviewPreview {
        confirmation_token: reject_token,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewReview {
            task_id: task_id.clone(),
            decision: IpcReviewDecision::Reject {
                reason: "race".into(),
            },
        },
    )
    .unwrap()
    else {
        panic!("expected reject preview");
    };

    let barrier = Arc::new(Barrier::new(2));
    let responses = thread::scope(|scope| {
        let accept = scope.spawn(|| {
            barrier.wait();
            send_request(
                daemon.socket_path(),
                IpcRequest::ConfirmReview {
                    task_id: task_id.clone(),
                    decision: IpcReviewDecision::Accept {},
                    confirmation_token: accept_token.clone(),
                },
            )
            .unwrap()
        });
        let reject = scope.spawn(|| {
            barrier.wait();
            send_request(
                daemon.socket_path(),
                IpcRequest::ConfirmReview {
                    task_id: task_id.clone(),
                    decision: IpcReviewDecision::Reject {
                        reason: "race".into(),
                    },
                    confirmation_token: reject_token.clone(),
                },
            )
            .unwrap()
        });
        [accept.join().unwrap(), reject.join().unwrap()]
    });

    assert_eq!(
        responses
            .iter()
            .filter(|response| {
                matches!(
                    response,
                    IpcResponse::ReviewApproved | IpcResponse::ReviewRejected
                )
            })
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
            .filter(|event| event.kind == "review_approved" || event.kind == "review_rejected")
            .count(),
        1
    );
}

#[test]
fn rework_creates_one_successor_attempt_with_feedback() {
    let (_directory, daemon, factory, session_id, root_task_id, capability) = active_runtime();
    let task_id = spawn_child(
        &daemon,
        &session_id,
        &root_task_id,
        &capability,
        "rework delivery",
    );
    report_delivery_and_wait_for_review(&daemon, &factory, &task_id);
    let first_start = factory.starts.lock().unwrap()[0].clone();
    let IpcResponse::ReviewPreview {
        confirmation_token, ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::PreviewReview {
            task_id: task_id.clone(),
            decision: IpcReviewDecision::Rework {
                feedback: "replace marker".into(),
            },
        },
    )
    .unwrap()
    else {
        panic!("expected rework preview");
    };
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ConfirmReview {
                task_id: task_id.clone(),
                decision: IpcReviewDecision::Rework {
                    feedback: "replace marker".into(),
                },
                confirmation_token: confirmation_token.clone(),
            },
        )
        .unwrap(),
        IpcResponse::ReviewReworkRequested
    ));
    let starts = factory.starts.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert_ne!(starts[1].attempt_id, first_start.attempt_id);
    assert!(
        starts[1]
            .initial_user_messages
            .iter()
            .any(|message| message.body.contains("replace marker"))
    );
    drop(starts);
    assert!(matches!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ConfirmReview {
                task_id,
                decision: IpcReviewDecision::Rework {
                    feedback: "replace marker".into(),
                },
                confirmation_token,
            },
        )
        .unwrap(),
        IpcResponse::Error { .. }
    ));
}

fn report_delivery_and_wait_for_review(daemon: &Daemon, factory: &DeliveryFactory, task_id: &str) {
    let started = factory.starts.lock().unwrap()[0].clone();
    factory.handles.lock().unwrap()[0].report_delivery(DeliveryReport::coding(
        "deadbeef",
        "main",
        started.workspace_lease_id.unwrap(),
        "verified",
    ));
    for _ in 0..100 {
        if matches!(send_request(daemon.socket_path(), IpcRequest::InspectTask { task_id: task_id.into() }).unwrap(), IpcResponse::TaskDetail(ref detail) if detail.state == "awaiting_parent_review")
        {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("task never reached awaiting_parent_review");
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
            session_id: session_id.into(),
            parent_task_id: root_task_id.into(),
            capability: capability.into(),
            objective: objective.into(),
            mode: Some("coding".into()),
        },
    )
    .unwrap();
    let IpcResponse::TaskSpawned { task_id } = response else {
        panic!("expected spawned child, got {response:?}");
    };
    task_id
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
            session_id,
            parent_task_id: root_task_id,
            capability: message_capability,
            objective: "deliver".into(),
            mode: Some("coding".into()),
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
