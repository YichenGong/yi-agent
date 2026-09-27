# Worktree Directory Reclaim Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reclaim worktree directories automatically on session exit and on a 7-day idle sweep, without ever discarding unmerged work and without breaking session reattachment.

**Architecture:** A new `AgentWorkspaceService::reclaim_worktree` removes only the worktree directory (never the branch ref, never the `task_workspaces` row), so commits stay reachable and reattachment keeps working. Because the row survives, `prepare_task_workspace` must gain a rebuild path (`git worktree add <path> <branch>`) for the case where the row outlives its directory. Two triggers call the reclaim: the `detach_application_root` handler seeds a background thread on TUI exit, and the daemon's existing per-minute tick runs the TTL sweep. `yi-agent daemon gc` is the manual surface for everything the automatic paths refuse.

**Tech Stack:** Rust (edition 2024), cargo workspace under `yi-agent-rs/`, `git` subprocesses for worktrees, SQLite via `rusqlite`, `tempfile` for test repositories.

## Global Constraints

- All commands run from the worktree root with `cargo test --manifest-path yi-agent-rs/Cargo.toml ...` so the worktree's own `target/` is used.
- Never run two cargo processes concurrently. Before each `cargo test`, check `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` and kill strays (they hold the target lock and cause exit 137 / hangs).
- Run `cd yi-agent-rs && cargo fmt --all && cd ..` before every commit.
- Commit messages use conventional commits (`feat:`, `fix:`, `docs:`). **Never** add a `Co-Authored-By` trailer.
- Never commit to `main`. All work happens on this worktree's branch.
- Reclaim removes the worktree **directory only**. It never deletes a branch ref and never deletes a `task_workspaces` row. Only `daemon gc` may do those, and only with explicit confirmation.
- Git never runs while holding the repository mutex. Lock order is always **supervisor → repository**.
- TTL is **7 days**.

---

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs` | Git-level primitives | Add `reclaim_directory`, `reattach_worktree`, `is_ancestor` |
| `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs` | Primitive tests | Add tests for the three new methods |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` | Workspace service trait | Add `reclaim_worktree` + `reattach_workspace` defaults |
| `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` | `DaemonWorkspaceService` impl | Implement both trait methods |
| `yi-agent-rs/crates/yi-agent-store/src/repository.rs` | Persistence | Add `reclaim_candidates` query, `detached_application_roots` query |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | Coordinator | Add `reclaim_session_worktrees`, `reclaim_idle_worktrees`, rebuild path in `prepare_task_workspace`, seed from `detach_application_root` |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | Daemon loop + IPC | TTL hook in the minute tick, `Gc` request/response |
| `yi-agent-rs/crates/yi-agent/src/config.rs` | CLI surface | `DaemonAction::Gc` |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | CLI dispatch | `daemon gc` handler |

---

## Task 1: Git primitives

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `WorktreeService::reclaim_directory(&self, repository_root: &Path, path: &Path) -> Result<(), WorktreeError>`
  - `WorktreeService::reattach_worktree(&self, repository_root: &Path, path: &Path, branch: &str) -> Result<(), WorktreeError>`
  - `WorktreeService::is_ancestor(&self, worktree: &Path, ancestor: &str, descendant: &str) -> Result<bool, WorktreeError>`

- [ ] **Step 1: Write the failing tests**

Append to `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`. The file already has `git(dir, args)` and `repository()` helpers; reuse them verbatim.

```rust
#[test]
fn reclaim_directory_removes_only_the_directory_and_keeps_the_branch() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-reclaim-root");
    let child_path = repo.path().join(".worktrees/yi-reclaim-child");
    let root = service
        .create_root(repo.path(), "feat/yi-reclaim-root", &root_path)
        .unwrap();
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-reclaim-child", &child_path)
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);
    let delivered = git(&child.path, &["rev-parse", "HEAD"]);

    service
        .reclaim_directory(repo.path(), &child.path)
        .unwrap();

    assert!(!child.path.exists(), "directory is gone");
    assert_eq!(
        git(repo.path(), &["rev-parse", "feat/yi-reclaim-child"]),
        delivered,
        "branch ref still pins the delivered commit"
    );
    assert_eq!(
        git(repo.path(), &["show", "feat/yi-reclaim-child:delivery.txt"]),
        "ready",
        "committed content is still recoverable"
    );
}

#[test]
fn reclaim_directory_refuses_a_dirty_worktree() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-dirty-root");
    let child_path = repo.path().join(".worktrees/yi-dirty-child");
    let root = service
        .create_root(repo.path(), "feat/yi-dirty-root", &root_path)
        .unwrap();
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-dirty-child", &child_path)
        .unwrap();
    std::fs::write(child.path.join("scratch.txt"), "uncommitted\n").unwrap();

    let error = service
        .reclaim_directory(repo.path(), &child.path)
        .unwrap_err();

    assert!(
        matches!(error, WorktreeError::DirtyChild { .. }),
        "unexpected error: {error}"
    );
    assert!(child.path.exists(), "dirty worktree is left in place");
}

#[test]
fn reattach_worktree_attaches_an_existing_branch() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-reattach-root");
    let child_path = repo.path().join(".worktrees/yi-reattach-child");
    let root = service
        .create_root(repo.path(), "feat/yi-reattach-root", &root_path)
        .unwrap();
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-reattach-child", &child_path)
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);
    let delivered = git(&child.path, &["rev-parse", "HEAD"]);

    service.reclaim_directory(repo.path(), &child.path).unwrap();
    assert!(!child.path.exists());

    service
        .reattach_worktree(repo.path(), &child.path, &child.branch)
        .unwrap();

    assert!(child.path.exists(), "worktree is restored");
    assert_eq!(
        git(&child.path, &["rev-parse", "HEAD"]),
        delivered,
        "restored tip equals the pre-reclaim tip"
    );
    assert_eq!(
        git(&child.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        child.branch
    );
    assert_eq!(
        std::fs::read_to_string(child.path.join("delivery.txt")).unwrap(),
        "ready\n"
    );
}

#[test]
fn reattach_worktree_fails_when_the_branch_is_absent() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-nobranch-root");
    let root = service
        .create_root(repo.path(), "feat/yi-nobranch-root", &root_path)
        .unwrap();
    let missing = repo.path().join(".worktrees/yi-nobranch-child");

    let error = service
        .reattach_worktree(repo.path(), &missing, "feat/does-not-exist")
        .unwrap_err();

    assert!(
        matches!(error, WorktreeError::Git { .. }),
        "unexpected error: {error}"
    );
    assert!(!missing.exists());
    let _ = root;
}

#[test]
fn is_ancestor_distinguishes_merged_from_unmerged_branches() {
    let (repo, base) = repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-ancestor-root");
    let child_path = repo.path().join(".worktrees/yi-ancestor-child");
    let root = service
        .create_root(repo.path(), "feat/yi-ancestor-root", &root_path)
        .unwrap();
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-ancestor-child", &child_path)
        .unwrap();
    std::fs::write(child.path.join("delivery.txt"), "ready\n").unwrap();
    git(&child.path, &["add", "delivery.txt"]);
    git(&child.path, &["commit", "-m", "child delivery"]);

    assert!(
        !service
            .is_ancestor(&root.path, &child.branch, &root.branch)
            .unwrap(),
        "an unmerged child branch is not an ancestor"
    );

    git(&root.path, &["merge", "--no-ff", &child.branch, "-m", "integrate"]);
    assert!(
        service
            .is_ancestor(&root.path, &child.branch, &root.branch)
            .unwrap(),
        "a merged child branch is an ancestor"
    );

    assert!(
        service.is_ancestor(&root.path, &base, "HEAD").unwrap(),
        "the base commit is always an ancestor of HEAD"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --test subagent_worktree`

Expected: FAIL to compile — `no method named reclaim_directory` / `reattach_worktree` / `is_ancestor`.

Note: `cargo test` accepts at most one positional test filter (`error: unexpected argument ... found` otherwise), so this runs the whole test binary rather than naming the five new tests. Step 4 does the same and asserts the full set passes.

- [ ] **Step 3: Write the implementation**

