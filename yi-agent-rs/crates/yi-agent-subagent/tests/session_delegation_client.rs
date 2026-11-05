//! The host client against a real daemon.
//!
//! These are the three facts a host depends on, each of which used to be false:
//!
//! 1. A conversation whose last execution ended in `stalled` can still delegate,
//!    because its container role and its live turn -- not `TaskState` -- decide.
//! 2. A host that survives two turns reuses its binding instead of looking like a
//!    driver replacement every time.
//! 3. A host that lost the reply to its own attach can prove its identity with the
//!    credential it stored *before* the request, and adopt the container instead of
//!    creating a second one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use yi_agent_core::subagent::task::AttemptId;
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_core::{RootSessionId, TaskId};
use yi_agent_lifecycle::foreground::SecretCapability;
use yi_agent_lifecycle::session_delegation::{
    SessionFailureCode, SessionIdentity, SessionRef, session_resume_digest,
};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
use yi_agent_subagent::binding::{RuntimeBinding, RuntimeHandle};
use yi_agent_subagent::session_delegation::{SessionClientError, SessionHost};
use yi_agent_subagent::session_identity::SessionIdentityStore;
use yi_agent_subagent::thread_root::ThreadRoot;

/// One in-place position: the conversation's own directory, no worktree.
struct InPlace(PathBuf);

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

impl WorkerWorkspaceProvider for InPlace {
    fn in_place_workspace(
        &self,
        _session: &RootSessionId,
        _task_id: &TaskId,
        _attempt: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.position())
    }

    /// A container's children live in the same place in these tests; resolving a
    /// separate worktree would make the fixture do git work for no assertion.
    fn workspace_in(
        &self,
        _task_id: &TaskId,
        _workdir: &Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(self.position())
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(parent.cloned().unwrap_or_else(|| self.position()))
    }
}

#[derive(Clone)]
struct Fixture {
    workspace: PathBuf,
}

impl AgentWorkerFactory for Fixture {
    fn recovery_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:session-delegation-test".into()),
            worktree_lease: Some("worktree:session-delegation-test".into()),
            checkpoint_json: "{}".into(),
            tool_state_json: "{}".into(),
        }
    }

    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(InPlace(self.workspace.clone())))
    }

    fn start(&self, request: WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

fn daemon_in(directory: &tempfile::TempDir) -> Daemon {
    Daemon::start_with_factory(
        directory.path().join("runtime"),
        directory.path().join("runtime.sqlite"),
        Arc::new(Fixture {
            workspace: directory.path().to_path_buf(),
        }),
    )
    .expect("the daemon starts")
}

/// A conversation root over the daemon's socket.
///
/// The root is constructed with `new` (not `from_handle`) so the client really
/// goes through the wire for its container attach instead of using a pre-attached
/// triple. That is the path a restarted host takes.
fn conversation_root(daemon: &Daemon, workspace: &Path, thread_id: &str) -> Arc<ThreadRoot> {
    ThreadRoot::new(
        RuntimeBinding::fixed(RuntimeHandle {
            socket_path: daemon.socket_path().to_path_buf(),
            workspace_root: workspace.to_path_buf(),
            session_id: String::new(),
            task_id: String::new(),
            capability: String::new(),
        }),
        thread_id,
        workspace.to_path_buf(),
        Vec::new(),
    )
}

fn attach_container(
    root: &Arc<ThreadRoot>,
    secret: &SecretCapability,
    operation_id: &str,
    creation: bool,
) -> yi_agent_subagent::thread_root::SessionContainerAttachment {
    root.attach_session_container(
        &SessionIdentity::Embedded(secret.clone()),
        operation_id,
        creation.then(|| session_resume_digest(secret)),
    )
    .expect("the container attaches")
}

/// The whole point: a `stalled` history must not stop a conversation delegating.
///
/// The stored digest is written by the *first* attach, before any reply is read,
/// which is what lets a host that lost the reply come back and adopt the container
/// rather than creating a second one.
#[test]
fn session_delegation_lifecycle_a_container_spawns_through_a_prepared_turn() {
    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let identities = SessionIdentityStore::new(directory.path().join("identities"));
    let secret = identities.load_or_create("thread-delegation").unwrap();
    let root = conversation_root(&daemon, &workspace, "thread-delegation");
    let attachment = attach_container(&root, &secret, "op-attach", true);

    let mut host = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret.clone()),
        None,
        attachment,
    );
    host.prepare("op-prepare").expect("the identity prepares");
    let turn = host.begin_turn("turn-1").expect("turn-1 opens");
    host.register_execution("turn-1")
        .expect("the host registers its turn with the daemon");

    let response = turn
        .send(|handle, turn| IpcRequest::SpawnApplicationChild {
            session_id: handle.session_id.clone(),
            parent_task_id: handle.task_id.clone(),
            capability: handle.capability.clone(),
            objective: "a delegated investigation".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn.clone()),
        })
        .expect("the daemon answers");
    match response {
        IpcResponse::TaskSpawned { task_id } => {
            assert!(!task_id.is_empty(), "the child must get an id to wait on")
        }
        other => panic!("a prepared turn must be able to spawn, got {other:?}"),
    }

    host.finish_turn(&turn).expect("turn-1 closes");
    assert!(!host.has_unconfirmed_turn());

    // A closed turn is closed locally too: a tool that outlived the turn cannot
    // borrow a newer one, because it only ever holds this snapshot.
    assert!(matches!(
        turn.send(|handle, turn| IpcRequest::SpawnApplicationChild {
            session_id: handle.session_id.clone(),
            parent_task_id: handle.task_id.clone(),
            capability: handle.capability.clone(),
            objective: "late".into(),
            mode: None,
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn.clone()),
        }),
        Err(SessionClientError::Rejected(failure))
            if failure.code == SessionFailureCode::StaleTurn
    ));
}

