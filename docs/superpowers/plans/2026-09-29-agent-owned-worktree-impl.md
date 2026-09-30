# Agent-Owned Worktrees Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the daemon from creating, tracking, accepting, and reclaiming git worktrees; make `spawn_agent` take an explicit `workdir`; rename the overloaded "workspace" vocabulary so it stops hiding a worktree.

**Architecture:** The runtime stops owning worktree lifecycle. A child's execution root comes from the `workdir` the parent passes, looked up in a `WorkerWorkspaceRegistry` the application factory supplies; when there is no match (root, read-only child) the position-provider returns the mapped path and the worker runs in place. Delivery reporting stays (spec §3.7) but its identity and inspection come from the workdir, not from a `task_workspaces` row. Every worktree-specific capability is deleted, including the `daemon gc` command.

**Tech Stack:** Rust (edition 2024), cargo workspace under `yi-agent-rs/`, `git` subprocesses, SQLite via `rusqlite`, `tempfile` in tests.

**Spec:** `docs/superpowers/specs/2026-09-29-agent-owned-worktree-design.md` (sections cited as §N).

## Global Constraints

- Run every command from the worktree root with `--manifest-path yi-agent-rs/Cargo.toml`, so the worktree's own `target/` is used and the main checkout is undisturbed.
- Never run two cargo processes concurrently. Before each `cargo test`, run `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` and kill strays; a leftover test binary holds the target lock and causes hangs or exit 137.
- Run `cd yi-agent-rs && cargo fmt --all && cd ..` before every commit. `git commit` hooks do not format for you.
- Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`). **Never** add a `Co-Authored-By` trailer.
- Never commit to `main`. All work happens on this worktree's branch.
- Lock order is always **supervisor → repository**. Git never runs while holding the repository mutex.
- `ignore_inside_repository` / `ignore_project_path` in `yi-agent-tools/src/worktree.rs` are **kept** (§3.5). They still serve `ignore_project_local_runtime_state`.
- Do not `DROP` an existing `task_workspaces` table or its rows (§3.6). Only stop reading and writing it, and drop the `CREATE TABLE` line from fresh schema.
- Default mode stays **read-only** (§3.4).

---

## Vocabulary (read before Task 1)

Today one word, "workspace", names four different things. This plan disambiguates them once, in Task 1, and every later task relies on the new names.

| Old name | New name | What it actually is |
| --- | --- | --- |
| `TaskWorkspaceMode` | `ChildWriteMode` | A write-permission switch, not a directory |
| `AgentWorkspaceService` | `WorkerWorkspaceProvider` | Maps a task to the path it runs in |
| `AgentWorkspaceService::prepare_root` | `WorkerWorkspaceProvider::in_place_workspace` | The path a root runs in |
| `AgentWorkspaceService::prepare_child` | `WorkerWorkspaceProvider::workspace_in` | The path a child runs in |
| `AgentWorkspaceService::prepare_read_only` | `WorkerWorkspaceProvider::read_only_workspace` | The path a read-only task runs in |
| `AgentWorkerFactory::workspace_service` | `AgentWorkerFactory::default_workspace_service` | Factory-global position provider |
| `AgentWorkerFactory::workspace_service_for_application_root` | `AgentWorkerFactory::workspace_service_for_project` | Per-attached-project position provider |
| `AgentWorkerFactory::application_root_workspace_matches` | `AgentWorkerFactory::project_workspace_matches` | Project identity check |
| `DaemonWorkspaceService` | `WorkerWorkspaces` | The git-backed implementation |

The word "workspace" is reserved for the **`WorkerWorkspace` record a worker receives** (its `path`, `lease_id`, etc.). The word "workdir" is the directory a parent passes to `spawn_agent`. "Worktree" appears only where the agent names one itself.

Two new core concepts the later tasks use:

- `SpawnRequest.workdir: Option<PathBuf>` — the directory the parent asked for.
- `WorkerWorkspaceRegistry` — the application-owned lookup from a workdir to the prepared `WorkerWorkspace`, plus workdir-based delivery inspection.

---

## File Structure

| File | Responsibility | Change |
| --- | --- | --- |
| `crates/yi-agent-core/src/subagent/task.rs` | `ChildWriteMode`, `AgentTask` | Rename; drop `workspace` + `WorkspaceLeaseId` (T1, T8) |
| `crates/yi-agent-core/src/subagent/worker.rs` | `WorkerWorkspaceProvider`, `WorkerWorkspaceRegistry`, `SpawnRequest`, `WorkerStart`, `AgentWorkerFactory` | Rename; split trait; add `workdir` (T1, T2) |
| `crates/yi-agent-core/src/subagent/supervisor.rs` | Task graph, per-task mode + workdir | Rename; carry `workdir` (T1, T2) |
| `crates/yi-agent-store/src/repository.rs` | Persistence | Drop `task_workspaces` reads/writes/DDL and the two recycle events (T8) |
| `crates/yi-agent-store/src/runtime.rs` | `RuntimeCoordinator` | Position-provider rewiring; delete reclaim + auto-accept (T2–T6) |
| `crates/yi-agent-store/src/ipc.rs` | Daemon loop + IPC | Delete `gc`, detach reclaim, tick reclaim (T6, T7) |
| `crates/yi-agent/src/subagent_runtime.rs` | `WorkerWorkspaces` (git-backed) | Position mapping, registry, prepared set, workdir inspect (T2, T4, T5) |
| `crates/yi-agent/src/config.rs` | CLI | Delete `DaemonAction::Gc` (T7) |
| `crates/yi-agent/src/main.rs` | CLI dispatch | Delete `gc_daemon_client`; keep `ignore_project_local_runtime_state` (T7, T9) |
| `crates/yi-agent-tools/src/worktree.rs` | Git primitives | Delete orchestration methods; keep `ignore_*` (T5–T6) |
| `crates/yi-agent-core/src/agent.rs` | System prompt | Reword "Subagent integration" (T9) |
| `docs/bug-list.md` | Status log | Update the worktree entry (T9) |

---

## Task 1: Rename the overloaded vocabulary

Behavior is identical after this task; only names change. It is a separate task because it is the only way to keep the tree compiling while later tasks delete things.

**Files:**
- Modify: `crates/yi-agent-core/src/subagent/task.rs`, `worker.rs`, `supervisor.rs`
- Modify: `crates/yi-agent-store/src/repository.rs`, `runtime.rs`, `ipc.rs`
- Modify: `crates/yi-agent/src/subagent_runtime.rs`, `tui/app.rs`
- Test: every file that implements the two traits

**Interfaces:**
- Consumes: nothing.
- Produces (names every later task uses):
  - `pub enum ChildWriteMode { Coding, ReadOnly }` with `as_str`, `parse`, `Default = ReadOnly`
  - `pub trait WorkerWorkspaceProvider { fn in_place_workspace(..); fn workspace_in(..); fn read_only_workspace(..); }`
  - `AgentWorkerFactory::default_workspace_service`, `::workspace_service_for_project`, `::project_workspace_matches`
  - All other trait methods keep their names for now: `supports_coding`, `inspect_delivery`, `cleanup_prepared`, `contains_commit`, `cleanup_accepted`, `reclaim_worktree`, `reattach_workspace`, `is_merged_into`.

- [x] **Step 1: Apply the mechanical renames**

```bash
cd yi-agent-rs
rg -l 'TaskWorkspaceMode' --glob '*.rs' | xargs sed -i '' 's/TaskWorkspaceMode/ChildWriteMode/g'
rg -l 'AgentWorkspaceService' --glob '*.rs' | xargs sed -i '' 's/AgentWorkspaceService/WorkerWorkspaceProvider/g'
rg -l 'prepare_read_only' --glob '*.rs' | xargs sed -i '' 's/prepare_read_only/read_only_workspace/g'
rg -l 'prepare_root' --glob '*.rs' | xargs sed -i '' 's/prepare_root/in_place_workspace/g'
rg -l 'prepare_child' --glob '*.rs' | xargs sed -i '' 's/prepare_child/workspace_in/g'
rg -l 'UnavailableWorkspaceService' --glob '*.rs' | xargs sed -i '' 's/UnavailableWorkspaceService/UnavailableWorkspaceProvider/g'
rg -l 'workspace_service_for_application_root' --glob '*.rs' | xargs sed -i '' 's/workspace_service_for_application_root/workspace_service_for_project/g'
rg -l 'application_root_workspace_matches' --glob '*.rs' | xargs sed -i '' 's/application_root_workspace_matches/project_workspace_matches/g'
```

`prepare_root`/`prepare_child` are also **test-local helpers** with unrelated bodies; the rename is safe (same signature, same semantics) but each must still compile.

- [x] **Step 2: Rename the factory-global accessor by hand**

`s/workspace_service(/default_workspace_service(/` is unsafe: the same spelling is a field name and a struct field initializer. Edit these three sites by hand instead.

`crates/yi-agent-core/src/subagent/worker.rs` — trait declaration and its default body:

```rust
    fn default_workspace_service(
        &self,
    ) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        None
    }

    fn workspace_service_for_project(
        &self,
        _workspace: &std::path::Path,
    ) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        self.default_workspace_service()
    }
