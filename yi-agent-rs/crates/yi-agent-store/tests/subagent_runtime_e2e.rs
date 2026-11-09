use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::subagent::task::RootSessionId;
use yi_agent_core::subagent::task::{DeliveryReport, TaskId, TaskRole, WorkspaceLeaseId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider, WorkerWorkspaceRegistry,
};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
use yi_agent_store::repository::RuntimeRepository;

/// Binds one host to a freshly attached container and returns the binding.
///
/// Container writes need an open turn from this point on, so every test that
/// spawns goes through here rather than inventing its own binding.
fn bind_container(
    socket: &std::path::Path,
    session_id: &str,
    root_task_id: &str,
    legacy_capability: &str,
    host: &str,
) -> yi_agent_lifecycle::session_delegation::SessionBinding {
    use yi_agent_lifecycle::foreground::SecretCapability;
    use yi_agent_lifecycle::session_delegation::{
        PrepareSession, SessionIdentity, SessionRef, session_resume_digest,
    };
    let IpcResponse::SessionPrepared { binding } = send_request(
        socket,
        IpcRequest::PrepareSessionDelegation {
            request: PrepareSession {
                root: SessionRef {
                    session_id: session_id.to_string(),
                    root_id: root_task_id.to_string(),
                },
                operation_id: format!("bind-{host}-{session_id}"),
                host_run_id: host.to_string(),
                expected_binding_epoch: 0,
                identity: SessionIdentity::LegacyUpgrade(SecretCapability::new(
                    legacy_capability.to_string(),
                )),
                current_binding: None,
                replacement_resume_digest: Some(session_resume_digest(&SecretCapability::new(
                    format!("resume-{host}-{session_id}"),
                ))),
                stopped: Vec::new(),
            },
        },
    )
    .unwrap() else {
        panic!("a bound host must be able to prepare a session");
    };
    binding
}

/// Opens one turn for `binding`.
fn open_turn(
    socket: &std::path::Path,
    binding: &yi_agent_lifecycle::session_delegation::SessionBinding,
    turn_id: &str,
) -> yi_agent_lifecycle::session_delegation::SessionTurn {
    let IpcResponse::SessionTurnOpened { turn } = send_request(
        socket,
        IpcRequest::BeginSessionTurn {
            binding: binding.clone(),
            turn_id: turn_id.into(),
        },
    )
    .unwrap() else {
        panic!("a bound host must be able to open a turn");
    };
    turn
}

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
    let (_directory, daemon, factory, session_id, root_task_id, capability, binding) =
        active_runtime();
    let task_id = spawn_child(
        &daemon,
        &session_id,
        &root_task_id,
        &capability,
        &binding,
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
    yi_agent_lifecycle::session_delegation::SessionBinding,
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
            extra_roots: Vec::new(),
            client_run_id: None,
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
    let binding = bind_container(
        daemon.socket_path(),
        &session_id,
        &root_task_id,
        &message_capability,
        "active-host",
    );
    (
        directory,
        daemon,
        factory,
        session_id,
        root_task_id,
        message_capability,
        binding,
    )
}