In `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`, add these three methods inside `impl WorktreeService`, directly after `remove_accepted_clean` (which spans lines 254-293, just before `contains_commit` at line 299):

```rust
    /// Remove only the worktree directory, leaving its branch ref intact.
    ///
    /// This is the safe automatic reclaim: the branch still pins every commit,
    /// so no work is lost and the worktree can be rebuilt later with
    /// [`Self::reattach_worktree`]. Git itself refuses a worktree that has
    /// modified or untracked files, which is the "clean" gate.
    ///
    /// `repository_root` is used as the working directory rather than the
    /// owner worktree, because the owner may itself have been reclaimed.
    pub fn reclaim_directory(
        &self,
        repository_root: &Path,
        path: &Path,
    ) -> Result<(), WorktreeError> {
        let status = git(path, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            return Err(WorktreeError::DirtyChild {
                path: path.to_path_buf(),
            });
        }
        let output = Command::new("git")
            .args(["worktree", "remove"])
            .arg(path)
            .current_dir(repository_root)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(())
    }

    /// Rebuild a reclaimed worktree from its surviving branch.
    ///
    /// `git worktree add -b` cannot be used here: the branch already exists, so
    /// `-b` fails with "a branch named '<branch>' already exists". Attaching the
    /// existing branch is the only correct form.
    pub fn reattach_worktree(
        &self,
        repository_root: &Path,
        path: &Path,
        branch: &str,
    ) -> Result<(), WorktreeError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        }
        let output = Command::new("git")
            .args(["worktree", "add"])
            .arg(path)
            .arg(branch)
            .current_dir(repository_root)
            .output()
            .map_err(|error| WorktreeError::Git {
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(WorktreeError::Git {
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(())
    }

    /// Whether `ancestor` is contained in `descendant`'s history.
    ///
    /// Exit code 0 means ancestor, 1 means not an ancestor, anything else is a
    /// real git error (for example an unknown revision).
    pub fn is_ancestor(
        &self,
        worktree: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> Result<bool, WorktreeError> {
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
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

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --test subagent_worktree`

Expected: all tests PASS, including the 5 new ones.

