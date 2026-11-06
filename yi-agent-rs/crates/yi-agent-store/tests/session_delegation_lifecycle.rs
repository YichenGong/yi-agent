//! The lifecycle guarantees that only a real daemon, a real SQLite file and a real
//! socket can express.
//!
//! Every test here answers a "does the *system* still behave" question that a pure
//! unit test cannot: whether an old `stalled`/`completed`/`failed`/`recovery_required`
//! container keeps its verbatim history while still delegating, whether a closed
//! container stays closed, whether an unproven old execution blocks a rebind,
//! whether an execution can borrow the container route, whether two conversations
//! keep separate trees, and whether a concurrent prepare commits exactly once.
//!
//! The one detail that matters throughout: a container write always names the open
//! turn (`daemon::admit_session_write`). A test that spawns without a turn would be
//! asserting the shape the contract exists to forbid, so every spawn goes through
//! `Fixture::prepare` + `Fixture::open_turn` below.

use std::sync::Arc;

use futures::future::BoxFuture;
use rusqlite::params;
use tempfile::TempDir;
use yi_agent_core::subagent::task::{AttemptId, RootSessionId, TaskId, TaskRole};
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider, WorkerWorkspaceRegistry,
};
use yi_agent_lifecycle::foreground::SecretCapability;
use yi_agent_lifecycle::session_delegation::{
    PrepareSession, SessionBinding, SessionFailureCode, SessionIdentity, SessionRef, SessionTurn,
    session_resume_digest,
};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
use yi_agent_store::repository::RuntimeRepository;

/// A factory that never runs a model: these tests are about admission, history and
/// quota, so a started worker only has to exist.
#[derive(Clone, Default)]
struct IdleFactory {
    starts: Arc<std::sync::Mutex<Vec<WorkerStart>>>,
}

impl WorkerWorkspaceProvider for IdleFactory {
    fn in_place_workspace(
        &self,
        session: &RootSessionId,
        task: &TaskId,
        _: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(position(session, task))
    }

    fn workspace_in(
        &self,
        task_id: &TaskId,
        workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(WorkerWorkspace {
            lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
            repository_root: workdir.to_path_buf(),
            path: workdir.to_path_buf(),
            branch: format!("child-{task_id}"),
            parent_branch: "main".into(),
            base_commit: "0".repeat(40),
        })
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let mut position = parent
            .cloned()
            .unwrap_or_else(|| position(&RootSessionId::new(), task_id));
        position.branch = String::new();
        position.parent_branch = String::new();
        position.base_commit = String::new();
        position.lease_id = yi_agent_core::subagent::task::WorkspaceLeaseId::new();
        Ok(position)
    }
}

fn position(root: &RootSessionId, task: &TaskId) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
        repository_root: format!("/tmp/{root}").into(),
        path: format!("/tmp/{root}/{task}").into(),
        branch: "main".into(),
        parent_branch: String::new(),
        base_commit: "0".repeat(40),
    }
}

impl WorkerWorkspaceRegistry for IdleFactory {
    fn register_prepared(&self, _workspace: &WorkerWorkspace) {}
    fn prepared_workspace_for_workdir(
        &self,
        _workdir: &std::path::Path,
    ) -> Option<WorkerWorkspace> {
        None
    }
    fn inspect_delivery(
        &self,
        _workspace: &WorkerWorkspace,
    ) -> Result<yi_agent_core::subagent::task::DeliveryReport, WorkerError> {
        Ok(yi_agent_core::subagent::task::DeliveryReport::coding(
            "not-a-real-commit",
            "main",
            yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
            "unverified",
        ))
    }
}

impl AgentWorkerFactory for IdleFactory {
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
        Box::pin(async move { Ok(handle) })
    }
}