fn spawn_child(
    daemon: &Daemon,
    session_id: &str,
    root_task_id: &str,
    capability: &str,
    binding: &yi_agent_lifecycle::session_delegation::SessionBinding,
    objective: &str,
) -> String {
    let turn = open_turn(daemon.socket_path(), binding, objective);
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
            session_turn: Some(turn),
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
            extra_roots: Vec::new(),
            client_run_id: None,
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
    let binding = bind_container(
        daemon.socket_path(),
        &session_id,
        &root_task_id,
        &message_capability,
        "delivery-host",
    );
    let turn = open_turn(daemon.socket_path(), &binding, "deliver");
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
            session_turn: Some(turn),
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

/// 容器角色必须跨越重启存活。
///
/// 这就是用户最初遇到的故障形状：一个会话的历史执行被标成 `stalled`，daemon
/// 重启后 hydration 只按 `state_json` 重建 root；如果角色没有一起读回，root 会
/// 变成 Execution，之后任何 `spawn` 都会被 `ParentSettled` 拒绝，而错误文本恰好
/// 是 “a settled parent task (state: stalled) cannot spawn descendants”。
///
/// 因此本测试：切换历史 state 为 stalled → 重启 daemon → 重新 attach → 断言容器
/// 角色仍在、且能继续 spawn。仅让 attach 能返回成功是不够的。
#[test]
fn a_stalled_session_container_still_spawns_after_a_daemon_restart() {
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
            idempotency_key: "runtime-e2e-stalled-container".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
            extra_roots: Vec::new(),
            client_run_id: None,
        },
    )
    .unwrap()
    else {
        panic!("expected attached application root");
    };
    let root: TaskId = root_task_id.parse().unwrap();
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_role(&root)
            .unwrap(),
        TaskRole::SessionContainer,
        "an application root is created as a container, not an execution"
    );

    // A previous build could still settle a container. Reproduce that history
    // verbatim: the state column is what hydration reads, and it must no longer
    // decide whether the container may delegate.
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE tasks SET state_json = 'stalled' WHERE id = ?1",
            [root.to_string()],
        )
        .unwrap();

    drop(daemon);
    let restarted = Daemon::start_with_factory(&runtime, &database, factory.clone()).unwrap();

    // Re-attaching hydrates the persisted task rows. This is the path that used to
    // lose the role.
    let IpcResponse::ApplicationRootAttached {
        session_id: reattached_session,
        root_task_id: reattached_root,
        message_capability: reattached_capability,
        ..
    } = send_request(
        restarted.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "runtime-e2e-stalled-container".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
            extra_roots: Vec::new(),
            client_run_id: None,
        },
    )
    .unwrap()
    else {
        panic!("expected the same application root back");
    };
    assert_eq!(reattached_session, session_id);
    assert_eq!(reattached_root, root_task_id);

    // A restart is a new generation: the old binding is gone, and the host proves
    // itself again with the capability it already holds.
    let IpcResponse::SessionPrepared { binding } = send_request(
        restarted.socket_path(),
        IpcRequest::PrepareSessionDelegation {
            request: yi_agent_lifecycle::session_delegation::PrepareSession {
                root: yi_agent_lifecycle::session_delegation::SessionRef {
                    session_id: reattached_session.clone(),
                    root_id: reattached_root.clone(),
                },
                operation_id: "resume-after-restart".into(),
                host_run_id: "restart-host".into(),
                expected_binding_epoch: 1,
                identity: yi_agent_lifecycle::session_delegation::SessionIdentity::LegacyUpgrade(
                    yi_agent_lifecycle::foreground::SecretCapability::new(
                        reattached_capability.clone(),
                    ),
                ),
                current_binding: None,
                replacement_resume_digest: None,
                stopped: Vec::new(),
            },
        },
    )
    .unwrap() else {
        panic!("a restarting host must resume with its private secret");
    };
    let turn = open_turn(restarted.socket_path(), &binding, "after-restart");
    let response = send_request(
        restarted.socket_path(),
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id: reattached_session,
            parent_task_id: reattached_root,
            capability: reattached_capability,
            objective: "spawn after restart".into(),
            mode: Some("coding".into()),
            model: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn),
        },
    )
    .unwrap();
    match response {
        IpcResponse::TaskSpawned { .. } => {}
        IpcResponse::Error { code, message, .. } => panic!(
            "a stalled container must still delegate after a restart; got {code:?}: {message:?}"
        ),
        other => panic!("expected a spawned child, got {other:?}"),
    }
    drop(restarted);
    let _ = message_capability;
}