- [ ] **Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/worktree.rs yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs
git commit -m "feat(tools): add worktree directory reclaim and reattach primitives"
```

---

## Task 2: Trait methods and `DaemonWorkspaceService` implementation

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (unit test module)

**Interfaces:**
- Consumes: `WorktreeService::reclaim_directory`, `WorktreeService::reattach_worktree` (Task 1).
- Produces:
  - `AgentWorkspaceService::reclaim_worktree(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError>`
  - `AgentWorkspaceService::reattach_workspace(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError>`

- [ ] **Step 1: Write the failing test**

Add to the test module in `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`, next to `daemon_workspace_cleanup_accepted_removes_child_worktree_and_branch` (around line 1696). That test's `initialize_git_repository(directory.path())` helper is already in scope; reuse it.

```rust
    #[test]
    fn daemon_workspace_reclaim_removes_directory_and_keeps_branch_and_workspace() {
        let directory = TempDir::new().unwrap();
        initialize_git_repository(directory.path());
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let root = service
            .prepare_root(&RootSessionId::new(), &TaskId::new(), &AttemptId::new())
            .unwrap();
        std::fs::write(root.path.join("delivery.txt"), "ready\n").unwrap();
        Command::new("git")
            .args(["add", "delivery.txt"])
            .current_dir(&root.path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "root delivery"])
            .current_dir(&root.path)
            .status()
            .unwrap();
        let delivered = git_output(&root.path, &["rev-parse", "HEAD"]).unwrap();

        service.reclaim_worktree(&root).unwrap();

        assert!(!root.path.exists(), "directory is reclaimed");
        assert!(
            Command::new("git")
                .args([
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", root.branch)
                ])
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success(),
            "branch ref survives the reclaim"
        );
        assert_eq!(
            git_output(directory.path(), &["rev-parse", &root.branch]).unwrap(),
            delivered
        );

        service.reattach_workspace(&root).unwrap();

        assert!(root.path.exists(), "worktree is rebuilt from the branch");
        assert_eq!(
            git_output(&root.path, &["rev-parse", "HEAD"]).unwrap(),
            delivered,
            "rebuild restores the delivered tip"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent daemon_workspace_reclaim`

Expected: FAIL to compile — `no method named reclaim_worktree` / `reattach_workspace` on `DaemonWorkspaceService`.

- [ ] **Step 3: Add the trait methods**

In `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`, add to `trait AgentWorkspaceService`, directly after `cleanup_accepted` (which spans lines 487-493; the trait's closing brace is line 494):

```rust
    /// Remove a task's worktree directory while keeping its branch ref and its
    /// `task_workspaces` row. This is the safe automatic reclaim: the branch
    /// still pins every commit, and the surviving row lets
    /// [`Self::reattach_workspace`] rebuild the directory on demand.
    ///
    /// The default is a no-op for non-git services.
    fn reclaim_worktree(&self, _workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        Ok(())
    }

    /// Rebuild a reclaimed worktree directory from its surviving branch.
    ///
    /// The default cannot run git and therefore reports failure, because a
    /// caller that reaches this point needs a usable directory.
    fn reattach_workspace(&self, _workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        Err(WorkerError::Startup(
            "workspace service cannot rebuild a reclaimed worktree".into(),
        ))
    }
```

- [ ] **Step 4: Implement both methods on `DaemonWorkspaceService`**

In `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`, inside `impl AgentWorkspaceService for DaemonWorkspaceService`, add directly after `cleanup_accepted` (which spans lines 330-346; the impl's closing brace is line 347):

```rust
    fn reclaim_worktree(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        if workspace.branch.is_empty() {
            // A read-only workspace owns no worktree; its `path` is the parent's
            // view and must not be removed.
            return Ok(());
        }
        self.service
            .reclaim_directory(&workspace.repository_root, &workspace.path)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }

    fn reattach_workspace(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        if workspace.branch.is_empty() {
            return Ok(());
        }
        self.service
            .reattach_worktree(
                &workspace.repository_root,
                &workspace.path,
                &workspace.branch,
            )
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent daemon_workspace`

Expected: all PASS, including the new `_reclaim_removes_directory_and_keeps_branch_and_workspace`.

- [ ] **Step 6: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat(core): add worktree reclaim and rebuild to the workspace service"
```

---

## Task 3: Rebuild path in `prepare_task_workspace`

This task is what makes Task 1's "keep the branch" decision usable. Without it, a surviving row hands a worker a directory that no longer exists.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: `AgentWorkspaceService::reattach_workspace` (Task 2).
- Produces: no new public API; `prepare_task_workspace` gains an internal rebuild branch.

- [ ] **Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`. This file already has `initialize_git_repository`, `git_ok`, `git_output`, `MessageRecordingFactory`, and `GitWorkspaceService`; reuse them. Add a `reclaim_worktree` implementation to the test `GitWorkspaceService` first, inside `impl AgentWorkspaceService for GitWorkspaceService` (starts at line 181):

```rust
    fn reclaim_worktree(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        let _ = Command::new("git")
            .args(["worktree", "remove", workspace.path.to_str().unwrap()])
            .current_dir(&workspace.repository_root)
            .status();
        Ok(())
    }

    fn reattach_workspace(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        Command::new("git")
            .args(["worktree", "add"])
            .arg(&workspace.path)
            .arg(&workspace.branch)
            .current_dir(&workspace.repository_root)
            .status()
            .map_err(|error| WorkerError::Startup(format!("git worktree add failed: {error}")))?;
        Ok(())
    }
```

Then add the test. A second `start_worker` on the same task is rejected with "task already owns a worker" (verified by the existing `duplicate_worker_start_does_not_fail_the_running_attempt` test), so the rebuild path is reached the way production reaches it: make the task terminal, then retry it. `retry_task` clears the worker handle, creates a successor attempt, and calls `start_worker` again (`runtime.rs:1704-1706`), which re-enters `prepare_task_workspace` and finds the row without a directory.

```rust
#[tokio::test]
async fn a_reclaimed_worktree_is_rebuilt_before_a_worker_starts() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let workspace = factory.starts.lock().unwrap()[0].workspace.clone().unwrap();
    let service = factory.workspace_service.clone().unwrap();
    service.reclaim_worktree(&workspace).unwrap();
    assert!(
        !workspace.path.exists(),
        "precondition: the directory is reclaimed while the row survives"
    );

    // Make the task terminal so retry is legal, then retry. The retry restarts
    // the worker, which must rebuild the directory before handing it over.
    coordinator.cancel_task(&session, &root, false).await.unwrap();
    coordinator.retry_task(&session, &root).await.unwrap();

    assert!(
        workspace.path.exists(),
        "the rebuild path restored the directory before the worker started"
    );
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_workspace_optional(&root)
            .unwrap()
            .is_some(),
        "the workspace row is untouched"
    );
}
```

`cancel_task` and `retry_task` are both `pub async fn` on `RuntimeCoordinator`; `retry_task` starts the successor worker itself when the factory reports available, which `MessageRecordingFactory` does by the trait default.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator a_reclaimed_worktree_is_rebuilt`

Expected: FAIL. The second `start_worker` reuses the row via `prepare_task_workspace` and the directory stays absent, so the final assertion fails. (Depending on the test factory, the worker may also fail to start; either way the test does not pass.)

- [ ] **Step 3: Implement the rebuild branch**

In `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, replace the row-hit fast path in `prepare_task_workspace` (lines 1615-1623):

```rust
        let existing = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_workspace_optional(task)?;
        if let Some(existing) = existing {
            supervisor
                .assign_workspace(task, existing.lease_id.clone())
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            return Ok(Some(existing));
        }
```

with:

```rust
        let existing = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_workspace_optional(task)?;
        if let Some(existing) = existing {
            // A reclaimed worktree keeps its row, so the row can outlive its
            // directory. Rebuild before handing the path to a worker.
            if !existing.path.exists() {
                let Some(service) = self.workspace_service_for(session) else {
                    return Ok(None);
                };
                service
                    .reattach_workspace(&existing)
                    .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            }
            supervisor
                .assign_workspace(task, existing.lease_id.clone())
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            return Ok(Some(existing));
        }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator a_reclaimed_worktree_is_rebuilt`

Expected: PASS.

Then run the whole coordinator suite to confirm nothing regressed:

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator`

Expected: all PASS. If it hangs, follow the `sample`-based deadlock triage in `CLAUDE.md`; the likely cause is a supervisor guard held across the new call.

- [ ] **Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "fix(store): rebuild a reclaimed worktree before starting its worker"
```

---

## Task 4: Reclaim candidates query

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `RuntimeRepository::reclaim_candidates(&self, root_session_id: &RootSessionId) -> Result<Vec<PersistedTaskDetail>, RepositoryError>`
  - `RuntimeRepository::detached_application_roots(&self) -> Result<Vec<RootSessionId>, RepositoryError>`

`PersistedTaskDetail` already exists (`repository.rs:317-326`) with `task_id`, `session_id`, `parent_task_id`, `depth`, `state`, `delivery_json`, `terminal_json`, `workspace`.

- [ ] **Step 1: Write the failing tests**

Add to `yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs`. The file already imports `AttemptId`, `RootSessionId`, `TaskId`, `WorkspaceLeaseId`, `WorkerWorkspace`, and `RuntimeRepository`, and defines `test_workspace(session, task)` taking **two** arguments. Reuse that helper rather than inventing one.

```rust
#[test]
fn reclaim_candidates_are_deepest_first_and_carry_their_workspace() {
    let directory = TempDir::new().unwrap();
    let mut repository =
        RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let session = RootSessionId::new();
    let root = TaskId::new();
    let child = TaskId::new();
    let root_attempt = AttemptId::new();
    let child_attempt = AttemptId::new();
    repository
        .create_task_with_attempt_and_objective(
            &root,
            &session,
            &root_attempt,
            1,
            "completed",
            "root objective",
            TaskWorkspaceMode::Coding,
        )
        .unwrap();
    repository
        .create_child_task_with_attempt(
            &child,
            &session,
            &root,
            1,
            &child_attempt,
            1,
            "completed",
        )
        .unwrap();
    repository
        .record_task_workspace(&root, &root_attempt, &test_workspace(&session, &root))
        .unwrap();
    let mut child_workspace = test_workspace(&session, &child);
    child_workspace.branch = "feat/child".into();
    repository
        .record_task_workspace(&child, &child_attempt, &child_workspace)
        .unwrap();

    let candidates = repository.reclaim_candidates(&session).unwrap();

    assert_eq!(candidates.len(), 2, "both tasks carry a workspace row");
    assert_eq!(
        candidates[0].task_id,
        child.to_string(),
        "the deeper task is listed first so reclaim runs child before parent"
    );
    assert_eq!(candidates[0].depth, 1);
    assert_eq!(candidates[1].task_id, root.to_string());
    assert_eq!(candidates[1].depth, 0);
    assert!(
        candidates.iter().all(|candidate| candidate.workspace.is_some()),
        "workspace is joined in"
    );
}

#[test]
fn reclaim_candidates_exclude_tasks_without_a_workspace_row() {
    let directory = TempDir::new().unwrap();
    let mut repository =
        RuntimeRepository::open(directory.path().join("runtime.sqlite")).unwrap();
    let session = RootSessionId::new();
    let task = TaskId::new();
    repository
        .create_task_with_attempt_and_objective(
            &task,
            &session,
            &AttemptId::new(),
            1,
            "completed",
            "objective",
            TaskWorkspaceMode::ReadOnly,
        )
        .unwrap();

    assert!(
        repository.reclaim_candidates(&session).unwrap().is_empty(),
        "a read-only task owns no worktree and is not a candidate"
    );
}
```

Two details that are easy to get wrong here, both verified against the schema:

- `create_task_with_attempt_and_objective` hardcodes `depth = 0` for the root, and `create_child_task_with_attempt` takes `depth` as its fourth argument. A direct child therefore has `depth = 1`, not `2`.
- `test_workspace` derives its path from the task id, so the root and child already get distinct paths and distinct branches. Do not overwrite `child_workspace.path`.

`TaskWorkspaceMode` must be in scope. If it is not already imported in this file, add it to the `yi_agent_core::subagent::task::{...}` use list.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test repository_decisions reclaim_candidates`

Expected: FAIL to compile — `no method named reclaim_candidates`. If the helper names `create_task_with_attempt_and_objective` / `create_child_task_with_attempt` / `record_task_workspace` / `test_workspace` differ from what this file uses, adapt the calls to the existing helpers before proceeding.

- [ ] **Step 3: Implement the query**

In `yi-agent-rs/crates/yi-agent-store/src/repository.rs`, add directly after `task_workspace_optional` (the function spans lines 3222-3267; add after line 3267). Reuse the row-decoding shape from `task_detail` (around line 3676) so the `PersistedTaskDetail` fields are populated consistently.

```rust
    /// Every task in a session that owns a worktree, deepest first.
    ///
    /// Ordering by `depth DESC` matters: a child's ancestry check runs with the
    /// owner worktree as its working directory, so the parent must be reclaimed
    /// after its children, never before.
    pub fn reclaim_candidates(
        &self,
        root_session_id: &RootSessionId,
    ) -> Result<Vec<PersistedTaskDetail>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT tasks.id, tasks.root_session_id, tasks.parent_id, tasks.depth,
                    tasks.state_json, tasks.delivery_json, attempts.terminal_json,
                    task_workspaces.lease_id, task_workspaces.repository_root,
                    task_workspaces.path, task_workspaces.branch,
                    task_workspaces.parent_branch, task_workspaces.base_commit
             FROM tasks
             JOIN task_workspaces ON task_workspaces.task_id = tasks.id
             LEFT JOIN attempts ON attempts.id = tasks.active_attempt_id
             WHERE tasks.root_session_id = ?1
             ORDER BY tasks.depth DESC, tasks.created_at, tasks.id",
        )?;
        let rows = statement
            .query_map(params![root_session_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u8>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|row| PersistedTaskDetail {
                task_id: row.0,
                session_id: row.1,
                parent_task_id: row.2,
                depth: row.3,
                state: row.4,
                delivery_json: row.5,
                terminal_json: row.6,
                workspace: Some(WorkerWorkspace {
                    lease_id: row.7.parse().unwrap_or_else(|_| WorkspaceLeaseId::new()),
                    repository_root: PathBuf::from(row.8),
                    path: PathBuf::from(row.9),
                    branch: row.10,
                    parent_branch: row.11,
                    base_commit: row.12,
                }),
            })
            .collect())
    }

    /// Sessions whose application root attachment is currently detached.
    ///
    /// A detached root sits in `paused`, which is not a terminal state, so it
    /// would never enter a terminal-only sweep. This is the clause that makes a
    /// root worktree reclaimable at all.
    pub fn detached_application_roots(
        &self,
    ) -> Result<Vec<RootSessionId>, RepositoryError> {
        let mut statement = self.connection.prepare(
            "SELECT root_session_id FROM application_root_attachments
             WHERE state = 'detached'",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|value| value.parse().ok())
            .collect())
    }
```

Confirm the imports `PathBuf`, `WorkspaceLeaseId`, and `PersistedTaskDetail` are already in scope in `repository.rs` (they are used by `task_detail` and the schema code). If `WorkspaceLeaseId` needs importing, add it to the existing `yi_agent_core::subagent::task::{...}` use list.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test repository_decisions reclaim_candidates`

Expected: both PASS.

Then run the whole suite for the crate:

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test repository_decisions`

Expected: all PASS.

- [ ] **Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs
git commit -m "feat(store): add reclaim candidate and detached root queries"
```

---

## Task 5: `reclaim_session_worktrees`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: `RuntimeRepository::reclaim_candidates` (Task 4), `AgentWorkspaceService::reclaim_worktree` (Task 2), `WorktreeService::is_ancestor` semantics via a new service hook.
- Produces: `RuntimeCoordinator::reclaim_session_worktrees(&self, session: &RootSessionId) -> usize` — returns the number of directories reclaimed. Private helpers `reclaim_candidates_in_session` and `reclaim_candidate_directories` are produced for Task 7. Also add to the trait:
  - `AgentWorkspaceService::is_merged_into(&self, owner: &WorkerWorkspace, branch: &str) -> Result<bool, WorkerError>`

- [ ] **Step 1: Add the trait hook and its `DaemonWorkspaceService` implementation**

In `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`, add to `trait AgentWorkspaceService` after `reattach_workspace`:

```rust
    /// Whether `branch` is already merged into `owner`'s current branch. Used
    /// to refuse reclaiming a worktree whose work has not been integrated. The
    /// default cannot inspect git and reports "not merged", which is the safe
    /// answer.
    fn is_merged_into(
        &self,
        _owner: &WorkerWorkspace,
        _branch: &str,
    ) -> Result<bool, WorkerError> {
        Ok(false)
    }
```

In `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`, inside `impl AgentWorkspaceService for DaemonWorkspaceService`, add after `reattach_workspace`:

```rust
    fn is_merged_into(
        &self,
        owner: &WorkerWorkspace,
        branch: &str,
    ) -> Result<bool, WorkerError> {
        if branch.is_empty() {
            return Ok(false);
        }
        let owner_branch = git_output(&owner.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .ok_or_else(|| WorkerError::Startup("Git workspace error: owner HEAD is detached".into()))?;
        self.service
            .is_ancestor(&owner.path, branch, owner_branch.trim())
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
    }
```

- [ ] **Step 2: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`. Extend the test `GitWorkspaceService` with the hook (inside its `impl AgentWorkspaceService`):

```rust
    fn is_merged_into(
        &self,
        owner: &WorkerWorkspace,
        branch: &str,
    ) -> Result<bool, WorkerError> {
        let owner_branch = git_output(&owner.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map_err(|error| WorkerError::Startup(error))?;
        let output = Command::new("git")
            .args(["merge-base", "--is-ancestor", branch, owner_branch.trim()])
            .current_dir(&owner.path)
            .status()
            .map_err(|error| WorkerError::Startup(format!("git merge-base failed: {error}")))?;
        Ok(output.success())
    }
```

Then add the test:

```rust
#[tokio::test]
async fn reclaim_session_worktrees_removes_merged_children_and_keeps_unmerged_ones() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let root_workspace = factory.starts.lock().unwrap()[0].workspace.clone().unwrap();

    let merged = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "merged child".into(),
            TaskWorkspaceMode::Coding,
        )
        .await
        .unwrap();
    coordinator.start_worker(&session, &merged).await.unwrap();
    let merged_workspace = factory.starts.lock().unwrap().last().unwrap().workspace.clone().unwrap();
    std::fs::write(merged_workspace.path.join("merged.txt"), "ready\n").unwrap();
    git_ok(&merged_workspace.path, &["add", "merged.txt"]).unwrap();
    git_ok(&merged_workspace.path, &["commit", "-m", "merged delivery"]).unwrap();
    git_ok(
        &root_workspace.path,
        &["merge", "--no-ff", &merged_workspace.branch, "-m", "integrate"],
    )
    .unwrap();

    let unmerged = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "unmerged child".into(),
            TaskWorkspaceMode::Coding,
        )
        .await
        .unwrap();
    coordinator.start_worker(&session, &unmerged).await.unwrap();
    let unmerged_workspace = factory.starts.lock().unwrap().last().unwrap().workspace.clone().unwrap();
    std::fs::write(unmerged_workspace.path.join("pending.txt"), "wip\n").unwrap();
    git_ok(&unmerged_workspace.path, &["add", "pending.txt"]).unwrap();
    git_ok(&unmerged_workspace.path, &["commit", "-m", "unmerged delivery"]).unwrap();

    let reclaimed = coordinator.reclaim_session_worktrees(&session);

    assert!(
        !merged_workspace.path.exists(),
        "a merged child's directory is reclaimed"
    );
    assert!(
        unmerged_workspace.path.exists(),
        "an unmerged child's directory is left alone"
    );
    assert_eq!(reclaimed, 1, "exactly one directory was reclaimed");
    assert!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_workspace_optional(&merged)
            .unwrap()
            .is_some(),
        "the row survives so the worktree can be rebuilt"
    );
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator reclaim_session_worktrees`

Expected: FAIL to compile — `no method named reclaim_session_worktrees`.

- [ ] **Step 4: Implement `reclaim_session_worktrees`**

In `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, add after `recycle_accepted_delivery` (which spans lines 2076-2131). This method is synchronous: it runs git subprocesses and must not be called from a context that holds the repository mutex.

```rust
    /// Reclaims every reclaimable worktree directory in a session.
    ///
    /// Removes directories only: branch refs and `task_workspaces` rows survive,
    /// so nothing is lost and every worktree can be rebuilt by
    /// `prepare_task_workspace`. Returns the number of directories reclaimed.
    ///
    /// Candidates are processed deepest first. A child's merge check runs with
    /// the owner worktree as its working directory, so reclaiming a parent first
    /// would break its children.
    pub fn reclaim_session_worktrees(&self, session: &RootSessionId) -> usize {
        let candidates = self.reclaim_candidates_in_session(session);
        self.reclaim_candidate_directories(session, candidates)
    }

    /// Reads the session's reclaim candidates under a short repository lock.
    ///
    /// Split out from [`Self::reclaim_session_worktrees`] so the TTL sweep can
    /// filter the same candidate set without re-reading it.
    fn reclaim_candidates_in_session(
        &self,
        session: &RootSessionId,
    ) -> Vec<crate::repository::PersistedTaskDetail> {
        let repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        match repository.reclaim_candidates(session) {
            Ok(candidates) => candidates,
            Err(error) => {
                eprintln!("yi-agent: reclaim candidate lookup failed for {session}: {error}");
                Vec::new()
            }
        }
    }

    /// Reclaims the directories of an already-selected candidate set.
    ///
    /// The caller chooses which candidates are eligible; this function enforces
    /// the merge check and the directory removal. Splitting the two means the
    /// "reclaim everything" path and the "reclaim only idle tasks" path share
    /// one implementation of ordering, the merge gate, and event recording.
    fn reclaim_candidate_directories(
        &self,
        session: &RootSessionId,
        candidates: Vec<crate::repository::PersistedTaskDetail>,
    ) -> usize {
        let Some(service) = self.workspace_service_for(session) else {
            return 0;
        };
        let mut reclaimed = 0;
        for candidate in candidates {
            let Some(child_workspace) = candidate.workspace.clone() else {
                continue;
            };
            let Some(task_id) = candidate.task_id.parse::<TaskId>().ok() else {
                continue;
            };
            // The owner is the parent's workspace when there is a parent, and the
            // repository root otherwise. A root's `parent_branch` is the main
            // branch, which a root branch rarely merges into, so the root is
            // reclaimed without a merge check.
            let owner_workspace = match candidate.parent_task_id.as_ref() {
                Some(parent) => {
                    let resolved = {
                        let repository = self
                            .repository
                            .lock()
                            .expect("runtime repository mutex poisoned");
                        let parsed = parent.parse::<TaskId>().ok();
                        match parsed {
                            Some(parsed) => repository.task_workspace_optional(&parsed).ok().flatten(),
                            None => None,
                        }
                    };
                    match resolved {
                        Some(workspace) => Some(workspace),
                        None => continue,
                    }
                }
                None => None,
            };
            if let Some(owner_workspace) = owner_workspace.as_ref() {
                match service.is_merged_into(owner_workspace, &child_workspace.branch) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        eprintln!(
                            "yi-agent: merge check failed for {task_id}, skipping reclaim: {error}"
                        );
                        continue;
                    }
                }
            }
            if !child_workspace.path.exists() {
                continue;
            }
            match service.reclaim_worktree(&child_workspace) {
                Ok(()) => {
                    reclaimed += 1;
                    self.record_recycle_event(&task_id, RuntimeEvent::TaskWorkspaceRecycled);
                }
                Err(error) => {
                    eprintln!("yi-agent: worktree reclaim failed for {task_id}: {error}");
                }
            }
        }
        reclaimed
    }
```

Note on lock discipline: this method takes the repository mutex only to read rows, and releases it before every `service.*` call. Git therefore never runs under the repository mutex.

Implementation note (added after review): the supervisor lock is *not* taken by the callers either, so the reclaim is **not** serialized against `retry_task`. It cannot be as written: `reclaim_candidate_directories` is a synchronous `fn` while the supervisor guard is a tokio `AsyncMutex`, and both callers (the minute tick and the detach arm in Task 7) run on plain `std::thread`s that cannot await it. The residual TOCTOU race is instead bounded by three properties — the state gate refuses a non-terminal child, reclaim removes only MERGED directories, and `prepare_task_workspace` rebuilds any directory it removed, so a worker that starts just after a reclaim gets a fresh worktree rather than a missing one.

`reclaim_candidates_in_session` and `reclaim_candidate_directories` are private helpers, deliberately separated: Task 7's TTL sweep filters the candidate set by idle time and then calls `reclaim_candidate_directories` directly, so the merge gate, deepest-first ordering, and event recording exist in exactly one place. A future change to any of those cannot diverge between the two triggers.

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator reclaim_session_worktrees`

Expected: PASS.

- [ ] **Step 6: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "feat(store): reclaim a session's merged worktree directories"
```

---

## Task 6: Seed reclaim from the detach handler

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: `RuntimeCoordinator::reclaim_session_worktrees` (Task 5).
- Produces: `detach_application_root` spawns a background reclaim thread; the response is unchanged (`IpcResponse::ApplicationRootDetached`).

- [ ] **Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`. That file has `application_root_daemon` and a `StaticWorkspaceService`; a recording service is needed to observe the reclaim.

First, widen `ApplicationRootFactory`'s field so any service can be injected. Change its declaration (the struct spans lines 269-272; the field is line 270) from the concrete type to the trait object:

```rust
#[derive(Clone)]
struct ApplicationRootFactory {
    workspace_service: Arc<dyn AgentWorkspaceService>,
    starts: Arc<Mutex<Vec<WorkerStart>>>,
}
```

Every existing construction site already passes `Arc::new(StaticWorkspaceService)`, which coerces to `Arc<dyn AgentWorkspaceService>` unchanged, so no other call site needs editing.

Then add the recording service next to the existing ones:

```rust
#[derive(Clone, Default)]
struct ReclaimRecordingWorkspaceService {
    reclaimed: Arc<Mutex<Vec<std::path::PathBuf>>>,
}

impl AgentWorkspaceService for ReclaimRecordingWorkspaceService {
    fn prepare_root(
        &self,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(test_workspace_for_ipc(
            &root_session_id.to_string(),
            &task_id.to_string(),
        ))
    }

    fn prepare_child(
        &self,
        _parent: &WorkerWorkspace,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        _attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Ok(test_workspace_for_ipc(
            &root_session_id.to_string(),
            &task_id.to_string(),
        ))
    }

    fn reclaim_worktree(&self, workspace: &WorkerWorkspace) -> Result<(), WorkerError> {
        self.reclaimed.lock().unwrap().push(workspace.path.clone());
        Ok(())
    }

    fn is_merged_into(
        &self,
        _owner: &WorkerWorkspace,
        _branch: &str,
    ) -> Result<bool, WorkerError> {
        Ok(true)
    }
}
```

Then the test. Note it calls `StartWorker` after activation: `attach_application_root` records the root's `task_workspaces` row by calling `prepare_task_workspace` (see the `record_application_root_attachment` block in `runtime.rs`), but `activate_application_root` does not touch the row. The row therefore exists after attach; the explicit `StartWorker` makes the test independent of that detail and exercises the path a real TUI takes.

```rust
#[test]
fn detaching_an_application_root_seeds_a_worktree_reclaim() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let reclaimed = Arc::new(Mutex::new(Vec::new()));
    let service = Arc::new(ReclaimRecordingWorkspaceService {
        reclaimed: Arc::clone(&reclaimed),
    });
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        Arc::new(ApplicationRootFactory {
            workspace_service: service,
            starts: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .unwrap();
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "tui-reclaim".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::ActivateApplicationRoot {
                session_id: session_id.clone(),
                root_task_id: root_task_id.clone(),
                capability: message_capability.clone(),
                objective: "first prompt".into(),
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootActivated
    );
    send_request(
        daemon.socket_path(),
        IpcRequest::StartWorker {
            session_id: session_id.clone(),
            task_id: root_task_id.clone(),
        },
    )
    .unwrap();

    assert_eq!(
        send_request(
            daemon.socket_path(),
            IpcRequest::DetachApplicationRoot {
                session_id,
                root_task_id,
                capability: message_capability,
            },
        )
        .unwrap(),
        IpcResponse::ApplicationRootDetached,
        "detach still answers with the same response"
    );

    // The reclaim runs on a background thread, so poll rather than assert once.
    for _ in 0..100 {
        if !reclaimed.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !reclaimed.lock().unwrap().is_empty(),
        "detach seeded a background reclaim"
    );
}
```

The root is reclaimed without a merge check (`reclaim_candidate_directories` skips the check when `parent_task_id` is `None`), which is why `is_merged_into` returning `true` here does not mask a bug.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_ipc detaching_an_application_root_seeds`

Expected: FAIL — `reclaimed` stays empty because `detach_application_root` never calls the reclaim.

- [ ] **Step 3: Implement the seeding**

In `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, modify `detach_application_root` (lines 971-1003). Keep every existing step; add the comment noting that callers seed the reclaim. Do **not** add a spawn here:

```rust
    pub async fn detach_application_root(
        &self,
        session: &RootSessionId,
        root_task: &TaskId,
        capability: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        self.authorize_application_root(session, root_task, capability)?;
        let attempt = {
            let supervisor = self.supervisor(session)?;
            let mut supervisor = supervisor.lock().await;
            let attempt = supervisor
                .task(root_task)
                .ok_or_else(|| RuntimeCoordinatorError::Supervisor("task does not exist".into()))?
                .active_attempt_id()
                .clone();
            supervisor
                .pause_foreground_task(root_task, PauseReason("foreground TUI detached".into()))
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            attempt
        };
        {
            let mut repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            repository.transition_task_and_attempt(
                root_task,
                &attempt,
                "paused",
                RuntimeEvent::TaskPaused,
            )?;
            repository.detach_application_root(session, root_task)?;
        }
        // The attachment is durably `detached` here, so a reclaimed root worktree
        // can never be observed as attached. The reclaim itself is seeded by the
        // IPC caller (see `ipc.rs`), because this method takes `&self` and cannot
        // clone the `Arc<RuntimeCoordinator>` a background thread needs.
        Ok(())
    }
```

Then, in `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`, the `DetachApplicationRoot` arm already holds `coordinator: &Arc<RuntimeCoordinator>`. Replace it with:

```rust
        IpcRequest::DetachApplicationRoot {
            session_id,
            root_task_id,
            capability,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let root_task_id = parse_id::<TaskId>(&root_task_id)?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(coordinator.detach_application_root(
                &session_id,
                &root_task_id,
                &capability,
            ))?;
            // Reclaim on a background thread: git is a synchronous subprocess and
            // the client's `send_request` sets no read timeout, so doing this
            // inline would make the TUI wait for the full duration. Detached is
            // safe because the reclaim is idempotent and re-runnable, and
            // `prepare_task_workspace` rebuilds any directory it removes.
            let reclaim_coordinator = Arc::clone(coordinator);
            let reclaim_session = session_id.clone();
            std::thread::spawn(move || {
                reclaim_coordinator.reclaim_session_worktrees(&reclaim_session);
            });
            Ok(IpcResponse::ApplicationRootDetached)
        }
```

The seeding lives in `ipc.rs`, not `runtime.rs`, for one concrete reason: `detach_application_root` takes `&self`, so it has no way to obtain an owned `Arc<RuntimeCoordinator>` to move into the thread. The `Arc` is in scope at the IPC call site. Do not add a `spawn_session_reclaim` helper or a `clone_for_background` method to `RuntimeCoordinator`; neither exists and neither is needed.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_ipc detaching_an_application_root_seeds`

Expected: PASS.

Then run the reattach tests, which must be unaffected because no row is deleted:

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_ipc detached_`

Expected: both PASS.

- [ ] **Step 5: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat(store): seed a worktree reclaim when an application root detaches"
```

---

## Task 7: TTL sweep on the daemon tick

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: `RuntimeRepository::reclaim_candidates`, `RuntimeRepository::detached_application_roots` (Task 4), `RuntimeCoordinator::reclaim_session_worktrees` (Task 5).
- Produces: `RuntimeCoordinator::reclaim_idle_worktrees(&self, now: DateTime<Utc>) -> usize` and a public constant `WORKTREE_RECLAIM_TTL: Duration`.

- [ ] **Step 1: Write the failing tests**

Add to `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`.

**Read this before writing the tests.** `reclaim_idle_worktrees` discovers work through `detached_application_roots()`, which selects `root_session_id FROM application_root_attachments WHERE state = 'detached'`. A session made by `create_session()` has **no** attachment row, so a test built on `create_session()` alone would sweep zero sessions and pass vacuously while proving nothing. Both tests below therefore seed a detached attachment row with `record_application_root_attachment` followed by `detach_application_root`. That is exactly the state a real TUI exit leaves behind.

The tests also need to age a task's `updated_at`. Write it directly through a SQLite connection, as the file already does elsewhere (see the `Connection::open` usage around line 3826).

```rust
/// Marks a session as a detached application root, which is the state the TTL
/// sweep discovers its work through.
fn mark_session_detached(
    database: &std::path::Path,
    session: &RootSessionId,
    root: &yi_agent_core::TaskId,
) {
    let mut repository = RuntimeRepository::open(database).unwrap();
    repository
        .record_application_root_attachment("ttl-fixture", session, root, "digest", "secret")
        .unwrap();
    repository
        .detach_application_root(session, root)
        .unwrap();
}

#[tokio::test]
async fn reclaim_idle_sweeps_a_detached_root_past_the_ttl() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let workspace = factory.starts.lock().unwrap()[0].workspace.clone().unwrap();
    mark_session_detached(&database, &session, &root);

    // Age the task past the TTL.
    let aged = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![aged, root.to_string()],
        )
        .unwrap();
    drop(connection);

    let reclaimed = coordinator.reclaim_idle_worktrees(chrono::Utc::now());

    assert_eq!(reclaimed, 1, "an idle detached root is reclaimed");
    assert!(!workspace.path.exists());
}