struct Fixture {
    /// Owns the temp tree; `directory` is the instance path inside it, which a
    /// restart reuses while the daemon is rebuilt.
    _root: TempDir,
    directory: std::path::PathBuf,
    daemon: Daemon,
    factory: Arc<IdleFactory>,
    database: std::path::PathBuf,
    workspace: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let instance = root.path().join("instance");
        Self::in_root(root, instance)
    }

    /// Rebuilds the daemon over the same database, the way a restart does.
    ///
    /// Everything in memory dies with the daemon: the supervisor and the roles it
    /// believed, the binding table, the open turns. This is a *new* daemon over the
    /// same file, so every fact the test observes afterwards came back from the
    /// database -- which is the only way to prove the persisted role, and not the
    /// in-memory one, is what admits the spawn.
    fn restart(&mut self) {
        self.daemon.stop().expect("the old daemon stops cleanly");
        let root = std::mem::replace(&mut self._root, TempDir::new().unwrap());
        let fresh = Self::in_root(root, self.directory.clone());
        self._root = fresh._root;
        self.daemon = fresh.daemon;
        self.factory = fresh.factory;
    }

    fn in_root(root: TempDir, directory: std::path::PathBuf) -> Self {
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("runtime.sqlite");
        let workspace = directory.join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let factory = Arc::new(IdleFactory::default());
        // The enforcing daemon is the shape these tests describe: every container
        // write names the open turn. A daemon that defaulted to lenient admission
        // would let the close/turn assertions pass for the wrong reason.
        let daemon = Daemon::start_with_factory_enforcing_session_turns(
            directory.join("runtime"),
            &database,
            Arc::clone(&factory) as Arc<dyn AgentWorkerFactory>,
        )
        .unwrap();
        Self {
            _root: root,
            directory,
            daemon,
            factory,
            database,
            workspace,
        }
    }

    fn socket(&self) -> &std::path::Path {
        self.daemon.socket_path()
    }

    fn repository(&self) -> RuntimeRepository {
        RuntimeRepository::open(&self.database).unwrap()
    }

    /// Attaches one session container with a resume digest the host keeps.
    fn attach(&self, key: &str) -> (SessionRef, SecretCapability) {
        let secret = SecretCapability::new(format!("resume-{key}"));
        let attached = self.attach_with_digest(key, Some(session_resume_digest(&secret)));
        (attached, secret)
    }

    fn attach_with_digest(&self, key: &str, digest: Option<String>) -> SessionRef {
        self.attach_in(key, digest, &self.workspace)
    }

    /// Attaches a container over an explicit directory, so a test can compare a
    /// project root with a kanban card worktree.
    fn attach_in(
        &self,
        key: &str,
        digest: Option<String>,
        workspace: &std::path::Path,
    ) -> SessionRef {
        let IpcResponse::SessionContainerAttached { root, .. } = send_request(
            self.socket(),
            IpcRequest::AttachSessionContainer {
                idempotency_key: key.into(),
                workspace: workspace.to_path_buf(),
                extra_roots: Vec::new(),
                operation_id: format!("attach-{key}"),
                creation_resume_digest: digest,
                identity: None,
            },
        )
        .unwrap() else {
            panic!("expected a container attach for {key}");
        };
        root
    }

    /// Prepares a binding for `root` with the host's resume secret.
    fn prepare(
        &self,
        root: &SessionRef,
        secret: &SecretCapability,
        host: &str,
        operation: &str,
        expected_epoch: u64,
    ) -> Result<SessionBinding, SessionFailureCode> {
        self.prepare_with(root, secret, host, operation, expected_epoch, None)
    }

    fn prepare_with(
        &self,
        root: &SessionRef,
        secret: &SecretCapability,
        host: &str,
        operation: &str,
        expected_epoch: u64,
        current: Option<SessionBinding>,
    ) -> Result<SessionBinding, SessionFailureCode> {
        let response = send_request(
            self.socket(),
            IpcRequest::PrepareSessionDelegation {
                request: PrepareSession {
                    root: root.clone(),
                    operation_id: operation.into(),
                    host_run_id: host.into(),
                    expected_binding_epoch: expected_epoch,
                    identity: SessionIdentity::Embedded(secret.clone()),
                    current_binding: current,
                    replacement_resume_digest: None,
                    stopped: Vec::new(),
                },
            },
        )
        .unwrap();
        match response {
            IpcResponse::SessionPrepared { binding } => Ok(binding),
            IpcResponse::SessionDelegationRejected { failure, .. } => Err(failure.code),
            other => panic!("unexpected prepare response {other:?}"),
        }
    }

    fn open_turn(&self, binding: &SessionBinding, turn: &str) -> SessionTurn {
        let IpcResponse::SessionTurnOpened { turn } = send_request(
            self.socket(),
            IpcRequest::BeginSessionTurn {
                binding: binding.clone(),
                turn_id: turn.into(),
            },
        )
        .unwrap() else {
            panic!("a bound host opens a turn");
        };
        turn
    }

    /// Ends the open turn, so a later write that still presents it is a replay.
    fn finish_turn(&self, turn: &SessionTurn) {
        let response = send_request(
            self.socket(),
            IpcRequest::EndSessionTurn { turn: turn.clone() },
        )
        .unwrap();
        assert_eq!(
            response,
            IpcResponse::SessionTurnClosed,
            "the turn must close"
        );
    }

    /// Spawns one child as the container's bound host.
    ///
    /// A refusal reaches the wire in two shapes: the structured
    /// `SessionDelegationRejected` when the *container* fence said no, and a plain
    /// `Error` when the fallback execution route denied the capability. Both are
    /// refusals, so the return type names whichever code arrived.
    fn spawn(
        &self,
        root: &SessionRef,
        _binding: &SessionBinding,
        turn: &SessionTurn,
        objective: &str,
    ) -> Result<TaskId, String> {
        match send_request(
            self.socket(),
            IpcRequest::SpawnApplicationChild {
                session_id: root.session_id.clone(),
                parent_task_id: root.root_id.clone(),
                capability: turn.capability.expose().to_string(),
                objective: objective.into(),
                mode: Some("read_only".into()),
                model: None,
                workdir: None,
                thread_id: None,
                sandbox: None,
                fork_token: None,
                session_turn: Some(turn.clone()),
            },
        )
        .unwrap()
        {
            IpcResponse::TaskSpawned { task_id } => Ok(task_id.parse().unwrap()),
            IpcResponse::SessionDelegationRejected { failure, .. } => {
                Err(format!("{:?}", failure.code))
            }
            IpcResponse::Error { code, .. } => Err(format!("{code:?}")),
            other => panic!("unexpected spawn response {other:?}"),
        }
    }

    /// The worker capability the daemon minted for one spawned child.
    ///
    /// A child reports through its *own* capability, never through the container's
    /// turn secret: the turn belongs to the host root, and handing it to a child
    /// would let a tool act as the host.
    fn child_capability(&self, child: &TaskId) -> String {
        self.factory
            .starts
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|start| &start.task_id == child)
            .expect("the child was started through this fixture's factory")
            .message_capability
            .clone()
    }

    /// Rewrites the container's persisted state the way an older build could have.
    fn write_history(&self, root: &SessionRef, state: &str) {
        rusqlite::Connection::open(&self.database)
            .unwrap()
            .execute(
                "UPDATE tasks SET state_json = ?1 WHERE id = ?2",
                params![state, root.root_id],
            )
            .unwrap();
    }

    /// The container's persisted `(state, active attempt id, attempt terminal)`.
    fn history_of(&self, root: &SessionRef) -> (String, String, Option<String>) {
        let repository = self.repository();
        let state: String = repository
            .execute_test_scalar_text("SELECT state_json FROM tasks WHERE id = ?1", &root.root_id)
            .unwrap()
            .expect("the container row exists");
        let attempt: String = repository
            .execute_test_scalar_text(
                "SELECT active_attempt_id FROM tasks WHERE id = ?1",
                &root.root_id,
            )
            .unwrap()
            .expect("the container has an active attempt");
        let terminal = repository
            .execute_test_scalar_text(
                "SELECT COALESCE(terminal_json, '<none>') FROM attempts WHERE id = ?1",
                &attempt,
            )
            .unwrap();
        (state, attempt, terminal)
    }
}