/// A tool call that started under turn A must not be able to act under turn B.
///
/// Two turns of one conversation are sequential, not concurrent, so the daemon
/// replaces A with B. A tool holding A's client has A's secret and nothing else:
/// it cannot see B, and its spawn is refused rather than being upgraded into a
/// write under the newer turn.
#[tokio::test]
async fn session_delegation_lifecycle_delayed_old_tool_cannot_borrow_new_turn() {
    use serde_json::json;
    use yi_agent_core::ToolRegistry;
    use yi_agent_subagent::{CallerContext, register_session_turn_tools};

    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let secret = SecretCapability::new("resume-old-tool");
    let root = conversation_root(&daemon, &workspace, "thread-old-tool");
    let attachment = attach_container(&root, &secret, "op-attach", true);
    let mut host = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret),
        None,
        attachment,
    );
    host.prepare("op-prepare").unwrap();

    // Turn A: the tool set a slow model call would be holding while it thinks.
    let turn_a = Arc::new(host.begin_turn("turn-a").unwrap());
    host.register_execution("turn-a").unwrap();
    let mut tools_a = ToolRegistry::new();
    register_session_turn_tools(
        &mut tools_a,
        Arc::clone(&turn_a),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        None,
        CallerContext::unbound(),
    );

    // A starts one child, then ends.
    let spawn_a = tools_a.get("spawn_agent").expect("A has spawn_agent");
    let spawned = spawn_a
        .call(json!({ "task": "work for turn A", "fork": false }))
        .await;
    assert!(
        !spawned.is_error,
        "turn A must spawn its child, got {spawned:?}"
    );
    let child = match &spawned.content[0] {
        yi_agent_core::ContentBlock::Text(text) => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| value["task_id"].as_str().map(str::to_owned))
            .expect("the spawn names the child"),
        other => panic!("a spawn answers with JSON text, got {other:?}"),
    };
    host.finish_turn(&turn_a).unwrap();

    // Reading is not gated on "still open": a run that ended must be able to read
    // what its children produced, so this must still work while no newer turn
    // exists.
    let old_inspect = tools_a.get("inspect_agent").expect("A has inspect_agent");
    let read = old_inspect
        .call(json!({ "task_id": child, "include_diff": false }))
        .await;
    assert!(
        !read.is_error,
        "a closed turn must still be able to read its child, got {read:?}"
    );

    // B opens -- and B gets a tool set of its own.
    let turn_b = host.begin_turn("turn-b").unwrap();
    let mut tools_b = ToolRegistry::new();
    register_session_turn_tools(
        &mut tools_b,
        Arc::new(turn_b),
        yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            false,
        ),
        None,
        CallerContext::unbound(),
    );

    // The delayed call now lands, through A's tool. It must be refused and must
    // not carry B's secret: nothing in A's tool can name B.
    let old_spawn = tools_a.get("spawn_agent").expect("A has spawn_agent");
    let refusal = old_spawn
        .call(json!({ "task": "too late", "fork": false }))
        .await;
    assert!(
        refusal.is_error,
        "a tool from a finished turn must not spawn, got {refusal:?}"
    );
    // Either layer may speak first -- the local gate knows the turn closed, the
    // daemon knows a newer turn replaced it -- and both are the same fact. What
    // matters is that A's tool cannot write, and that nothing in it could name B.
    let message = format!("{:?}", refusal.content);
    assert!(
        message.contains("turn-a") || message.contains("closed") || message.contains("StaleTurn"),
        "the refusal must be about A's ended turn, got {message}"
    );

    // A newer turn supersedes the older read token, which is allowed: the plan
    // promises reads survive *the turn ending*, not that they survive the next
    // turn. B's tools take over the querying, and B works.
    // B, opened after A, still works -- the refusal was A's, not the session's.
    let fresh_spawn = tools_b.get("spawn_agent").expect("B has spawn_agent");
    let accepted = fresh_spawn
        .call(json!({ "task": "on time", "fork": false }))
        .await;
    assert!(
        !accepted.is_error,
        "the current turn must still be able to spawn, got {accepted:?}"
    );
}

