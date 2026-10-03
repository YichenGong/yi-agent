//! End-to-end coverage for forking a caller's conversation into a subagent over
//! the daemon socket: the caller opens an upload, pushes its history as base64
//! chunks, and names the resulting token on the spawn request. The daemon must
//! carry the token through to the child's worker start, and must refuse a spawn
//! whose token is unknown, incomplete, or already consumed rather than silently
//! spawning without the fork.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use tempfile::TempDir;
use yi_agent_core::Message;
use yi_agent_core::RootSessionId;
use yi_agent_core::subagent::task::{AttemptId, TaskId, WorkspaceLeaseId};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_store::ipc::{Daemon, IpcErrorCode, IpcRequest, IpcResponse, send_request};

struct StaticWorkspaceService {
    repository_root: PathBuf,
}

impl WorkerWorkspaceProvider for StaticWorkspaceService {
    fn in_place_workspace(
        &self,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace_for(
            &self.repository_root,
            &format!("{root_session_id}/{task_id}"),
        ))
    }

    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace_for(&self.repository_root, "workdir"))
    }

    fn read_only_workspace(
        &self,
        _parent: Option<&WorkerWorkspace>,
        task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace_for(
            &self.repository_root,
            &format!("read-only/{task_id}"),
        ))
    }
}

fn workspace_for(root: &std::path::Path, name: &str) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: root.to_path_buf(),
        path: root.join(name),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    }
}

/// Records every worker start so a test can assert what the fork token turned
/// into. The worker itself is inert.
struct RecordingFactory {
    repository_root: PathBuf,
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}

impl AgentWorkerFactory for RecordingFactory {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:fork-ipc".into()),
            worktree_lease: Some("worktree:fork-ipc".into()),
            checkpoint_json: "{}".into(),
            tool_state_json: "{}".into(),
        }
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(StaticWorkspaceService {
            repository_root: self.repository_root.clone(),
        }))
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        let handle = WorkerHandle::new(request.cancellation.clone());
        self.starts.lock().unwrap().push(request);
        Box::pin(async move { Ok(handle) })
    }
}

struct ForkDaemon {
    _directory: TempDir,
    daemon: Daemon,
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}

/// Starts a real daemon whose factory records worker starts, attaches one
/// application root, and returns its capability so the caller can fork.
fn forked_daemon() -> (ForkDaemon, String, String, String) {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let project = directory.path().join("project");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(RecordingFactory {
        repository_root: project.clone(),
        starts: Arc::clone(&starts),
    });
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
            idempotency_key: "fork-ipc".into(),
            workspace: project,
        },
    )
    .unwrap()
    else {
        panic!("expected an application root attachment");
    };
    (
        ForkDaemon {
            _directory: directory,
            daemon,
            starts,
        },
        session_id,
        root_task_id,
        message_capability,
    )
}

/// Uploads `messages` for `session`/`caller` and returns the fork token.
fn upload_history(
    socket: &std::path::Path,
    session: &str,
    caller: &str,
    capability: &str,
    messages: &[Message],
) -> String {
    let payload = serde_json::to_vec(messages).unwrap();
    let IpcResponse::ForkUploadStarted { fork_token } = send_request(
        socket,
        IpcRequest::BeginForkUpload {
            session_id: session.into(),
            caller_task_id: caller.into(),
            capability: capability.into(),
            total_bytes: payload.len() as u64,
        },
    )
    .unwrap() else {
        panic!("expected a fork upload token");
    };
    let mut received = 0u64;
    for (seq, chunk) in payload.chunks(256 * 1024).enumerate() {
        let data = STANDARD.encode(chunk);
        let IpcResponse::ForkChunkAccepted {
            received: cumulative,
        } = send_request(
            socket,
            IpcRequest::AppendForkChunk {
                fork_token: fork_token.clone(),
                seq: seq as u64,
                data,
            },
        )
        .unwrap()
        else {
            panic!("expected an accepted fork chunk");
        };
        received = cumulative;
    }
    assert_eq!(
        received,
        payload.len() as u64,
        "every declared byte must have arrived"
    );
    fork_token
}

