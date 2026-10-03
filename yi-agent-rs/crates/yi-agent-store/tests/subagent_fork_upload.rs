//! The store-side half of conversation forking: a caller uploads its history as
//! base64 chunks and only receives decoded messages once the declared byte count
//! has arrived. These tests exercise the in-memory state machine through the
//! coordinator, with an attached application root as the authorized caller.

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
use yi_agent_store::runtime::{RuntimeCoordinator, RuntimeCoordinatorError};

/// The upload machine never starts a worker for the root; the factory only has
/// to resolve the read-only application-root workspace that attaching the root
/// requires. It still records every `WorkerStart` so a test can read the worker
/// capability the supervisor minted for a child and act as that child caller.
#[derive(Default)]
struct StaticWorkspaceFactory {
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}

fn workspace(root: &RootSessionId, task: &TaskId) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: std::path::PathBuf::from(format!("/tmp/yi-agent-fork-test/{root}")),
        path: std::path::PathBuf::from(format!("/tmp/yi-agent-fork-test/{root}/{task}")),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    }
}

impl WorkerWorkspaceProvider for StaticWorkspaceFactory {
    fn in_place_workspace(
        &self,
        root: &RootSessionId,
        task: &TaskId,
        _attempt: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(root, task))
    }

    fn workspace_in(
        &self,
        task: &TaskId,
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(&RootSessionId::new(), task))
    }

    fn read_only_workspace(
        &self,
        _parent: Option<&WorkerWorkspace>,
        task: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace(&RootSessionId::new(), task))
    }
}

impl AgentWorkerFactory for StaticWorkspaceFactory {
    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(StaticWorkspaceFactory {
            starts: Arc::clone(&self.starts),
        }))
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:fork-test".into()),
            worktree_lease: Some("worktree:fork-test".into()),
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

struct ForkFixture {
    _directory: TempDir,
    coordinator: RuntimeCoordinator,
    session: RootSessionId,
    caller: TaskId,
    capability: String,
    /// Every worker start the factory observed, so a test can recover the worker
    /// capability the supervisor minted for a spawned child.
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}

/// The worker start the factory recorded for `task`, if any.
fn worker_start_for(starts: &Arc<Mutex<Vec<WorkerStart>>>, task: &TaskId) -> WorkerStart {
    starts
        .lock()
        .unwrap()
        .iter()
        .find(|start| &start.task_id == task)
        .cloned()
        .expect("the task's worker started")
}

/// Attaches a real application root so the fork upload has a capability-bearing
/// caller. `fork_max_bytes` overrides the payload ceiling when supplied.
async fn attached_root(fork_max_bytes: Option<u64>) -> ForkFixture {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let coordinator = RuntimeCoordinator::open(
        &database,
        Arc::new(StaticWorkspaceFactory {
            starts: Arc::clone(&starts),
        }),
    )
    .unwrap();
    let coordinator = match fork_max_bytes {
        Some(bytes) => coordinator.with_fork_max_bytes(bytes),
        None => coordinator,
    };
    let attached = coordinator
        .attach_application_root("fork-upload-fixture", &directory.path().join("project"))
        .await
        .unwrap();
    ForkFixture {
        _directory: directory,
        coordinator,
        session: attached.session_id,
        caller: attached.root_task_id,
        capability: attached.message_capability,
        starts,
    }
}

fn encoded_history(messages: &[Message]) -> (String, u64) {
    let payload = serde_json::to_string(messages).unwrap();
    let total = payload.len() as u64;
    (STANDARD.encode(payload.as_bytes()), total)
}

#[tokio::test]
async fn a_complete_upload_is_accepted_and_consumed_once() {
    let fixture = attached_root(None).await;
    let (encoded, total) = encoded_history(&[Message::user("parent"), Message::user("more")]);

    let token = fixture
        .coordinator
        .begin_fork_upload(
            &fixture.session,
            &fixture.caller,
            &fixture.capability,
            total,
        )
        .await
        .unwrap();
    let received = fixture
        .coordinator
        .append_fork_chunk(&token, 0, &encoded)
        .unwrap();
    assert_eq!(received, total);

    let messages = fixture
        .coordinator
        .take_fork_messages(&token, &fixture.session, &fixture.caller)
        .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0], Message::user("parent"));
    assert_eq!(messages[1], Message::user("more"));

    assert!(
        fixture
            .coordinator
            .take_fork_messages(&token, &fixture.session, &fixture.caller)
            .is_err(),
        "taking a fork payload is one-shot"
    );
}

#[tokio::test]
async fn an_incomplete_upload_is_rejected() {
    let fixture = attached_root(None).await;
    let (encoded, _total) = encoded_history(&[Message::user("parent")]);
    let declared = encoded.len() as u64 + 4;

    let token = fixture
        .coordinator
        .begin_fork_upload(
            &fixture.session,
            &fixture.caller,
            &fixture.capability,
            declared,
        )
        .await
        .unwrap();
    let received = fixture
        .coordinator
        .append_fork_chunk(&token, 0, &encoded)
        .unwrap();
    assert!(
        received < declared,
        "only part of the declared payload arrived"
    );

    assert!(
        fixture
            .coordinator
            .take_fork_messages(&token, &fixture.session, &fixture.caller)
            .is_err(),
        "an incomplete payload is never handed back"
    );
}