```

`crates/yi-agent-store/src/runtime.rs:583`:

```rust
        let workspace_service = factory.default_workspace_service();
```

`crates/yi-agent-store/src/runtime.rs:701` — already `self.factory.workspace_service_for_project(requested_workspace)` after Step 1.

- [x] **Step 3: Rename the struct definition too**

`crates/yi-agent-store/src/ipc.rs` and the test factories declare `fn workspace_service(&self)`. Update each declaration to `default_workspace_service`. Find them with:

```bash
cd yi-agent-rs && rg -n 'fn workspace_service\b' --glob '*.rs'
```

Only the declarations change; bodies are untouched.

- [x] **Step 4: Verify the whole workspace still compiles**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --no-run && cargo test -p yi-agent-store --no-run && cargo test -p yi-agent --no-run
```

Expected: all three compile. Any `cannot find`/`no method` error names a site Step 1–3 missed.

- [x] **Step 5: Verify core behavior is unchanged**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core
```

Expected: PASS, same count as before the rename.

- [x] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "refactor: disambiguate the workspace vocabulary"
```

---

## Task 2: Carry `workdir` from `spawn_agent` to the worker

**Files:**
- Modify: `crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `crates/yi-agent-store/src/runtime.rs`
- Modify: `crates/yi-agent/src/subagent_runtime.rs`
- Test: `crates/yi-agent-core/src/subagent/supervisor.rs` (unit tests)

**Interfaces:**
- Consumes: `WorkerWorkspaceProvider` (T1).
- Produces:
  - `SpawnRequest.workdir: Option<PathBuf>`
  - `AgentSupervisor::spawn_with_objective(&mut self, parent_id: TaskId, objective: String, mode: ChildWriteMode, workdir: Option<PathBuf>) -> Result<TaskId, SpawnError>`
  - `AgentSupervisor::spawn_workdir(&self, task: &TaskId) -> Option<PathBuf>`
  - `RuntimeCoordinator::spawn_child_with_objective(..., workdir: Option<PathBuf>)`
  - `WorkerWorkspaceProvider::workspace_in` no longer derives a temp path; it resolves a registered one (see Task 4).

- [x] **Step 1: Write the failing core test**

Append to the `#[cfg(test)] mod tests` block in `crates/yi-agent-core/src/subagent/supervisor.rs`:

```rust
    #[test]
    fn spawn_request_carries_the_parents_workdir() {
        let mut supervisor = AgentSupervisor::new(RootSessionId::new());
        let root = supervisor.root_task_id().clone();
        let child = supervisor
            .spawn_with_objective(
                root,
                "implement".into(),
                ChildWriteMode::Coding,
                Some(PathBuf::from("/tmp/yi-agent-impl")),
            )
            .unwrap();

        assert_eq!(
            supervisor.spawn_workdir(&child),
            Some(PathBuf::from("/tmp/yi-agent-impl"))
        );
    }
```

- [x] **Step 2: Run it to confirm it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib spawn_request_carries_the_parents_workdir
```

Expected: FAIL to compile — `spawn_with_objective` takes 3 arguments, and `spawn_workdir` does not exist.

- [x] **Step 3: Create `SpawnRequest` and carry the field**

`SpawnRequest` does not exist yet; the parallel `WorkerStart` already does, so follow its shape. Add it to `crates/yi-agent-core/src/subagent/worker.rs`:

```rust
/// Everything a parent decision and the runtime position provider need to
/// place one spawned task. The parent supplies the objective and the mode; the
/// runtime supplies the resolved workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub objective: String,
    pub mode: ChildWriteMode,
    /// The directory the parent asked this task to run in. `None` means "you
    /// decide", which is what a read-only task and the root both use.
    pub workdir: Option<PathBuf>,
}

impl SpawnRequest {
    pub fn new(objective: String, mode: ChildWriteMode, workdir: Option<PathBuf>) -> Self {
        Self {
            objective,
            mode,
            workdir,
        }
    }