#[tokio::test]
async fn reclaim_idle_keeps_a_fresh_detached_root() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();
    let workspace = factory.starts.lock().unwrap()[0].workspace.clone().unwrap();
    mark_session_detached(&database, &session, &root);

    // No ageing: the row was just written.
    let reclaimed = coordinator.reclaim_idle_worktrees(chrono::Utc::now());

    assert_eq!(reclaimed, 0, "the TTL has not elapsed");
    assert!(
        workspace.path.exists(),
        "a recently detached root keeps its worktree"
    );
}

#[tokio::test]
async fn reclaim_idle_never_sweeps_an_awaiting_review_child() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(GitWorkspaceService::new(repository_root.clone()))),
        ..Default::default()
    });
    let (coordinator, session, parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let child_workspace = factory.starts.lock().unwrap()[1].workspace.clone().unwrap();
    assert_eq!(
        RuntimeRepository::open(&database)
            .unwrap()
            .task_state(&child)
            .unwrap(),
        "awaiting_parent_review"
    );
    // The sweep only visits detached sessions, so the session must be detached
    // for this test to exercise the state filter rather than the session filter.
    mark_session_detached(&database, &session, &parent);

    let aged = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![aged, child.to_string()],
        )
        .unwrap();
    drop(connection);

    coordinator.reclaim_idle_worktrees(chrono::Utc::now());

    assert!(
        child_workspace.path.exists(),
        "an un-integrated delivery is never reclaimed, however old"
    );
}
```

Three things about this fixture, each verified against the code:

- `record_application_root_attachment` takes `(idempotency_key, root_session_id, root_task_id, capability_digest, capability_secret)` and writes `state = 'attached'`; `detach_application_root` then flips it to `'detached'`. Both are `&mut self` on `RuntimeRepository`, so they need a mutable binding.
- `delivered_child_coordinator` returns `(coordinator, session, parent, child, delivery)`. The second `WorkerStart` (`starts[1]`) is the child's, which is why the test indexes `[1]`.
- The third test detaches the session on purpose: without it the sweep would skip the session for the wrong reason (no attachment row) and the assertion would pass vacuously.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator reclaim_idle`