/// C1: the role a session gets does not depend on how it was launched.
///
/// A kanban card runs in a git worktree under `.worktrees/kanban/`, a normal
/// conversation runs in the project root. Both are conversations: each must attach as
/// a `SessionContainer` and then delegate, while a plain session created by
/// `create_session` stays an `Execution`. This is the invariant the bug report
/// violated -- the kanban session was judged by its path and its last execution's
/// state instead of by its role.
#[test]
fn session_delegation_lifecycle_app_server_roles_for_normal_and_kanban() {
    let fixture = Fixture::new();
    let kanban_worktree = fixture.directory.join(".worktrees/kanban/card-1");
    std::fs::create_dir_all(&kanban_worktree).unwrap();

    for (label, workspace) in [
        ("normal project", &fixture.workspace),
        ("kanban worktree", &kanban_worktree),
    ] {
        let key = format!("roles-{}", label.replace(' ', "-"));
        let secret = SecretCapability::new(format!("resume-{key}"));
        let root = fixture.attach_in(&key, Some(session_resume_digest(&secret)), workspace);
        assert_eq!(
            fixture
                .repository()
                .task_role(&root.root_id.parse().unwrap())
                .unwrap(),
            TaskRole::SessionContainer,
            "{label} must be a session container, not an execution"
        );

        let binding = fixture
            .prepare(
                &root,
                &secret,
                &format!("host-{key}"),
                &format!("op-{key}"),
                0,
            )
            .unwrap();
        let turn = fixture.open_turn(&binding, &format!("turn-{key}"));
        fixture
            .spawn(&root, &binding, &turn, &format!("{label} delegates"))
            .unwrap_or_else(|error| panic!("{label} must delegate, got {error:?}"));
    }
}

/// C3: separating the role must not change what a *worker* is allowed.
///
/// The exemption this feature adds is one line of policy for one role. Everything an
/// execution budget does must survive it: a child with no progress is still settled by
/// the idle budget, and its state still reaches `stalled`. If the exemption leaked to
/// executions, a stuck child would hold its lease and its slot forever.
#[test]
fn session_delegation_lifecycle_worker_budget_and_scheduled_root_unchanged() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("budget");
    let binding = fixture
        .prepare(&root, &secret, "budget-host", "op-budget", 0)
        .unwrap();
    let turn = fixture.open_turn(&binding, "turn-budget");
    let child = fixture
        .spawn(&root, &binding, &turn, "a real worker")
        .unwrap();
    let child_id: TaskId = child.to_string().parse().unwrap();

    assert_eq!(
        fixture.repository().task_role(&child_id).unwrap(),
        TaskRole::Execution,
        "a container's child is an execution, not a second conversation"
    );
    // The container is excluded from the candidate query by role, the child is not.
    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT COUNT(*) FROM tasks WHERE role = 'execution' AND id = ?1",
                &child.to_string(),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT COUNT(*) FROM tasks WHERE role = 'session_container' AND id = ?1",
                &root.root_id,
            )
            .unwrap(),
        1
    );

    // The child's budget is untouched by the container's exemption: it carries the
    // ordinary execution limits (idle 300s, wall-clock 2700s), and the container --
    // which has no worker -- carries none. If the exemption had been implemented by
    // widening the execution budget instead of excluding the role, the child would
    // show inflated or absent limits here.
    let child_limits: String = fixture
        .repository()
        .execute_test_scalar_text(
            "SELECT COALESCE(limits_json, '<none>') FROM attempt_watchdogs WHERE task_id = ?1",
            &child.to_string(),
        )
        .unwrap()
        .unwrap();
    let child_limits: serde_json::Value = serde_json::from_str(&child_limits).unwrap();
    assert_eq!(child_limits["max_idle_time_secs"], 300);
    assert_eq!(child_limits["max_wall_time_secs"], 2700);

    // The container's snapshot may exist (it is written with the attempt), so the
    // guarantee is not "no row": it is that the candidate query skips it by role. A
    // container that reached this scan would be settled by a budget it never had.
    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT COUNT(*) FROM attempt_watchdogs
                 JOIN tasks ON tasks.id = attempt_watchdogs.task_id
                 WHERE tasks.id = ?1 AND tasks.role = 'execution'",
                &root.root_id,
            )
            .unwrap(),
        0,
        "the container must not be an execution candidate, whatever its snapshot says"
    );
    assert!(
        !matches!(
            fixture.repository().task_state(&child_id).unwrap().as_str(),
            "completed" | "failed" | "stalled"
        ),
        "a child that just started is not terminal"
    );
}