    pub fn with_workdir(mut self, workdir: PathBuf) -> Self {
        self.workdir = Some(workdir);
        self
    }
}
```

`AgentSupervisor::spawn_with_objective` takes a `SpawnRequest`:

```rust
    pub fn spawn_with_objective(
        &mut self,
        parent_id: TaskId,
        request: SpawnRequest,
    ) -> Result<TaskId, SpawnError> {
```

and where the child is built:

```rust
        let mut child = AgentTask::new_child(parent.root_session_id.clone(), parent_id.clone());
        child.depth = child_depth;
        let child_id = child.id.clone();
        self.tasks.insert(child_id.clone(), child);
        self.objectives
            .insert(child_id.clone(), request.objective.clone());
        self.workspace_modes
            .insert(child_id.clone(), request.mode);
        self.workdirs.insert(child_id.clone(), request.workdir);
```

Add the map and the accessor next to `workspace_modes`:

```rust
    workdirs: HashMap<TaskId, Option<PathBuf>>,
```

```rust
        self.workdirs = HashMap::new();
```

to every constructor (there are three: `new`, `new_with_objective`, and the two `from_recovered*` paths — the compiler lists them), and:

```rust
    pub fn spawn_workdir(&self, task: &TaskId) -> Option<PathBuf> {
        self.workdirs.get(task).cloned().flatten()
    }
```

The `workdirs` constructor call is `self.workdirs = HashMap::new();`, added wherever `self.workspace_modes = HashMap::new();` already appears — the compiler lists those `from_recovered*` constructors.

The `spawn` helper (the legacy 2-argument form) passes a read-only request:

```rust
    pub fn spawn(&mut self, parent_id: TaskId, objective: String) -> Result<TaskId, SpawnError> {
        self.spawn_with_objective(
            parent_id,
            SpawnRequest::new(objective, ChildWriteMode::ReadOnly, None),
        )
    }
```

- [x] **Step 4: Update every call site the compiler names**

Production call sites (`rg -n 'spawn_with_objective' crates/ --glob '*.rs' | grep -v '/tests/'`):
- `crates/yi-agent-core/src/subagent/supervisor.rs:1009` — the internal `spawn` helper (now passes a `SpawnRequest`).
- `crates/yi-agent-core/src/subagent/supervisor.rs:1816` (the core `spawn_agent` tool) — reads `workdir` from the tool arguments:

```rust
        let workdir = match args.get("workdir") {
            None => None,
            Some(Value::String(value)) if !value.trim().is_empty() => {
                Some(PathBuf::from(value))
            }
            Some(Value::String(_)) => return ToolResult::error("workdir must not be blank"),
            Some(_) => return ToolResult::error("workdir must be a string"),
        };
        let request = SpawnRequest::new(task.to_string(), mode, workdir);
```

- `crates/yi-agent-store/src/runtime.rs:1195` — `spawn_child_with_objective` forwards it (Step 5).

`start_worker`'s recovered-root replay (`runtime.rs:430`–`449`) does **not** call `spawn_with_objective`; it rebuilds the supervisor with `from_recovered_root` / `from_recovered_gated_root`, whose constructors must initialize the new `workdirs` map (Step 3). No signature change is needed there.

- [x] **Step 5: Thread it through the coordinator**

`crates/yi-agent-store/src/runtime.rs`:

```rust
    pub async fn spawn_child_with_objective(
        &self,
        session: &RootSessionId,
        parent: &TaskId,
        objective: String,
        workspace_mode: ChildWriteMode,
        model: Option<String>,
        workdir: Option<PathBuf>,
    ) -> Result<TaskId, RuntimeCoordinatorError> {
```

and at the `spawn_with_objective` call:

```rust
            let child = supervisor.spawn_with_objective(
                parent.clone(),
                objective.clone(),
                workspace_mode,
                workdir.clone(),
            )?;
```

`spawn_child_and_admit` and `spawn_application_child` gain the same trailing `workdir: Option<PathBuf>` parameter and forward it.

- [x] **Step 6: Run the test to confirm it passes**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib spawn_request_carries_the_parents_workdir
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "feat: carry a parent-chosen workdir into spawn requests"
```

---

## Task 3: Delete the `supports_coding` gate

`supports_coding` exists only to refuse `Coding` when there is no repository (§3.2). With the workdir coming from the parent, the daemon is no longer the entity that needs a repository.

**Files:**
- Modify: `crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `crates/yi-agent-store/src/runtime.rs`
- Modify: `crates/yi-agent/src/subagent_runtime.rs`
- Test: `crates/yi-agent-store/tests/runtime_coordinator.rs`, `crates/yi-agent-store/src/runtime.rs` (evidence reason)

**Interfaces:**
- Consumes: T1 renames.
- Produces: `RuntimeCoordinatorError::CodingRequiresGitRepository` is removed; `start_worker`'s failure evidence maps every provisioning failure to `workspace_provision_failed`.

- [x] **Step 1: Delete the trait method**

Remove from `WorkerWorkspaceProvider` (`worker.rs`) the declaration and its default body:

```rust
    /// Whether this service can create coding worktrees. A non-git service
    /// returns `false`, forcing the session into read-only mode.
    fn supports_coding(&self) -> bool {
        true
    }
```

- [x] **Step 2: Delete the `DaemonWorkspaceService` override**

In `crates/yi-agent/src/subagent_runtime.rs` remove:

```rust
    fn supports_coding(&self) -> bool {
        self.is_git_repository
    }
```

The `is_git_repository` field becomes unused; if the compiler now warns, it is deleted in Task 4 when the struct is reshaped.

- [x] **Step 3: Delete every gate**

`crates/yi-agent-store/src/runtime.rs`:

```rust
            let session_supports_coding = self
                .workspace_service_for(session)
                .is_some_and(|service| service.supports_coding());
            if workspace_mode == ChildWriteMode::Coding
                && session_supports_coding
                && supervisor.workspace_mode(parent) == ChildWriteMode::ReadOnly
            {
                return Err(RuntimeCoordinatorError::Supervisor(
                    "read-only tasks cannot spawn coding children".into(),
                ));
            }
```

Delete that block. Delete `if !service.supports_coding() { return Err(RuntimeCoordinatorError::CodingRequiresGitRepository); }` in `prepare_task_workspace`. Delete the now-unreachable arm in `start_worker`:

```rust
                let reason = match &error {
                    RuntimeCoordinatorError::CodingRequiresGitRepository => {
                        "coding_requires_git_repository"
                    }
                    _ => "workspace_provision_failed",
                };
```

becomes:

```rust
                let reason = "workspace_provision_failed";
```

Delete the variant from the error enum:

```rust
    #[error("coding requires a git repository")]
    CodingRequiresGitRepository,
```

- [x] **Step 4: Delete the tests that asserted the gate**

```bash
cd yi-agent-rs && rg -n 'coding_requires_git_repository|read-only tasks cannot spawn coding children|supports_coding' --glob '*.rs'
```

Delete each resulting test (`coding_child_fails_clearly_without_a_git_repository` in `runtime_coordinator.rs`, `runtime_coordinator.rs:4126`, and the two `subagent_runtime.rs` unit tests `git_workspace_service_supports_coding` / the non-git assertion at `2345`). They assert a behavior the spec deletes.

- [x] **Step 5: Verify**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --no-run && cargo test -p yi-agent-store --test runtime_coordinator
```

Expected: compiles; the remaining `runtime_coordinator` cases pass.

- [x] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "refactor: drop the daemon-side coding git gate"
```

---

## Task 4: Run every task in place; worktrees come from the parent

This is the core of §3.2 and §3.3. After it, `start_worker` no longer creates a directory: it looks the request's workdir up in a registry the application populated, and falls back to an in-place path.

**Files:**
- Modify: `crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `crates/yi-agent-store/src/runtime.rs`
- Modify: `crates/yi-agent/src/subagent_runtime.rs`
- Test: `crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: `SpawnRequest.workdir`, `AgentSupervisor::spawn_workdir` (T2).
- Produces:
  - `pub trait WorkerWorkspaceRegistry: Send + Sync { fn register_prepared(&self, workspace: &WorkerWorkspace); fn prepared_workspace_for_workdir(&self, workdir: &Path) -> Option<WorkerWorkspace>; }`
  - `AgentWorkerFactory::worker_workspace_registry(&self) -> Option<Arc<dyn WorkerWorkspaceRegistry>>` (default `None`)
  - `WorkerWorkspaceProvider::workspace_in` now takes a resolved workspace, not a parent worktree:

    ```rust
    fn workspace_in(&self, task: &TaskId, workdir: &Path) -> Result<WorkerWorkspace, WorkerError>;
    ```

  - `WorkerStart.workspace` must be `Some`; a `None` remains a startup error (the invariant from `2026-09-26-subagent-readonly-default-design.md` §2 is preserved).

- [x] **Step 1: Reuse the file's existing fixture**

There is no shared harness in `runtime_coordinator.rs`; each test builds its own. The one this task needs already exists — `WorkspaceObservingFactory` (line 460) records every `WorkerStart` into a `starts` vector and installs a `StaticWorkspaceService`; `worker_receives_its_persisted_workspace_before_provider_start` (line 724) is the template. First make the test doubles honour the split trait:

- `StaticWorkspaceService` gains `impl WorkerWorkspaceRegistry` (it already holds the one `WorkerWorkspace`): `register_prepared` stores it, `prepared_workspace_for_workdir` returns it when the canonical paths match, `inspect_delivery` returns an error ("test provider cannot inspect").
- `WorkspaceObservingFactory::workspace_service()` returns it (renamed to `default_workspace_service` in Task 1), and the new `worker_workspace_registry()` returns the same `Arc`.

- [x] **Step 2: Write the failing coordinator test**

Add next to `worker_receives_its_persisted_workspace_before_provider_start`:

```rust
#[tokio::test]
async fn a_coding_child_runs_in_the_workdir_its_parent_prepared() {
    let directory = TempDir::new().unwrap();
    let project_root = directory.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let prepared = directory.path().join("prepared-child");
    std::fs::create_dir_all(&prepared).unwrap();
    let database = directory.path().join("runtime.sqlite");
    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: project_root,
        path: prepared.clone(),
        branch: "feat/yi-agent-prepared-child".into(),
        parent_branch: "main".into(),
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
    };
    let starts = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        database: database.clone(),
        starts: Arc::clone(&starts),
        handles: Arc::new(Mutex::new(Vec::new())),
        workspace_service: Arc::new(StaticWorkspaceService {
            workspace: workspace.clone(),
        }),
    });
    // The parent prepared this directory; the runtime must find it, not make one.
    factory
        .worker_workspace_registry()
        .unwrap()
        .register_prepared(&workspace);
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator
        .spawn_child_and_admit(
            &session,
            &root,
            "implement".into(),
            ChildWriteMode::Coding,
            None,
            Some(prepared.clone()),
        )
        .await
        .unwrap();

    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = starts.lock().unwrap();
    let started = starts
        .iter()
        .find(|start| start.task_id == child)
        .expect("the child worker started");
    assert_eq!(
        started.workspace.as_ref().map(|workspace| &workspace.path),
        Some(&prepared)
    );
    assert!(
        !prepared.join(".worktrees").exists(),
        "no worktree was created"
    );
}
```

- [x] **Step 3: Run it to confirm it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator a_coding_child_runs_in_the_workdir_its_parent_prepared
```

