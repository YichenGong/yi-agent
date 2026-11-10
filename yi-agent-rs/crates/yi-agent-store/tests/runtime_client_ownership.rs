//! Client ownership and admission, driven through the public store API.
//!
//! The unit tests in `lifecycle.rs` cover the state machine in isolation. These
//! cover the property the spec actually cares about: releasing one frontend must
//! not strand or steal another frontend's tree, including descendants created by
//! tasks rather than by the frontend directly.

use yi_agent_store::lifecycle::{ExitReason, RuntimeClients};

#[test]
fn closing_app_admission_preserves_tui_root() {
    let mut clients = RuntimeClients::default();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_root("tui", "thread:b", "g1").unwrap();
    assert_eq!(
        clients.close_client_admission("app"),
        vec!["thread:a".to_string()]
    );
    assert!(clients.authorize("app", "thread:a", "g1").is_err());
    assert!(clients.authorize("tui", "thread:b", "g1").is_ok());
}

#[test]
fn ownership_propagates_to_grandchildren() {
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_child_root("thread:a", "task:1", "g1").unwrap();
    clients.bind_child_root("task:1", "task:2", "g1").unwrap();

    // The grandchild belongs to the app, so the TUI cannot admit under it.
    assert!(clients.authorize("app", "task:2", "g1").is_ok());
    assert!(clients.authorize("tui", "task:2", "g1").is_err());
}

#[test]
fn closing_one_client_does_not_reclaim_the_others_descendants() {
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients
        .bind_child_root("thread:a", "task:app-1", "g1")
        .unwrap();
    clients.bind_root("tui", "thread:b", "g1").unwrap();
    clients
        .bind_child_root("thread:b", "task:tui-1", "g1")
        .unwrap();

    clients.close_client_admission("app");

    // The TUI's own tree still admits work...
    assert!(clients.authorize("tui", "thread:b", "g1").is_ok());
    assert!(clients.authorize("tui", "task:tui-1", "g1").is_ok());
    // ...while everything under the app is closed.
    assert!(clients.authorize("app", "thread:a", "g1").is_err());
    assert!(clients.authorize("app", "task:app-1", "g1").is_err());
}

#[test]
fn a_root_cannot_be_stolen_by_thread_id() {
    // A new frontend that guesses a live thread id must not inherit the session
    // (and with it, the authority to drive someone else's running work).
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    let error = clients.bind_root("tui", "thread:a", "g1").unwrap_err();
    assert!(error.contains("owned by client app"), "{error}");
}

#[test]
fn an_exit_reason_is_recorded_once_per_root() {
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.close_client_admission("app");
    // A maintenance barrier over an already-closed root must not add a second row.
    clients.begin_maintenance(1).unwrap();
    clients.close_client_admission("app");

    let exited: Vec<_> = clients.exited().iter().collect();
    assert_eq!(exited.len(), 1, "{exited:?}");
    assert_eq!(exited[0].reason, ExitReason::ClientReleased);
    assert_eq!(exited[0].owner, "app");
    assert_eq!(exited[0].generation, "g1");
    assert_eq!(exited[0].reason.as_str(), "client_released");
}

#[test]
fn binding_is_idempotent_for_the_same_owner_and_generation() {
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    assert_eq!(clients.active_count(), 1);
}

#[test]
fn a_new_generation_must_rebind_explicitly() {
    // Reusing a root across generations without rebinding would silently carry
    // the previous generation's authority into the new daemon.
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    assert!(clients.bind_root("app", "thread:a", "g2").is_err());
    assert!(clients.authorize("app", "thread:a", "g2").is_err());
}

#[test]
fn an_unbound_root_is_not_authorized() {
    let clients = RuntimeClients::new();
    assert!(clients.authorize("app", "thread:a", "g1").is_err());
}

#[test]
fn a_closed_client_cannot_bind_new_roots() {
    let mut clients = RuntimeClients::new();
    clients.close_client_admission("app");
    assert!(clients.bind_root("app", "thread:new", "g1").is_err());
    assert!(clients.is_closed("app"));
}

#[test]
fn roots_of_lists_only_the_clients_roots_in_order() {
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:z", "g1").unwrap();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_root("tui", "thread:b", "g1").unwrap();
    assert_eq!(
        clients.roots_of("app"),
        vec!["thread:a".to_string(), "thread:z".to_string()]
    );
}

// ---- persistence ----------------------------------------------------------