fn spawn_with_token(
    socket: &std::path::Path,
    session: &str,
    parent: &str,
    capability: &str,
    fork_token: Option<String>,
) -> IpcResponse {
    send_request(
        socket,
        IpcRequest::SpawnApplicationChild {
            workdir: None,
            session_id: session.into(),
            parent_task_id: parent.into(),
            capability: capability.into(),
            objective: "child task".into(),
            mode: None,
            model: None,
            thread_id: None,
            sandbox: None,
            fork_token,
        },
    )
    .unwrap()
}

/// The number of tasks the daemon holds for a session. Used to prove a refused
/// fork does not leave a half-created child behind.
fn session_task_count(socket: &std::path::Path, session: &str) -> usize {
    let IpcResponse::TaskSummaries { tasks } = send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: Some(session.into()),
            active_only: false,
        },
    )
    .unwrap() else {
        panic!("expected a task summary list");
    };
    tasks.len()
}

#[test]
fn a_forked_spawn_carries_the_history_to_the_child_worker() {
    let (fixture, session, root, capability) = forked_daemon();
    let history = vec![Message::user("parent said"), Message::user("and more")];
    let token = upload_history(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        &history,
    );

    let IpcResponse::TaskSpawned { task_id } = spawn_with_token(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        Some(token),
    ) else {
        panic!("a valid fork token must spawn the child");
    };

    let starts = fixture.starts.lock().unwrap();
    let start = starts
        .iter()
        .find(|start| start.task_id.to_string() == task_id)
        .expect("the forked child's worker started");
    assert_eq!(
        start.fork_messages.as_deref(),
        Some(history.as_slice()),
        "the uploaded history reaches the child worker"
    );
}

#[test]
fn a_spawn_with_an_unknown_token_is_refused() {
    let (fixture, session, root, capability) = forked_daemon();
    let before = session_task_count(fixture.daemon.socket_path(), &session);

    let response = spawn_with_token(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        Some("bogus".into()),
    );

    assert!(
        matches!(
            response,
            IpcResponse::Error {
                code: IpcErrorCode::Validation,
                ..
            }
        ),
        "an unknown token is an explicit validation failure, got {response:?}"
    );
    assert_eq!(
        session_task_count(fixture.daemon.socket_path(), &session),
        before,
        "a refused fork must not leave a half-created child behind"
    );
    assert!(
        fixture.starts.lock().unwrap().is_empty(),
        "a refused fork must never start a worker without it"
    );
}

#[test]
fn a_spawn_with_an_incomplete_upload_is_refused() {
    let (fixture, session, root, capability) = forked_daemon();
    let payload = serde_json::to_vec(&[Message::user("parent said")]).unwrap();
    // Declare more than is sent: the token must not be usable until every byte
    // named at begin has arrived.
    let IpcResponse::ForkUploadStarted { fork_token } = send_request(
        fixture.daemon.socket_path(),
        IpcRequest::BeginForkUpload {
            session_id: session.clone(),
            caller_task_id: root.clone(),
            capability: capability.clone(),
            total_bytes: payload.len() as u64 + 4,
        },
    )
    .unwrap() else {
        panic!("expected a fork upload token");
    };
    send_request(
        fixture.daemon.socket_path(),
        IpcRequest::AppendForkChunk {
            fork_token: fork_token.clone(),
            seq: 0,
            data: STANDARD.encode(&payload),
        },
    )
    .unwrap();

    let response = spawn_with_token(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        Some(fork_token),
    );
    assert!(
        matches!(
            response,
            IpcResponse::Error {
                code: IpcErrorCode::Validation,
                ..
            }
        ),
        "an incomplete upload cannot seed a child, got {response:?}"
    );
    assert!(fixture.starts.lock().unwrap().is_empty());
}

#[test]
fn a_token_is_consumed_exactly_once_so_a_replay_is_refused() {
    let (fixture, session, root, capability) = forked_daemon();
    let history = vec![Message::user("parent said")];
    let token = upload_history(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        &history,
    );

    assert!(matches!(
        spawn_with_token(
            fixture.daemon.socket_path(),
            &session,
            &root,
            &capability,
            Some(token.clone()),
        ),
        IpcResponse::TaskSpawned { .. }
    ));

    let replay = spawn_with_token(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        Some(token),
    );
    assert!(
        matches!(
            replay,
            IpcResponse::Error {
                code: IpcErrorCode::Validation,
                ..
            }
        ),
        "a consumed token cannot be replayed, got {replay:?}"
    );
}