Expected: FAIL — `spawn_child_and_admit` takes no `workdir` yet, and the worker receives a generated `.worktrees/...` path.

- [x] **Step 4: Add the registry trait**

In `crates/yi-agent-core/src/subagent/worker.rs`:

```rust
/// Application-owned lookup from a `spawn_agent` workdir to the workspace the
/// application prepared for it. The runtime never creates a directory; it only
/// asks whether one was prepared.
pub trait WorkerWorkspaceRegistry: Send + Sync {
    fn register_prepared(&self, workspace: &WorkerWorkspace);

    fn prepared_workspace_for_workdir(&self, workdir: &std::path::Path) -> Option<WorkerWorkspace>;
}
```

and on `AgentWorkerFactory`:

```rust
    /// The application's prepared-workspace registry, when it has one.
    fn worker_workspace_registry(&self) -> Option<Arc<dyn WorkerWorkspaceRegistry>> {
        None
    }
```

- [x] **Step 5: Implement it in `yi-agent`**

In `crates/yi-agent/src/subagent_runtime.rs`, reshape `DaemonWorkspaceService` into `WorkerWorkspaces` and give it the registry:

```rust
pub struct WorkerWorkspaces {
    repository_root: PathBuf,
    prepared: Mutex<HashMap<PathBuf, WorkerWorkspace>>,
}

impl WorkerWorkspaceRegistry for WorkerWorkspaces {
    fn register_prepared(&self, workspace: &WorkerWorkspace) {
        self.prepared
            .lock()
            .expect("prepared workspace mutex poisoned")
            .insert(canonical(workspace.path.clone()), workspace.clone());
    }

    fn prepared_workspace_for_workdir(&self, workdir: &std::path::Path) -> Option<WorkerWorkspace> {
        self.prepared
            .lock()
            .expect("prepared workspace mutex poisoned")
            .get(&canonical(workdir.to_path_buf()))
            .cloned()
    }
}
```

`workspace_in` resolves rather than creates:

```rust
    fn workspace_in(&self, task: &TaskId, workdir: &Path) -> Result<WorkerWorkspace, WorkerError> {
        WorkerWorkspaceRegistry::prepared_workspace_for_workdir(self, workdir).ok_or_else(|| {
            WorkerError::Startup(format!(
                "no prepared workspace for workdir {}; the parent must prepare it before spawning {task}",
                workdir.display()
            ))
        })
    }
```

`in_place_workspace` and `read_only_workspace` both return the position for that task:

```rust
    fn in_place_workspace(&self, _root_session_id: &RootSessionId, task: &TaskId, _attempt_id: &AttemptId) -> Result<WorkerWorkspace, WorkerError> {
        self.read_only_workspace(None, task)
    }

    fn read_only_workspace(&self, parent: Option<&WorkerWorkspace>, _task: &TaskId) -> Result<WorkerWorkspace, WorkerError> {
        let path = parent
            .map(|workspace| workspace.path.clone())
            .unwrap_or_else(|| self.repository_root.clone());
        Ok(in_place_workspace_at(self.repository_root.clone(), path))
    }
```

where `in_place_workspace_at` centralizes the empty-branch record:

```rust
fn in_place_workspace_at(repository_root: PathBuf, path: PathBuf) -> WorkerWorkspace {
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root,
        path,
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    }
}
```

`DaemonAgentWorkerFactory` keeps one `Arc<WorkerWorkspaces>` and exposes it both ways:

```rust
    fn worker_workspace_registry(&self) -> Option<Arc<dyn WorkerWorkspaceRegistry>> {
        Some(self.workspaces.clone())
    }

    fn workspace_service_for_project(
        &self,
        workspace: &std::path::Path,
    ) -> Option<Arc<dyn WorkerWorkspaceProvider>> {
        Some(Arc::new(WorkerWorkspaces::new(workspace.to_path_buf())))
    }
```

- [x] **Step 6: Rewrite `prepare_task_workspace`**

`crates/yi-agent-store/src/runtime.rs` — replace the body:

```rust
    fn prepare_task_workspace(
        &self,
        supervisor: &mut AgentSupervisor,
        session: &RootSessionId,
        task: &TaskId,
        _attempt: &AttemptId,
        workspace_mode: ChildWriteMode,
    ) -> Result<Option<WorkerWorkspace>, RuntimeCoordinatorError> {
        // A coding task runs where its parent prepared it. The runtime resolves
        // the path; it never creates one.
        let workspace = if workspace_mode == ChildWriteMode::Coding {
            let workdir = supervisor.spawn_workdir(task).ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(format!(
                    "coding task {task} was spawned without a workdir"
                ))
            })?;
            let registry = self.factory.worker_workspace_registry().ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "coding task requires a prepared-workspace registry".into(),
                )
            })?;
            let prepared = registry
                .prepared_workspace_for_workdir(&workdir)
                .ok_or_else(|| {
                    RuntimeCoordinatorError::Supervisor(format!(
                        "no workspace was prepared for workdir {}",
                        workdir.display()
                    ))
                })?;
            let provider = self
                .workspace_service_for(session)
                .ok_or(RuntimeCoordinatorError::WorkspaceServiceUnavailable)?;
            provider
                .workspace_in(task, &workdir)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?
                .with_lease(prepared.lease_id)
        } else {
            let provider = self
                .workspace_service_for(session)
                .ok_or(RuntimeCoordinatorError::WorkspaceServiceUnavailable)?;
            let parent = self.nearest_ancestor_workspace(supervisor, task)?;
            provider
                .read_only_workspace(parent.as_ref(), task)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?
        };
        supervisor
            .assign_workspace(task, workspace.lease_id.clone())
            .map_err(RuntimeCoordinatorError::Supervisor)?;
        Ok(Some(workspace))
    }
```