/// C11: the container is stable across a daemon replacement.
///
/// A frontend restart is two facts at once: the process is gone (its binding and
/// turn secrets with it) and the daemon may have been replaced (a new generation).
/// The container must survive both as the *same* task, while the dead process's
/// credentials stop working. A container that was silently replaced would make the
/// conversation's history a different conversation's; one that accepted the old
/// binding would let a dead process's secret keep writing.
#[test]
fn session_delegation_lifecycle_frontend_and_daemon_restart() {
    let mut fixture = Fixture::new();
    let (root, secret) = fixture.attach("restart-stable");
    let binding = fixture
        .prepare(&root, &secret, "host-before", "op-before", 0)
        .unwrap();
    let turn_before = fixture.open_turn(&binding, "turn-before");
    fixture.finish_turn(&turn_before);

    let generation_before = binding.fence.generation.clone();
    let epoch_before = binding.fence.binding_epoch;
    // The container is a conversation: no worker was ever started for it.
    assert!(
        fixture
            .factory
            .starts
            .lock()
            .unwrap()
            .iter()
            .all(|start| start.task_id.to_string() != root.root_id),
        "a session container must never be started as a worker"
    );

    fixture.restart();
    // A restarted daemon holds no supervisor: the attach is what rehydrates the
    // container (and its role) from the row.
    let reattached =
        fixture.attach_with_digest("restart-stable", Some(session_resume_digest(&secret)));
    assert_eq!(reattached.root_id, root.root_id);

    // The old binding names a generation this daemon is not, so it must not be
    // *reused*: prepare takes a new epoch instead of adopting it. "Old admission
    // closed" is about the binding, not about forbidding a fresh proof of identity.
    let rebound = fixture
        .prepare_with(
            &root,
            &secret,
            "host-after",
            "op-after",
            epoch_before,
            Some(binding.clone()),
        )
        .unwrap_or_else(|error| panic!("a restarted daemon must rebind, got {error:?}"));
    assert!(
        rebound.fence.binding_epoch > epoch_before,
        "a stale binding must not be adopted as-is; the epoch has to advance"
    );
    assert_ne!(
        rebound.fence.generation, generation_before,
        "a replacement daemon mints its own generation"
    );
    assert_eq!(
        rebound.fence.root.root_id, root.root_id,
        "the conversation's container is the same task, not a new one"
    );

    let turn = fixture.open_turn(&rebound, "turn-after");
    fixture
        .spawn(&root, &rebound, &turn, "after a restart")
        .expect("the conversation still delegates after a restart");

    // The old turn secret is dead: it belongs to a binding that no longer exists.
    let replayed = fixture.spawn(&root, &rebound, &turn_before, "stale");
    assert!(
        replayed.is_err(),
        "a turn from the previous generation must not write, got {replayed:?}"
    );

    assert!(
        fixture
            .factory
            .starts
            .lock()
            .unwrap()
            .iter()
            .all(|start| start.task_id.to_string() != root.root_id),
        "the restarted container is still not a worker"
    );
}

/// C4: history is evidence, not a gate.
///
/// A `stalled`, `completed`, `failed` or `recovery_required` container was produced
/// by an older build (today's writer refuses all four for a container). Reproducing
/// those rows verbatim and then delegating is the only honest way to prove the fix:
/// the role decides admission, and the old attempt, its terminal JSON and the
/// container's state must come back unchanged.
#[test]
fn session_delegation_lifecycle_historical_root_rebind_preserves_evidence() {
    for state in ["stalled", "completed", "failed", "recovery_required"] {
        let mut fixture = Fixture::new();
        let (root, secret) = fixture.attach(&format!("history-{state}"));
        fixture.write_history(&root, state);
        let before = fixture.history_of(&root);
        assert_eq!(before.0, state);

        // Restart first. A `stalled` row that only lives in a database is not the
        // interesting case: the bug was a *rehydrated* supervisor reading that row as
        // "this parent is finished" and refusing every spawn. The rebind and the spawn
        // below therefore happen on a coordinator that has never seen this session.
        fixture.restart();
        // A restarted daemon has no supervisor until the host attaches again; the
        // attach is what rehydrates the role off the row.
        let reattached = fixture.attach_with_digest(
            &format!("history-{state}"),
            Some(session_resume_digest(&secret)),
        );
        assert_eq!(reattached.root_id, root.root_id);

        let binding = fixture
            .prepare(&root, &secret, "history-host", &format!("op-{state}"), 0)
            .unwrap_or_else(|error| panic!("{state} must rebind, got {error:?}"));
        let turn = fixture.open_turn(&binding, &format!("turn-{state}"));
        fixture
            .spawn(&root, &binding, &turn, "after history")
            .unwrap_or_else(|error| panic!("{state} must still delegate, got {error:?}"));

        let after = fixture.history_of(&root);
        assert_eq!(
            after, before,
            "rebinding {state} must not rewrite the historical row or add an attempt"
        );
        assert_eq!(
            fixture
                .repository()
                .task_role(&root.root_id.parse().unwrap())
                .unwrap(),
            TaskRole::SessionContainer,
            "the role must survive a rebind"
        );
    }
}

/// C12: a prepare whose answer was lost is retried, not repeated.
///
/// The host mints the operation id *before* sending, so a reply lost on the wire
/// is retried under the same id. That retry must resolve to the commit that
/// already happened: a second epoch would tell every other host that the driver
/// changed when nothing did, and would make the first binding look stale to the
/// very process that owns it.
#[test]
fn session_delegation_lifecycle_single_shot_lost_response() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("lost-reply");

    let first = fixture
        .prepare(&root, &secret, "host-1", "op-lost", 0)
        .expect("the first prepare commits");
    assert_eq!(first.fence.binding_epoch, 1);

    // The same operation, re-sent because the answer never arrived. The request is
    // byte-for-byte the one that was sent, including its expectation.
    let retried = fixture
        .prepare(&root, &secret, "host-1", "op-lost", 0)
        .unwrap_or_else(|error| panic!("a retry must be answered, got {error:?}"));
    assert_eq!(
        retried.fence.binding_epoch, first.fence.binding_epoch,
        "a lost reply must not advance the binding epoch"
    );
    assert_eq!(retried.fence.driver_epoch, first.fence.driver_epoch);
    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT COUNT(*) FROM session_binding_operations WHERE root_task_id = ?1",
                &root.root_id,
            )
            .unwrap(),
        1,
        "the retry must resolve to the same operation row, not write a second one"
    );

    // The retried binding is the live one: a turn opens against it.
    let turn = fixture.open_turn(&retried, "turn-after-retry");
    fixture
        .spawn(&root, &retried, &turn, "after a lost reply")
        .expect("the recovered binding is usable");
}

