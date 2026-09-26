# Subagent Worktree Recycling Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make a child subagent's delivered commit actually reach its parent's branch, and automatically
recycle the child's worktree, branch, and `task_workspaces` row once that integration is proven.

**Architecture:** Three layers. (A1) The shared system prompt tells a parent agent to `git merge --no-ff`
a delivered commit into its own worktree. (A2) On every daemon reconcile pass, the runtime sweeps tasks in
`AwaitingParentReview`, and for each one whose delivered commit is already an ancestor of its parent's
worktree HEAD, calls `RuntimeCoordinator::accept_review(child, IntegrationValidation::passed(..))`. (B) The
tail of `accept_review` calls a new `AgentWorkspaceService::cleanup_accepted` to remove the child worktree
and branch, then deletes the child's `task_workspaces` row. A2's ancestry check is the only proof of
integration; the runtime never parses agent text.

**Tech Stack:** Rust (edition 2024), cargo workspace under `yi-agent-rs/`, `git` subprocesses for worktrees,
SQLite for the runtime repository, `wiremock`/unit tests only (no API key needed).

---

## Before you start

Read these first:

- `docs/superpowers/specs/2026-09-26-subagent-worktree-recycling-design.md` — the approved design. Its A2
  section was corrected during planning: the sweep is **not** keyed to a parent completion event, because
  the application root is not a daemon worker (it runs in-process in the TUI / headless driver). Read the
  corrected A2 rationale before touching Task 7.
- `CLAUDE.md` (repo root) — branch/worktree rules, `cargo fmt --all` before commit, no `Co-Authored-By`
  trailer, per-crate test runs, no concurrent cargo processes.

All commands below run from the worktree root
`/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/subagent-worktree-recycling`.
Use `cargo test --manifest-path yi-agent-rs/Cargo.toml ...` so the worktree's own `target/` is used. Before
every `cargo test`, run `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` and kill stray test
binaries if any (they hold the target lock and cause exit 137 / hangs).

Commit after each task with a conventional-commit message and no `Co-Authored-By` line.

---

## Task 1: `WorktreeService::contains_commit`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`

**Step 1: Write the failing test**

Append to `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`:

```rust
#[test]
fn contains_commit_reports_whether_a_revision_is_an_ancestor_of_head() {
    let (repo, base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-contains");
    let root = service
        .create_root(repo.path(), "feat/yi-contains", &root_path)
        .unwrap();

    assert!(service.contains_commit(&root.path, &base).unwrap());

    let child_path = repo.path().join(".worktrees/yi-contains-child");
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-contains-child", &child_path)
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);
    let child_head = git(&child.path, &["rev-parse", "HEAD"]);

    // The child commit exists but is not yet in the parent's HEAD.
    assert!(!service.contains_commit(&root.path, &child_head).unwrap());

    git(&root.path, &["merge", "--no-ff", &child_head, "-m", "integrate"]);
    assert!(service.contains_commit(&root.path, &child_head).unwrap());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --test subagent_worktree contains_commit`
Expected: FAIL to compile — `no method named contains_commit`.

**Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`, add this method inside `impl WorktreeService`,
directly above `remove_clean` (around line 294):

```rust
    /// Whether `rev` is already contained in the worktree's current HEAD.
    ///
    /// Exit code 0 means ancestor, 1 means not an ancestor, anything else is a
    /// real git error (for example an unknown revision).
    pub fn contains_commit(&self, worktree: &Path, rev: &str) -> Result<bool, WorktreeError> {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", rev, "HEAD"])
            .current_dir(worktree)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --test subagent_worktree contains_commit`
Expected: PASS (1 test).

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/worktree.rs yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs
git commit -m "feat(tools): add WorktreeService::contains_commit"
```

---

## Task 2: Trait methods and supervisor enumeration

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Test: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` (unit test module)

**Step 1: Write the failing test**

Add to the `#[cfg(test)]` module at the bottom of
`yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` (find it with
`grep -n "mod tests" yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`):

```rust
#[test]
fn tasks_awaiting_parent_review_lists_only_review_waiters() {
    use crate::subagent::task::DeliveryId;

    let session = RootSessionId::new();
    let mut supervisor = AgentSupervisor::new(session.clone());
    let root = supervisor.root_task_id().clone();
    let child = supervisor
        .spawn_with_objective(root.clone(), "child".into())
        .unwrap();
    let delivery = crate::subagent::task::DeliveryReport::coding(
        "deadbeef",
        "main",
        WorkspaceLeaseId::new(),
        "evidence",
    );
    supervisor
        .task_mut_for_test(&child)
        .set_state_for_test(TaskState::AwaitingParentReview(delivery.id.clone()));

    assert_eq!(supervisor.tasks_awaiting_parent_review(), vec![child]);
}

#[test]
fn tasks_awaiting_parent_review_is_empty_without_review_waiters() {
    let supervisor = AgentSupervisor::new(RootSessionId::new());
    assert!(supervisor.tasks_awaiting_parent_review().is_empty());
}
```

If `task_mut_for_test` / `set_state_for_test` do not exist, replace the first test body with a direct map
manipulation using the existing test helpers in that module. Inspect the module first
(`grep -n "fn task_mut_for_test\|fn set_state_for_test\|mod tests" .../supervisor.rs`) and reuse whatever
helper already mutates a task's state in existing tests. If none exists, add this helper inside
`impl AgentSupervisor` gated on `#[cfg(test)]`:

```rust
    #[cfg(test)]
    pub(crate) fn set_state_for_test(&mut self, task: &TaskId, state: TaskState) {
        let task = self.tasks.get_mut(task).expect("task exists");
        task.state = state;
    }
```

(Confirm `AgentTask::state` is a public field; it is declared `pub state: TaskState` at
`crates/yi-agent-core/src/subagent/task.rs:435`.)

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core --lib tasks_awaiting_parent_review`
Expected: FAIL to compile — `no method named tasks_awaiting_parent_review`.

**Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`, add this method next to `children_of`
(around line 324):

```rust
    /// Direct and transitive tasks currently waiting for their parent's review.
    pub fn tasks_awaiting_parent_review(&self) -> Vec<TaskId> {
        let mut ids = self
            .tasks
            .iter()
            .filter(|(_, task)| matches!(task.state(), TaskState::AwaitingParentReview(_)))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ids.sort_by_key(|id| id.to_string());
        ids
    }
```

Confirm `TaskState` is already imported in that file (it is used at line 679).

In `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`, add two defaulted methods to
`trait AgentWorkspaceService` (after `cleanup_prepared`, around line 433):

```rust
    /// Whether `commit` is already contained in `owner`'s worktree HEAD. The
    /// default cannot inspect git and therefore reports "not integrated".
    fn contains_commit(
        &self,
        _owner: &WorkerWorkspace,
        _commit: &str,
    ) -> Result<bool, WorkerError> {
        Ok(false)
    }

    /// Remove an accepted child's worktree and branch once `owner` provably
    /// contains its delivery. The default is a no-op for non-git services.
    fn cleanup_accepted(
        &self,
        _owner: &WorkerWorkspace,
        _child: &WorkerWorkspace,
    ) -> Result<(), WorkerError> {
        Ok(())
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core --lib tasks_awaiting_parent_review`
Expected: PASS (2 tests).

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs
git commit -m "feat(core): add workspace recycle hooks and review-waiter listing"
```

---

## Task 3: `DaemonWorkspaceService` implementation

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (unit test module)

**Step 1: Write the failing test**

Add to the test module in `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`, next to the existing
`daemon_workspace_cleanup_removes_prepared_root_worktree_and_branch` test (around line 1417). Reuse that
test's git-repository setup helpers verbatim (`grep -n "fn daemon_workspace_cleanup_removes_prepared_root_worktree_and_branch"
yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` to find it, and copy its repo-creation code):

```rust
#[test]
fn daemon_workspace_cleanup_accepted_removes_child_worktree_and_branch() {
    // --- copy the repo setup from daemon_workspace_cleanup_removes_prepared_root_worktree_and_branch ---
    let service = DaemonWorkspaceService::new(repo.path().to_path_buf());
    let root = service
        .prepare_root(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
        .unwrap();
    let child = service
        .prepare_child(&root, &RootSessionId::new(), &TaskId::new(), &AttemptId::new())
        .unwrap();

    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    run_git(&child.path, &["add", "delivery.txt"]);
    run_git(&child.path, &["commit", "-m", "child delivery"]);

    // Not integrated yet: the child head is not an ancestor of the owner,
    // and refusal is git-level, leaving the worktree in place.
    assert!(!service.contains_commit(&root, &child.branch).unwrap());
    let error = service.cleanup_accepted(&root, &child).unwrap_err();
    assert!(error.to_string().contains("has not been merged"), "unexpected error: {error}");
    assert!(child.path.exists());

    run_git(&root.path, &["merge", "--no-ff", &child.branch, "-m", "integrate"]);
    assert!(service.contains_commit(&root, &child.branch).unwrap());

    service.cleanup_accepted(&root, &child).unwrap();
    assert!(!child.path.exists());
    assert!(
        Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{}", child.branch)])
            .current_dir(&repo.path())
            .status()
            .unwrap()
            .code()
            .is_some_and(|code| code != 0)
    );
}
```

Use whatever git-command helper the neighbouring test already uses (for example a `git(dir, args)` helper);
rename `run_git` to match it. If the existing test builds `RootSessionId`/`TaskId`/`AttemptId` differently,
follow its exact construction. `RootSessionId`, `TaskId`, and `AttemptId` must be in scope; add imports
matching the existing test module.

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --lib daemon_workspace_cleanup_accepted`
Expected: FAIL to compile — `no method named cleanup_accepted` / `contains_commit` on `DaemonWorkspaceService`.

**Step 3: Write minimal implementation**

In `impl AgentWorkspaceService for DaemonWorkspaceService` in
`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`, add after `cleanup_prepared` (around line 280):

```rust
    fn contains_commit(
        &self,
        owner: &WorkerWorkspace,
        commit: &str,
    ) -> Result<bool, WorkerError> {
        self.service
            .contains_commit(&owner.path, commit)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }

    fn cleanup_accepted(
        &self,
        owner: &WorkerWorkspace,
        child: &WorkerWorkspace,
    ) -> Result<(), WorkerError> {
        self.service
            .remove_accepted_clean(
                &owner.path,
                &yi_agent_tools::worktree::ChildWorktree {
                    path: child.path.clone(),
                    branch: child.branch.clone(),
                    parent_branch: child.parent_branch.clone(),
                    base_commit: child.base_commit.clone(),
                },
            )
            .map_err(|error| WorkerError::Startup(format!("Git workspace cleanup error: {error}")))
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --lib daemon_workspace_cleanup`
Expected: PASS (2 tests: the existing `_prepared_root_...` and the new `_accepted_...`).

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat(app): implement accepted-worktree recycle in DaemonWorkspaceService"
```

---

## Task 4: `RuntimeRepository::delete_task_workspace`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/src/repository.rs` (unit test module)

**Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `yi-agent-rs/crates/yi-agent-store/src/repository.rs` (find it with
`grep -n "mod tests" .../repository.rs`). Reuse the module's existing repository/workspace helpers; the test
below assumes a helper that opens a fresh repository and one that records a workspace — adapt names to the
ones already present:

```rust
#[test]
fn delete_task_workspace_removes_the_row_and_is_idempotent() {
    let repository = test_repository();
    let task = TaskId::new();
    repository
        .record_task_workspace(&task, &AttemptId::new(), &test_workspace())
        .unwrap();
    assert!(repository.task_workspace_optional(&task).unwrap().is_some());

    repository.delete_task_workspace(&task).unwrap();
    assert!(repository.task_workspace_optional(&task).unwrap().is_none());

    // Second delete is a no-op, not an error.
    repository.delete_task_workspace(&task).unwrap();
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --lib delete_task_workspace`
Expected: FAIL to compile — `no method named delete_task_workspace`.

**Step 3: Write minimal implementation**

In `impl RuntimeRepository` in `yi-agent-rs/crates/yi-agent-store/src/repository.rs`, add directly after
`task_workspace_optional` (around line 3239):

```rust
    /// Deletes a task's workspace assignment row. Idempotent: a missing row is
    /// not an error, so recycling can be retried safely.
    pub fn delete_task_workspace(&self, task: &TaskId) -> Result<(), RepositoryError> {
        self.connection.execute(
            "DELETE FROM task_workspaces WHERE task_id = ?1",
            params![task.to_string()],
        )?;
        Ok(())
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --lib delete_task_workspace`
Expected: PASS (1 test).

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs
git commit -m "feat(store): add idempotent delete_task_workspace"
```

---

## Task 5: Recycling runtime events

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/src/repository.rs` (unit test module)

**Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `yi-agent-rs/crates/yi-agent-store/src/repository.rs`:

```rust
#[test]
fn recycle_events_round_trip_through_name_and_parse() {
    for event in [
        RuntimeEvent::TaskWorkspaceRecycled,
        RuntimeEvent::TaskWorkspaceRecycleFailed,
    ] {
        assert_eq!(RuntimeEvent::parse(event.name().to_string()).unwrap(), event);
    }
}
```

Note: `RuntimeEvent::name` and `RuntimeEvent::parse` are private (`fn name` / `fn parse`). This test lives in
the same module, so it can call them. If the module's existing tests already exercise `name`/`parse`, copy
their pattern instead.

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --lib recycle_events_round_trip`
Expected: FAIL to compile — no variant `TaskWorkspaceRecycled`.

**Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent-store/src/repository.rs`:

1. Add the two variants to `enum RuntimeEvent` (around line 99, after `ReviewRejected`):

```rust
    TaskWorkspaceRecycled,
    TaskWorkspaceRecycleFailed,
```

2. Add arms to `RuntimeEvent::name` (around line 129):

```rust
            Self::TaskWorkspaceRecycled => "task_workspace_recycled",
            Self::TaskWorkspaceRecycleFailed => "task_workspace_recycle_failed",
```

3. Add arms to `RuntimeEvent::parse` (around line 156, after `"review_rejected"`):

```rust
            "task_workspace_recycled" => Ok(Self::TaskWorkspaceRecycled),
            "task_workspace_recycle_failed" => Ok(Self::TaskWorkspaceRecycleFailed),
```

4. In `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`, add matching arms to `runtime_event_name` (around
line 2024, before the closing brace). This function is an exhaustive match and will not compile until
updated:

```rust
        crate::repository::RuntimeEvent::TaskWorkspaceRecycled => "task_workspace_recycled",
        crate::repository::RuntimeEvent::TaskWorkspaceRecycleFailed => {
            "task_workspace_recycle_failed"
        }
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --lib recycle_events_round_trip`
Expected: PASS (1 test).

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "feat(store): add workspace recycle runtime events"
```

---

## Task 6: Recycle in `accept_review`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Step 1: Write the failing test**

In `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`, extend
`CleanupRecordingWorkspaceService` (line 386) with an `accepted` recorder and the new trait method:

```rust
#[derive(Clone)]
struct CleanupRecordingWorkspaceService {
    workspace: WorkerWorkspace,
    cleaned: Arc<Mutex<Vec<WorkerWorkspace>>>,
    accepted: Arc<Mutex<Vec<(WorkerWorkspace, WorkerWorkspace)>>>,
    cleanup_error: Option<&'static str>,
}
```

Inside `impl AgentWorkspaceService for CleanupRecordingWorkspaceService`, add:

```rust
    fn cleanup_accepted(
        &self,
        owner: &WorkerWorkspace,
        child: &WorkerWorkspace,
    ) -> Result<(), WorkerError> {
        self.accepted
            .lock()
            .unwrap()
            .push((owner.clone(), child.clone()));
        if let Some(error) = self.cleanup_error {
            return Err(WorkerError::Startup(error.into()));
        }
        Ok(())
    }
```

Update the two existing constructions of this struct (lines ~740 and ~782) to add
`accepted: Arc::new(Mutex::new(Vec::new())),`.

Then add the test:

```rust
#[tokio::test]
async fn accepted_review_recycles_the_child_workspace() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: directory.path().join("repo"),
        path: directory.path().join("repo/.worktrees/child"),
        branch: "feat/child".into(),
        parent_branch: "main".into(),
        base_commit: "base".into(),
    };
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(CleanupRecordingWorkspaceService {
            workspace: workspace.clone(),
            cleaned: Arc::new(Mutex::new(Vec::new())),
            accepted: Arc::clone(&accepted),
            cleanup_error: None,
        })),
        ..Default::default()
    });
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;

    coordinator
        .accept_review(&child, IntegrationValidation::passed("integrated"))
        .await
        .unwrap();

    assert_eq!(accepted.lock().unwrap().len(), 1);
    let (owner, recycled_child) = accepted.lock().unwrap()[0].clone();
    assert_eq!(owner.branch, "feat/child");
    assert_eq!(recycled_child.branch, "feat/child");
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_workspace_optional(&child)
            .unwrap()
            .is_none(),
        "the child's workspace row is deleted after recycling"
    );
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator accepted_review_recycles`
Expected: FAIL — `accepted` stays empty because `accept_review` never calls `cleanup_accepted`.

**Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`:

1. Add a workspace-service resolver next to `prepare_task_workspace`. Put it directly above
   `prepare_task_workspace` (around line 1451):

```rust
    fn workspace_service_for(
        &self,
        session: &RootSessionId,
    ) -> Option<Arc<dyn yi_agent_core::subagent::worker::AgentWorkspaceService>> {
        self.application_root_workspace_services
            .lock()
            .expect("runtime application root workspace service mutex poisoned")
            .get(session)
            .cloned()
            .or_else(|| self.workspace_service.clone())
    }
```

2. Replace the inline resolution in `prepare_task_workspace` (lines 1469-1478) with a call to it:

```rust
        let Some(service) = self.workspace_service_for(session) else {
            return Ok(None);
        };
```

3. Add the recycling helper after `accept_review` (around line 1905):

```rust
    /// Best-effort recycling after an accepted review is durable. Never fails
    /// the accept: a recycle failure leaves the worktree in place for later
    /// handling and records an event. Git runs as a subprocess, so this is
    /// deliberately called after every lock is released.
    async fn recycle_accepted_delivery(
        &self,
        session: &RootSessionId,
        owner: &TaskId,
        child: &TaskId,
    ) {
        let Some(service) = self.workspace_service_for(session) else {
            return;
        };
        let (owner_workspace, child_workspace) = {
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            (
                repository.task_workspace_optional(owner),
                repository.task_workspace_optional(child),
            )
        };
        let (owner_workspace, child_workspace) = match (owner_workspace, child_workspace) {
            (Ok(Some(owner_workspace)), Ok(Some(child_workspace))) => {
                (owner_workspace, child_workspace)
            }
            (Err(error), _) | (_, Err(error)) => {
                eprintln!("yi-agent: accepted worktree recycle failed for {child}: {error}");
                self.record_recycle_event(child, RuntimeEvent::TaskWorkspaceRecycleFailed);
                return;
            }
            // A genuinely absent workspace row is not a recycle failure: nothing to recycle.
            _ => return,
        };
        match service.cleanup_accepted(&owner_workspace, &child_workspace) {
            Ok(()) => {
                let deleted = self
                    .repository
                    .lock()
                    .expect("runtime repository mutex poisoned")
                    .delete_task_workspace(child);
                if deleted.is_ok() {
                    self.record_recycle_event(child, RuntimeEvent::TaskWorkspaceRecycled);
                } else {
                    self.record_recycle_event(child, RuntimeEvent::TaskWorkspaceRecycleFailed);
                }
            }
            Err(error) => {
                eprintln!("yi-agent: accepted worktree recycle failed for {child}: {error}");
                self.record_recycle_event(child, RuntimeEvent::TaskWorkspaceRecycleFailed);
            }
        }
    }

    fn record_recycle_event(&self, task: &TaskId, event: RuntimeEvent) {
        let _ = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .append_event(task, event);
    }
```