`WorkerWorkspace::with_lease` is a new two-line helper on the core struct:

```rust
    pub fn with_lease(mut self, lease_id: WorkspaceLeaseId) -> Self {
        self.lease_id = lease_id;
        self
    }
```

`WorkspaceServiceUnavailable` is a new error variant replacing `CodingRequiresGitRepository`:

```rust
    #[error("session has no workspace position provider")]
    WorkspaceServiceUnavailable,
```

- [x] **Step 7: Delete root's worktree provisioning (§3.2)**

`crates/yi-agent-store/src/runtime.rs` — delete `root_mode_for` and its two call sites (lines 707, 844); both become the constant:

```rust
        let root_mode = ChildWriteMode::ReadOnly;
```

The comment at line 430 (`a factory-global guess would wrongly promote a non-git root to Coding`) goes with it: there is no `Coding` root anymore.

- [x] **Step 8: Run the test to confirm it passes**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator a_coding_child_runs_in_the_workdir_its_parent_prepared
```

Expected: PASS.

- [x] **Step 9: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "feat: run tasks in place and take child workdirs from the parent"
```

---

## Task 5: Workdir-based delivery inspection

Delivery reporting stays (§3.7); only its worktree machinery goes. `inspect_delivery` stops building a `ChildWorktree` and probes the workdir instead.