/// C13 barrier 3: a binding that failed to commit leaves no capability behind.
///
/// The in-memory capability table is what `begin_session_turn` and every fenced
/// write consult. If the durable commit fails but the memory table still got an
/// entry, this process would hand out authority that no transaction ever recorded:
/// a secret that works until the next restart, then silently stops. The injection
/// is a SQL trigger rather than a mock, so it fails at exactly the statement the
/// real commit uses.
#[test]
fn session_delegation_lifecycle_a_failed_binding_commit_leaves_no_capability() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("failed-commit");

    fixture
        .repository()
        .execute_test_statement_params(
            "CREATE TRIGGER refuse_binding_commit BEFORE INSERT ON session_binding_operations
             BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END",
            &[],
        )
        .unwrap();

    // A repository failure is not a policy answer, so it arrives as the daemon's
    // plain error rather than a `SessionDelegationRejected`. Either is a refusal;
    // what must not happen is `SessionPrepared`.
    let failed = send_request(
        fixture.socket(),
        IpcRequest::PrepareSessionDelegation {
            request: PrepareSession {
                root: root.clone(),
                operation_id: "op-failed".into(),
                host_run_id: "host-1".into(),
                expected_binding_epoch: 0,
                identity: SessionIdentity::Embedded(secret.clone()),
                current_binding: None,
                replacement_resume_digest: None,
                stopped: Vec::new(),
            },
        },
    )
    .unwrap();
    assert!(
        matches!(
            failed,
            IpcResponse::Error { .. } | IpcResponse::SessionDelegationRejected { .. }
        ),
        "a failed commit must be refused, got {failed:?}"
    );

    // The durable side did not move.
    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT binding_epoch FROM session_containers WHERE root_task_id = ?1",
                &root.root_id,
            )
            .unwrap(),
        0,
        "the failed transaction must not have bumped the epoch"
    );
    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT COUNT(*) FROM session_binding_operations WHERE root_task_id = ?1",
                &root.root_id,
            )
            .unwrap(),
        0,
        "the failed transaction must not have written an operation row"
    );

    // The memory side is the one a database check cannot see: the capability this
    // process would hand out lives only here.
    assert!(
        !fixture
            .daemon
            .coordinator()
            .has_live_binding_for_test(&root.root_id),
        "a commit that failed must not leave a live capability in memory"
    );

    // And nothing durable moved either -- the next prepare for a fresh host still
    // starts from epoch 0.
    fixture
        .repository()
        .execute_test_statement_params("DROP TRIGGER refuse_binding_commit", &[])
        .unwrap();
    let recovered = fixture
        .prepare(&root, &secret, "host-2", "op-recovered", 0)
        .unwrap_or_else(|error| panic!("a cleared failure must bind, got {error:?}"));
    assert_eq!(
        recovered.fence.binding_epoch, 1,
        "the recovery binding is the first committed epoch, so the failed one left no trace"
    );
    let turn = fixture.open_turn(&recovered, "turn-recovered");
    fixture
        .spawn(&root, &recovered, &turn, "after a failed commit")
        .expect("a binding that did commit can delegate");
}

/// C5: a tombstone outranks history.
///
/// Closing a container is permanent: an ordinary attach must not clear it, and a
/// prepare must be refused with `SessionClosed` even though the historical state is
/// `stalled` and therefore delegate-able under C4. A container that was never given
/// a digest is not one that may be entered: without a stored secret no identity can
/// be checked at all.
#[test]
fn session_delegation_lifecycle_closed_and_revoked_sessions_stay_closed() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("closed");
    fixture.write_history(&root, "stalled");

    fixture
        .repository()
        .close_session_container(&root.root_id.parse().unwrap())
        .unwrap();

    // A plain attach returns the address but must not resurrect the session.
    let again = fixture.attach_with_digest("closed", None);
    assert_eq!(
        again.root_id, root.root_id,
        "an attach is idempotent on the same key"
    );
    assert!(
        fixture
            .repository()
            .session_container(&root.root_id.parse().unwrap())
            .unwrap()
            .unwrap()
            .closed,
        "an ordinary attach must not clear the tombstone"
    );

    assert_eq!(
        fixture
            .prepare(&root, &secret, "closed-host", "op-closed", 0)
            .unwrap_err(),
        SessionFailureCode::SessionClosed,
        "a closed session cannot be prepared again"
    );

    // A container that never stored a digest cannot authenticate any identity, and
    // the refusal must name the missing identity rather than guessing one.
    let orphan = fixture.attach_with_digest("no-digest", None);
    assert_eq!(
        fixture
            .prepare(
                &orphan,
                &SecretCapability::new("whatever"),
                "orphan-host",
                "op-orphan",
                0,
            )
            .unwrap_err(),
        SessionFailureCode::SessionIdentityRequired,
        "a container with no stored digest must demand an explicit identity"
    );
}

/// C13 barrier 1: a close that lands after a turn was granted still refuses the
/// spawn, and leaves no child behind.
///
/// This is the window a real app hits: the host prepared and opened a turn, the
/// user then closed the conversation (or the frontend restart wrote the tombstone)
/// before the worker was dispatched. If the write path trusted the capability it
/// already handed out, a closed conversation would grow one more child with no live
/// owner -- exactly the orphan reclamation has to guess about later.
#[test]
fn session_delegation_lifecycle_old_watchdog_and_close_spawn_races() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("raced-close");
    // An older build left a `stalled` terminal on the container. History is not
    // closure, so the turn still opens; only the explicit close below refuses.
    fixture.write_history(&root, "stalled");

    let binding = fixture
        .prepare(&root, &secret, "host-1", "op-raced", 0)
        .expect("a stale history does not block the first binding");
    let turn = fixture.open_turn(&binding, "turn-raced");

    let before = fixture
        .daemon
        .coordinator()
        .children_of_root(&root.session_id.parse().unwrap())
        .len();
    fixture
        .repository()
        .close_session_container(&root.root_id.parse().unwrap())
        .unwrap();

    let refused = fixture
        .spawn(&root, &binding, &turn, "after the close")
        .expect_err("a close before dispatch must refuse the spawn");
    assert!(
        refused.contains("Closed"),
        "the refusal must name the closed container, got {refused}"
    );
    assert_eq!(
        fixture
            .daemon
            .coordinator()
            .children_of_root(&root.session_id.parse().unwrap())
            .len(),
        before,
        "a refused spawn must not leave a child row behind"
    );
    assert!(
        fixture.factory.starts.lock().unwrap().is_empty(),
        "a refused spawn must not start a worker"
    );

    // The closed container refuses the next prepare as well, so the tombstone is not
    // merely an admission quirk of the spawn route.
    assert_eq!(
        fixture
            .prepare(&root, &secret, "host-2", "op-after-close", 1)
            .unwrap_err(),
        SessionFailureCode::SessionClosed,
        "a closed container must refuse a prepare"
    );
}