#[test]
fn a_binding_and_its_exit_reason_survive_a_reopen() {
    // The in-memory table is per generation; this is what lets a restart explain
    // what happened and distinguish "never bound" from "bound then closed".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let repository = yi_agent_store::repository::RuntimeRepository::open(&path).unwrap();

    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    let recorded = yi_agent_store::lifecycle::store::record_client_close(
        &repository,
        &clients,
        "app",
        "g1",
        ExitReason::ClientReleased,
    )
    .unwrap();
    assert_eq!(recorded, vec!["thread:a".to_string()]);
    drop(repository);

    let reopened = yi_agent_store::repository::RuntimeRepository::open(&path).unwrap();
    let (owner, generation, reason) = reopened.root_owner("thread:a").unwrap().unwrap();
    assert_eq!(owner, "app");
    assert_eq!(generation, "g1");
    assert_eq!(reason.as_deref(), Some("client_released"));
}

#[test]
fn the_first_terminal_reason_is_kept() {
    // Overwriting would erase the difference between "the client left" and
    // "maintenance drained it", which is exactly what an operator needs to know.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let repository = yi_agent_store::repository::RuntimeRepository::open(&path).unwrap();

    repository
        .record_root_owner("thread:a", "app", "g1", None)
        .unwrap();
    repository
        .mark_root_exited("thread:a", ExitReason::ClientReleased.as_str())
        .unwrap();
    repository
        .mark_root_exited("thread:a", ExitReason::Maintenance.as_str())
        .unwrap();

    let (_, _, reason) = repository.root_owner("thread:a").unwrap().unwrap();
    assert_eq!(reason.as_deref(), Some("client_released"));
}

#[test]
fn rebinding_in_a_new_generation_clears_the_old_exit() {
    // A session reopened in a later daemon generation is live again; leaving the old
    // reason in place would make the next restart treat it as already closed.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");
    let repository = yi_agent_store::repository::RuntimeRepository::open(&path).unwrap();

    repository
        .record_root_owner("thread:a", "app", "g1", Some("client_released"))
        .unwrap();
    repository
        .record_root_owner("thread:a", "app", "g2", None)
        .unwrap();

    let (owner, generation, reason) = repository.root_owner("thread:a").unwrap().unwrap();
    assert_eq!(owner, "app");
    assert_eq!(generation, "g2");
    assert_eq!(reason, None);
}

#[test]
fn an_unknown_root_has_no_owner_record() {
    let dir = tempfile::tempdir().unwrap();
    let repository =
        yi_agent_store::repository::RuntimeRepository::open(dir.path().join("runtime.sqlite"))
            .unwrap();
    assert!(repository.root_owner("thread:none").unwrap().is_none());
}

// ---- real coordinator: ownership races ------------------------------------

use std::sync::Arc;

use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, StopWitness, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    WorkerWorkspace, WorkerWorkspaceProvider,
};
use yi_agent_store::runtime::RuntimeCoordinator;

struct JoinedFactory;

/// A workspace provider is required for an application root to attach: the root
/// runs in place, and the runtime refuses to attach without one.
struct StaticWorkspaces;

impl WorkerWorkspaceProvider for StaticWorkspaces {
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
        _workdir: &std::path::Path,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(workspace_for("resolved", &task_id.to_string()))
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
        Some(Arc::new(StaticWorkspaces))
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

fn open_coordinator() -> (tempfile::TempDir, RuntimeCoordinator) {
    let directory = tempfile::tempdir().unwrap();
    let coordinator = RuntimeCoordinator::open(
        directory.path().join("runtime.sqlite"),
        Arc::new(JoinedFactory),
    )
    .unwrap();
    (directory, coordinator)
}

#[tokio::test]
async fn a_released_client_cannot_admit_new_descendants() {
    // The escape the plan names: a descendant spawned after the client's release
    // must not be admitted just because the tree already exists.
    let (_directory, coordinator) = open_coordinator();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.bind_root("app", &root.to_string()).unwrap();

    coordinator.close_client_admission("app");

    let error = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "should not start".into(),
            yi_agent_core::subagent::task::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(
        error.is_err(),
        "a closed client's root must not admit a new child"
    );
}

#[tokio::test]
async fn a_descendant_inherits_its_parents_owner() {
    let (_directory, coordinator) = open_coordinator();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.bind_root("app", &root.to_string()).unwrap();
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "child".into(),
            yi_agent_core::subagent::task::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    coordinator
        .bind_child_root(&root.to_string(), &child.to_string())
        .unwrap();

    // The app owns the grandchild through the parent chain...
    assert!(
        coordinator
            .authorize_work("app", &child.to_string())
            .is_ok()
    );
    // ...and no other client may act under it.
    assert!(
        coordinator
            .authorize_work("tui", &child.to_string())
            .is_err()
    );
}

#[tokio::test]
async fn release_and_spawn_racing_never_leaves_an_admitted_orphan() {
    // The plan asks for a barrier-driven race between client release and a
    // descendant spawn. Whichever order wins, the invariant is the same: either
    // the spawn was admitted before admission closed (and then the root is still
    // the app's), or it was refused. It must never be admitted *after* the
    // close, because that work would have no owner to reclaim it.
    for _ in 0..25 {
        let (_directory, coordinator) = open_coordinator();
        let session = coordinator.create_session().unwrap();
        let root = coordinator.root_task_id(&session).unwrap();
        coordinator.bind_root("app", &root.to_string()).unwrap();

        let coordinator = Arc::new(coordinator);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let release = {
            let coordinator = Arc::clone(&coordinator);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                coordinator.close_client_admission("app");
            })
        };
        let spawn = {
            let coordinator = Arc::clone(&coordinator);
            let barrier = Arc::clone(&barrier);
            let session = session.clone();
            let root = root.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                coordinator
                    .spawn_child_with_objective(
                        &session,
                        &root,
                        "racer".into(),
                        yi_agent_core::subagent::task::ChildWriteMode::ReadOnly,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await
            })
        };
        release.await.unwrap();
        let spawned = spawn.await.unwrap();

        // Each iteration asserts the invariant that matters: a child that was
        // admitted is attributable to its owner, and a child that could not be
        // admitted does not exist as work. Both properties must hold without the
        // test binding anything itself -- the production path records ownership as
        // part of the commit, and a helper doing it here would hide the case where it
        // does not.
        match spawned {
            Ok(child) => {
                // The client may already be closed by now, so authorization is
                // *not* the invariant. The invariant is discoverability: the child
                // must already be enumerable as the app's, or reclamation could never
                // find the work it admitted.
                assert!(
                    coordinator
                        .owned_roots_of("app")
                        .contains(&child.to_string()),
                    "admitted work must remain attributable to its owner"
                );
            }
            Err(error) => {
                let rendered = error.to_string();
                assert!(
                    rendered.contains("admission")
                        || rendered.contains("rejects new work")
                        || rendered.contains("closed"),
                    "a refusal must explain itself, got: {rendered}"
                );
            }
        }
    }
}