#[tokio::test]
async fn a_duplicate_or_out_of_order_seq_is_rejected() {
    let fixture = attached_root(None).await;
    let (encoded, total) = encoded_history(&[Message::user("parent"), Message::user("more")]);
    let token = fixture
        .coordinator
        .begin_fork_upload(
            &fixture.session,
            &fixture.caller,
            &fixture.capability,
            total,
        )
        .await
        .unwrap();

    assert!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 1, &encoded)
            .is_err(),
        "a chunk cannot arrive before its predecessor"
    );
    assert_eq!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 0, &encoded)
            .unwrap(),
        total
    );
    assert!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 0, &encoded)
            .is_err(),
        "a sequence number cannot be replayed"
    );
    assert!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 5, &encoded)
            .is_err(),
        "a sequence number cannot be skipped"
    );
}

#[tokio::test]
async fn a_total_over_the_cap_is_rejected_at_begin() {
    let fixture = attached_root(Some(4)).await;

    let error = fixture
        .coordinator
        .begin_fork_upload(&fixture.session, &fixture.caller, &fixture.capability, 5)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            RuntimeCoordinatorError::ForkTooLarge { total: 5, max: 4 }
        ),
        "an oversized payload is rejected before any data flows, got {error:?}"
    );
}

#[tokio::test]
async fn the_cap_boundary_is_inclusive_at_begin() {
    let fixture = attached_root(Some(8)).await;

    assert!(
        fixture
            .coordinator
            .begin_fork_upload(&fixture.session, &fixture.caller, &fixture.capability, 8)
            .await
            .is_ok(),
        "a declaration of exactly the cap is accepted"
    );

    let error = fixture
        .coordinator
        .begin_fork_upload(&fixture.session, &fixture.caller, &fixture.capability, 9)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            RuntimeCoordinatorError::ForkTooLarge { total: 9, max: 8 }
        ),
        "one byte over the cap is rejected, got {error:?}"
    );
}

#[tokio::test]
async fn a_payload_that_exactly_fills_the_cap_is_accepted_and_a_further_chunk_is_rejected() {
    let fixture = attached_root(None).await;
    let (encoded, total) = encoded_history(&[Message::user("parent"), Message::user("more")]);

    // The declared total and the ceiling coincide: the accumulated bound must
    // reject the first byte past the cap, not just the declared value.
    let fixture = ForkFixture {
        coordinator: fixture.coordinator.with_fork_max_bytes(total),
        ..fixture
    };
    let token = fixture
        .coordinator
        .begin_fork_upload(
            &fixture.session,
            &fixture.caller,
            &fixture.capability,
            total,
        )
        .await
        .unwrap();

    let received = fixture
        .coordinator
        .append_fork_chunk(&token, 0, &encoded)
        .unwrap();
    assert_eq!(received, total, "the stream exactly fills the declared cap");

    assert!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 1, "AAAA")
            .is_err(),
        "a chunk beyond the filled cap is rejected"
    );

    let messages = fixture
        .coordinator
        .take_fork_messages(&token, &fixture.session, &fixture.caller)
        .unwrap();
    assert_eq!(messages.len(), 2);
}

#[tokio::test]
async fn a_zero_byte_declaration_is_rejected_at_begin() {
    let fixture = attached_root(None).await;

    let error = fixture
        .coordinator
        .begin_fork_upload(&fixture.session, &fixture.caller, &fixture.capability, 0)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeCoordinatorError::ForkUpload(_)),
        "an empty fork payload is refused at begin, got {error:?}"
    );
}

#[tokio::test]
async fn a_chunk_that_overflows_the_expected_length_is_rejected() {
    let fixture = attached_root(None).await;
    let (encoded, total) = encoded_history(&[Message::user("parent")]);
    let token = fixture
        .coordinator
        .begin_fork_upload(
            &fixture.session,
            &fixture.caller,
            &fixture.capability,
            total - 1,
        )
        .await
        .unwrap();

    assert!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 0, &encoded)
            .is_err(),
        "decoded bytes must never exceed the declared length"
    );
}

#[tokio::test]
async fn an_aborted_upload_has_no_live_token() {
    let fixture = attached_root(None).await;
    let token = fixture
        .coordinator
        .begin_fork_upload(&fixture.session, &fixture.caller, &fixture.capability, 4)
        .await
        .unwrap();

    fixture.coordinator.abort_fork_upload(&token).unwrap();

    assert!(
        fixture
            .coordinator
            .append_fork_chunk(&token, 0, "AAAA")
            .is_err()
    );
    assert!(
        fixture
            .coordinator
            .take_fork_messages(&token, &fixture.session, &fixture.caller)
            .is_err()
    );
    assert!(
        fixture.coordinator.abort_fork_upload(&token).is_err(),
        "aborting an unknown token fails explicitly"
    );
}

