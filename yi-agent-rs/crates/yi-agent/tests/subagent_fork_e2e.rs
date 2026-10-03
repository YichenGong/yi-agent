//! End-to-end proof of the context-fork chain, driven through the *real*
//! registered delegation tool.
//!
//! This is the strongest level of the chain: an in-process daemon runs the real
//! [`DaemonAgentWorkerFactory`] over a test-defined recording provider, a caller
//! session with history is bound to a [`CallerContext`], and `spawn_agent` is
//! the very tool the application registers (`register_attached_root_tools`). The
//! assertions land on the forked child worker's *first provider request*, so
//! what is proven is the whole wire:
//!
//!   registered `spawn_agent` -> chunked `BeginForkUpload`/`AppendForkChunk`
//!   -> daemon `take_fork_messages` -> `WorkerStart.fork_messages`
//!   -> the worker seeds its session -> the provider request carries the history.
//!
//! A second run omits `fork` entirely and must reach the provider with only the
//! objective, proving the default stays the old, unforked behavior.
//!
//! It is deliberately *not* degraded to a client-only assertion: the child's
//! provider request is observed, so a break anywhere between the tool and the
//! worker fails here.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use yi_agent_core::Message;
use yi_agent_core::{
    AgentConfig, ContentBlock, Provider, ProviderError, ProviderEvent, ProviderRequest, Session,
    StopReason, ToolRegistry,
};
use yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, send_request};
use yi_agent_subagent::binding::{RuntimeBinding, RuntimeHandle};
use yi_agent_subagent::thread_root::ThreadRoot;
use yi_agent_subagent::{AttachedRoot, CallerContext, DaemonAgentWorkerFactory};

/// A marker that can only appear in the child's request if the caller's history
/// was actually forked in, never if the objective alone was sent.
const PARENT_HISTORY: &str = "PARENT_HISTORY_MARKER: the caller asked for X";
/// A marker that proves the objective itself still reaches the child.
const OBJECTIVE: &str = "OBJECTIVE_MARKER: continue the task";

/// Records every provider request so a test can inspect what the child worker
/// actually sent on its first turn. The stream stops immediately, so no agent
/// loop runs past the request under test.
#[derive(Default)]
struct RecordingProvider {
    requests: Mutex<Vec<ProviderRequest>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn call_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(futures::stream::iter([ProviderEvent::Stop {
            reason: StopReason::EndTurn,
        }])
        .boxed())
    }
}

impl RecordingProvider {
    /// The first recorded request, or `None` while the worker is still starting.
    fn first_request(&self) -> Option<ProviderRequest> {
        self.requests.lock().unwrap().first().cloned()
    }

    /// Blocks until the child's first provider request lands, then returns a
    /// flattened view of its message texts in order. Polling beats a guessed
    /// sleep: the worker runs on its own thread and its start is asynchronous.
    fn first_request_texts(&self) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(request) = self.first_request() {
                return texts_of(&request);
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the child worker to call the provider"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn texts_of(request: &ProviderRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn init_git_repo(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }
}

/// Everything one run needs, kept alive together: the git project, the runtime
/// directory, the daemon, the recording provider, the registered tools, and the
/// caller's live session.
struct ForkFixture {
    _repo: tempfile::TempDir,
    _runtime: tempfile::TempDir,
    /// Held for the daemon's lifetime: dropping it stops the socket the child
    /// worker talks to. Never read after construction.
    _daemon: Daemon,
    provider: Arc<RecordingProvider>,
    registry: ToolRegistry,
    session: Arc<Mutex<Session>>,
}

impl ForkFixture {
    /// Brings up the whole chain: a clean git project, a real-factory daemon,
    /// an attached + activated application root, the real registered delegation
    /// tools, and a bound caller session holding one history message.
    fn start() -> Self {
        let repo = tempfile::TempDir::new().unwrap();
        init_git_repo(repo.path());
        let runtime = tempfile::TempDir::new().unwrap();
        let database = runtime.path().join("runtime.sqlite");
        let socket = yi_agent_store::ipc::socket_path_for(runtime.path()).unwrap();

        // The real worker factory, over a provider whose requests the test can
        // inspect. `.with_workspace` gives it a git project so the coordinator's
        // admission resolves a real position for the child.
        let provider = Arc::new(RecordingProvider::default());
        let factory = DaemonAgentWorkerFactory::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            socket,
        )
        .with_workspace(repo.path().to_path_buf());
        let daemon = Daemon::start_with_factory(runtime.path(), &database, Arc::new(factory))
            .expect("a real-factory daemon starts");

        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            workspace,
        } = send_request(
            daemon.socket_path(),
            IpcRequest::AttachApplicationRoot {
                idempotency_key: "fork-e2e".into(),
                workspace: repo.path().to_path_buf(),
            },
        )
        .expect("the runtime socket must accept an application root")
        else {
            panic!("expected an application root attachment");
        };
        let activated = send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                objective: "drive the fork end-to-end".into(),
            },
        )
        .expect("activation must reach the socket");
        assert!(
            matches!(activated, IpcResponse::ApplicationRootActivated),
            "the root must activate, got {activated:?}"
        );

        let root = ThreadRoot::from_handle(
            RuntimeBinding::fixed(RuntimeHandle {
                socket_path: daemon.socket_path().to_path_buf(),
                workspace_root: workspace.path.clone(),
                session_id: session_id.clone(),
                task_id: root_task_id.clone(),
                capability: message_capability.clone(),
            }),
            AttachedRoot {
                session_id,
                task_id: root_task_id,
                capability: message_capability,
                workspace,
            },
        );

        // The caller's live conversation: history is present *before* the tool
        // call, exactly as it would be in a running root agent.
        let session = Arc::new(Mutex::new(Session::new()));
        session.lock().unwrap().push(Message::user(PARENT_HISTORY));
        let caller = CallerContext::new(Arc::clone(&session));

        // The real registered tool, not a hand-rolled IPC client.
        let mut registry = ToolRegistry::new();
        yi_agent_subagent::register_attached_root_tools(
            &mut registry,
            root,
            yi_agent_tools::SandboxController::new(
                yi_agent_core::autonomy::YoloSwitch::new(false),
                yi_agent_tools::SandboxMode::WorkspaceWrite,
                false,
            ),
            caller,
        );

        Self {
            _repo: repo,
            _runtime: runtime,
            _daemon: daemon,
            provider,
            registry,
            session,
        }
    }

    /// Drives the registered `spawn_agent` with `args` and returns the result.
    fn spawn(&self, args: serde_json::Value) -> yi_agent_core::ToolResult {
        let tool = self
            .registry
            .get("spawn_agent")
            .expect("the registered root exposes spawn_agent");
        let runtime = tokio::runtime::Runtime::new().expect("a test runtime starts");
        runtime.block_on(tool.call(args))
    }

    fn first_request_texts(&self) -> Vec<String> {
        self.provider.first_request_texts()
    }

    fn first_request_message_count(&self) -> usize {
        self.provider
            .first_request()
            .expect("the child's first request was recorded")
            .messages
            .len()
    }
}

