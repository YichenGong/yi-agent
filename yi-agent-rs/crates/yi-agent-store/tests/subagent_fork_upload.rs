//! The store-side half of conversation forking: a caller uploads its history as
//! base64 chunks and only receives decoded messages once the declared byte count
//! has arrived. These tests exercise the in-memory state machine through the
//! coordinator, with an attached application root as the authorized caller.

use std::sync::Arc;

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

/// The upload machine never starts a worker; the factory only has to resolve the
/// read-only application-root workspace that attaching the root requires.
#[derive(Default)]
struct StaticWorkspaceFactory;

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
        Some(Arc::new(StaticWorkspaceFactory))
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
        Box::pin(async move { Ok(WorkerHandle::new(request.cancellation)) })
    }
}

struct ForkFixture {
    _directory: TempDir,
    coordinator: RuntimeCoordinator,
    session: RootSessionId,
    caller: TaskId,
    capability: String,
}

/// Attaches a real application root so the fork upload has a capability-bearing
/// caller. `fork_max_bytes` overrides the payload ceiling when supplied.
async fn attached_root(fork_max_bytes: Option<u64>) -> ForkFixture {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator =
        RuntimeCoordinator::open(&database, Arc::new(StaticWorkspaceFactory)).unwrap();
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
        .unwrap_err();
    assert!(
        matches!(error, RuntimeCoordinatorError::AuthorityDenied(_)),
        "a forged capability cannot open an upload, got {error:?}"
    );
}