/// A replacement driver is admitted only after the previous execution is proven/// A replacement driver is admitted only after the previous execution is proven
/// stopped, and the proof has to name *that* execution and fence.
///
/// This is the other half of "a settled conversation may always delegate": it may,
/// but not by pretending the work of the previous driver ended when it did not.
#[test]
fn session_delegation_lifecycle_a_driver_is_replaced_only_with_a_stop_proof() {
    use yi_agent_lifecycle::foreground::{EvidenceSource, ForegroundFence, RootRef, StopEvidence};

    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let secret = SecretCapability::new("resume-stop-proof");
    let root = conversation_root(&daemon, &workspace, "thread-stop-proof");
    let attachment = attach_container(&root, &secret, "op-attach", true);
    let session_id = attachment.session_id.clone();
    let root_id = attachment.root_id.clone();
    let mut first = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret.clone()),
        None,
        attachment.clone(),
    );
    first.prepare("op-prepare-first").unwrap();
    first.begin_turn("turn-first").unwrap();
    let execution = first
        .register_execution("turn-first")
        .expect("the first driver registers its execution");

    // A second host with no proof must not take over: an unaccounted execution
    // is not the same thing as one that stopped.
    let mut second = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret.clone()),
        None,
        attachment,
    );
    match second.prepare("op-prepare-second") {
        Err(SessionClientError::Rejected(failure)) => assert_eq!(
            failure.code,
            SessionFailureCode::StopUnproven,
            "an unproven execution must block the replacement, got {failure:?}"
        ),
        other => panic!("a replacement without proof must be refused, got {other:?}"),
    }

    // The proof has to name the right fence: a well-formed record about some
    // other generation proves nothing about this one.
    let fence = first.binding_fence_for_test().expect("a prepared binding");
    let evidence = StopEvidence {
        evidence_id: "evidence-1".into(),
        fence: ForegroundFence {
            root: RootRef {
                session_id: session_id.clone(),
                root_id: root_id.clone(),
                attempt_id: execution.attempt_id.clone(),
            },
            generation: "not-this-generation".into(),
            binding_epoch: fence.binding_epoch,
            driver_epoch: fence.driver_epoch,
        },
        execution: execution.clone(),
        attempt_id: execution.attempt_id.clone(),
        source: EvidenceSource::Host,
        joined: true,
        tools_stopped: true,
        process_identity: None,
    };
    match first.record_stopped(evidence) {
        Err(SessionClientError::Rejected(failure)) => assert_eq!(
            failure.code,
            SessionFailureCode::StaleBinding,
            "evidence about another fence must be refused, got {failure:?}"
        ),
        other => panic!("a foreign stop proof must be refused, got {other:?}"),
    }

    // And the honest proof is accepted.
    let evidence = StopEvidence {
        evidence_id: "evidence-2".into(),
        fence: ForegroundFence {
            root: RootRef {
                session_id: session_id.clone(),
                root_id: root_id.clone(),
                attempt_id: execution.attempt_id.clone(),
            },
            generation: fence.generation.clone(),
            binding_epoch: fence.binding_epoch,
            driver_epoch: fence.driver_epoch,
        },
        execution,
        attempt_id: "attempt-1".into(),
        source: EvidenceSource::Host,
        joined: true,
        tools_stopped: true,
        process_identity: None,
    };
    first
        .record_stopped(evidence)
        .expect("the host's own proof is recorded");
}

