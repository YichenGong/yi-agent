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

    let ledger =
        AuthorizationLedger::verifier(directory.path().to_string_lossy(), "daemon-1", [3u8; 32]);
    coordinator.adopt_managed_authority(ledger);
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
    let mut guardian = AuthorizationLedger::new(directory.path().to_string_lossy(), "daemon-1");
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue(
            "app-run-1",
            &workspace.to_string_lossy(),
            LeaseMode::Business,
        )
        .expect("a business client must be authorizable");

    let mut daemon =
        AuthorizationLedger::verifier(directory.path().to_string_lossy(), "daemon-1", secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .expect("the verifier must accept its guardian's binding");
    coordinator.adopt_managed_authority(daemon);

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

    let mut guardian = AuthorizationLedger::new(directory.path().to_string_lossy(), "daemon-1");
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue(
            "app-run-1",
            &workspace.to_string_lossy(),
            LeaseMode::Business,
        )
        .unwrap();
    // A verifier only knows what its guardian tells it, so the binding can be
    // installed; the withdrawal is what has to arrive next. That ordering is also
    // the realistic one: the release happens while the daemon is serving.
    let mut daemon =
        AuthorizationLedger::verifier(directory.path().to_string_lossy(), "daemon-1", secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .unwrap();
    coordinator.adopt_managed_authority(daemon);
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

    let mut guardian = AuthorizationLedger::new(directory.path().to_string_lossy(), "daemon-1");
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue(
            "app-run-1",
            &elsewhere.to_string_lossy(),
            LeaseMode::Business,
        )
        .unwrap();

    let mut daemon =
        AuthorizationLedger::verifier(directory.path().to_string_lossy(), "daemon-1", secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .unwrap();
    coordinator.adopt_managed_authority(daemon);

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

    let mut guardian = AuthorizationLedger::new(directory.path().to_string_lossy(), "daemon-1");
    let secret = guardian.generation_secret();
    let authorization = guardian
        .issue(
            "app-run-1",
            &workspace.to_string_lossy(),
            LeaseMode::Business,
        )
        .unwrap();

    let mut daemon =
        AuthorizationLedger::verifier(directory.path().to_string_lossy(), "daemon-1", secret);
    daemon
        .apply(AuthorizationUpdate::Bind(Box::new(authorization.clone())))
        .unwrap();
    coordinator.adopt_managed_authority(daemon);

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

/// A legacy capability is spent exactly once.
///
/// Replaying it must not be a second upgrade: the whole point of the one-shot is
/// that an old application secret cannot be turned into a permanent resume
/// credential every time an attacker repeats it.
#[tokio::test]
async fn session_delegation_lifecycle_legacy_identity_is_not_public_key() {
    use yi_agent_lifecycle::foreground::SecretCapability;
    use yi_agent_lifecycle::session_delegation::{
        PrepareSession, SessionFailureCode, SessionIdentity, SessionRef, session_resume_digest,
    };

    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);
    let attached = coordinator
        .attach_application_root("project:legacy:/p:1", &workspace, &[])
        .await
        .expect("an unmanaged attach must keep working");
    let root = SessionRef {
        session_id: attached.session_id.to_string(),
        root_id: attached.root_task_id.to_string(),
    };
    // The attach response is the only place the legacy capability ever travels.
    let legacy_capability = attached.message_capability.clone();
    let resume_secret = SecretCapability::new("resume-secret");
    let resume_digest = session_resume_digest(&resume_secret);

    // A key that was never minted buys nothing, not even a first upgrade.
    let forged = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-forged".into(),
            host_run_id: "host-1".into(),
            expected_binding_epoch: 0,
            identity: SessionIdentity::LegacyUpgrade(SecretCapability::new("not-the-secret")),
            current_binding: None,
            replacement_resume_digest: Some("resume-digest".into()),
            stopped: Vec::new(),
        })
        .unwrap_err();
    assert!(forged.to_string().contains("AuthorityDenied"), "{forged}");

    let upgraded = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-upgrade".into(),
            host_run_id: "host-1".into(),
            expected_binding_epoch: 0,
            identity: SessionIdentity::LegacyUpgrade(SecretCapability::new(
                legacy_capability.clone(),
            )),
            current_binding: None,
            replacement_resume_digest: Some(resume_digest.clone()),
            stopped: Vec::new(),
        })
        .expect("the holder of the attached capability may upgrade once");
    assert_eq!(upgraded.fence.binding_epoch, 1);

    let replay = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-replay".into(),
            host_run_id: "host-2".into(),
            expected_binding_epoch: 1,
            identity: SessionIdentity::LegacyUpgrade(SecretCapability::new(
                legacy_capability.clone(),
            )),
            current_binding: None,
            replacement_resume_digest: Some("resume-digest-2".into()),
            stopped: Vec::new(),
        })
        .unwrap_err();
    assert!(
        matches!(
            replay,
            yi_agent_store::runtime::RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::AuthorityDenied,
                ..
            }
        ),
        "{replay}"
    );

    // After the upgrade the private resume secret is the identity that works.
    let resumed = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-resume".into(),
            host_run_id: "host-2".into(),
            expected_binding_epoch: 1,
            identity: SessionIdentity::Embedded(resume_secret.clone()),
            current_binding: None,
            replacement_resume_digest: None,
            stopped: Vec::new(),
        })
        .expect("the private resume secret resumes the container");
    assert_eq!(resumed.fence.binding_epoch, 2);
}