Confirm `RuntimeEvent` is imported in `runtime.rs` (it is used for `RuntimeEvent::TaskDelivered` etc.).

4. Call it at the end of `accept_review`, after `release_resident_lease` and before `Ok(())`
   (around line 1903):

```rust
        drop(supervisor);
        self.release_resident_lease(task);
        self.recycle_accepted_delivery(&session, &parent, task).await;
        Ok(())
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator accepted_review_recycles`
Expected: PASS (1 test).

Also run the existing review tests to confirm nothing regressed:
`cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator review`
Expected: all PASS.

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "feat(store): recycle accepted child worktree after review acceptance"
```

---

## Task 7: Integration sweep in `reconcile_worker_events`

This is the task that makes the feature actually fire in production. Read the corrected A2 section of the
design first: the application root is not a daemon worker, so the sweep runs on every reconcile pass and
checks git directly.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Step 1: Write the failing tests**

In `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`, make `GitWorkspaceService` (line 165)
support the two new hooks with real git. Add inside `impl AgentWorkspaceService for GitWorkspaceService`:

```rust
    fn contains_commit(
        &self,
        owner: &WorkerWorkspace,
        commit: &str,
    ) -> Result<bool, WorkerError> {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", commit, "HEAD"])
            .current_dir(&owner.path)
            .output()
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
        Ok(output.status.success())
    }

    fn cleanup_accepted(
        &self,
        owner: &WorkerWorkspace,
        child: &WorkerWorkspace,
    ) -> Result<(), WorkerError> {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", child.path.to_str().unwrap()])
            .current_dir(&owner.path)
            .status();
        let _ = Command::new("git")
            .args(["branch", "-D", &child.branch])
            .current_dir(&owner.path)
            .status();
        Ok(())
    }