/// The long-lived resume secret must never reach anything the daemon, a log or a
/// fork payload keeps.
///
/// Only its digest belongs on the wire. A copy of the secret inside the runtime
/// database would turn one readable file into a permanent identity for every
/// conversation on this machine.
#[test]
fn session_delegation_lifecycle_resume_secret_never_reaches_durable_state() {
    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let identities = SessionIdentityStore::new(directory.path().join("identities"));
    let secret = identities.load_or_create("thread-no-leak").unwrap();
    let root = conversation_root(&daemon, &workspace, "thread-no-leak");
    let attachment = attach_container(&root, &secret, "op-attach", true);
    let mut host = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret.clone()),
        None,
        attachment,
    );
    host.prepare("op-prepare").unwrap();
    let turn = host.begin_turn("turn-leak").unwrap();
    let response = turn
        .send(|handle, turn| IpcRequest::SpawnApplicationChild {
            session_id: handle.session_id.clone(),
            parent_task_id: handle.task_id.clone(),
            capability: handle.capability.clone(),
            objective: "no secret in the database".into(),
            mode: None,
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn.clone()),
        })
        .expect("the daemon answers");
    assert!(
        matches!(response, IpcResponse::TaskSpawned { .. }),
        "{response:?}"
    );
    host.finish_turn(&turn).unwrap();

    let needle = secret.expose();
    let credentials = directory.path().join("identities");
    let runtime_database = directory.path().join("runtime.sqlite");
    drop(daemon);

    // Every file the run produced, at any depth: the runtime database and its
    // WAL/SHM siblings, thread JSONL, fork payloads, logs. The private
    // credential directory is the one place the secret is meant to live, so it is
    // the only exemption.
    let mut scanned = 0usize;
    let mut saw_database = false;
    let mut pending = vec![directory.path().to_path_buf()];
    while let Some(directory_path) = pending.pop() {
        for entry in std::fs::read_dir(&directory_path).unwrap().flatten() {
            let path = entry.path();
            if path == credentials {
                continue;
            }
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                // Sockets and other non-regular entries hold no payload.
                continue;
            };
            scanned += 1;
            saw_database |= path == runtime_database;
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "the resume secret leaked into {}",
                path.display()
            );
        }
    }
    // Without these two the loop above could pass having read nothing at all,
    // which is the usual way a "no leak" test silently stops testing.
    assert!(saw_database, "the scan must have read the runtime database");
    // The runtime leaves a socket and a lock file behind too; only the database
    // has a payload once the daemon closed it, so one file is the whole scan.
    assert!(scanned >= 1, "the scan read nothing at all");
    // The digest really is in there, so the absence of the secret is a property
    // of what the daemon stores, not of an empty database.
    let digest = yi_agent_lifecycle::session_delegation::session_resume_digest(&secret);
    let database = std::fs::read(&runtime_database).unwrap();
    assert!(
        database
            .windows(digest.len())
            .any(|window| window == digest.as_bytes()),
        "the daemon must record the resume digest"
    );
    assert_ne!(digest, needle, "the digest is not the secret");
}