Expected: FAIL to compile — `no method named reclaim_idle_worktrees`.

- [ ] **Step 3: Implement `reclaim_idle_worktrees`**

In `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`, add near the other constants at the top of the file (alongside `REVIEW_CONFIRMATION_TTL`, around line 44):

```rust
/// How long a task must be idle before its worktree directory is reclaimed.
///
/// Conservative by default: reclaim removes only the directory, keeps the branch
/// and the workspace row, and is therefore reversible via
/// `prepare_task_workspace`'s rebuild path.
pub const WORKTREE_RECLAIM_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
```

Then add the method after `reclaim_session_worktrees`:

```rust
    /// Reclaims directories for tasks that have been idle past the TTL.
    ///
    /// Two candidate sources, because a terminal-only sweep would miss the most
    /// common leak:
    ///
    /// 1. terminal tasks whose `updated_at` is older than the TTL
    /// 2. the root of a detached session, which sits in `paused` — not a terminal
    ///    state — and therefore never enters a terminal-only sweep
    ///
    /// A task in `awaiting_parent_review` is not terminal and its session is not
    /// detached, so an un-integrated delivery is never reclaimed.
    pub fn reclaim_idle_worktrees(&self, now: DateTime<Utc>) -> usize {
        let cutoff = now - chrono::Duration::from_std(WORKTREE_RECLAIM_TTL)
            .unwrap_or_else(|_| chrono::Duration::days(7));
        let sessions = {
            let repository = self
                .repository
                .lock()
                .expect("runtime repository mutex poisoned");
            match repository.detached_application_roots() {
                Ok(roots) => roots,
                Err(error) => {
                    eprintln!("yi-agent: detached root lookup failed: {error}");
                    Vec::new()
                }
            }
        };
        let mut reclaimed = 0;
        for session in sessions {
            reclaimed += self.reclaim_idle_session(&session, cutoff);
        }
        reclaimed
    }

    /// Reclaims the idle, reclaimable directories of one session.
    fn reclaim_idle_session(
        &self,
        session: &RootSessionId,
        cutoff: DateTime<Utc>,
    ) -> usize {
        let candidates = self.reclaim_candidates_in_session(session);
        let idle = candidates
            .into_iter()
            .filter(|candidate| {
                let is_root = candidate.parent_task_id.is_none();
                // A terminal child is idle-sweepable; a non-terminal child is not.
                // The root qualifies through the detached-session clause instead,
                // because a detached root sits in `paused`, which is not terminal.
                if !is_root && !task_state_is_terminal(&candidate.state) {
                    return false;
                }
                let updated = {
                    let repository = self
                        .repository
                        .lock()
                        .expect("runtime repository mutex poisoned");
                    let parsed = candidate.task_id.parse::<TaskId>().ok();
                    match parsed {
                        Some(parsed) => repository.task_updated_at(&parsed).ok().flatten(),
                        None => None,
                    }
                };
                matches!(updated, Some(updated) if updated < cutoff)
            })
            .collect::<Vec<_>>();
        // Hand the filtered set to the shared reclaim so the merge gate,
        // deepest-first ordering, and event recording stay in one place.
        self.reclaim_candidate_directories(session, idle)
    }
```