**Files:**
- Modify: `crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `crates/yi-agent/src/subagent_runtime.rs`
- Modify: `crates/yi-agent-store/src/runtime.rs`
- Modify: `crates/yi-agent-tools/src/worktree.rs`
- Test: `crates/yi-agent-tools/tests/subagent_worktree.rs`

**Interfaces:**
- Consumes: `WorkerWorkspaceRegistry` (T4).
- Produces:
  - `WorkerWorkspaceRegistry::inspect_delivery(&self, workspace: &WorkerWorkspace) -> Result<DeliveryReport, WorkerError>` — moved onto the registry, off the position-provider trait.
  - `yi_agent_tools::worktree::WorkdirDelivery { branch: String, base_commit: String, head_commit: String, clean: bool }`
  - `WorktreeService::inspect_workdir(&self, workdir: &Path, base: &str) -> Result<WorkdirDelivery, WorktreeError>`

- [x] **Step 1: Write the failing tools test**

```rust
#[test]
fn inspect_workdir_reports_head_and_cleanliness_without_a_worktree() {
    let (repo, _base) = repository();
    let service = WorktreeService::new();
    let before = git(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(repo.path().join("delivery.txt"), "ready\n").unwrap();
    git(repo.path(), &["add", "delivery.txt"]);
    git(repo.path(), &["commit", "-m", "delivery"]);

    let dirty = service.inspect_workdir(repo.path(), &before).unwrap();
    assert!(dirty.clean, "committed work is clean");
    assert_ne!(dirty.head_commit, before);

    std::fs::write(repo.path().join("delivery.txt"), "not committed\n").unwrap();
    let dirty = service.inspect_workdir(repo.path(), &before).unwrap();
    assert!(!dirty.clean, "an uncommitted edit is reported dirty");
}
```

- [x] **Step 2: Run it to confirm it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree inspect_workdir_reports_head_and_cleanliness_without_a_worktree
```

Expected: FAIL — no method `inspect_workdir`.

- [x] **Step 3: Implement the primitive**

In `crates/yi-agent-tools/src/worktree.rs`:

```rust
pub struct WorkdirDelivery {
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    pub clean: bool,
}

impl WorktreeService {
    /// Reads a workdir's delivery facts with plain git probes. It never requires
    /// the directory to be a registered worktree.
    pub fn inspect_workdir(
        &self,
        workdir: &Path,
        base: &str,
    ) -> Result<WorkdirDelivery, WorktreeError> {
        let head_commit = git(workdir, &["rev-parse", "HEAD"])?;
        let clean = git(workdir, &["status", "--porcelain"])?.trim().is_empty();
        if !clean {
            return Err(WorktreeError::DirtyWorkdir {
                path: workdir.to_path_buf(),
            });
        }
        if head_commit == base {
            return Err(WorktreeError::NoCommitsBeyond {
                path: workdir.to_path_buf(),
            });
        }
        Ok(WorkdirDelivery {
            branch: current_branch(workdir)?,
            base_commit: base.to_owned(),
            head_commit,
            clean,
        })
    }
}
```

Add the two error variants. Their `Display` must keep the substrings the retry prompts match:

```rust
    #[error("child worktree is dirty: {path}")]
    DirtyWorkdir { path: PathBuf },
    #[error("child delivery has no commits beyond base: {path}")]
    NoCommitsBeyond { path: PathBuf },
```

`subagent_runtime.rs:779` and `:783` match on `"child worktree is dirty"` and `"child delivery has no commits beyond"`, so these strings are load-bearing.

- [x] **Step 4: Move inspection onto the registry**

Add to `WorkerWorkspaceRegistry` in `worker.rs`:

```rust
    fn inspect_delivery(
        &self,
        workspace: &WorkerWorkspace,
    ) -> Result<DeliveryReport, WorkerError>;
```

and remove `inspect_delivery` from `WorkerWorkspaceProvider`. In `subagent_runtime.rs`, `WorkerWorkspaces::inspect_delivery` derives the base from the recorded task branch and delegates:

```rust
    fn inspect_delivery(&self, workspace: &WorkerWorkspace) -> Result<DeliveryReport, WorkerError> {
        let delivery = self
            .service
            .inspect_workdir(&workspace.path, &workspace.base_commit)
            .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))?;
        Ok(DeliveryReport::coding(
            delivery.head_commit,
            delivery.branch,
            workspace.lease_id.clone(),
            serde_json::json!({
                "kind": "clean_delivery",
                "branch": delivery.branch,
                "base_commit": delivery.base_commit,
                "head_commit": delivery.head_commit,
                "clean": delivery.clean,
            })
            .to_string(),
        ))
    }
```

- [x] **Step 5: Point both consumers at the registry**

`crates/yi-agent-store/src/runtime.rs:2487` (`confirm_review`):

```rust
        let inspected_commit = if let Some(workspace) = preview_workspace.as_ref() {
            if let Some(registry) = self.factory.worker_workspace_registry() {
                registry
                    .inspect_delivery(workspace)
                    .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?
                    .commit
            } else {
                delivery.commit.clone()
            }
        } else {
            delivery.commit.clone()
        };
```

`crates/yi-agent/src/subagent_runtime.rs:689` (worker terminal) — replace `service.inspect_delivery(&workspace_for_delivery)` with the same registry call, keeping the dirty/empty retry branches verbatim.

- [x] **Step 6: Delete the worktree-shaped primitive**

Remove `WorktreeService::inspect_delivery` and `ChildWorktree` from `worktree.rs`, and the tests for them from `subagent_worktree.rs`. `merge_inspected_delivery`, `validate_parent_base`, `merge_accepted`, `remove_accepted_clean` are deleted in Task 6; this step only removes the inspection path Task 5 replaced.

- [x] **Step 7: Verify**

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree && cargo test -p yi-agent --bin yi-agent
```

Expected: PASS. The child's dirty-delivery retry prompt still fires, because the error strings are unchanged.

- [x] **Step 8: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "refactor: inspect deliveries through the workdir, not the worktree"
```

---

## Task 6: Delete automatic acceptance and reclaim

**Files:**
- Modify: `crates/yi-agent-store/src/runtime.rs`
- Modify: `crates/yi-agent-store/src/ipc.rs`
- Modify: `crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `crates/yi-agent/src/subagent_runtime.rs`
- Modify: `crates/yi-agent-tools/src/worktree.rs`
- Test: `crates/yi-agent-store/tests/runtime_coordinator.rs`, `crates/yi-agent-store/tests/runtime_ipc.rs`, `crates/yi-agent-tools/tests/subagent_worktree.rs`

**Interfaces:**
- Consumes: T5.
- Produces:
  - `WorkerWorkspaceProvider` keeps only `in_place_workspace`, `workspace_in`, `read_only_workspace`.
  - `WorkerWorkspaceRegistry` keeps only `register_prepared`, `prepared_workspace_for_workdir`, `inspect_delivery`.
  - `RuntimeCoordinator::accept_review` no longer calls a recycle.
  - `WORKTREE_RECLAIM_TTL` is deleted.

- [x] **Step 1: Delete `reconcile_integrated_deliveries` and its caller**

In `runtime.rs`, remove the whole `reconcile_integrated_deliveries` method (line 3263) and its call inside the reconcile pass. The comment "It runs on every reconcile pass because the application root is not a daemon worker" goes with it: there is nothing left to observe.

- [x] **Step 2: Delete the recycle chain**

Remove `recycle_accepted_delivery` (line 2118) and the call at the end of `accept_review`:

```rust
        self.recycle_accepted_delivery(&session, &parent, task)
            .await;
```

This leaves `accept_review` with one job: record the accepted review. That is §3.5's "自动验收" removal; the parent's own merge is the integration, and the daemon no longer watches for it.

- [x] **Step 3: Delete the reclaim surface**

Remove from `runtime.rs`: `reclaim_session_worktrees`, `reclaim_idle_worktrees`, `reclaim_idle_session`, `reclaim_candidate_directories`, `reclaim_candidates_in_session`, `record_recycle_event`, `task_state_is_terminal` if it has no other caller, and `WORKTREE_RECLAIM_TTL` (line 51).

Remove from `ipc.rs` the detach-time call (line 2621) and the daemon tick's reclaim thread (line 804). The detach handler becomes:

```rust
            runtime.block_on(coordinator.detach_application_root(
                &session_id,
                &root_task_id,
                &capability,
            ))?;
            Ok(IpcResponse::ApplicationRootDetached)
```

The tick keeps `evaluate_schedules` and `reconcile_worker_events` and drops the spawn block.

- [x] **Step 4: Delete the trait methods and the git primitives**

Remove from `WorkerWorkspaceProvider` and `WorkerWorkspaceRegistry`: `cleanup_prepared`, `contains_commit`, `cleanup_accepted`, `reclaim_worktree`, `reattach_workspace`, `is_merged_into`. Remove their implementations from `WorkerWorkspaces` and from `UnavailableWorkspaceProvider`.

Remove from `worktree.rs`: `create_root`, `create_child`, `merge_accepted`, `merge_inspected_delivery`, `validate_parent_base`, `remove_accepted_clean`, `reclaim_directory`, `reattach_worktree`, `remove_created`, `remove_clean`, `contains_commit`, `is_ancestor`, plus the now-unused helpers `same_path`, `ensure_worktree_target_available`, `add_worktree`, `ensure_worktree_parent_is_ignored`, `resolve_parent_base`, and the `ChildWorktree` struct (there is no `RootWorktree`; `create_root` returns a `ChildWorktree`). Keep `ignore_inside_repository`, `ignore_project_path`, `containing_worktree_root`, `repository_relative_ignore_entry`, `deepest_existing_directory`, `canonicalize_deepest_existing`, `append_exclude_entry`, `current_branch`, `git`.

- [x] **Step 5: Delete the tests for the deleted behavior**

```bash
cd yi-agent-rs && rg -n 'reclaim|recycle|cleanup_accepted|contains_commit|is_merged_into|merge_inspected_delivery|remove_accepted_clean|reattach_worktree|reclaim_directory|create_child|create_root' crates/yi-agent-store/tests crates/yi-agent-tools/tests
```

Delete every hit. In `runtime_coordinator.rs` this includes `accepted_review_recycles_the_child_workspace` (1203), `a_reclaimed_worktree_is_rebuilt_before_a_worker_starts` (4144), `reclaim_session_worktrees_removes_merged_children_and_keeps_unmerged_ones` (4190), `reclaim_session_worktrees_keeps_a_running_childs_directory` (4524). In `runtime_ipc.rs`: `detaching_an_application_root_seeds_a_worktree_reclaim` (1433).

- [x] **Step 6: Verify**

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree && cargo test -p yi-agent-store --test runtime_coordinator
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "refactor: drop automatic delivery acceptance and worktree reclaim"
```

---

## Task 7: Delete `yi-agent daemon gc`

**Files:**
- Modify: `crates/yi-agent-store/src/ipc.rs`
- Modify: `crates/yi-agent/src/config.rs`, `main.rs`
- Test: `crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: T6 (the reclaim the command drove).
- Produces: `IpcRequest::PreviewGc` / `ConfirmGc` and `IpcResponse::GcPreview` / `GcCompleted` / `IpcGcEntry` no longer exist.

- [x] **Step 1: Delete the IPC surface**

Remove from `ipc.rs`: the `PreviewGc` and `ConfirmGc` request variants (lines 197–202), the `GcPreview`/`GcCompleted`/`IpcGcEntry` response types (392–397, 594–), `preview_gc`, `confirm_gc`, `gc_entries`, the two dispatch arms (1152, 1156, 2729), and the `gc` half of `ConfirmationStore` (`issue_gc`, `consume_gc`, and its token map).

- [x] **Step 2: Delete the CLI surface**

`config.rs` — remove the variant:

```rust
    /// List reclaimable worktrees and, with confirmation, remove them.
    Gc,
```

`main.rs` — remove `DaemonAction::Gc => gc_daemon_client(&runtime),` from the dispatch, the `gc_daemon_client` function (line 648), the `DaemonAction::Gc |` arm at line 618, and the parse test at line 2279.

- [x] **Step 3: Delete the tests**

```bash
cd yi-agent-rs && rg -n 'PreviewGc|ConfirmGc|GcPreview|GcCompleted|gc_' crates/yi-agent-store/tests crates/yi-agent/src
```

Delete every hit, including `gc_preview_lists_a_detached_sessions_reclaimable_worktree` (`runtime_ipc.rs:2263`) and the three `runtime_ipc.rs:2165-2244` gc cases.

- [x] **Step 4: Verify**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent && cargo test -p yi-agent-store --test runtime_ipc
```

Expected: PASS. `yi-agent daemon gc` now fails as an unknown subcommand, which is the intended surface.

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "refactor: remove the daemon gc command"
```

---

## Task 8: Retire the persisted workspace

**Files:**
- Modify: `crates/yi-agent-store/src/repository.rs`
- Modify: `crates/yi-agent-store/src/ipc.rs`
- Modify: `crates/yi-agent-core/src/subagent/task.rs`, `worker.rs`
- Modify: `crates/yi-agent-store/src/runtime.rs`
- Test: `crates/yi-agent-store/tests/repository_decisions.rs`, `runtime_ipc.rs`

**Interfaces:**
- Consumes: T6 (nothing reads the row), T5 (identity comes from the workdir).
- Produces:
  - `AgentTask` has no `workspace` field; `WorkspaceLeaseId` is deleted.
  - `WorkerStart.workspace_lease_id` and `AgentTask::validate_for`'s workspace clause are deleted.
  - `RuntimeRepository` has no `task_workspaces` access.

- [x] **Step 1: Write the failing schema test**

```rust
#[test]
fn a_fresh_database_has_no_task_workspaces_table() {
    let repository = RuntimeRepository::open_in_memory().unwrap();
    let exists: bool = repository
        .connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'task_workspaces'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);

    assert!(!exists, "fresh schema must not create task_workspaces");
}
```

`RuntimeRepository::open_in_memory` and a test-visible `connection` accessor already exist for `repository_decisions.rs`; reuse them, or add `#[cfg(test)] pub(crate) fn table_exists(&self, name: &str) -> bool`.