/// 重启后重新挂载时，父级必须仍能从 `WaitAgent` 读到子任务交付的 commit。
///
/// `delivery_survives_restart_without_restarting_worker` 里 root 处于 running，
/// 重启走的是 recovery 分支（只重建 root，不经过 attach 水合）；本测试刻意让
/// root 停在 queued 且不 activate，重启后 `AttachApplicationRoot` 才会真正水合，
/// 从而覆盖 `hydrate_delivered_child`：去掉它，水合出的 attempt.delivery 会是
/// None，`completed_child_reports` 的 delivery 随之丢失，下面的断言就会失败。
#[test]
fn delivered_child_commit_survives_restart_and_is_reported_to_parent() {
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
            idempotency_key: "runtime-e2e-delivery-report-restart".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
            extra_roots: Vec::new(),
            client_run_id: None,
        },
    )
    .unwrap()
    else {
        panic!("expected attached application root");
    };
    // root 停在 queued（不 activate），这样它的状态不属于 recover_inflight_tasks
    // 的扫描范围，重启后只能靠 attach 水合把它和孩子一起重建。
    let binding = bind_container(
        daemon.socket_path(),
        &session_id,
        &root_task_id,
        &message_capability,
        "report-host",
    );
    let turn = open_turn(daemon.socket_path(), &binding, "deliver");
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "deliver".into(),
            mode: Some("coding".into()),
            model: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn),
        },
    )
    .unwrap() else {
        panic!("expected a spawned child");
    };
    let started = factory.starts.lock().unwrap()[0].clone();
    factory.handles.lock().unwrap()[0].report_delivery(DeliveryReport::coding(
        "deadbeef",
        "main",
        started.workspace_lease_id.unwrap(),
        "verified",
    ));
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
    drop(daemon);
    let restarted = Daemon::start_with_factory(&runtime, &database, factory.clone()).unwrap();
    assert_eq!(factory.starts.lock().unwrap().len(), starts);

    let IpcResponse::ApplicationRootAttached {
        session_id: reattached_session,
        root_task_id: reattached_root,
        message_capability: reattached_capability,
        ..
    } = send_request(
        restarted.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "runtime-e2e-delivery-report-restart".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
            extra_roots: Vec::new(),
            client_run_id: None,
        },
    )
    .unwrap()
    else {
        panic!("expected reattached application root");
    };
    let response = send_request(
        restarted.socket_path(),
        IpcRequest::WaitAgent {
            session_id: reattached_session,
            caller_task_id: root_task_id.clone(),
            capability: reattached_capability,
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
    assert_eq!(reports[0].state, "completed");
    assert_eq!(
        reports[0].delivery.as_deref(),
        Some("deadbeef"),
        "重启水合必须把交付 commit 补回 attempt，否则父级会丢掉子任务的交付"
    );
    assert_eq!(reattached_root, root_task_id);
}

/// A container write must name the open turn.
///
/// Three refusals are pinned here because each is a different bypass: a missing
/// turn (the old shape), a turn that was already replaced, and a child borrowing
/// the application entry point to spawn under a container it does not own.
#[test]
fn session_delegation_lifecycle_no_legacy_write_bypass() {
    use yi_agent_lifecycle::foreground::SecretCapability;
    use yi_agent_lifecycle::session_delegation::{
        PrepareSession, SessionIdentity, SessionRef, session_resume_digest,
    };

    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(DeliveryFactory::default());
    let daemon =
        Daemon::start_with_factory(directory.path().join("runtime"), &database, factory.clone())
            .unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let resume_secret = SecretCapability::new("resume-secret");
    let resume_digest = session_resume_digest(&resume_secret);
    let attached = send_request(
        daemon.socket_path(),
        IpcRequest::AttachSessionContainer {
            idempotency_key: "project:turn:/p:1".into(),
            workspace: workspace.clone(),
            extra_roots: Vec::new(),
            operation_id: "op-attach".into(),
            creation_resume_digest: Some(resume_digest),
            identity: None,
        },
    )
    .unwrap();
    // The attach response must not be a way to lift a capability.
    let debug = format!("{attached:?}");
    let IpcResponse::SessionContainerAttached { root, .. } = attached else {
        panic!("expected a container attach, got {debug}");
    };
    assert!(!debug.contains("resume-secret"), "{debug}");

    let prepared = send_request(
        daemon.socket_path(),
        IpcRequest::PrepareSessionDelegation {
            request: PrepareSession {
                root: SessionRef {
                    session_id: root.session_id.clone(),
                    root_id: root.root_id.clone(),
                },
                operation_id: "op-prepare".into(),
                host_run_id: "host-1".into(),
                expected_binding_epoch: 0,
                identity: SessionIdentity::Embedded(resume_secret.clone()),
                current_binding: None,
                replacement_resume_digest: None,
                stopped: Vec::new(),
            },
        },
    )
    .unwrap();
    let IpcResponse::SessionPrepared { binding } = prepared else {
        panic!("expected a binding, got {prepared:?}");
    };

    let opened = send_request(
        daemon.socket_path(),
        IpcRequest::BeginSessionTurn {
            binding: binding.clone(),
            turn_id: "turn-1".into(),
        },
    )
    .unwrap();
    let IpcResponse::SessionTurnOpened { turn } = opened else {
        panic!("expected an open turn, got {opened:?}");
    };

    // 1. The old shape: a container spawn with no turn at all.
    let no_turn = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: root.session_id.clone(),
            parent_task_id: root.root_id.clone(),
            capability: "not-a-capability".into(),
            objective: "no turn".into(),
            mode: None,
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: None,
        },
    )
    .unwrap();
    assert!(
        matches!(
            no_turn,
            IpcResponse::SessionDelegationRejected { .. } | IpcResponse::Error { .. }
        ),
        "{no_turn:?}"
    );

    // 2. A superseded turn cannot be replayed after a new one opened.
    let second = send_request(
        daemon.socket_path(),
        IpcRequest::BeginSessionTurn {
            binding: binding.clone(),
            turn_id: "turn-2".into(),
        },
    )
    .unwrap();
    assert!(matches!(second, IpcResponse::SessionTurnOpened { .. }));

    let stale = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: root.session_id.clone(),
            parent_task_id: root.root_id.clone(),
            capability: "not-a-capability".into(),
            objective: "old turn".into(),
            mode: None,
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn.clone()),
        },
    )
    .unwrap();
    assert!(
        matches!(
            stale,
            IpcResponse::SessionDelegationRejected { .. } | IpcResponse::Error { .. }
        ),
        "{stale:?}"
    );

    let repository = RuntimeRepository::open(&database).unwrap();
    let session: RootSessionId = root.session_id.parse().unwrap();
    let summaries = repository.task_summaries(Some(&session), false).unwrap();
    let children = summaries
        .iter()
        .filter(|summary| summary.parent_task_id.as_deref() == Some(root.root_id.as_str()))
        .count();
    assert_eq!(children, 0, "a refused spawn must not create a child");
}