Add the state predicate next to the other free functions in `runtime.rs`, near `persisted_depth`:

```rust
/// Whether a persisted task state is one the state machine calls terminal.
///
/// `paused` and `awaiting_parent_review` are deliberately absent: a detached root
/// sits in `paused`, and an un-integrated delivery sits in
/// `awaiting_parent_review`. Neither may be swept by idle alone.
fn task_state_is_terminal(state: &str) -> bool {
    matches!(
        state,
        "completed"
            | "completed_no_changes"
            | "blocked"
            | "stalled"
            | "timed_out"
            | "budget_exhausted"
            | "failed"
            | "cancelled"
            | "recovery_required"
    )
}
```

Add the repository accessor to `yi-agent-rs/crates/yi-agent-store/src/repository.rs`, after `task_state`:

```rust
    /// A task's last transition time, used as the idle clock for reclaim.
    ///
    /// Returns `None` for a task with no row, which callers treat as "not idle
    /// enough to sweep" rather than as an error.
    pub fn task_updated_at(
        &self,
        task: &TaskId,
    ) -> Result<Option<DateTime<Utc>>, RepositoryError> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT updated_at FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|parsed| parsed.with_timezone(&Utc))
                .or_else(|| {
                    // SQLite's CURRENT_TIMESTAMP is 'YYYY-MM-DD HH:MM:SS' in UTC.
                    NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S")
                        .ok()
                        .map(|naive| naive.and_utc())
                })
        }))
    }
```