```

Then add the two tests:

```rust
#[tokio::test]
async fn integrated_delivery_is_accepted_and_recycled_on_reconcile() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;

    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
    let parent_workspace = factory.starts.lock().unwrap()[0].workspace.clone().unwrap();
    let child_workspace = factory.starts.lock().unwrap()[1].workspace.clone().unwrap();

    // The parent (application root) integrates the child by merging its branch.
    git_ok(
        &parent_workspace.path,
        &["merge", "--no-ff", &child_workspace.branch, "-m", "integrate"],
    )
    .unwrap();

    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "completed"
    );
    assert!(!child_workspace.path.exists(), "recycled worktree is gone");
}

#[tokio::test]
async fn unmerged_delivery_stays_awaiting_review_across_reconcile() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let (coordinator, _session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let child_workspace = factory.starts.lock().unwrap()[1].workspace.clone().unwrap();

    coordinator.reconcile_worker_events().await.unwrap();

    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
    assert!(child_workspace.path.exists());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator integrated_delivery_is_accepted unmerged_delivery_stays`
Expected: `integrated_delivery_is_accepted_and_recycled_on_reconcile` FAILS — the child stays
`awaiting_parent_review` because nothing calls `accept_review`. `unmerged_delivery_stays...` PASSES already
(it asserts the status quo); keep it as the regression guard for the sweep.

**Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`:

1. Add the sweep method. Put it directly below `reconcile_worker_events` (after line 2576):

```rust
    /// Accepts deliveries whose commit a parent has already integrated.
    ///
    /// This is the production entry to `accept_review`. It never parses agent
    /// text: the ancestry of the child's delivered commit in the parent's
    /// worktree HEAD is the only proof of integration. It runs on every
    /// reconcile pass because the application root is not a daemon worker, so
    /// its merge is only observable through git.
    ///
    /// Failures are logged, not propagated: reconcile runs on every IPC request
    /// and a transient mismatch must not fail unrelated traffic.
    async fn reconcile_integrated_deliveries(&self) {
        let supervisors = self
            .supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .iter()
            .map(|(session, supervisor)| (session.clone(), Arc::clone(supervisor)))
            .collect::<Vec<_>>();
        let mut awaiting: Vec<(RootSessionId, TaskId, TaskId, String)> = Vec::new();
        for (session, supervisor) in &supervisors {
            let supervisor = supervisor.lock().await;
            for child in supervisor.tasks_awaiting_parent_review() {
                let Some(task) = supervisor.task(&child) else {
                    continue;
                };
                let Some(parent) = task.parent_id.clone() else {
                    continue;
                };
                let Some(commit) = task
                    .active_attempt()
                    .delivery
                    .as_ref()
                    .map(|delivery| delivery.commit.clone())
                else {
                    continue;
                };
                awaiting.push((session.clone(), child, parent, commit));
            }
        }
        for (session, child, parent, commit) in awaiting {
            let Some(service) = self.workspace_service_for(&session) else {
                continue;
            };
            let owner_workspace = {
                let repository = self
                    .repository
                    .lock()
                    .expect("runtime repository mutex poisoned");
                repository.task_workspace_optional(&parent)
            };
            let owner_workspace = match owner_workspace {
                Ok(Some(workspace)) => workspace,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!(
                        "yi-agent: integration workspace lookup failed for {child}: {error}"
                    );
                    continue;
                }
            };
            match service.contains_commit(&owner_workspace, &commit) {
                Ok(true) => {
                    let integration = IntegrationValidation::passed(format!(
                        "child commit {commit} is an ancestor of parent {parent} HEAD"
                    ));
                    if let Err(error) = self.accept_review(&child, integration).await {
                        eprintln!(
                            "yi-agent: integrated delivery accept failed for {child}: {error}"
                        );
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "yi-agent: integration ancestry check failed for {child}: {error}"
                    );
                }
            }
        }
    }
```

Confirm `IntegrationValidation` is imported in `runtime.rs` (it appears in `accept_review`'s signature, so
it is).

2. Call it at the end of `reconcile_worker_events`, after the `updates` loop and before `Ok(())`
   (around line 2575):

```rust
        for (task_id, attempt, state, event, terminal_json) in updates {
            // ... existing body unchanged ...
        }
        self.reconcile_integrated_deliveries().await;
        Ok(())
    }
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator integrated_delivery_is_accepted unmerged_delivery_stays`
Expected: both PASS.

Then run the whole coordinator suite to catch lock-ordering regressions:
`cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator`
Expected: all PASS. If it hangs, follow the `sample`-based deadlock triage in `CLAUDE.md`; the most likely
cause is a supervisor lock held across the `accept_review` call — verify the `for (session, supervisor) in
&supervisors` loop drops its guard before the second `for` loop runs.

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "feat(store): accept and recycle integrated deliveries on reconcile"
```

---

## Task 8: Parent integration instruction in the system prompt

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs` (unit test module)

**Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `yi-agent-rs/crates/yi-agent/src/main.rs` (near
`resolve_system_prompt_none_uses_default`, line 1672):

```rust
#[test]
fn default_system_prompt_requires_parent_integration_of_deliveries() {
    let prompt = yi_agent_core::AgentConfig::default_system_prompt();
    assert!(
        prompt.contains("git merge --no-ff"),
        "default prompt must instruct parents to merge delivered commits"
    );
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --lib default_system_prompt_requires_parent_integration`
Expected: FAIL — the prompt has no merge instruction.

**Step 3: Write minimal implementation**

In `yi-agent-rs/crates/yi-agent-core/src/agent.rs`, inside the `default_system_prompt()` raw string,
append a new section before the closing `"#` (after the `File discovery:` block, around line 162):

```
Subagent integration:
- When a delegated child reports a delivery, integrate it yourself before you
  finish: run `git merge --no-ff <commit>` in your own worktree, resolve any
  conflicts, and re-run the relevant verification.
- A child is only completed, and its worktree only recycled, once its delivered
  commit is an ancestor of your HEAD. If you never merge it, it stays in review.
```

**Step 4: Run test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --lib default_system_prompt`
Expected: PASS.

**Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(core): instruct parents to integrate delivered commits"
```

---

## Task 9: Project documentation

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/bug-list.md`

**Step 1: Update the module file**

Read `docs/project-management/subagent-runtime.md`. Add entries for the two new capabilities, each with a
verifiable criterion, using only `[x]` / `[ ]` / `[-]` status markers:

- Integrated deliveries are accepted automatically:
  `[x]` — `RuntimeCoordinator::reconcile_integrated_deliveries` (`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`),
  verified by `cargo test -p yi-agent-store --test runtime_coordinator integrated_delivery_is_accepted_and_recycled_on_reconcile`.
- Accepted child worktrees, branches, and workspace rows are recycled:
  `[x]` — `AgentWorkspaceService::cleanup_accepted` (`yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`),
  verified by `cargo test -p yi-agent --lib daemon_workspace_cleanup_accepted_removes_child_worktree_and_branch`.

**Step 2: Update the index counts**

In `docs/project-management/README.md`, update the `subagent-runtime` row's "完成 / 总计" count to reflect
the two new completed entries.

**Step 3: Reconcile the bug list**

In `docs/bug-list.md`, find the entry about subagent worktrees/branches accumulating (search for
`worktree` and `feat/yi-agent-`). If this change fully addresses it, flip it to `[x]` and cite the new test
command. If it only partially addresses it (recycling happens for accepted deliveries only; failed,
cancelled, and rejected tasks are out of scope per the design), keep it `[ ]` and add a one-line note that
accepted-delivery recycling landed while the non-integrated case remains open.

**Step 4: Commit**

```bash
git add docs/project-management/subagent-runtime.md docs/project-management/README.md docs/bug-list.md
git commit -m "docs: record subagent worktree recycling"
```

---

## Final verification

Run each crate's suite serially (never `--workspace`), checking for stray cargo processes first:

```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || true
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent
cd yi-agent-rs && cargo fmt --all -- --check && cd ..
```

Expected: all green, no formatting diff.

The real-provider gate stays `#[ignore]` and is not run in CI:
`cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --test subagent_real_e2e -- --ignored`.
Note that `real_subagent_accepts_delivery_into_parent_history`
(`yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs:421`) currently drives the IPC `Accept` path and
tells the parent not to wait; under this design the parent must merge. That test's prompt and assertions may
need updating so the parent performs the merge — treat it as a follow-up, not a blocker, since it is
ignored in CI.

Then finish the branch with `superpowers:finishing-a-development-branch`.