#[test]
fn a_spawn_without_a_token_spawns_unforked() {
    let (fixture, session, root, capability) = forked_daemon();

    let IpcResponse::TaskSpawned { task_id } = spawn_with_token(
        fixture.daemon.socket_path(),
        &session,
        &root,
        &capability,
        None,
    ) else {
        panic!("an unforked spawn still works");
    };

    let starts = fixture.starts.lock().unwrap();
    let start = starts
        .iter()
        .find(|start| start.task_id.to_string() == task_id)
        .expect("the unforked child's worker started");
    assert!(
        start.fork_messages.is_none(),
        "omitting the token keeps the old behavior: no fork"
    );
}

/// Spawns an ordinary (unforked) child and returns its task id.
fn spawn_child(socket: &std::path::Path, session: &str, parent: &str, objective: &str) -> String {
    let IpcResponse::TaskSpawned { task_id } = send_request(
        socket,
        IpcRequest::SpawnChild {
            session_id: session.into(),
            parent_task_id: parent.into(),
            objective: objective.into(),
            mode: None,
            model: None,
            workdir: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .unwrap() else {
        panic!("expected a spawned child");
    };
    task_id
}

/// The worker capability the daemon minted for `task`, read from the recorded
/// worker start: this is the handle a subagent holds for its own conversation.
fn child_capability(starts: &Arc<Mutex<Vec<WorkerStart>>>, task: &str) -> String {
    let starts = starts.lock().unwrap();
    let start = starts
        .iter()
        .find(|start| start.task_id.to_string() == task)
        .expect("the child's worker started");
    let capability = start.message_capability.clone();
    assert!(
        !capability.is_empty(),
        "the daemon minted a worker capability for the child"
    );
    capability
}

/// The Critical, end to end over the socket: a child subagent forks its own
/// conversation. Its caller task and worker capability - not the application
/// root's - open the upload, and the token seeds a forked grandchild.
#[test]
fn a_child_caller_forks_its_own_child_over_the_socket() {
    let (fixture, session, root, _root_capability) = forked_daemon();
    let child = spawn_child(fixture.daemon.socket_path(), &session, &root, "child");
    let capability = child_capability(&fixture.starts, &child);

    let history = vec![Message::user("the child's own history")];
    let token = upload_history(
        fixture.daemon.socket_path(),
        &session,
        &child,
        &capability,
        &history,
    );

    let grandchild = {
        let IpcResponse::TaskSpawned { task_id } = send_request(
            fixture.daemon.socket_path(),
            IpcRequest::SpawnChild {
                session_id: session.clone(),
                parent_task_id: child.clone(),
                objective: "grandchild".into(),
                mode: None,
                model: None,
                workdir: None,
                sandbox: None,
                fork_token: Some(token),
            },
        )
        .unwrap() else {
            panic!("a child's fork token must spawn its own child");
        };
        task_id
    };

    let starts = fixture.starts.lock().unwrap();
    let start = starts
        .iter()
        .find(|start| start.task_id.to_string() == grandchild)
        .expect("the forked grandchild's worker started");
    assert_eq!(
        start.fork_messages.as_deref(),
        Some(history.as_slice()),
        "the child's uploaded history reaches the forked grandchild worker"
    );
}

/// The widening is not a free pass: a child caller (or any caller) presenting a
/// capability that is not its own is still refused, and no token is created.
#[test]
fn a_child_presenting_a_foreign_capability_is_denied() {
    let (fixture, session, root, root_capability) = forked_daemon();
    let child = spawn_child(fixture.daemon.socket_path(), &session, &root, "child");

    let response = send_request(
        fixture.daemon.socket_path(),
        IpcRequest::BeginForkUpload {
            session_id: session.clone(),
            caller_task_id: child,
            capability: root_capability,
            total_bytes: 4,
        },
    )
    .unwrap();

    assert!(
        matches!(
            response,
            IpcResponse::Error {
                code: IpcErrorCode::AuthorityDenied,
                ..
            }
        ),
        "the root capability named for a child caller cannot open an upload, got {response:?}"
    );
}