/// A managed identity on a daemon with no guardian is refused, not trusted.
#[tokio::test]
async fn session_delegation_lifecycle_managed_identity_needs_a_guardian() {
    use yi_agent_lifecycle::session_delegation::{
        PrepareSession, SessionFailureCode, SessionIdentity, SessionRef,
    };
    use yi_agent_lifecycle::types::RootAuthorization;

    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    assert!(!coordinator.is_managed());
    let workspace = workspace(&directory);
    let attached = coordinator
        .attach_application_root("project:managed:/p:1", &workspace, &[])
        .await
        .unwrap();

    let error = coordinator
        .prepare_session(PrepareSession {
            root: SessionRef {
                session_id: attached.session_id.to_string(),
                root_id: attached.root_task_id.to_string(),
            },
            operation_id: "op-managed".into(),
            host_run_id: "host-1".into(),
            expected_binding_epoch: 0,
            identity: SessionIdentity::Managed(RootAuthorization {
                scope_key: "scope".into(),
                daemon_instance_id: "daemon".into(),
                client_run_id: "host-1".into(),
                root_id: attached.root_task_id.to_string(),
                capability: "capability".into(),
            }),
            current_binding: None,
            replacement_resume_digest: None,
            stopped: Vec::new(),
        })
        .unwrap_err();
    assert!(
        matches!(
            error,
            yi_agent_store::runtime::RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::AuthorityDenied,
                ..
            }
        ),
        "{error}"
    );
}

/// An unsettled execution of the previous driver blocks a replacement.
///
/// The blocking is the feature: without it a new host could take over while the
/// old one's tools may still be writing, which is exactly what "no stop proof"
/// must never permit.
#[tokio::test]
async fn session_delegation_lifecycle_replacement_requires_a_stop_proof() {
    use yi_agent_lifecycle::foreground::SecretCapability;
    use yi_agent_lifecycle::session_delegation::{
        PrepareSession, SessionFailureCode, SessionIdentity, SessionRef, session_resume_digest,
    };

    let directory = TempDir::new().unwrap();
    let coordinator = coordinator(&directory);
    let workspace = workspace(&directory);
    let attached = coordinator
        .attach_application_root("project:stop:/p:1", &workspace, &[])
        .await
        .unwrap();
    let root = SessionRef {
        session_id: attached.session_id.to_string(),
        root_id: attached.root_task_id.to_string(),
    };
    let resume_secret = SecretCapability::new("resume-secret");
    let resume_digest = session_resume_digest(&resume_secret);
    let binding = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-1".into(),
            host_run_id: "host-1".into(),
            expected_binding_epoch: 0,
            identity: SessionIdentity::Embedded(resume_secret.clone()),
            current_binding: None,
            replacement_resume_digest: None,
            stopped: Vec::new(),
        })
        .unwrap_err();
    // The fresh container has no resume digest, so the first prepare must say so
    // rather than silently accepting a secret it never stored.
    assert!(
        matches!(
            binding,
            yi_agent_store::runtime::RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::SessionIdentityRequired,
                ..
            }
        ),
        "{binding}"
    );

    let upgraded = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-upgrade".into(),
            host_run_id: "host-1".into(),
            expected_binding_epoch: 0,
            identity: SessionIdentity::LegacyUpgrade(SecretCapability::new(
                attached.message_capability.clone(),
            )),
            current_binding: None,
            replacement_resume_digest: Some(resume_digest.clone()),
            stopped: Vec::new(),
        })
        .expect("upgrade binds the first driver");

    // Simulate a host that started a turn and vanished without a clean stop.
    coordinator
        .record_host_execution(
            yi_agent_store::runtime::session_delegation::RegisterExecution {
                root_task_id: attached.root_task_id,
                fence: upgraded.fence.clone(),
                turn_id: "turn-1".to_string(),
            },
        )
        .expect("an execution is registered before it starts");

    let blocked = coordinator
        .prepare_session(PrepareSession {
            root: root.clone(),
            operation_id: "op-2".into(),
            host_run_id: "host-2".into(),
            expected_binding_epoch: upgraded.fence.binding_epoch,
            identity: SessionIdentity::Embedded(resume_secret.clone()),
            current_binding: None,
            replacement_resume_digest: None,
            stopped: Vec::new(),
        })
        .unwrap_err();
    assert!(
        matches!(
            blocked,
            yi_agent_store::runtime::RuntimeCoordinatorError::SessionDelegation {
                code: SessionFailureCode::StopUnproven,
                ..
            }
        ),
        "{blocked}"
    );
}
