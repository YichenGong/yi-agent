//! The attach gate on a managed daemon.
//!
//! The lifecycle unit tests prove the ledger's own algebra. What they cannot prove
//! is that the coordinator *consults* it: a gate that exists but is never asked is
//! indistinguishable from no gate at all, and the whole point of
//! `RootAuthorization` is that an App attach must not be honoured merely because
//! the caller knew a stable id.
//!
//! So these drive the real `attach_application_root` entry point. The unmanaged
//! case is asserted first and matters just as much: the CLI, the tests, and
//! `daemon serve` have no guardian, and their attach behaviour must not change.

use std::sync::Arc;

use tempfile::TempDir;
use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, StopWitness, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_lifecycle::authorization::{AuthorizationLedger, AuthorizationUpdate};
use yi_agent_lifecycle::types::LeaseMode;
use yi_agent_store::runtime::RuntimeCoordinator;

struct JoinedFactory;

/// An application root runs in place, so the runtime refuses to attach without a
/// workspace provider. This one hands back the requested project unchanged: the
/// gate under test is about authority, not about worktree provisioning.
struct InPlaceWorkspaces;

impl WorkerWorkspaceProvider for InPlaceWorkspaces {
    fn in_place_workspace(
        &self,
        root_session_id: &yi_agent_core::subagent::task::RootSessionId,
        task_id: &yi_agent_core::subagent::task::TaskId,
        _attempt_id: &yi_agent_core::subagent::task::AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace_for(
            &root_session_id.to_string(),
            &task_id.to_string(),
        ))
    }

    fn workspace_in(
        &self,
        task_id: &yi_agent_core::subagent::task::TaskId,
        workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let session = workdir.to_string_lossy().into_owned();
        Ok(workspace_for(&session, &task_id.to_string()))
    }

    fn read_only_workspace(
        &self,
        parent: Option<&WorkerWorkspace>,
        task_id: &yi_agent_core::subagent::task::TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let mut workspace = match parent {
            Some(parent) => parent.clone(),
            None => workspace_for("read-only", &task_id.to_string()),
        };
        workspace.lease_id = yi_agent_core::subagent::task::WorkspaceLeaseId::new();
        workspace.branch = String::new();
        workspace.parent_branch = String::new();
        workspace.base_commit = String::new();
        Ok(workspace)
    }
}

fn workspace_for(session: &str, task: &str) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
        repository_root: format!("/tmp/repositories/{session}").into(),
        path: format!("/tmp/repositories/{session}/.worktrees/{task}").into(),
        branch: format!("feat/task-{task}"),
        parent_branch: "main".into(),
        base_commit: "fedcba9876543210fedcba9876543210fedcba98".into(),
    }
}

impl AgentWorkerFactory for JoinedFactory {
    fn default_workspace_service(&self) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(InPlaceWorkspaces))
    }

    fn recovery_context(&self) -> WorkerRecoveryContext {
        Default::default()
    }

    fn start(
        &self,
        request: WorkerStart,
    ) -> futures::future::BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
        Box::pin(async move {
            let (witness, confirmation) = StopWitness::pair();
            witness.mark_joined();
            Ok(WorkerHandle::new(request.cancellation).with_confirmation(confirmation))
        })
    }
}

/// A coordinator over a fresh database, started the way the daemon starts one.
fn coordinator(directory: &TempDir) -> RuntimeCoordinator {
    RuntimeCoordinator::open(
        directory.path().join("runtime.sqlite"),
        Arc::new(JoinedFactory),
    )
    .expect("a fresh database must open")
}

/// The guardian's half of the handshake, pinned to the daemon it will talk to.
///
/// A managed daemon now refuses a ledger whose generation is not its own, so a
/// fixture cannot invent one: the guardian must be told the daemon's generation,
/// which is exactly the value the real bring-up passes down from the guardian's
/// ledger. `verifier` is used on both sides so the secret is the daemon's too.
fn guardian_for(directory: &TempDir, coordinator: &RuntimeCoordinator) -> AuthorizationLedger {
    AuthorizationLedger::new(directory.path().to_string_lossy(), coordinator.generation())
}

fn verifier_for(
    directory: &TempDir,
    coordinator: &RuntimeCoordinator,
    secret: [u8; 32],
) -> AuthorizationLedger {
    AuthorizationLedger::verifier(
        directory.path().to_string_lossy(),
        coordinator.generation(),
        secret,
    )
}

/// A root id shaped like the ones the guardian derives: a canonical UUID.
const ROOT_ID: &str = "f3a91c48-2d6b-4e07-8a15-6c9b0d2e4f71";

fn workspace(directory: &TempDir) -> std::path::PathBuf {
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    project
}

/// The compatibility contract: a daemon with no guardian admits the same attach it
/// always did, keyed by the caller's own idempotency key.
#[tokio::test]
async fn an_unmanaged_daemon_attaches_with_the_callers_own_key() {
    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    assert!(!coordinator.is_managed());

    let workspace = workspace(&directory);
    let attached = coordinator
        .attach_application_root("project:1234:/p:uuid", &workspace, &[])
        .await
        .expect("an unmanaged attach must keep working");
    assert!(!attached.root_task_id.to_string().is_empty());
}