- [x] **Step 2: Run it to confirm it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test repository_decisions a_fresh_database_has_no_task_workspaces_table
```

Expected: FAIL — the table exists.

- [x] **Step 3: Drop the DDL and the accessors**

Delete the `CREATE TABLE task_workspaces (...)` statement from the `current_version < 7` migration block (line 4659). Keep the `application_root_attachments` statement in that block. Do not add a migration that drops the table (§3.6).

Delete `record_task_workspace`, `task_workspace`, `task_workspace_optional`, `delete_task_workspace`, `reclaim_candidates`, `task_workspace_mode` if unused, and the `TaskWorkspaceNotFound`/`InvalidTaskWorkspace` variants. Remove the `LEFT JOIN task_workspaces` clauses from `task_snapshots` (3485), `reclaim_candidates`, and the subscription queries (3995), and the `task_workspaces` columns from their row mappers.

Delete the `TaskWorkspaceRecycled` and `TaskWorkspaceRecycleFailed` variants and their `as_str`/`parse` arms.

- [x] **Step 4: Drop the identity fields**

`task.rs` — delete `AgentTask.workspace`, `with_workspace`, and the clause:

```rust
        if task.workspace.as_ref() != Some(&self.workspace) {
            return Err("delivery workspace does not match task workspace");
        }
```

`worker.rs` — delete `WorkerStart.workspace_lease_id` and `WorkspaceLeaseId` itself (after T5 the lease is still on `WorkerWorkspace`; keep the field but make it runtime-assigned, not persisted — the type stays, the persisted copies go).

`runtime.rs` — `nearest_ancestor_workspace` (line 1616) currently walks ancestors reading `repository.task_workspace_optional`. Its only caller is the read-only branch that Task 4 already rewrote (`prepare_task_workspace`, line 745), so retarget it at the ancestor's declared workdir instead of a row:

```rust
    fn nearest_ancestor_workspace(
        &self,
        supervisor: &AgentSupervisor,
        task: &TaskId,
    ) -> Result<Option<WorkerWorkspace>, RuntimeCoordinatorError> {
        let mut current = supervisor.task(task).and_then(|task| task.parent_id.clone());
        while let Some(ancestor) = current {
            let workdir = supervisor.spawn_workdir(&ancestor).or_else(|| {
                supervisor.workspace(&ancestor).map(|lease| lease.path.clone())
            });
            if let Some(workdir) = workdir {
                return Ok(Some(in_place_workspace_at(workdir.clone(), workdir)));
            }
            current = supervisor.task(&ancestor).and_then(|task| task.parent_id.clone());
        }
        Ok(None)
    }
```

`in_place_workspace_at` is the helper Task 4 added; move it into `yi-agent-store` and export it, or duplicate its three lines. Also delete `delivery_diff`'s workspace lookup (line 2779), replacing the diff directory with the task's `spawn_workdir`:

```rust
            let Some(workdir) = supervisor_for(task).spawn_workdir(task) else {
                return Ok(None);
            };
            (workdir, delivery)
```

`inspect_task` and the subscription snapshot in `ipc.rs` lose the `workspace` field they read from `IpcTask.workspace` / `IpcTaskDetail.workspace`; delete those fields and their `#[serde(default)]` attributes.

- [x] **Step 5: Delete the tests**

```bash
cd yi-agent-rs && rg -n 'task_workspace|TaskWorkspace|workspace_lease|WorkspaceAssignedFactory|delivery workspace does not match' crates/yi-agent-store/tests crates/yi-agent-core/src crates/yi-agent-store/src
```

Delete each hit. In `repository_decisions.rs` that is the whole `task_workspace_*` group (lines 39–172). In `runtime_coordinator.rs`: `worker_receives_its_persisted_workspace_before_provider_start` (724), `child_recovery_context_uses_the_persisted_workspace_assignment` (768), `workspace_record_failure_*` (895, 937), `child_workspace_lease_identity_reaches_worker_and_durable_task` (1020), `read_only_child_runs_in_place_without_a_workspace_row` (3966). In `runtime_ipc.rs`: `inspect_task_includes_the_authoritative_recorded_workspace` (4855) and the two subscription cases (4929, 4959).

- [x] **Step 6: Verify**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test repository_decisions && cargo test -p yi-agent-store --test runtime_coordinator && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent --bin yi-agent
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "refactor: stop persisting task workspaces"
```

---

## Task 9: Reword the prompt and the status log

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs`
- Modify: `crates/yi-agent/src/main.rs`
- Modify: `docs/bug-list.md`
- Test: `crates/yi-agent/src/main.rs` (prompt test)

**Interfaces:**
- Consumes: everything above.
- Produces: `default_system_prompt` tells the parent to prepare and pass a workdir.

- [x] **Step 1: Change the failing prompt test**

`crates/yi-agent/src/main.rs:1735`:

```rust
    #[test]
    fn default_system_prompt_requires_a_workdir_for_isolation() {
        let prompt = yi_agent_core::AgentConfig::default_system_prompt();
        assert!(
            prompt.contains("git worktree add"),
            "default prompt must tell a parent to prepare an isolated workdir"
        );
        assert!(
            prompt.contains("workdir"),
            "default prompt must name the spawn_agent workdir argument"
        );
    }
```

- [x] **Step 2: Run it to confirm it fails**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent default_system_prompt_requires_a_workdir_for_isolation
```

Expected: FAIL — the prompt still says `git merge --no-ff` and never says `workdir`.

- [x] **Step 3: Replace the prompt section**

`crates/yi-agent-core/src/agent.rs`:

```
Subagent integration:
- A child runs in the `workdir` you pass to `spawn_agent`. When a change needs
  isolation, create the worktree yourself first: `git worktree add <path> -b
  <branch>`, then pass `<path>` as the child's `workdir`. When it does not,
  pass the directory the child should work in directly.
- A read-only child writes nothing; a coding child may write anywhere inside its
  `workdir`. You are responsible for integrating its commit: run
  `git merge --no-ff <commit>` yourself and re-run the relevant verification.
- The runtime no longer creates, tracks, or merges worktrees for you."#
```

Keep the surrounding `File discovery:` and `Task execution:` blocks untouched.

- [x] **Step 4: Run the test to confirm it passes**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent default_system_prompt_requires_a_workdir_for_isolation
```

Expected: PASS.

- [x] **Step 5: Update the status log**

In `docs/bug-list.md`, replace the worktree entry (line 16) with:

```markdown
- [x] 当前启动subagent runtime 就会创建worktree。太多了怎么清理。（修复：daemon 不再自动建 worktree、不再跟踪/验收/回收子任务工作区。root 原地运行在项目目录；`spawn_agent` 新增必填 `workdir`，父 agent 需要隔离时自己 `git worktree add` 再传路径。删除 `AgentWorkspaceService` 的编排方法、`reclaim_session_worktrees`/`reclaim_idle_worktrees`、`yi-agent daemon gc`、`task_workspaces` 的读写（旧表保留不 DROP）、以及"已进父历史才算完成"的自动验收；交付上报保留，身份改由 workdir 派生。见 [设计](../superpowers/specs/2026-09-29-agent-owned-worktree-design.md)、[计划](../superpowers/plans/2026-09-29-agent-owned-worktree-impl.md)。验证：`cargo test -p yi-agent-store --test runtime_coordinator`、`cargo test -p yi-agent-store --test runtime_ipc`、`cargo test -p yi-agent --bin yi-agent`，以及干净 git 项目下 `run --subagents` 不产生 `.worktrees/yi-agent-*-root`）
```

- [x] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "docs: point the agent prompt and status log at agent-owned worktrees"
```

---

## Task 10: New coverage and the end-to-end assertion

**Files:**
- Test: `crates/yi-agent-store/tests/runtime_coordinator.rs`
- Test: `crates/yi-agent/tests/subagent_real_e2e.rs`
- Test: `crates/yi-agent/src/subagent_runtime.rs`

**Interfaces:**
- Consumes: every task above.
- Produces: the four behaviors §6 requires.

- [x] **Step 1: Write the four unit/integration tests**

Reuse the `TempDir` + `RuntimeCoordinator::open(&database, factory)` shape from Task 4 Step 2. Both tests below pass `StaticWorkspaceService` (whose registry holds the project root as the in-place position) and read the recorded `starts` vector for the path the worker actually received.

`runtime_coordinator.rs`:

```rust
#[tokio::test]
async fn a_root_runs_in_the_project_directory_without_creating_a_worktree() {
    let directory = TempDir::new().unwrap();
    let project_root = directory.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let database = directory.path().join("runtime.sqlite");
    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: project_root.clone(),
        path: project_root.clone(),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    };
    let starts = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        database: database.clone(),
        starts: Arc::clone(&starts),
        handles: Arc::new(Mutex::new(Vec::new())),
        workspace_service: Arc::new(StaticWorkspaceService {
            workspace: workspace.clone(),
        }),
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    coordinator.start_worker(&session, &root).await.unwrap();

    let starts = starts.lock().unwrap();
    assert_eq!(
        starts[0].workspace.as_ref().map(|workspace| &workspace.path),
        Some(&project_root)
    );
    assert!(!project_root.join(".worktrees").exists());
}

#[tokio::test]
async fn mode_only_changes_write_access_not_the_directory() {
    let directory = TempDir::new().unwrap();
    let project_root = directory.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let database = directory.path().join("runtime.sqlite");
    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: project_root.clone(),
        path: project_root.clone(),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    };
    let starts = Arc::new(Mutex::new(Vec::new()));
    let factory = Arc::new(WorkspaceObservingFactory {
        database: database.clone(),
        starts: Arc::clone(&starts),
        handles: Arc::new(Mutex::new(Vec::new())),
        workspace_service: Arc::new(StaticWorkspaceService {
            workspace: workspace.clone(),
        }),
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    let child = coordinator
        .spawn_child_and_admit(
            &session,
            &root,
            "audit".into(),
            ChildWriteMode::ReadOnly,
            None,
            Some(project_root.clone()),
        )
        .await
        .unwrap();

    coordinator.start_worker(&session, &child).await.unwrap();

    let starts = starts.lock().unwrap();
    let started = starts.iter().find(|start| start.task_id == child).unwrap();
    assert_eq!(
        started.workspace.as_ref().map(|workspace| &workspace.path),
        Some(&project_root),
        "a read-only child still runs in its position, not a generated directory"
    );
    assert_eq!(started.workspace_mode, ChildWriteMode::ReadOnly);
}
```

`subagent_runtime.rs` (unit, over `WorkerWorkspaces`):

```rust
    #[test]
    fn a_workdir_outside_any_repository_is_still_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let workspaces = WorkerWorkspaces::new(directory.path().to_path_buf());

        let workspace = workspaces
            .read_only_workspace(None, &TaskId::new())
            .expect("a non-git workdir still gets an in-place position");

        assert_eq!(workspace.path, directory.path());
    }
```

`tempfile` is a dev-dependency of both `yi-agent` (line 49) and `yi-agent-store` (line 29), so no manifest change is needed.

- [x] **Step 2: Run them to confirm they fail**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator a_root_runs_in_the_project_directory && cargo test -p yi-agent --bin yi-agent a_workdir_outside_any_repository
```

Expected: FAIL — `WorkerStart.workspace_mode` is not yet public to the test, and the root still receives a generated `.worktrees/yi-agent-*-root` path.

- [x] **Step 3: Make `WorkerStart.workspace_mode` readable by the fixture**

If `starts` cannot read `workspace_mode`, widen it to `pub` (it already is, line 40 of `worker.rs`) and confirm `WorkspaceObservingFactory` copies it into the recorded start. No production behavior changes.

- [x] **Step 4: Run them to confirm they pass**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator a_root_runs_in_the_project_directory && cargo test -p yi-agent-store --test runtime_coordinator mode_only_changes_write_access_not_the_directory && cargo test -p yi-agent --bin yi-agent a_workdir_outside_any_repository
```

Expected: PASS.

- [x] **Step 5: Add the end-to-end assertion**

In `crates/yi-agent/tests/subagent_real_e2e.rs`, the existing harness runs a clean git project. It is opt-in (`#[ignore]` without API keys). Add the assertion §7 requires:

```rust
    let root_worktrees = project_root.join(".worktrees");
    assert!(
        !root_worktrees.exists()
            || std::fs::read_dir(&root_worktrees)
                .unwrap()
                .all(|entry| !entry.unwrap().file_name().to_string_lossy().starts_with("yi-agent-")),
        "a subagent run must not leave a daemon-created root worktree"
    );
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&project_root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "the project checkout must be clean after the run"
    );
```

- [x] **Step 6: Run the full verification list from §7**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent --bin yi-agent
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A
git commit -m "test: cover in-place roots, explicit workdirs, and the no-worktree exit"
```

---

## Final verification

- [x] `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator`
- [x] `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc`
- [x] `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
- [x] `cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree` (should now hold only `ignore_*` cases)
- [x] `cd yi-agent-rs && cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cd yi-agent-rs && cargo fmt --all -- --check`
- [x] Clean git project, `run --subagents`: no `.worktrees/yi-agent-*-root`, clean checkout on exit
- [x] `git diff --stat main` touches no `task_workspaces` `DROP`, and no branch ref deletion

## Open decision recorded

`read_only_workspace(None, ..)` for the root returns the git top level (`WorkerWorkspaces.repository_root`). The review decision (2026-09-30) was "父agent直接在workdir": the root's in-place position is the **project workdir the application attached**, which for an attached root is the same directory as the git top level only when the workdir is the repository root. If a future workdir is a subdirectory of its repository, the position must be `requested_workspace`, not `rev-parse --show-toplevel`. Task 4 pins the position from `WorkerWorkspaces::new(workspace)`, whose argument is already the attached `requested_workspace`, so the subdirectory case is carried by the constructor argument and needs no further change.