Confirm `chrono` and its `NaiveDateTime` are already available in `repository.rs` (the crate uses `chrono` for timestamps; if not, add `use chrono::{DateTime, NaiveDateTime, Utc};` to the existing import block).

- [ ] **Step 4: Hook the sweep into the daemon tick**

In `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`, modify the minute tick in the listener thread (lines 663-673):

```rust
            let mut last_schedule_minute = None;
            while !thread_stop.load(Ordering::Acquire) {
                let _ = runtime.block_on(coordinator.reconcile_worker_events());
                let now = Local::now();
                let minute = now
                    .with_second(0)
                    .and_then(|value| value.with_nanosecond(0));
                if minute != last_schedule_minute {
                    let _ = coordinator.evaluate_schedules(now);
                    // Reclaim runs on its own thread: git is slow and this loop
                    // must stay responsive to accept().
                    let reclaim_coordinator = Arc::clone(&coordinator);
                    std::thread::spawn(move || {
                        reclaim_coordinator.reclaim_idle_worktrees(chrono::Utc::now());
                    });
                    last_schedule_minute = minute;
                }
```

Confirm `chrono` is a dependency of `yi-agent-store` (it is used for `Local::now()` already); if `chrono::Utc` needs a use statement, add `use chrono::Utc;` to the existing `chrono` import.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store --test runtime_coordinator reclaim_idle`

Expected: both PASS.

Then run the full crate suites serially:

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-store`

Expected: all PASS.

- [ ] **Step 6: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/src/repository.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "feat(store): reclaim idle worktree directories on the daemon tick"
```

---

## Task 8: `yi-agent daemon gc`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs` (unit test module)

**Interfaces:**
- Consumes: `RuntimeRepository::reclaim_candidates` (Task 4).
- Produces:
  - `DaemonAction::Gc`
  - `IpcRequest::PreviewGc`
  - `IpcRequest::ConfirmGc { confirmation_token: String }`
  - `IpcResponse::GcPreview { entries: Vec<IpcGcEntry>, confirmation_token: String, expires_in_secs: u64 }`
  - `IpcResponse::GcCompleted { removed: usize }`
  - `pub struct IpcGcEntry { pub task_id: String, pub branch: String, pub path: String, pub state: String, pub merged: bool, pub dirty: bool }`

- [ ] **Step 1: Write the failing test**

Add to the unit test module in `yi-agent-rs/crates/yi-agent/src/main.rs`:

```rust
#[test]
fn daemon_action_exposes_gc() {
    use clap::Parser;
    let cli = crate::Cli::try_parse_from(["yi-agent", "daemon", "gc"]).unwrap();
    assert!(
        matches!(
            cli.command,
            Some(crate::config::Command::Daemon {
                action: crate::config::DaemonAction::Gc
            })
        ),
        "daemon gc must parse"
    );
}
```

If the CLI type is named differently, inspect `config.rs` for the `#[derive(Parser)]` struct and use its actual name.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent daemon_action_exposes_gc`

Expected: FAIL — no variant `Gc`.

- [ ] **Step 3: Add the CLI variant and IPC surface**

In `yi-agent-rs/crates/yi-agent/src/config.rs`, add to `DaemonAction` (lines 256-266):

```rust
    /// List reclaimable worktrees and, with confirmation, remove them.
    Gc,
```

In `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`, add to `IpcRequest` after `DetachApplicationRoot` (the variant spans lines 151-155):

```rust
    PreviewGc,
    ConfirmGc {
        confirmation_token: String,
    },
```

Add to `IpcResponse` after `ApplicationRootDetached` (around line 312):

```rust
    GcPreview {
        entries: Vec<IpcGcEntry>,
        confirmation_token: String,
        expires_in_secs: u64,
    },
    GcCompleted {
        removed: usize,
    },
```

Add the entry type near `IpcSchedule`:

```rust
/// One reclaimable worktree, as reported by `daemon gc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcGcEntry {
    pub task_id: String,
    pub branch: String,
    pub path: String,
    pub state: String,
    /// Whether `branch` is already contained in its parent's HEAD.
    pub merged: bool,
    /// Whether the worktree has modified or untracked files.
    pub dirty: bool,
}
```

- [ ] **Step 4: Implement the handlers**

In `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`, extend `ConfirmationStore` with a gc scope. `CancelScope` is a **struct** (`ipc.rs:50-54`) holding `task_ids`, `active_leases`, and `unmerged_deliveries`, not an enum, so the gc scope is an empty one and the literal task id is `"gc"`:

```rust
    fn issue_gc(&self) -> String {
        self.issue(
            "gc".into(),
            false,
            CancelScope {
                task_ids: Vec::new(),
                active_leases: Vec::new(),
                unmerged_deliveries: Vec::new(),
            },
        )
    }

    fn consume_gc(&self, token: &str) -> bool {
        self.consume(
            token,
            "gc",
            false,
            &CancelScope {
                task_ids: Vec::new(),
                active_leases: Vec::new(),
                unmerged_deliveries: Vec::new(),
            },
        )
    }