#[tokio::test]
async fn attaching_an_application_root_binds_its_owner() {
    // The bind must happen on the real attach path, not only when a test calls
    // `bind_root` directly: otherwise nothing in production ever populates the
    // ownership table and every admission check passes vacuously.
    let (_directory, coordinator) = open_coordinator();
    let workspace = tempfile::tempdir().unwrap();
    let attached = coordinator
        .attach_application_root("app", workspace.path(), &[], None)
        .await
        .unwrap();

    assert_eq!(
        coordinator.owned_roots_of("app"),
        vec![attached.root_task_id.to_string()]
    );
    // A different client cannot act under the app's root.
    assert!(
        coordinator
            .authorize_work("tui", &attached.root_task_id.to_string())
            .is_err()
    );
}

#[tokio::test]
async fn a_released_owner_cannot_reattach() {
    // Closing admission is the point of no return for this daemon generation:
    // re-attaching would otherwise resurrect an identity whose work was already
    // reclaimed.
    let (_directory, coordinator) = open_coordinator();
    let workspace = tempfile::tempdir().unwrap();
    coordinator
        .attach_application_root("app", workspace.path(), &[], None)
        .await
        .unwrap();
    coordinator.close_client_admission("app");

    let error = coordinator
        .attach_application_root("app", workspace.path(), &[], None)
        .await
        .expect_err("a closed owner must not re-attach in the same generation");
    assert!(
        error.to_string().contains("closed admission"),
        "the refusal must name the reason, got: {error}"
    );
}

#[test]
fn foreground_root_lifecycle_descendants_are_part_of_the_closing_tree() {
    // A release that reclaimed only the roots bound directly to the frontend would
    // leave grandchildren running with nobody to ask. Descendants record the client as
    // their owner, so the returned tree must already contain them -- the property is
    // that the reclaim set is complete, not that a walk happens.
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_root("tui", "thread:b", "g1").unwrap();
    clients.bind_child_root("thread:a", "task:1", "g1").unwrap();
    clients.bind_child_root("task:1", "task:2", "g1").unwrap();
    clients
        .bind_child_root("thread:b", "task:other", "g1")
        .unwrap();

    assert_eq!(
        clients.close_client_admission("app"),
        vec![
            "task:1".to_string(),
            "task:2".to_string(),
            "thread:a".to_string()
        ],
        "the closing client's whole tree, and nothing of the TUI's"
    );
    // Nothing of the other client's tree moved.
    assert!(clients.authorize("tui", "task:other", "g1").is_ok());
    assert!(clients.authorize("app", "task:2", "g1").is_err());
}

#[test]
fn foreground_root_lifecycle_release_is_reported_once() {
    // A reclaim loop may run twice (a retry after a transient stop failure); the
    // second pass must return the same tree rather than an empty or growing one, or
    // the retry would look like there was nothing left to reclaim.
    let mut clients = RuntimeClients::new();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_child_root("thread:a", "task:1", "g1").unwrap();
    let first = clients.close_client_admission("app");
    let second = clients.close_client_admission("app");
    assert_eq!(first, second);
    assert_eq!(clients.exited().len(), 2);
}