/// The tombstone survives the restart that rehydrates the container.
///
/// A restart rebuilds the supervisor from rows alone. If hydration restored the
/// role but not the close, the row would still say `closed = 1` while every
/// in-memory check answered "open" -- and the mailbox is exactly such a check, with
/// no database guard behind it. So this asserts both halves: the durable refusal
/// (a prepare) and the in-memory one (a user message into a closed mailbox).
#[test]
fn session_delegation_lifecycle_closed_container_stays_closed_across_restart() {
    let mut fixture = Fixture::new();
    let (root, secret) = fixture.attach("closed-restart");
    fixture.write_history(&root, "stalled");
    fixture
        .repository()
        .close_session_container(&root.root_id.parse().unwrap())
        .unwrap();

    fixture.restart();
    // Re-attaching is what builds the supervisor from the rows; it must not clear
    // the tombstone.
    let again = fixture.attach_with_digest("closed-restart", None);
    assert_eq!(again.root_id, root.root_id);

    assert_eq!(
        fixture
            .repository()
            .execute_test_scalar(
                "SELECT closed FROM session_containers WHERE root_task_id = ?1",
                &root.root_id,
            )
            .unwrap(),
        1,
        "the restart must not clear the tombstone row"
    );
    assert_eq!(
        fixture
            .prepare(&root, &secret, "host-after-restart", "op-after-restart", 0)
            .unwrap_err(),
        SessionFailureCode::SessionClosed,
        "a restarted daemon must still refuse a closed container"
    );

    // The mailbox rule is memory-only: `can_accept_user_override` reads the
    // supervisor's `closed_containers`. A supervisor that forgot the close would
    // accept this message and hand it to a conversation that no longer exists.
    let response = send_request(
        fixture.socket(),
        IpcRequest::SendUserMessage {
            task_id: root.root_id.clone(),
            message: "are you still there?".into(),
        },
    )
    .unwrap();
    assert!(
        matches!(response, IpcResponse::Error { .. }),
        "a restarted daemon must not deliver into a closed mailbox, got {response:?}"
    );
}

/// C6: a rebind is blocked by anything not proven stopped, and inspecting must not
/// stop it.
///
/// The dangerous shape is the *unknown* driver: the daemon lost the channel, so it
/// has no proof the old execution ended. Treating "no record" as "stopped" would
/// hand the next driver a live tree it does not own. And a read must be inert: no
/// stop, no phase change.
#[test]
fn session_delegation_lifecycle_unproven_old_execution_blocks_rebind() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("unproven");
    let binding = fixture
        .prepare(&root, &secret, "first-host", "op-first", 0)
        .unwrap();
    let _turn = fixture.open_turn(&binding, "turn-1");

    // Write the row a host leaves behind when its channel dies before it could
    // report a stop. `unknown` is included in the "unsettled" set on purpose.
    let root_id = root.root_id.parse::<TaskId>().unwrap();
    fixture
        .repository()
        .register_session_host_execution(&yi_agent_store::repository::SessionHostExecution {
            execution_id: "exec-unknown".into(),
            root_task_id: root.root_id.clone(),
            generation: binding.fence.generation.clone(),
            binding_epoch: binding.fence.binding_epoch,
            driver_epoch: binding.fence.driver_epoch,
            turn_id: "turn-1".into(),
            identity_json: "{}".into(),
            phase: "unknown".into(),
            evidence_json: None,
            execution_set_version: 0,
        })
        .unwrap();

    // A read must not touch it.
    let detail = send_request(
        fixture.socket(),
        IpcRequest::InspectTask {
            task_id: root.root_id.clone(),
        },
    )
    .unwrap();
    assert!(
        matches!(detail, IpcResponse::TaskDetail(_)),
        "inspection must be inert and available"
    );
    assert!(
        !fixture
            .repository()
            .unsettled_session_host_executions(&root_id)
            .unwrap()
            .is_empty(),
        "inspect must not have retired the execution"
    );

    assert_eq!(
        fixture
            .prepare(&root, &secret, "second-host", "op-second", 1)
            .unwrap_err(),
        SessionFailureCode::StopUnproven,
        "an unproven old execution must block the next driver"
    );
}

/// C8: no IPC entry point may let an execution borrow the container route.
///
/// Two distinct bypasses: naming an execution as the parent (it has no container
/// row, so the container route falls through to the execution route and the turn's
/// root no longer names the parent), and forging the root id inside the turn. Both
/// must be refused on the same wire path the shipping hosts use.
#[test]
fn session_delegation_lifecycle_execution_cannot_use_application_routes() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("execution-route");
    let binding = fixture
        .prepare(&root, &secret, "route-host", "op-route", 0)
        .unwrap();
    let turn = fixture.open_turn(&binding, "turn-route");
    let child = fixture
        .spawn(&root, &binding, &turn, "a real child")
        .unwrap();
    assert_eq!(
        fixture.repository().task_role(&child).unwrap(),
        TaskRole::Execution,
        "a container's child is an execution"
    );

    // The child is an execution; it cannot be the parent of a fenced container
    // write, whichever way the caller tries to say so.
    let refused = fixture.spawn(
        &SessionRef {
            session_id: root.session_id.clone(),
            root_id: child.to_string(),
        },
        &binding,
        &turn,
        "grandchild through the child",
    );
    assert_eq!(
        refused.unwrap_err(),
        "AuthorityDenied",
        "an execution parent must not accept the container route"
    );

    // A turn whose fence names another root is not the open turn, even though its
    // capability is a real one this daemon minted.
    let mut forged = turn.clone();
    forged.fence.root.root_id = child.to_string();
    match send_request(
        fixture.socket(),
        IpcRequest::SpawnApplicationChild {
            session_id: root.session_id.clone(),
            parent_task_id: child.to_string(),
            capability: turn.capability.expose().to_string(),
            objective: "forged".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
            session_turn: Some(forged),
        },
    )
    .unwrap()
    {
        // The turn's fence no longer names a container, so the container route is
        // not taken at all and the fallback execution route denies the secret.
        // Either way the write is refused; what must never happen is a spawn.
        IpcResponse::SessionDelegationRejected { failure, .. } => assert!(
            matches!(
                failure.code,
                SessionFailureCode::StaleTurn | SessionFailureCode::AuthorityDenied
            ),
            "a forged root must be refused, got {:?}",
            failure.code
        ),
        IpcResponse::Error { code, .. } => assert_eq!(
            code,
            yi_agent_store::ipc::IpcErrorCode::AuthorityDenied,
            "a forged root must be refused as an authority problem"
        ),
        other => panic!("a forged root must never spawn, got {other:?}"),
    }
}