#[tokio::test]
async fn a_take_from_a_mismatched_tenant_is_rejected() {
    let fixture = attached_root(None).await;
    let (encoded, total) = encoded_history(&[Message::user("parent"), Message::user("more")]);
    let token = fixture
        .coordinator
        .begin_fork_upload(
            &fixture.session,
            &fixture.caller,
            &fixture.capability,
            total,
        )
        .await
        .unwrap();
    fixture
        .coordinator
        .append_fork_chunk(&token, 0, &encoded)
        .unwrap();

    let other_caller = TaskId::new();
    assert!(
        fixture
            .coordinator
            .take_fork_messages(&token, &fixture.session, &other_caller)
            .is_err(),
        "a different caller cannot claim the payload"
    );
    let other_session = RootSessionId::new();
    assert!(
        fixture
            .coordinator
            .take_fork_messages(&token, &other_session, &fixture.caller)
            .is_err(),
        "a different session cannot claim the payload"
    );

    let messages = fixture
        .coordinator
        .take_fork_messages(&token, &fixture.session, &fixture.caller)
        .unwrap();
    assert_eq!(
        messages.len(),
        2,
        "the owning caller still claims the payload"
    );
}

#[tokio::test]
async fn begin_rejects_an_unauthorized_caller() {
    let fixture = attached_root(None).await;

    let error = fixture
        .coordinator
        .begin_fork_upload(&fixture.session, &fixture.caller, "app-root-forged", 4)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeCoordinatorError::AuthorityDenied(_)),
        "a forged capability cannot open an upload, got {error:?}"
    );
}

/// The Critical this test closes: a subagent forks its own conversation, so the
/// fork-upload authority cannot be application-root-only. A child authenticates
/// with the worker capability the supervisor minted for its own task, opens an
/// upload, and the token seeds a forked grandchild.
#[tokio::test]
async fn a_child_caller_opens_a_fork_upload_and_forks_its_own_child() {
    let fixture = attached_root(None).await;
    // The root spawns a child through the child-capability-bearing path. The
    // child's worker start carries the capability that authenticates it.
    let child = fixture
        .coordinator
        .spawn_child_and_admit(
            &fixture.session,
            &fixture.caller,
            "child objective".into(),
            yi_agent_core::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let child_start = worker_start_for(&fixture.starts, &child);
    let child_capability = child_start.message_capability.clone();
    assert!(
        !child_capability.is_empty(),
        "the supervisor minted a worker capability for the child"
    );

    let history = vec![Message::user("child saw this"), Message::user("and this")];
    let (encoded, total) = encoded_history(&history);
    let token = fixture
        .coordinator
        .begin_fork_upload(&fixture.session, &child, &child_capability, total)
        .await
        .expect("a child can open a fork upload with its own worker capability");
    fixture
        .coordinator
        .append_fork_chunk(&token, 0, &encoded)
        .unwrap();

    // The grandchild's fork must be taken for the child caller and the child's
    // parent, exactly as the child's own spawn would name them.
    let grandchild = fixture
        .coordinator
        .spawn_child_and_admit(
            &fixture.session,
            &child,
            "grandchild objective".into(),
            yi_agent_core::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            Some(token.clone()),
        )
        .await
        .unwrap();
    let grandchild_start = worker_start_for(&fixture.starts, &grandchild);
    assert_eq!(
        grandchild_start.fork_messages.as_deref(),
        Some(history.as_slice()),
        "the child's uploaded history reaches the forked grandchild worker"
    );
}

/// The root capability is per-attachment, and a worker capability is per-task:
/// a capability valid for neither the attachment nor the caller is refused, and
/// no upload state is created. This is the "still rejects a foreign caller"
/// guard that keeps widening `begin_fork_upload` honest.
#[tokio::test]
async fn a_wrong_capability_creates_no_upload() {
    let fixture = attached_root(None).await;
    let child = fixture
        .coordinator
        .spawn_child_and_admit(
            &fixture.session,
            &fixture.caller,
            "child objective".into(),
            yi_agent_core::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let other_child = fixture
        .coordinator
        .spawn_child_and_admit(
            &fixture.session,
            &fixture.caller,
            "second child".into(),
            yi_agent_core::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let other_capability = worker_start_for(&fixture.starts, &other_child)
        .message_capability
        .clone();

    // A root capability named for a child caller, and another child's worker
    // capability, are both invalid: neither authenticates this caller.
    for capability in [fixture.capability.clone(), other_capability] {
        let error = fixture
            .coordinator
            .begin_fork_upload(&fixture.session, &child, &capability, 4)
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeCoordinatorError::AuthorityDenied(_)),
            "a capability that is not the caller's own is refused, got {error:?}"
        );
    }
}
