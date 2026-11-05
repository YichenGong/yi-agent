//! The host-side driver against a real daemon.
//!
//! The unit tests in `src/foreground.rs` use a recording transport, so they pin the state
//! machine but say nothing about whether the daemon agrees with it. This file closes that
//! gap: a real daemon, a real socket, a real `ThreadRoot`, and the driver doing what a
//! host does -- bind, open a turn, wait, finish, read back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use yi_agent_core::subagent::task::AttemptId;
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_core::{RootSessionId, TaskId};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
use yi_agent_subagent::binding::{RuntimeBinding, RuntimeHandle};
use yi_agent_subagent::foreground::{ForegroundClientError, ForegroundDriver};
use yi_agent_subagent::thread_root::ThreadRoot;

/// Enough durable evidence for the runtime to consider a worker resumable.
fn recovery_context() -> WorkerRecoveryContext {
    WorkerRecoveryContext {
        workspace_lease_id: Some("workspace:foreground-test".into()),
        worktree_lease: Some("worktree:foreground-test".into()),
        checkpoint_json: r#"{"git_head":"test","git_status":""}"#.into(),
        tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
    }
}

/// Resolves the conversation's own directory as the root's position.
struct InPlace(PathBuf);

impl WorkerWorkspaceProvider for InPlace {
    fn in_place_workspace(
        &self,
        _session: &RootSessionId,
        _task_id: &TaskId,
        _attempt: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.position())
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        // The driver tests never spawn a child, so an in-place position is the only one
        // that has to resolve. Refusing the rest keeps the fixture honest rather than
        // silently answering with a directory the request never mentioned.
        Err(WorkerError::Startup(
            "the foreground driver fixture resolves in-place positions only".into(),
        ))
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(parent.cloned().unwrap_or_else(|| self.position()))
    }
}

impl InPlace {
    fn position(&self) -> WorkerWorkspace {
        WorkerWorkspace {
            lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
            repository_root: self.0.clone(),
            path: self.0.clone(),
            branch: "main".into(),
            parent_branch: String::new(),
            base_commit: "0".repeat(40),
        }
    }
}

#[derive(Clone)]
struct Fixture {
    workspace: PathBuf,
}

impl AgentWorkerFactory for Fixture {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        recovery_context()
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(InPlace(self.workspace.clone())))
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        // A handle that never runs anything: these tests are about admission and activity
        // reports, and a fake worker that actually did work would only add noise.
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

fn daemon_with_foreground(directory: &tempfile::TempDir) -> Daemon {
    Daemon::start_with_factory_and_activation(
        directory.path().join("runtime"),
        directory.path().join("runtime.sqlite"),
        Arc::new(Fixture {
            workspace: directory.path().to_path_buf(),
        }),
        yi_agent_store::foreground::ForegroundActivation::for_test(true),
    )
    .expect("the daemon starts")
}

/// The pieces a host holds: the thread root plus the authority the guardian would grant.
fn attach(
    daemon: &Daemon,
    workspace: &Path,
) -> (
    Arc<ThreadRoot>,
    yi_agent_lifecycle::types::RootAuthorization,
) {
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "foreground-driver".into(),
            workspace: workspace.to_path_buf(),
            extra_roots: Vec::new(),
        },
    )
    .expect("the root attaches")
    else {
        panic!("expected an attachment");
    };

    let intent = yi_agent_lifecycle::foreground::RootBindIntent {
        operation_id: "prepare-1".into(),
        thread_key: "thread:foreground-driver".into(),
        workspace: workspace.to_path_buf(),
        extra_roots: Vec::new(),
        previous_root_capability: None,
    };
    let authorization = daemon
        .coordinator_for_test()
        .install_foreground_authority_for_test(&root_task_id, "app", intent)
        .expect("the bridge installs authority");

    let binding = RuntimeBinding::fixed(RuntimeHandle {
        socket_path: daemon.socket_path().to_path_buf(),
        workspace_root: workspace.to_path_buf(),
        session_id: session_id.clone(),
        task_id: root_task_id.clone(),
        capability: message_capability.clone(),
    });
    let root = ThreadRoot::from_handle(
        binding,
        yi_agent_subagent::AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace: InPlace(workspace.to_path_buf()).position(),
        },
    );
    (root, authorization)
}