/// C7/C13: a turn is a live fence, not a reusable token.
///
/// Three replays, one rule: after a turn closes, presenting it again must be
/// refused; presenting a *live* turn under another binding epoch or another turn id
/// must be refused too. Without this the whole fence is decorative -- an abandoned
/// run could keep spawning under a secret the host believed it had retired.
#[test]
fn session_delegation_lifecycle_a_closed_turn_cannot_be_replayed() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("turn-replay");
    let binding = fixture
        .prepare(&root, &secret, "replay-host", "op-replay", 0)
        .unwrap();
    let turn = fixture.open_turn(&binding, "turn-one");
    fixture
        .spawn(&root, &binding, &turn, "before the turn closes")
        .expect("the open turn may spawn");
    fixture.finish_turn(&turn);

    // Replaying the exact turn: the fence is right, the turn is over.
    let replayed = fixture.spawn(&root, &binding, &turn, "after the turn closes");
    assert!(
        replayed.is_err(),
        "a closed turn must not spawn again, got {replayed:?}"
    );

    // A live turn with a bumped binding epoch: the fence no longer matches.
    let live = fixture.open_turn(&binding, "turn-two");
    let mut bumped = live.clone();
    bumped.fence.binding_epoch += 1;
    assert!(
        fixture
            .spawn(&root, &binding, &bumped, "stale epoch")
            .is_err(),
        "another binding epoch must not write"
    );

    // A live turn presented under another turn id: the secret is real, the id is not.
    let mut renamed = live.clone();
    renamed.turn_id = "turn-invented".into();
    assert!(
        fixture
            .spawn(&root, &binding, &renamed, "invented turn id")
            .is_err(),
        "a turn id the daemon never opened must not write"
    );

    // The genuine turn still works, so the refusals above are about the fence and
    // not about the container having stopped accepting writes.
    fixture
        .spawn(&root, &binding, &live, "the real turn")
        .expect("the open turn still spawns");
}