```

Both `issue` and `consume` are private to `ipc.rs` and already take a `CancelScope` by value and by reference respectively (`ipc.rs:62` and `ipc.rs:79`), so no signature changes are needed. `IpcCancelLease` and `IpcCancelDelivery` are the element types used by the cancel path.

Then add the request arms in `respond` (after the `DetachApplicationRoot` arm):

```rust
        IpcRequest::PreviewGc => {
            let entries = gc_entries(&repository)?;
            let token = confirmations.issue_gc();
            Ok(IpcResponse::GcPreview {
                entries,
                confirmation_token: token,
                expires_in_secs: CONFIRMATION_TTL.as_secs(),
            })
        }
        IpcRequest::ConfirmGc {
            confirmation_token,
        } => {
            if !confirmations.consume_gc(&confirmation_token) {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "gc confirmation token is invalid or expired",
                )));
            }
            let sessions = {
                let repository = RuntimeRepository::open(database_path)?;
                let mut sessions = repository.detached_application_roots()?;
                sessions.dedup();
                sessions
            };
            let mut removed = 0;
            for session in sessions {
                removed += coordinator.reclaim_session_worktrees(&session);
            }
            Ok(IpcResponse::GcCompleted { removed })
        }
```

Note `gc_entries` takes only the repository: it never needs the coordinator, because it only reads rows and shells out to git. Do not pass `coordinator` to it.

Add the listing helper as a free function in `ipc.rs`:

```rust
/// Lists reclaimable worktrees with the context a user needs to decide.
///
/// Read-only: it inspects rows and runs `git status` / `git merge-base` in the
/// listed worktrees. It removes nothing, so a dirty or unmerged worktree can be
/// reported safely.
fn gc_entries(repository: &RuntimeRepository) -> Result<Vec<IpcGcEntry>, IpcError> {
    let mut entries = Vec::new();
    let mut sessions = repository.detached_application_roots()?;
    sessions.dedup();
    for session in sessions {
        for candidate in repository.reclaim_candidates(&session)? {
            let Some(workspace) = candidate.workspace.clone() else {
                continue;
            };
            let dirty = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&workspace.path)
                .output()
                .map(|output| !output.stdout.is_empty())
                .unwrap_or(false);
            let merged = match candidate.parent_task_id.as_ref() {
                Some(parent) => {
                    let owner = {
                        let parsed = parent.parse::<TaskId>().ok();
                        match parsed {
                            Some(parsed) => repository.task_workspace_optional(&parsed).ok().flatten(),
                            None => None,
                        }
                    };
                    match owner {
                        Some(owner) => {
                            let owner_branch = Command::new("git")
                                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                                .current_dir(&owner.path)
                                .output()
                                .ok()
                                .map(|output| {
                                    String::from_utf8_lossy(&output.stdout).trim().to_owned()
                                });
                            match owner_branch {
                                Some(owner_branch) => Command::new("git")
                                    .args([
                                        "merge-base",
                                        "--is-ancestor",
                                        &workspace.branch,
                                        &owner_branch,
                                    ])
                                    .current_dir(&owner.path)
                                    .status()
                                    .map(|status| status.success())
                                    .unwrap_or(false),
                                None => false,
                            }
                        }
                        None => false,
                    }
                }
                None => false,
            };
            entries.push(IpcGcEntry {
                task_id: candidate.task_id.clone(),
                branch: workspace.branch.clone(),
                path: workspace.path.display().to_string(),
                state: candidate.state.clone(),
                merged,
                dirty,
            });
        }
    }
    Ok(entries)
}
```

Confirm `Command` and `TaskId` are in scope in `ipc.rs` (both are used already: `use std::process::Command;` at `ipc.rs:8`).

Note on scope: this task implements the **listing** and the **automatic-scope reclaim** only. Deleting branch refs and `task_workspaces` rows — the operations the spec marks as forfeiting reattachment — are deliberately not implemented here. They are a separate, explicitly confirmed operation, and adding them without a distinct confirmation path would violate the spec's requirement that row deletion be labelled as irreversible.

- [ ] **Step 5: Wire the CLI handler**

In `yi-agent-rs/crates/yi-agent/src/main.rs`, add the dispatch next to the existing `DaemonAction` handling (find it with `grep -n "DaemonAction::" yi-agent-rs/crates/yi-agent/src/main.rs`). Mirror the `Status` arm's structure:

```rust
        DaemonAction::Gc => {
            let response = yi_agent_store::ipc::send_request(
                &socket_path,
                yi_agent_store::ipc::IpcRequest::PreviewGc,
            )?;
            let yi_agent_store::ipc::IpcResponse::GcPreview {
                entries,
                confirmation_token,
                ..
            } = response
            else {
                anyhow::bail!("daemon rejected gc preview: {response:?}");
            };
            if entries.is_empty() {
                println!("no reclaimable worktrees");
                return Ok(());
            }
            for entry in &entries {
                println!(
                    "{}  branch={}  merged={}  dirty={}  state={}\n    {}",
                    entry.task_id, entry.branch, entry.merged, entry.dirty, entry.state, entry.path
                );
            }
            println!(
                "\nRemoving directories is reversible; deleting branches and workspace rows is NOT \
                 and forfeits session reattachment."
            );
            let response = yi_agent_store::ipc::send_request(
                &socket_path,
                yi_agent_store::ipc::IpcRequest::ConfirmGc {
                    confirmation_token,
                },
            )?;
            match response {
                yi_agent_store::ipc::IpcResponse::GcCompleted { removed } => {
                    println!("reclaimed {removed} worktree director{}", if removed == 1 { "y" } else { "ies" });
                }
                other => anyhow::bail!("daemon rejected gc: {other:?}"),
            }
            Ok(())
        }
```

Adapt `socket_path` and the return type to the surrounding function's actual names.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent daemon_action_exposes_gc`

Expected: PASS.

Then confirm the whole binary crate still builds and passes:

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent`

Expected: all PASS.

- [ ] **Step 7: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "feat(app): add daemon gc for reclaimable worktrees"
```

---

## Task 9: Documentation

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/bug-list.md`

- [ ] **Step 1: Add the capability entries**

Read `docs/project-management/subagent-runtime.md`. Add entries using only `[x]` / `[ ]` / `[-]` markers, each with a verifiable criterion:

- Worktree directories are reclaimed when a session detaches: `[x]` — `RuntimeCoordinator::reclaim_session_worktrees` (`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`), verified by `cargo test -p yi-agent-store --test runtime_coordinator reclaim_session_worktrees_removes_merged_children_and_keeps_unmerged_ones`.
- Idle worktree directories are reclaimed after 7 days: `[x]` — `RuntimeCoordinator::reclaim_idle_worktrees`, verified by `cargo test -p yi-agent-store --test runtime_coordinator reclaim_idle`.
- A reclaimed worktree is rebuilt before its worker starts: `[x]` — `prepare_task_workspace` rebuild branch, verified by `cargo test -p yi-agent-store --test runtime_coordinator a_reclaimed_worktree_is_rebuilt_before_a_worker_starts`.
- Reattachment survives a reclaim: `[x]` — no row or branch is deleted, verified by `cargo test -p yi-agent-store --test runtime_ipc detached_`.
- Manual reclaim surface: `[x]` — `yi-agent daemon gc`, verified by `cargo test -p yi-agent --bin yi-agent daemon_action_exposes_gc`.

- [ ] **Step 2: Update the index count**

In `docs/project-management/README.md`, update the `subagent-runtime` row's "完成 / 总计" count for the five new entries.

- [ ] **Step 3: Reconcile the bug list**

In `docs/bug-list.md`, update the worktree accumulation entry. Note that exit-time and TTL reclaim landed for clean worktrees, that dirty or unmerged worktrees are now listed by `daemon gc`, and that branch refs are deliberately retained so reattachment keeps working.

- [ ] **Step 4: Commit**

```bash
git add docs/project-management/subagent-runtime.md docs/project-management/README.md docs/bug-list.md
git commit -m "docs: record worktree directory reclaim"
```

---

## Final Verification

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

Then confirm the end-to-end property the design exists to guarantee, in a scratch repository:

```bash
# 1. Create a root worktree, commit, reclaim the directory.
# 2. Confirm the branch ref and its commits survive.
# 3. Confirm `git worktree add <path> <branch>` restores the tip exactly.
```

These three facts are already covered by `reclaim_directory_removes_only_the_directory_and_keeps_the_branch` and `reattach_worktree_attaches_an_existing_branch` in Task 1.

Finally, finish the branch with `superpowers:finishing-a-development-branch`.