#[test]
fn foreground_root_lifecycle_a_driver_binds_reports_and_reads_back_through_the_daemon() {
    let directory = tempfile::TempDir::new().expect("a temp dir");
    let daemon = daemon_with_foreground(&directory);
    let (root, authorization) = attach(&daemon, directory.path());

    let mut driver = ForegroundDriver::bind(root, authorization, "bind-1".into())
        .expect("the daemon issues a binding");
    assert_eq!(
        driver.last_snapshot().phase,
        yi_agent_lifecycle::foreground::ActivityPhase::Idle,
        "a fresh driver starts from the idle baseline the daemon reported"
    );

    driver.begin_turn("turn-1").expect("turn-1 opens");
    driver
        .waiting(yi_agent_lifecycle::foreground::ActivityPhase::WaitingForChildren)
        .expect("a waiting phase inside an open turn is accepted");
    assert_eq!(
        driver.last_snapshot().phase,
        yi_agent_lifecycle::foreground::ActivityPhase::WaitingForChildren
    );

    driver
        .finish_turn("turn-1")
        .expect("the matching finish is accepted");
    assert_eq!(
        driver.last_snapshot().phase,
        yi_agent_lifecycle::foreground::ActivityPhase::Idle
    );

    // A finish for the turn that just closed is refused, and it does not open a turn.
    assert!(matches!(
        driver.finish_turn("turn-1"),
        Err(ForegroundClientError::Protocol(_))
    ));

    driver
        .begin_turn("turn-2")
        .expect("a closed turn leaves room");
    driver.finish_turn("turn-2").expect("turn-2 closes");

    // The daemon's own view agrees with the driver's, which is the point of going through
    // the socket instead of the recording transport.
    let snapshot = driver
        .refresh()
        .expect("the committed snapshot is readable");
    assert_eq!(snapshot.update.turn_id.as_deref(), Some("turn-2"));
}

#[test]
fn foreground_root_lifecycle_a_replayed_bind_hands_back_a_usable_driver() {
    let directory = tempfile::TempDir::new().expect("a temp dir");
    let daemon = daemon_with_foreground(&directory);
    let (root, authorization) = attach(&daemon, directory.path());

    let mut first =
        ForegroundDriver::bind(Arc::clone(&root), authorization.clone(), "bind-1".into())
            .expect("the first bind succeeds");
    first.begin_turn("turn-1").expect("turn-1 opens");

    // The host restarted and retried the same operation. The daemon must answer with the
    // same epoch *and* the same capability, so the new handle continues the conversation
    // rather than displacing the one that is already writing.
    let mut second =
        ForegroundDriver::bind(root, authorization, "bind-1".into()).expect("the replay succeeds");
    assert_eq!(
        first.binding().fence.driver_epoch,
        second.binding().fence.driver_epoch
    );
    assert_eq!(
        first.binding().driver_capability.expose(),
        second.binding().driver_capability.expose()
    );
    assert!(
        second.binding().last_seq >= 1,
        "the replay is told where the sequence has reached instead of restarting at 0"
    );
    second
        .finish_turn("turn-1")
        .expect("the replayed driver can close the open turn");
}

#[test]
fn foreground_root_lifecycle_a_second_driver_operation_is_refused_until_a_stop_is_proven() {
    let directory = tempfile::TempDir::new().expect("a temp dir");
    let daemon = daemon_with_foreground(&directory);
    let (root, authorization) = attach(&daemon, directory.path());

    let _first = ForegroundDriver::bind(Arc::clone(&root), authorization.clone(), "bind-1".into())
        .expect("the first bind succeeds");

    // `ForegroundDriver` deliberately has no `Debug` impl (it holds a capability), so the
    // assertion goes through a match instead of `expect_err`.
    match ForegroundDriver::bind(root, authorization, "bind-2".into()) {
        Err(ForegroundClientError::Rejected(failure)) => assert_eq!(
            failure.code,
            yi_agent_lifecycle::foreground::ForegroundErrorCode::StopUnproven,
            "the caller has to learn that a driver still holds the root"
        ),
        Err(other) => panic!("expected a structured refusal, got {other:?}"),
        Ok(_) => panic!("a takeover without a proven stop must be refused"),
    }
}