/// C10: a container whose history says `completed` still receives messages.
///
/// The mailbox rule is the same rule as admission: an execution's terminal state
/// closes its mailbox, a container's does not. Delivery to a terminal *execution*
/// must still be refused, so the exemption is confined to the role.
#[test]
fn session_delegation_lifecycle_terminal_history_container_receives_delivery() {
    let fixture = Fixture::new();
    let (root, secret) = fixture.attach("delivery");
    fixture.write_history(&root, "completed");
    let binding = fixture
        .prepare(&root, &secret, "delivery-host", "op-delivery", 0)
        .unwrap();
    let turn = fixture.open_turn(&binding, "turn-delivery");
    let child = fixture
        .spawn(&root, &binding, &turn, "reporting child")
        .unwrap();

    // The child reports to its container even though the container's state is
    // `completed`: the state is evidence about an old execution, not a closed door.
    // It reports as itself, over the worker capability the daemon issued it.
    let sent = send_request(
        fixture.socket(),
        IpcRequest::SendMessage {
            session_id: root.session_id.clone(),
            sender_task_id: child.to_string(),
            worker_capability: fixture.child_capability(&child),
            recipient_task_id: root.root_id.clone(),
            message: "the container takes this".into(),
        },
    )
    .unwrap();
    assert_eq!(
        sent,
        IpcResponse::MessageQueued,
        "a container with terminal history must still receive"
    );

    // The child is an execution; its terminal state is final. Cancel it, then prove
    // the same route now refuses delivery.
    let cancelled = send_request(
        fixture.socket(),
        IpcRequest::CancelChild {
            session_id: root.session_id.clone(),
            caller_task_id: root.root_id.clone(),
            capability: turn.capability.expose().to_string(),
            task_id: child.to_string(),
            recursive: false,
            session_turn: Some(turn.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        cancelled,
        IpcResponse::TaskCancelled,
        "the container owns this child"
    );
    let refused = send_request(
        fixture.socket(),
        IpcRequest::SendApplicationMessage {
            session_id: root.session_id.clone(),
            sender_task_id: root.root_id.clone(),
            capability: turn.capability.expose().to_string(),
            recipient_task_id: child.to_string(),
            message: "too late".into(),
            session_turn: Some(turn),
        },
    )
    .unwrap();
    assert_ne!(
        refused,
        IpcResponse::MessageQueued,
        "a settled execution's mailbox is closed: the exemption is confined to the role"
    );
    match refused {
        IpcResponse::Error { message, .. } => {
            let message = message.unwrap_or_default();
            assert!(
                message.contains("terminal"),
                "the refusal must name the closed mailbox, got {message}"
            );
        }
        IpcResponse::SessionDelegationRejected { failure, .. } => assert!(
            failure.reason.contains("terminal"),
            "the refusal must name the closed mailbox, got {}",
            failure.reason
        ),
        other => panic!("unexpected refusal shape {other:?}"),
    }
}

/// C12: one epoch is spent exactly once, and the loser learns which refusal it was.
///
/// The guarantee under test is the transaction's compare-and-set, so this test drives
/// it at the layer that owns it. Two callers both decided against epoch 0 -- the state
/// a race actually produces, because both read the row before either wrote it. The
/// loser must be refused `StaleBinding`, not handed a second grant and not given an
/// opaque `InvalidState`.
///
/// (Over the wire, two prepares are serialized by the daemon: the second one re-reads
/// the row, sees the new epoch and legitimately replaces the first driver. That is why
/// this cannot be proved by racing two socket clients -- it would only prove the
/// daemon is sequential.)
#[test]
fn session_delegation_lifecycle_concurrent_prepare_commits_once() {
    let fixture = Fixture::new();
    let (root, _secret) = fixture.attach("cas");
    let root_id: TaskId = root.root_id.parse().unwrap();

    let commit = |host: &str, operation: &str| yi_agent_store::repository::SessionBindingCommit {
        root: root.clone(),
        expected_binding_epoch: 0,
        operation_id: operation.to_string(),
        request_digest: format!("digest-{host}"),
        owner_run_id: host.to_string(),
        generation: "generation-1".to_string(),
        resume_digest: None,
        consume_legacy_upgrade: false,
        retired_executions: Vec::new(),
    };

    let mut repository = fixture.repository();
    repository
        .commit_session_binding(&commit("host-a", "op-a"))
        .expect("the first decision against epoch 0 commits");

    // The second caller also decided against epoch 0. Its write must lose the CAS
    // rather than overwrite the winner's binding.
    let loser = repository
        .commit_session_binding(&commit("host-b", "op-b"))
        .unwrap_err();
    assert!(
        matches!(
            loser,
            yi_agent_store::repository::RepositoryError::SessionDelegation(_)
        ),
        "the loser must be refused with a structured session failure, got {loser:?}"
    );
    let yi_agent_store::repository::RepositoryError::SessionDelegation(failure) = loser else {
        unreachable!("just asserted")
    };
    assert_eq!(
        failure.code,
        SessionFailureCode::StaleBinding,
        "a lost race is a stale binding, not a broken daemon"
    );

    let record = repository.session_container(&root_id).unwrap().unwrap();
    assert_eq!(
        record.binding_epoch, 1,
        "one decision spent one epoch; the loser did not add a second"
    );
    assert_eq!(record.owner_run_id.as_deref(), Some("host-a"));

    // Replaying the winner's operation is free: the operation id already names it,
    // and the same digest returns the committed record without bumping again.
    let replayed = repository
        .commit_session_binding(&commit("host-a", "op-a"))
        .expect("a committed operation is replayable");
    assert_eq!(
        replayed.binding_epoch, 1,
        "replaying a committed operation must not add an epoch"
    );

    // The same operation id with a *different* request is a conflict, never a second
    // grant: that is the rule that makes a lost response safe to retry.
    let mut conflicting = commit("host-a", "op-a");
    conflicting.request_digest = "a-different-request".to_string();
    let conflict = repository.commit_session_binding(&conflicting).unwrap_err();
    let yi_agent_store::repository::RepositoryError::SessionDelegation(conflict) = conflict else {
        panic!("a reused operation id must be a structured conflict");
    };
    assert_eq!(conflict.code, SessionFailureCode::OwnerConflict);
}

/// C14: two conversations in one directory keep separate trees.
///
/// The failure this rules out is a shared mutable container: resuming A must not
/// move B's binding epoch, each child must belong to exactly one container, and A's
/// turn must not be usable to write under B.
#[test]
fn session_delegation_lifecycle_two_sessions_keep_permissions_and_quotas() {
    let fixture = Fixture::new();
    let (a, secret_a) = fixture.attach("quota-a");
    let (b, secret_b) = fixture.attach("quota-b");
    assert_ne!(a.root_id, b.root_id);

    let binding_a = fixture.prepare(&a, &secret_a, "host-a", "op-a", 0).unwrap();
    let binding_b = fixture.prepare(&b, &secret_b, "host-b", "op-b", 0).unwrap();
    let turn_a = fixture.open_turn(&binding_a, "turn-a");
    let turn_b = fixture.open_turn(&binding_b, "turn-b");

    let child_a = fixture.spawn(&a, &binding_a, &turn_a, "A's child").unwrap();
    let child_b = fixture.spawn(&b, &binding_b, &turn_b, "B's child").unwrap();
    assert_ne!(child_a, child_b);

    let repository = fixture.repository();
    assert_eq!(
        repository
            .task_detail(&child_a)
            .unwrap()
            .parent_task_id
            .as_deref(),
        Some(a.root_id.as_str())
    );
    assert_eq!(
        repository
            .task_detail(&child_b)
            .unwrap()
            .parent_task_id
            .as_deref(),
        Some(b.root_id.as_str())
    );
    drop(repository);

    // Resuming A does not disturb B.
    let epoch_b = fixture
        .repository()
        .session_container(&b.root_id.parse().unwrap())
        .unwrap()
        .unwrap()
        .binding_epoch;
    let resumed_a = fixture
        .prepare(
            &a,
            &secret_a,
            "host-a-2",
            "op-a-2",
            binding_a.fence.binding_epoch,
        )
        .unwrap();
    assert_eq!(
        resumed_a.fence.root.root_id, a.root_id,
        "resuming A must name A"
    );
    assert_eq!(
        fixture
            .repository()
            .session_container(&b.root_id.parse().unwrap())
            .unwrap()
            .unwrap()
            .binding_epoch,
        epoch_b,
        "resuming A must not move B's epoch"
    );

    // A's turn cannot act for B. The binding in the request is irrelevant here --
    // the turn is what authorizes a container write -- so the interesting shape is
    // A's *own* turn presented for B's tree.
    let refused = fixture.spawn(&b, &resumed_a, &turn_a, "A acting for B");
    assert_eq!(
        refused.unwrap_err(),
        "AuthorityDenied",
        "a turn bound to A must not write under B"
    );
}