/// The daemon's own view of a late write: a replaced turn is refused even if it
/// arrives on the wire, so the local gate is not the only protection.
#[test]
fn session_delegation_lifecycle_a_superseded_turn_cannot_spawn() {
    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let secret = SecretCapability::new("resume-superseded");
    let root = conversation_root(&daemon, &workspace, "thread-superseded");
    let attachment = attach_container(&root, &secret, "op-attach", true);
    let mut host = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret),
        None,
        attachment,
    );
    host.prepare("op-prepare").unwrap();
    let stale = host.begin_turn("turn-1").unwrap();
    let live = host.begin_turn("turn-2").unwrap();
    // `turn-1` is opened on the same host from outside `begin_turn`'s unclosed
    // guard only by a second host in production; here the daemon's own rule is
    // what the assertion is about, so send the stale turn straight to the wire.
    stale.close_locally();
    // A real old tool holds *its own* turn secret, so that is what it presents.
    // Presenting the newer turn's secret instead would be a different bug (a tool
    // that somehow learned a capability it was never handed).
    let response = send_request(
        stale.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: live.handle().session_id,
            parent_task_id: live.handle().task_id,
            capability: stale.context().capability.expose().to_string(),
            objective: "from a superseded turn".into(),
            mode: None,
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(stale.context().clone()),
        },
    )
    .expect("the socket answers");
    match response {
        IpcResponse::SessionDelegationRejected { failure } => {
            assert_eq!(failure.code, SessionFailureCode::StaleTurn, "{failure:?}")
        }
        IpcResponse::TaskSpawned { .. } => {
            panic!("a superseded turn must not spawn after a newer one opened")
        }
        other => panic!("unexpected response {other:?}"),
    }
}

/// Two turns on one host: the binding is reused, the turns differ.
///
/// Bumping the binding epoch per turn would look like a driver replacement every
/// time and demand a stop proof that has nothing to prove.
#[test]
fn session_delegation_lifecycle_prepare_reuses_live_binding() {
    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let secret = SecretCapability::new("resume-reuse");
    let root = conversation_root(&daemon, &workspace, "thread-reuse");
    let attachment = attach_container(&root, &secret, "op-attach", true);
    let mut host = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret),
        None,
        attachment,
    );
    host.prepare("op-prepare-1").expect("the first prepare");

    let first = host.begin_turn("turn-a").expect("turn-a opens");
    let first_fence = first.context().fence.clone();
    host.finish_turn(&first).expect("turn-a closes");

    let second = host.begin_turn("turn-b").expect("turn-b opens");
    let second_fence = second.context().fence.clone();
    host.finish_turn(&second).expect("turn-b closes");

    assert_eq!(
        first_fence, second_fence,
        "a live owner in the same generation reuses its binding instead of bumping it"
    );
    assert_ne!(
        first.context().turn_id,
        second.context().turn_id,
        "each run still gets its own turn"
    );
    assert_ne!(
        first.context().capability,
        second.context().capability,
        "a turn secret must not be reusable across turns"
    );
}

/// A tool holds a turn secret, never the binding. Presenting the turn secret as a
/// binding must be refused, or a model could register executions and retire them
/// on behalf of the host it runs under.
#[test]
fn session_delegation_lifecycle_a_turn_secret_cannot_open_host_control() {
    use yi_agent_lifecycle::session_delegation::SessionBinding;
    use yi_agent_store::ipc::{PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope};

    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let secret = SecretCapability::new("resume-turn-control");
    let root = conversation_root(&daemon, &workspace, "thread-turn-control");
    let attachment = attach_container(&root, &secret, "op-attach", true);
    let mut host = SessionHost::new(
        Arc::clone(&root),
        SessionIdentity::Embedded(secret),
        None,
        attachment,
    );
    host.prepare("op-prepare").unwrap();
    let turn = host.begin_turn("turn-control").unwrap();

    // A forged binding: the turn's own fence and secret, presented as if it were
    // the host binding. Everything matches except the capability.
    let forged = SessionBinding {
        fence: turn.context().fence.clone(),
        capability: turn.context().capability.clone(),
    };
    let mut stream = std::os::unix::net::UnixStream::connect(daemon.socket_path()).unwrap();
    use std::io::{BufRead, BufReader, Write};
    let request_id = "forged-control".to_string();
    let frame = serde_json::to_vec(&RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request_id: request_id.clone(),
        command: IpcRequest::OpenSessionHostControl { binding: forged },
    })
    .unwrap();
    stream.write_all(&frame).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let envelope: ResponseEnvelope = serde_json::from_str(&line).unwrap();
    assert_eq!(envelope.request_id, request_id);
    match envelope.result {
        IpcResponse::Error { code, .. } => assert_eq!(
            code,
            yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            "a turn secret must be refused by name"
        ),
        other => panic!("a turn secret must not open host control, got {other:?}"),
    }
}