/// A managed daemon refuses an attach whose key the guardian never minted.
///
/// The key here is a plausible-looking application-root id: exactly what an
/// app-server would have sent before this change, and exactly what a caller who
/// guessed a client id would send.
#[tokio::test]
async fn a_managed_daemon_refuses_an_attach_the_guardian_never_authorized() {
    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);

    let ledger = verifier_for(&directory, &coordinator, [3u8; 32]);
    coordinator
        .adopt_managed_authority(ledger)
        .expect("the ledger names this coordinator\'s generation");
    assert!(coordinator.is_managed());

    let error = coordinator
        .attach_application_root("project:1234:/p:uuid", &workspace, &[])
        .await
        .expect_err("a managed daemon must not admit an unauthorized attach");
    let rendered = format!("{error}");
    assert!(rendered.contains("authorization"), "{rendered}");
}

/// The authorized case, end to end: a capability the guardian minted *is* the
/// attach key, and the attach succeeds.
#[tokio::test]
async fn a_managed_daemon_admits_the_capability_its_guardian_minted() {
    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);

    // The guardian's half, in-process: same secret, same generation.
    let mut guardian = guardian_for(&directory, &coordinator);
    let secret = guardian.generation_secret();
    // The root id is a daemon task id, which is a UUID: a managed attach now creates
    // exactly the root named here, so a fixture that passed a directory would be
    // asserting an identity the daemon could never create.
    let authorization = guardian
        .issue("app-run-1", ROOT_ID, LeaseMode::Business)
        .expect("a business client must be authorizable");

    let mut daemon = verifier_for(&directory, &coordinator, secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .expect("the verifier must accept its guardian's binding");
    coordinator
        .adopt_managed_authority(daemon)
        .expect("the ledger names this coordinator\'s generation");

    let attached = coordinator
        .attach_application_root(&authorization.capability, &workspace, &[])
        .await
        .expect("an authorized attach must be admitted");
    // The owner is the guardian's client, not the capability: the capability is a
    // secret and must never become the identity the daemon binds a root to.
    assert!(
        coordinator
            .authorize_work("app-run-1", &attached.root_task_id.to_string())
            .is_ok(),
        "the root must be owned by the client the guardian authorized"
    );
    assert!(
        coordinator
            .authorize_work(
                &authorization.capability,
                &attached.root_task_id.to_string()
            )
            .is_err(),
        "the capability must not act as a client id"
    );
}

/// A released client's capability stops working, which is what makes reclamation
/// real: authority must not outlive the lease that justified it.
#[tokio::test]
async fn a_released_client_cannot_attach_with_its_old_capability() {
    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);

    let mut guardian = guardian_for(&directory, &coordinator);
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue("app-run-1", ROOT_ID, LeaseMode::Business)
        .unwrap();
    // A verifier only knows what its guardian tells it, so the binding can be
    // installed; the withdrawal is what has to arrive next. That ordering is also
    // the realistic one: the release happens while the daemon is serving.
    let mut daemon = verifier_for(&directory, &coordinator, secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .unwrap();
    coordinator
        .adopt_managed_authority(daemon)
        .expect("the ledger names this coordinator\'s generation");
    coordinator
        .apply_authority_update(AuthorizationUpdate::RevokeClient {
            client_run_id: "app-run-1".to_string(),
        })
        .expect("the release must reach the daemon");

    let error = coordinator
        .attach_application_root(&authorization.capability, &workspace, &[])
        .await
        .expect_err("a released client must not attach");
    assert!(format!("{error}").contains("authorization"), "{error}");
}

/// A capability for one project must not attach a different one: the binding names
/// a root, and honouring it elsewhere would put work in the wrong runtime.
#[tokio::test]
async fn a_capability_for_another_project_is_refused() {
    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);
    let elsewhere = directory.path().join("other");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let mut guardian = guardian_for(&directory, &coordinator);
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue(
            "app-run-1",
            &elsewhere.to_string_lossy(),
            LeaseMode::Business,
        )
        .unwrap();

    let mut daemon = verifier_for(&directory, &coordinator, secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .unwrap();
    coordinator
        .adopt_managed_authority(daemon)
        .expect("the ledger names this coordinator\'s generation");

    let error = coordinator
        .attach_application_root(&authorization.capability, &workspace, &[])
        .await
        .expect_err("a capability for another root must not attach this one");
    assert!(format!("{error}").contains("names"), "{error}");
}

/// Revocation arrives after the daemon is already serving, so it has to be applied
/// through the live update path rather than only at adoption.
#[tokio::test]
async fn a_revocation_applied_while_serving_takes_effect() {
    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);

    let mut guardian = guardian_for(&directory, &coordinator);
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue("app-run-1", ROOT_ID, LeaseMode::Business)
        .unwrap();

    let mut daemon = verifier_for(&directory, &coordinator, secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .unwrap();
    coordinator
        .adopt_managed_authority(daemon)
        .expect("the ledger names this coordinator\'s generation");

    // Authorized now...
    coordinator
        .attach_application_root(&authorization.capability, &workspace, &[])
        .await
        .expect("the first attach must be admitted");

    // ...and after the guardian withdraws it, the same key is refused.
    coordinator
        .apply_authority_update(AuthorizationUpdate::RevokeClient {
            client_run_id: "app-run-1".to_string(),
        })
        .expect("a live revocation must apply");
    let error = coordinator
        .attach_application_root(&authorization.capability, &workspace, &[])
        .await
        .expect_err("the withdrawn capability must stop working");
    assert!(format!("{error}").contains("authorization"), "{error}");
}