/// The whole point of the feature: `spawn_agent{fork:true}` must hand the
/// caller's history to the child, and the child must actually start from it.
///
/// The assertion is on the child worker's *first provider request*, downstream
/// of the socket, the daemon, and `WorkerStart` -- not on a client-side echo.
#[test]
fn fork_true_spawns_a_child_whose_first_request_carries_the_caller_history() {
    let fixture = ForkFixture::start();

    let result = fixture.spawn(serde_json::json!({
        "task": OBJECTIVE,
        "fork": true,
    }));
    assert!(
        !result.is_error,
        "a forked spawn must be accepted: {:?}",
        result.content
    );

    let texts = fixture.first_request_texts();
    let history = texts
        .iter()
        .position(|text| text.contains(PARENT_HISTORY))
        .unwrap_or_else(|| {
            panic!("the forked child's first request must carry the caller's history: {texts:?}")
        });
    let objective = texts
        .iter()
        .position(|text| text.contains(OBJECTIVE))
        .unwrap_or_else(|| panic!("the objective must still reach the child: {texts:?}"));
    assert!(
        history < objective,
        "the caller's history must be a prefix, ahead of the objective: {texts:?}"
    );
    assert_eq!(
        fixture.first_request_message_count(),
        2,
        "one inherited history message plus the objective"
    );

    // The caller's own session is untouched: a fork copies, it does not move.
    assert_eq!(
        fixture.session.lock().unwrap().messages().len(),
        1,
        "forking must not consume the caller's history"
    );
}

/// The regression side of the contract: omitting `fork` keeps today's behavior
/// -- the child's first request is the objective alone.
#[test]
fn an_unforked_spawn_sends_only_the_objective_to_the_child() {
    let fixture = ForkFixture::start();

    // `fork` is omitted entirely, which is the default path.
    let result = fixture.spawn(serde_json::json!({ "task": OBJECTIVE }));
    assert!(
        !result.is_error,
        "an unforked spawn must be accepted: {:?}",
        result.content
    );

    let texts = fixture.first_request_texts();
    assert!(
        texts.iter().any(|text| text.contains(OBJECTIVE)),
        "the objective must reach the child: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains(PARENT_HISTORY)),
        "an unforked child must not inherit the caller's history: {texts:?}"
    );
    assert_eq!(
        fixture.first_request_message_count(),
        1,
        "an unforked child sends exactly its objective"
    );
}

/// An explicit `fork:false` takes the same unforked path as omission, so the
/// default is not the only way to keep the old behavior.
#[test]
fn fork_false_matches_the_unforked_path() {
    let fixture = ForkFixture::start();

    let result = fixture.spawn(serde_json::json!({
        "task": OBJECTIVE,
        "fork": false,
    }));
    assert!(
        !result.is_error,
        "a fork:false spawn must be accepted: {:?}",
        result.content
    );

    let texts = fixture.first_request_texts();
    assert!(
        !texts.iter().any(|text| text.contains(PARENT_HISTORY)),
        "fork:false must not inherit the caller's history: {texts:?}"
    );
    assert_eq!(fixture.first_request_message_count(), 1);
}

/// A read-only child (the default mode) must fork just as well as a coding one.
/// The history is protocol-valid either way, and the position resolution differs
/// between the two, so this pins the read-only branch.
#[test]
fn a_forked_read_only_child_still_receives_the_history() {
    let fixture = ForkFixture::start();

    let result = fixture.spawn(serde_json::json!({
        "task": OBJECTIVE,
        "mode": "read_only",
        "fork": true,
    }));
    assert!(
        !result.is_error,
        "a forked read-only spawn must be accepted: {:?}",
        result.content
    );

    let texts = fixture.first_request_texts();
    assert!(
        texts.iter().any(|text| text.contains(PARENT_HISTORY)),
        "a read-only forked child must still carry the history: {texts:?}"
    );
    assert!(texts.iter().any(|text| text.contains(OBJECTIVE)));
    assert_eq!(fixture.first_request_message_count(), 2);
}