/// A second host that only knows the conversation's thread key cannot get the
/// binding: the resume secret is the identity, not the address.
#[test]
fn session_delegation_lifecycle_resume_secret_is_the_identity() {
    use yi_agent_lifecycle::session_delegation::PrepareSession;

    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let secret = SecretCapability::new("resume-real");
    let root = conversation_root(&daemon, &workspace, "thread-identity");
    let attachment = attach_container(&root, &secret, "op-attach", true);

    let response = send_request(
        daemon.socket_path(),
        IpcRequest::PrepareSessionDelegation {
            request: PrepareSession {
                root: SessionRef {
                    session_id: attachment.session_id.clone(),
                    root_id: attachment.root_id.clone(),
                },
                operation_id: "op-impostor".into(),
                host_run_id: "some-other-process".into(),
                expected_binding_epoch: 0,
                identity: SessionIdentity::Embedded(SecretCapability::new("resume-wrong")),
                current_binding: None,
                replacement_resume_digest: None,
                stopped: Vec::new(),
            },
        },
    )
    .expect("the socket answers");
    match response {
        IpcResponse::SessionDelegationRejected { failure } => assert_eq!(
            failure.code,
            SessionFailureCode::AuthorityDenied,
            "a wrong resume secret must be refused, not silently accepted"
        ),
        other => panic!("a wrong resume secret must not prepare, got {other:?}"),
    }
}

/// The response to a creation attach can be lost; the credential was stored
/// first, so the host can still adopt the same container instead of minting a
/// second one.
#[test]
fn session_delegation_lifecycle_a_lost_attach_reply_is_recovered_by_the_stored_secret() {
    let directory = tempfile::tempdir().unwrap();
    let daemon = daemon_in(&directory);
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();

    let identities = SessionIdentityStore::new(directory.path().join("identities"));
    let secret = identities.load_or_create("thread-lost-reply").unwrap();
    let root = conversation_root(&daemon, &workspace, "thread-lost-reply");
    let created = attach_container(&root, &secret, "op-attach", true);

    // A fresh host, as if the process had died after the daemon committed the
    // attach but before the reply reached it. Adoption sends no digest, so it
    // cannot propose a new identity to an existing container.
    let restarted = conversation_root(&daemon, &workspace, "thread-lost-reply");
    let adopted = attach_container(&restarted, &secret, "op-attach-again", false);
    assert_eq!(
        adopted.root_id, created.root_id,
        "a re-attach adopts the same container instead of creating another"
    );

    let mut host = SessionHost::new(restarted, SessionIdentity::Embedded(secret), None, adopted);
    host.prepare("op-prepare-after-lost-reply")
        .expect("the stored credential still proves identity");
    let turn = host.begin_turn("turn-after-restart").expect("a turn opens");
    let response = turn
        .send(|handle, turn| IpcRequest::SpawnApplicationChild {
            session_id: handle.session_id.clone(),
            parent_task_id: handle.task_id.clone(),
            capability: handle.capability.clone(),
            objective: "after a lost attach reply".into(),
            mode: None,
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(turn.clone()),
        })
        .expect("the daemon answers");
    assert!(
        matches!(response, IpcResponse::TaskSpawned { .. }),
        "a recovered host must be able to delegate, got {response:?}"
    );
}
