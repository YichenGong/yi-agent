# Subagent Read-Only Default Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make subagents default to read-only in-place execution (no git worktree), and create a worktree only when the parent explicitly spawns `mode: "coding"`; degrade non-git sessions to read-only.

**Architecture:** Add a per-task `TaskWorkspaceMode` (`Coding` | `ReadOnly`). It is stored on the in-memory supervisor (mirroring the existing `objectives` map), persisted in a new `tasks.workspace_mode` column, and copied onto `WorkerStart.workspace_mode` so the application factory can pick the sandbox and the delivery path. Read-only tasks still receive a `WorkerWorkspace` (so the "every worker has a workspace" invariant and `AttachedApplicationRoot` are untouched) whose `path` is the nearest coding ancestor's worktree (else the application root) and whose `branch`/`base_commit` are empty; no `task_workspaces` row is written.

**Tech Stack:** Rust (edition 2024), `rusqlite` (SQLite migrations), `serde`/`serde_json`, `tokio`, `async-trait`, crate-local integration tests.

---

## Shared context (read before starting)

**Crates and files:**
- `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs` — domain types (`AgentTask`, `TaskDepth`, ...).
- `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` — `WorkerStart`, `WorkerWorkspace`, `AgentWorkspaceService`, `AgentWorkerFactory` traits.
- `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` — `AgentSupervisor`, `spawn_with_objective`, `start_worker_with_provider_turn_gate`, `SpawnAgentTool`.
- `yi-agent-rs/crates/yi-agent-store/src/repository.rs` — SQLite schema/migrations and task persistence.
- `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` — `RuntimeCoordinator`, `prepare_task_workspace`, `start_worker`, `attach_application_root`, spawn methods, `WorkspaceAssignedFactory`.
- `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` — `IpcRequest` spawn variants + handlers.
- `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` — `DaemonAgentWorkerFactory`, `DaemonWorkspaceService`, `DaemonSpawnAgentTool`, `DaemonApplicationSpawnAgentTool`.

**Approved design:** `docs/superpowers/specs/2026-09-26-subagent-readonly-default-design.md`.

**Current anchors (verified; line numbers drift as you edit — re-grep by symbol name):**
- `WorkerStart` struct `worker.rs:27-44`; builders `worker.rs:87-127`; `WorkerWorkspace` `worker.rs:17-25`; `AgentWorkspaceService` trait `worker.rs:406-454`; `AgentWorkerFactory` trait `worker.rs:486-557`.
- `AgentSupervisor` struct `supervisor.rs:77-89`; `objectives` map init in `new_with_objective:105`, `from_recovered_root:130`, `from_hydrated_review_root:191`; `insert_hydrated_review_child:215`; `insert_recovered_child:252`; `start_worker_with_provider_turn_gate:475`; `spawn_with_objective:916`; `spawn_child:913`; `SpawnAgentTool` `supervisor.rs:1650-1692`.
- `LATEST_SCHEMA_VERSION = 8` `repository.rs:22`; `migrate` `repository.rs:4288`; version-8 idempotent ALTER precedent `repository.rs:4508-4525`; `tasks` DDL `repository.rs:4308-4314`; `create_child_task_with_attempt_and_objective` `repository.rs:971`; `create_task_with_attempt_and_objective` `repository.rs:898`; `PersistedRecoveredTask` `repository.rs:352`; `recovered_tasks` `repository.rs:3371`; `task_workspace_optional` `repository.rs:3201`; `task_detail` `repository.rs:3626`.
- `AttachedApplicationRoot` `runtime.rs:97-102`; `attach_application_root` `runtime.rs:634`; `spawn_application_child` `runtime.rs:928`; `spawn_child_with_objective` `runtime.rs:1049`; `spawn_child_and_admit` `runtime.rs:1105`; `start_worker` `runtime.rs:1123`; `WorkspaceAssignedFactory` `runtime.rs:154-199`; `workspace_service_for` `runtime.rs:1451`; `prepare_task_workspace` `runtime.rs:1463`.
- `IpcRequest` `ipc.rs:134-137`; `SpawnChild` `ipc.rs:164-168`; `SpawnApplicationChild` `ipc.rs:169-174`; handler arms `ipc.rs:2147-2186`.
- `DaemonAgentWorkerFactory` `subagent_runtime.rs:32-41`; `worker_tool_registry` `subagent_runtime.rs:90-99`; `DaemonWorkspaceService::new` `subagent_runtime.rs:169-180`; `start_with_provider_turn_gate` `subagent_runtime.rs:409`; delivery branch `subagent_runtime.rs:549-562`; `DaemonApplicationSpawnAgentTool` schema/parse `subagent_runtime.rs:945-960`; `DaemonSpawnAgentTool` schema/parse `subagent_runtime.rs:1040-1055`.

**Test/commit discipline (from `CLAUDE.md`):**
- Before any `cargo test`, run `ps aux | grep cargo` and confirm no other cargo process. Never `cargo test --workspace` (OOM/exit 137). Run per crate.
- Before every commit: `cd yi-agent-rs && cargo fmt --all` (no auto-format hook exists).
- Commit messages: conventional commits, first line ≤72 chars, NO `Co-Authored-By` trailer.
- Work happens on branch `feat/subagent-readonly-default` in `.worktrees/feat/subagent-readonly-default`. Never edit `main`.

**Deliberate refinements to the approved design (rationale inline):**
1. The DDL column default is `'coding'` (not `'read_only'`). The `ALTER TABLE` backfills existing rows; existing sessions own worktrees and must keep coding behavior. The read-only default is enforced at the spawn boundary (spawn tools default to `read_only` and every child insert specifies a mode explicitly).
2. Non-git detection uses a new `AgentWorkspaceService::supports_coding(&self) -> bool` (default `true`) instead of a factory probe, because the coordinator already holds the service at both the attach site and the provisioning site. This keeps the probe next to the path knowledge it needs.
3. Recovery attestation is intentionally left mode-agnostic: `recovery_context_for` and `preflight_recovery` both compute tool names with the session sandbox, so they stay mutually consistent. Read-only tasks therefore recover normally.

**Sandbox facts to rely on (verify in Task 7):** `yi_agent_tools::SandboxMode` has `ReadOnly`/`WorkspaceWrite`/`DangerFullAccess` (`yi-agent-tools/src/sandbox.rs:11-18`); `allows_writes()` is `mode != ReadOnly` (`sandbox.rs:51-53`); `register_builtin_tools_with_sandbox` skips registering `write`/`edit` when `!allows_writes()` (`yi-agent-tools/src/lib.rs:67`).

---

## Task 1: `TaskWorkspaceMode` type and `WorkerStart.workspace_mode`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs` (add enum)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs:25-27` (re-export)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:17-127` (field + builder)
- Test: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` (`#[cfg(test)] mod tests`, existing at `worker.rs:129`)

**Step 1: Write the failing test**

Append to the existing `mod tests` in `worker.rs`:

```rust
#[test]
fn worker_start_defaults_to_read_only_and_accepts_an_override() {
    let default = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new());
    assert_eq!(default.workspace_mode, TaskWorkspaceMode::ReadOnly);

    let coding = WorkerStart::new(TaskId::new(), AttemptId::new(), RootSessionId::new())
        .with_workspace_mode(TaskWorkspaceMode::Coding);
    assert_eq!(coding.workspace_mode, TaskWorkspaceMode::Coding);
}
```

Add the parse/round-trip test to `task.rs`'s test module (create `#[cfg(test)] mod workspace_mode_tests` at the end of `task.rs` if none exists):

```rust
#[cfg(test)]
mod workspace_mode_tests {
    use super::TaskWorkspaceMode;

    #[test]
    fn workspace_mode_round_trips_through_its_storage_string() {
        for mode in [TaskWorkspaceMode::Coding, TaskWorkspaceMode::ReadOnly] {
            assert_eq!(TaskWorkspaceMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(TaskWorkspaceMode::parse("writable"), None);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib workspace_mode`
Expected: FAIL to compile — `TaskWorkspaceMode` not found.

**Step 3: Implement**

In `task.rs` (near the other task enums, before `AgentTask`), add:

```rust
/// Whether a task owns a writable git worktree or runs in place read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskWorkspaceMode {
    /// Create a git worktree; the worker may write files and deliver a commit.
    Coding,
    /// No worktree; run in the parent's view with a read-only sandbox and
    /// return a text result.
    ReadOnly,
}

impl TaskWorkspaceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Coding => "coding",
            Self::ReadOnly => "read_only",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "coding" => Some(Self::Coding),
            "read_only" => Some(Self::ReadOnly),
            _ => None,
        }
    }
}

impl Default for TaskWorkspaceMode {
    fn default() -> Self {
        Self::ReadOnly
    }
}
```

In `lib.rs`, extend the re-export:

```rust
pub use subagent::task::{
    AgentTask, AttemptId, RootSessionId, TaskDepth, TaskEvent, TaskId, TaskState,
    TaskWorkspaceMode,
};
```

In `worker.rs`, import the type (extend the existing `use super::task::{...}` line):

```rust
use super::task::{
    AttemptId, DeliveryReport, MessageId, RootSessionId, TaskId, TaskWorkspaceMode,
    WorkspaceLeaseId,
};
```

Add the field to `WorkerStart` (after `workspace`):

```rust
    /// Whether this worker owns a writable worktree or runs read-only in place.
    pub workspace_mode: TaskWorkspaceMode,
```

Set it in `WorkerStart::new` (in the struct literal, after `workspace: None,`):

```rust
            workspace_mode: TaskWorkspaceMode::default(),
```

Add the builder (after `with_workspace`):

```rust
    pub fn with_workspace_mode(mut self, workspace_mode: TaskWorkspaceMode) -> Self {
        self.workspace_mode = workspace_mode;
        self
    }
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib workspace_mode`
Expected: PASS (2 tests).

Then confirm the crate still builds: `cd yi-agent-rs && cargo build -p yi-agent-core`
Expected: success.

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs \
        yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs \
        yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): add TaskWorkspaceMode and WorkerStart.workspace_mode"
```

---

## Task 2: Supervisor per-task mode map and spawn wiring

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` (struct `77-89`, constructors `105/130/191`, `insert_hydrated_review_child:215`, `insert_recovered_child:252`, `start_worker_with_provider_turn_gate:475`, `spawn_with_objective:916`, `spawn_child:913`, `SpawnAgentTool:1650-1692`)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:1068` (caller)
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`:

```rust
#[test]
fn children_default_to_read_only_and_can_be_spawned_as_coding() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();

    let read_only = supervisor
        .spawn_with_objective(root.clone(), "audit".into(), TaskWorkspaceMode::ReadOnly)
        .unwrap();
    let coding = supervisor
        .spawn_with_objective(root.clone(), "implement".into(), TaskWorkspaceMode::Coding)
        .unwrap();

    assert_eq!(
        supervisor.workspace_mode(&read_only),
        TaskWorkspaceMode::ReadOnly
    );
    assert_eq!(supervisor.workspace_mode(&coding), TaskWorkspaceMode::Coding);
    // Root with no explicit entry defaults to coding.
    assert_eq!(supervisor.workspace_mode(&root), TaskWorkspaceMode::Coding);
}
```

Also update the existing call at `subagent_supervisor.rs:75` (`.spawn_with_objective(root, "Audit the scheduler fairness tests".into())`) to pass `TaskWorkspaceMode::ReadOnly`.

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor children_default_to_read_only`
Expected: FAIL to compile — `spawn_with_objective` takes 2 args, `workspace_mode` missing.

**Step 3: Implement**

Add the import at the top of `supervisor.rs` (it already imports task types — extend it):

```rust
use super::task::TaskWorkspaceMode;
```

Add the field to `AgentSupervisor` (after `objectives: HashMap<TaskId, String>,`):

```rust
    workspace_modes: HashMap<TaskId, TaskWorkspaceMode>,
```

Initialize `workspace_modes: HashMap::new(),` in each struct literal that currently sets `objectives` — `new_with_objective` (~`supervisor.rs:118`), `from_recovered_root` (~`153`), `from_hydrated_review_root` (~`203`). `from_recovered_gated_root` delegates to `from_recovered_root`, so it needs no change.

Add the accessors (place near `objective(&self, task_id)` at `supervisor.rs:308`):

```rust
    pub fn set_workspace_mode(&mut self, task_id: &TaskId, mode: TaskWorkspaceMode) {
        self.workspace_modes.insert(task_id.clone(), mode);
    }

    /// The task's workspace mode. A root with no explicit entry is coding (it
    /// owns session isolation); an unregistered non-root task is read-only.
    pub fn workspace_mode(&self, task_id: &TaskId) -> TaskWorkspaceMode {
        if let Some(mode) = self.workspace_modes.get(task_id) {
            return *mode;
        }
        match self.tasks.get(task_id).map(|task| task.depth) {
            Some(super::task::TaskDepth::Root) => TaskWorkspaceMode::Coding,
            _ => TaskWorkspaceMode::ReadOnly,
        }
    }
```

Update `spawn_with_objective` signature (`supervisor.rs:916`) and its body to insert the mode. Current signature: `pub fn spawn_with_objective(&mut self, parent_id: TaskId, objective: String) -> Result<TaskId, String>`. New:

```rust
    pub fn spawn_with_objective(
        &mut self,
        parent_id: TaskId,
        objective: String,
        workspace_mode: TaskWorkspaceMode,
    ) -> Result<TaskId, String> {
```

Inside, where it does `self.objectives.insert(child_id.clone(), objective);` (~`946`), also add:

```rust
        self.workspace_modes.insert(child_id.clone(), workspace_mode);
```

Update `spawn_child` (`supervisor.rs:913`) to pass the default:

```rust
        self.spawn_with_objective(
            parent_id,
            "Complete the delegated task.".into(),
            TaskWorkspaceMode::ReadOnly,
        )
```

Update `insert_recovered_child` (`supervisor.rs:252`) to take `workspace_mode: TaskWorkspaceMode` as a final parameter and insert it next to `self.objectives.insert(...)` (~`295`):

```rust
        self.workspace_modes.insert(task_id.clone(), workspace_mode);
```

Update `insert_hydrated_review_child` (`supervisor.rs:215`) to take `workspace_mode: TaskWorkspaceMode` as a final parameter and insert it next to `self.objectives.insert(task_id.clone(), objective);` (~`230`):

```rust
        self.workspace_modes.insert(task_id.clone(), workspace_mode);
```

Update `start_worker_with_provider_turn_gate` (`supervisor.rs:496-511`): add `.with_workspace_mode(self.workspace_mode(task_id))` to the `WorkerStart` builder chain (e.g. right after `.with_objective(objective)`):

```rust
        let start = WorkerStart::new(
            task.id.clone(),
            task.active_attempt_id().clone(),
            task.root_session_id.clone(),
        )
        .with_objective(objective)
        .with_workspace_mode(self.workspace_mode(task_id))
        .with_message_capability(Uuid::new_v4().to_string())
        .with_initial_user_messages(
            /* unchanged */
        );
```

Update the core `SpawnAgentTool` (`supervisor.rs:1664-1690`): extend the schema and parse `mode`, defaulting to `read_only`.

```rust
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "Delegated objective." },
                "mode": {
                    "type": "string",
                    "enum": ["coding", "read_only"],
                    "description": "Use 'coding' only when the child must change files. Defaults to 'read_only'."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let Some(task) = args.get("task").and_then(Value::as_str) else {
            return ToolResult::error("task is required");
        };
        if task.trim().is_empty() {
            return ToolResult::error("task must not be empty");
        }
        let mode = match args.get("mode").and_then(Value::as_str) {
            None => TaskWorkspaceMode::ReadOnly,
            Some(value) => match TaskWorkspaceMode::parse(value) {
                Some(mode) => mode,
                None => return ToolResult::error("mode must be 'coding' or 'read_only'"),
            },
        };
        let mut supervisor = self
            .tools
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match supervisor.spawn_with_objective(self.tools.caller.clone(), task.to_string(), mode) {
            Ok(task_id) => ToolResult::text(
                json!({ "task_id": task_id.to_string(), "status": "queued" }).to_string(),
            ),
            Err(error) => ToolResult::error(error.to_string()),
        }
    }
```

Finally, fix the store caller at `runtime.rs:1068`:

```rust
            let child = supervisor.spawn_with_objective(
                parent.clone(),
                objective.clone(),
                workspace_mode,
            )?;
```

where `workspace_mode` is a new parameter of `spawn_child_with_objective` threaded in Task 6. For now (to keep this task compiling on its own), temporarily pass `yi_agent_core::TaskWorkspaceMode::ReadOnly` and leave a `// Task 6 threads the requested mode through here.` comment. Task 6 replaces it.

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor`
Expected: PASS, including the new test.

Run: `cd yi-agent-rs && cargo build -p yi-agent-store`
Expected: success (confirms the `runtime.rs:1068` caller compiles).

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs \
        yi-agent-rs/crates/yi-agent-store/src/runtime.rs \
        yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "feat(core): track per-task workspace mode in the supervisor"
```

---

## Task 3: Persist `workspace_mode` (schema v9)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs` (`LATEST_SCHEMA_VERSION:22`, `migrate:4288`, `create_child_task_with_attempt_and_objective:971`, `create_task_with_attempt_and_objective:898`, `PersistedRecoveredTask:352`, `recovered_tasks:3371`)
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` (assertions at `76`, `339`, `368`, `1587`)
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Step 1: Write the failing test**

Add to `runtime_ipc.rs`:

```rust
#[test]
fn workspace_mode_is_persisted_and_recovered() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 9);

    let session = RootSessionId::new();
    let root = TaskId::new();
    let root_attempt = AttemptId::new();
    repository
        .create_task_with_attempt_and_objective(
            &root,
            &session,
            &root_attempt,
            1,
            "queued",
            "root",
            TaskWorkspaceMode::Coding,
        )
        .unwrap();
    assert_eq!(
        repository.task_workspace_mode(&root).unwrap(),
        TaskWorkspaceMode::Coding
    );

    let child = TaskId::new();
    let child_attempt = AttemptId::new();
    repository
        .create_child_task_with_attempt_and_objective(
            &child,
            &session,
            &root,
            1,
            &child_attempt,
            1,
            "queued",
            "child",
            TaskWorkspaceMode::ReadOnly,
        )
        .unwrap();
    assert_eq!(
        repository.task_workspace_mode(&child).unwrap(),
        TaskWorkspaceMode::ReadOnly
    );
}
```

Add `use yi_agent_core::TaskWorkspaceMode;` to the test file imports if not present.

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc workspace_mode_is_persisted`
Expected: FAIL to compile — `task_workspace_mode` missing, `create_*` arity mismatch, `schema_version` is 8.

**Step 3: Implement**

In `repository.rs`:
- Change `const LATEST_SCHEMA_VERSION: i64 = 8;` to `9`.
- Append a version-9 block to `migrate` (after the `current_version < 8` block, before `Ok(())`):

```rust
    if current_version < 9 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'workspace_mode'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // Existing rows own worktrees, so backfill them as coding. New
            // inserts always specify the mode explicitly.
            transaction.execute_batch(
                "ALTER TABLE tasks ADD COLUMN workspace_mode TEXT NOT NULL DEFAULT 'coding';",
            )?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (9)", [])?;
        transaction.commit()?;
    }
```

- Extend `create_task_with_attempt_and_objective` (`repository.rs:898`) with a `workspace_mode: TaskWorkspaceMode` parameter after `objective`, and add the column to its INSERT:

```rust
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json, workspace_mode)
             VALUES (?1, ?2, NULL, 0, ?3, 1, ?4, ?5, ?6)",
            params![
                task.to_string(),
                root.to_string(),
                state,
                attempt.to_string(),
                delivery_json,
                workspace_mode.as_str(),
            ],
        )?;
```

- Extend `create_child_task_with_attempt_and_objective` (`repository.rs:971`) the same way (parameter after `objective`):

```rust
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json, workspace_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8)",
            params![
                task.to_string(),
                root.to_string(),
                parent.to_string(),
                depth,
                state,
                attempt.to_string(),
                delivery_json,
                workspace_mode.as_str(),
            ],
        )?;
```

- Add the getter (near `task_workspace_optional`, `repository.rs:3201`):

```rust
    pub fn task_workspace_mode(
        &self,
        task: &TaskId,
    ) -> Result<TaskWorkspaceMode, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT workspace_mode FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match value {
            Some(value) => TaskWorkspaceMode::parse(&value).ok_or_else(|| {
                RepositoryError::UnknownEventKind {
                    kind: format!("invalid workspace_mode in store: {value}"),
                }
            }),
            None => Err(RepositoryError::TaskNotFound {
                task: task.to_string(),
            }),
        }
    }
```

(If `RepositoryError::UnknownEventKind` is not the idiomatic variant here, reuse whichever existing `RepositoryError` variant the surrounding code uses for malformed stored values — `recovered_tasks` uses `UnknownEventKind` for parse failures.)

- Add `pub workspace_mode: TaskWorkspaceMode,` to `PersistedRecoveredTask` (`repository.rs:352`).
- In `recovered_tasks` (`repository.rs:3371`), add `tasks.workspace_mode` to the SELECT (append as the last column), add `row.get::<_, String>(12)?` to the tuple, destructure it as `workspace_mode`, and map it:

```rust
                    workspace_mode: TaskWorkspaceMode::parse(&workspace_mode).ok_or_else(|| {
                        RepositoryError::UnknownEventKind {
                            kind: format!("invalid workspace_mode in store: {workspace_mode}"),
                        }
                    })?,
```

- Add `use yi_agent_core::TaskWorkspaceMode;` to the imports if absent.

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc workspace_mode_is_persisted`
Expected: PASS.

Update the four `schema_version() == 8` assertions in `runtime_ipc.rs` (lines `76`, `339`, `368`, `1587`) to `== 9`, and the legacy-fixture cleanup at `runtime_ipc.rs:83` (`DELETE FROM schema_migrations WHERE version IN (7, 8);`) to `IN (7, 8, 9)`.

Then run the full store integration file (this also compiles the changed call sites):
Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc`
Expected: PASS. (If it fails to compile because `create_session_with_objective` / `create_task_with_attempt_and_objective` callers need the new argument, fix them by passing `TaskWorkspaceMode::Coding`; Task 5 makes the root mode dynamic.)

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs \
        yi-agent-rs/crates/yi-agent-store/src/runtime.rs \
        yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat(store): persist task workspace mode (schema v9)"
```

---

## Task 4: `AgentWorkspaceService` read-only support and git probe

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:406-454` (trait)
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs:163-305` (`DaemonWorkspaceService`)
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (in-file `#[cfg(test)]`) or `yi-agent-rs/crates/yi-agent/tests/`

**Step 1: Write the failing test**

In `subagent_runtime.rs`, add a test that a non-git directory reports `supports_coding() == false` and that `prepare_read_only` yields an empty-branch workspace at the repository root:

```rust
#[test]
fn non_git_workspace_service_supports_only_read_only() {
    let directory = tempfile::TempDir::new().unwrap();
    let service = DaemonWorkspaceService::new(directory.path().to_path_buf());

    assert!(!service.supports_coding());

    let workspace = service
        .prepare_read_only(None, &TaskId::new())
        .expect("read-only workspace is always available");
    assert_eq!(workspace.path, directory.path());
    assert!(workspace.branch.is_empty());
    assert!(workspace.base_commit.is_empty());
}

#[test]
fn read_only_workspace_inherits_the_parent_path() {
    let directory = tempfile::TempDir::new().unwrap();
    let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
    let parent = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: directory.path().to_path_buf(),
        path: directory.path().join("parent-view"),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    };

    let workspace = service.prepare_read_only(Some(&parent), &TaskId::new()).unwrap();
    assert_eq!(workspace.path, parent.path);
}
```

(Ensure `tempfile` is a dev-dependency of `yi-agent` — it already is, used by existing tests.)

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent non_git_workspace_service_supports_only_read_only`
Expected: FAIL to compile — `supports_coding` / `prepare_read_only` not found.

**Step 3: Implement**

In `worker.rs`, add to `trait AgentWorkspaceService` (before `inspect_delivery`):

```rust
    /// Whether this service can create coding worktrees. A non-git service
    /// returns `false`, forcing the session into read-only mode.
    fn supports_coding(&self) -> bool {
        true
    }

    /// Supplies the in-place execution root for a read-only task. `parent` is
    /// the nearest ancestor workspace, when one exists. The default cannot
    /// invent a path and therefore fails.
    fn prepare_read_only(
        &self,
        _parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        Err(WorkerError::Startup(
            "workspace service does not support read-only tasks".into(),
        ))
    }
```

In `subagent_runtime.rs`, store the git probe result on `DaemonWorkspaceService`:

```rust
pub struct DaemonWorkspaceService {
    repository_root: PathBuf,
    worktree_root: PathBuf,
    is_git_repository: bool,
    service: yi_agent_tools::worktree::WorktreeService,
}

impl DaemonWorkspaceService {
    pub fn new(workspace: PathBuf) -> Self {
        let git_root = git_output(&workspace, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
        let is_git_repository = git_root.is_some();
        let repository_root = git_root.unwrap_or(workspace);
        let worktree_root = repository_root.join(".worktrees");
        Self {
            repository_root,
            worktree_root,
            is_git_repository,
            service: yi_agent_tools::worktree::WorktreeService::new(),
        }
    }
```

Add the trait impl methods to `impl AgentWorkspaceService for DaemonWorkspaceService` (near `prepare_root`):

```rust
    fn supports_coding(&self) -> bool {
        self.is_git_repository
    }

    fn prepare_read_only(
        &self,
        parent: Option<&WorkerWorkspace>,
        _task_id: &TaskId,
    ) -> Result<WorkerWorkspace, WorkerError> {
        let path = parent
            .map(|workspace| workspace.path.clone())
            .unwrap_or_else(|| self.repository_root.clone());
        Ok(WorkerWorkspace {
            lease_id: WorkspaceLeaseId::new(),
            repository_root: self.repository_root.clone(),
            path,
            branch: String::new(),
            parent_branch: String::new(),
            base_commit: String::new(),
        })
    }
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent non_git_workspace_service_supports_only_read_only read_only_workspace_inherits_the_parent_path`
Expected: PASS.

Also confirm core builds (trait default methods): `cd yi-agent-rs && cargo build -p yi-agent-core`
Expected: success.

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs \
        yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat(core): add read-only workspace provisioning to the workspace service"
```

---

## Task 5: Coordinator provisioning by mode

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` (`prepare_task_workspace:1463`, `start_worker:1123`, `attach_application_root:634`, `create_session_with_objective:600`, recovery hydration `374/389/401/409/468`, `insert_hydrated_review_child` callers)
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Step 1: Write the failing tests**

Add to `runtime_coordinator.rs` two tests (use the existing Git-fixture helpers in that file — mirror how `accepted_review_recycles_the_child_workspace` builds a session/root):

```rust
#[test]
fn read_only_child_runs_in_place_without_a_workspace_row() {
    // Build a git fixture + attached application root (copy the setup used by
    // accepted_review_recycles_the_child_workspace).
    // Spawn one read-only child and start its worker.
    // Assert:
    //   repository.task_workspace_optional(&child).unwrap().is_none()
    //   the started worker's WorkerStart.workspace.path == root workspace path
    //   (capture via the recording factory used in that test file).
}

#[test]
fn coding_child_fails_clearly_without_a_git_repository() {
    // Build a NON-git TempDir, attach it as an application root, then spawn a
    // coding child. Starting the worker must fail with evidence whose
    // "reason" == "coding_requires_git_repository".
}
```

Prefer reusing the file's existing recording factory helper (grep for `RecordingFactory` / the struct that captures `WorkerStart`) so you can assert on `workspace_mode` and `workspace.path`.

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator read_only_child_runs_in_place`
Expected: FAIL — read-only children currently create worktrees.

**Step 3: Implement**

**5a. `prepare_task_workspace`** (`runtime.rs:1463`) — add a `workspace_mode: TaskWorkspaceMode` parameter and branch:

```rust
    fn prepare_task_workspace(
        &self,
        supervisor: &mut AgentSupervisor,
        session: &RootSessionId,
        task: &TaskId,
        attempt: &AttemptId,
        workspace_mode: TaskWorkspaceMode,
    ) -> Result<Option<WorkerWorkspace>, RuntimeCoordinatorError> {
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
        let Some(service) = self.workspace_service_for(session) else {
            return Ok(None);
        };
        if workspace_mode == TaskWorkspaceMode::ReadOnly {
            let parent = self.nearest_ancestor_workspace(supervisor, task)?;
            let workspace = service
                .prepare_read_only(parent.as_ref(), task)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            supervisor
                .assign_workspace(task, workspace.lease_id.clone())
                .map_err(RuntimeCoordinatorError::Supervisor)?;
            return Ok(Some(workspace));
        }
        if !service.supports_coding() {
            return Err(RuntimeCoordinatorError::Supervisor(
                "coding_requires_git_repository".into(),
            ));
        }
        // ... existing prepare_child / prepare_root + record_task_workspace block unchanged ...
    }
```

Add the ancestor-walk helper:

```rust
    /// The nearest ancestor task's workspace, if any, walking `parent_id`
    /// upward. Used as the read-only execution root so a read-only child sees
    /// its parent's current view rather than a clean baseline.
    fn nearest_ancestor_workspace(
        &self,
        supervisor: &AgentSupervisor,
        task: &TaskId,
    ) -> Result<Option<WorkerWorkspace>, RuntimeCoordinatorError> {
        let mut current = supervisor
            .task(task)
            .and_then(|task| task.parent_id.clone());
        let repository = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned");
        while let Some(ancestor) = current {
            if let Some(workspace) = repository.task_workspace_optional(&ancestor)? {
                return Ok(Some(workspace));
            }
            current = supervisor
                .task(&ancestor)
                .and_then(|task| task.parent_id.clone());
        }
        Ok(None)
    }
```

**5b. `start_worker`** (`runtime.rs:1162`): read the mode from the supervisor and pass it, and map the coding-without-git failure to a clear reason:

```rust
        let workspace_mode = supervisor.workspace_mode(task);
        let workspace_assignment = match self.prepare_task_workspace(
            &mut supervisor,
            session,
            task,
            &attempt,
            workspace_mode,
        ) {
            Ok(workspace) => workspace,
            Err(error) => {
                let reason = if error.to_string().contains("coding_requires_git_repository") {
                    "coding_requires_git_repository"
                } else {
                    "workspace_provision_failed"
                };
                let evidence = serde_json::to_string(&serde_json::json!({
                    "reason": reason,
                    "error": error.to_string(),
                }))
                .expect("workspace failure evidence is serializable");
                /* unchanged transition + fail_task + return */
            }
        };
```

**5c. `attach_application_root`** (`runtime.rs:634`): after obtaining `service` (line ~649), decide the root mode and set it on the supervisor; create the session with that mode.

After `let service = ...?;` add:

```rust
        let root_mode = if service.supports_coding() {
            TaskWorkspaceMode::Coding
        } else {
            TaskWorkspaceMode::ReadOnly
        };
```

In the new-root path, change the session creation (`runtime.rs:703-704`) to pass the mode, then set it on the supervisor:

```rust
        let session_id = self.create_session_with_objective_and_mode(
            "TUI application root pending activation.".into(),
            root_mode,
        )?;
        // ...
        let mut supervisor = supervisor_handle.lock().await;
        let root_task_id = supervisor.root_task_id().clone();
        supervisor.set_workspace_mode(&root_task_id, root_mode);
        // ...
        let workspace = self
            .prepare_task_workspace(&mut supervisor, &session_id, &root_task_id, &attempt, root_mode)?
            .ok_or_else(|| { /* unchanged */ })?;
```

In the existing-attachment path, after `ensure_application_root_supervisor(&existing)?` (line ~688), also set the recovered root's mode. Because `ensure_application_root_supervisor` may build the supervisor internally, set the mode inside that function instead (see 5d).

**5d. Root mode on recovery** (`runtime.rs:399-417`): when a recovered root supervisor is constructed, set its root mode from the service. Inside `ensure_application_root_supervisor` (and the analogous recovery loop that calls `from_recovered_root` / `from_recovered_gated_root`), after constructing the supervisor:

```rust
                let mut supervisor = /* from_recovered_gated_root / from_recovered_root */;
                let is_git = self
                    .workspace_service_for(&task.session_id)
                    .is_some_and(|service| service.supports_coding());
                supervisor.set_workspace_mode(
                    supervisor.root_task_id().clone_ref(),
                    if is_git { TaskWorkspaceMode::Coding } else { TaskWorkspaceMode::ReadOnly },
                );
```

(Use `let root_id = supervisor.root_task_id().clone(); supervisor.set_workspace_mode(&root_id, mode);` to satisfy the borrow checker.)

**5e. Recovered children and hydrated review children**: pass `task.workspace_mode` into `insert_recovered_child` (`runtime.rs:389`) as the new final argument. For `insert_hydrated_review_child` (`runtime.rs:374`, `468`, `795`), read the mode from the repository in that loop:

```rust
                        let mode = repository.task_workspace_mode(&task.task_id)?;
                        supervisor
                            .insert_hydrated_review_child(hydrated, objective, mode)
```

**5f. `create_session_with_objective_and_mode`** (new, in `runtime.rs` near `create_session_with_objective:600`):

```rust
    pub fn create_session_with_objective_and_mode(
        &self,
        objective: String,
        workspace_mode: TaskWorkspaceMode,
    ) -> Result<RootSessionId, RuntimeCoordinatorError> {
        self.ensure_admitting()?;
        let session_id = RootSessionId::new();
        let supervisor = AgentSupervisor::new_with_objective(session_id.clone(), objective.clone());
        let root_id = supervisor.root_task_id().clone();
        let root_attempt = supervisor
            .task(&root_id)
            .expect("new root task exists")
            .active_attempt()
            .clone();
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .create_task_with_attempt_and_objective(
                &root_id,
                &session_id,
                &root_attempt.id,
                root_attempt.number,
                "queued",
                &objective,
                workspace_mode,
            )?;
        self.supervisors
            .lock()
            .expect("runtime supervisor mutex poisoned")
            .insert(session_id.clone(), Arc::new(AsyncMutex::new(supervisor)));
        Ok(session_id)
    }
```

Rewrite `create_session_with_objective` to delegate:

```rust
    pub fn create_session_with_objective(
        &self,
        objective: String,
    ) -> Result<RootSessionId, RuntimeCoordinatorError> {
        self.create_session_with_objective_and_mode(objective, TaskWorkspaceMode::Coding)
    }
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator`
Expected: PASS, including both new tests and the 9 existing recycling tests.

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs \
        yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "feat(store): provision workspaces by task mode and degrade non-git roots"
```

---

## Task 6: Thread `mode` through the spawn path (IPC + tools)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` (`SpawnChild:164`, `SpawnApplicationChild:169`, handlers `2147-2186`)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` (`spawn_application_child:928`, `spawn_child_with_objective:1049`, `spawn_child_and_admit:1105`; replace the Task 2 placeholder)
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (`DaemonApplicationSpawnAgentTool:935-978`, `DaemonSpawnAgentTool:1030-1072`)
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Step 1: Write the failing test**

Add to `runtime_ipc.rs`:

```rust
#[test]
fn daemon_spawn_agent_honors_the_coding_mode() {
    // Attach a git application root, spawn a child with mode "coding" through
    // the daemon socket, and assert the child's persisted
    // repository.task_workspace_mode(&child).unwrap() == TaskWorkspaceMode::Coding
    // and that a workspace row exists.
}

#[test]
fn daemon_spawn_agent_defaults_to_read_only() {
    // Same setup but omit "mode"; assert the child's mode is ReadOnly and
    // task_workspace_optional(&child).unwrap().is_none().
}
```

Follow the existing daemon-socket helper style in `runtime_ipc.rs` (the file already opens a real socket and sends `IpcRequest`s).

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc daemon_spawn_agent_defaults_to_read_only`
Expected: FAIL — the child is created coding because the mode is not threaded.

**Step 3: Implement**

**6a. IPC variants** (`ipc.rs:164-174`): add an optional mode field with a serde default so old frames stay valid:

```rust
    SpawnChild {
        session_id: String,
        parent_task_id: String,
        objective: String,
        #[serde(default)]
        mode: Option<String>,
    },
    SpawnApplicationChild {
        session_id: String,
        parent_task_id: String,
        capability: String,
        objective: String,
        #[serde(default)]
        mode: Option<String>,
    },
```

**6b. IPC handlers** (`ipc.rs:2147-2186`): parse the mode and pass it down:

```rust
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id,
            objective,
            mode,
        } => {
            let session_id = parse_id::<RootSessionId>(&session_id)?;
            let parent_task_id = parse_id::<TaskId>(&parent_task_id)?;
            let mode = parse_workspace_mode(mode)?;
            /* unchanged runtime build */
            let task_id = runtime.block_on(coordinator.spawn_child_and_admit(
                &session_id,
                &parent_task_id,
                objective,
                mode,
            ))?;
            Ok(IpcResponse::TaskSpawned { task_id: task_id.to_string() })
        }
```

Add the helper near the other parse helpers in `ipc.rs`:

```rust
fn parse_workspace_mode(
    mode: Option<String>,
) -> Result<yi_agent_core::TaskWorkspaceMode, IpcError> {
    match mode.as_deref() {
        None => Ok(yi_agent_core::TaskWorkspaceMode::ReadOnly),
        Some(value) => yi_agent_core::TaskWorkspaceMode::parse(value).ok_or_else(|| {
            IpcError::InvalidRequest(format!("mode must be 'coding' or 'read_only', got {value}"))
        }),
    }
}
```

(Use whichever `IpcError` variant the surrounding code uses for bad input; grep `IpcError::` in `ipc.rs`.)

**6c. Coordinator spawn methods** (`runtime.rs`): add `workspace_mode: TaskWorkspaceMode` to `spawn_application_child`, `spawn_child_and_admit`, and `spawn_child_with_objective`, pass it to `create_child_task_with_attempt_and_objective`, and pass it to `supervisor.spawn_with_objective` (replacing the Task 2 placeholder):

```rust
            let child =
                supervisor.spawn_with_objective(parent.clone(), objective.clone(), workspace_mode)?;
```

and

```rust
            .create_child_task_with_attempt_and_objective(
                &child,
                session,
                parent,
                depth,
                &attempt.id,
                attempt.number,
                "queued",
                &objective,
                workspace_mode,
            )?;
```

`spawn_child` (the 2-arg convenience) keeps its signature and calls `spawn_child_with_objective(..., TaskWorkspaceMode::ReadOnly)`.

**6d. Daemon spawn tools** (`subagent_runtime.rs`): extend both schemas and parse `mode` (mirror the core tool change from Task 2):

```rust
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "Delegated objective." },
                "mode": {
                    "type": "string",
                    "enum": ["coding", "read_only"],
                    "description": "Use 'coding' only when the child must change files. Defaults to 'read_only'."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }
```

In `call`, after validating `task`:

```rust
        let mode = match args.get("mode").and_then(Value::as_str) {
            None => "read_only",
            Some(value @ ("coding" | "read_only")) => value,
            Some(_) => return ToolResult::error("mode must be 'coding' or 'read_only'"),
        };
```

and include `mode: Some(mode.to_string())` in the `IpcRequest::SpawnChild` / `SpawnApplicationChild` struct literal.

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc`
Expected: PASS, including both new tests.

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: PASS (the daemon tool schemas changed; existing spawn tests still pass).

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs \
        yi-agent-rs/crates/yi-agent-store/src/runtime.rs \
        yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs \
        yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat(store): thread workspace mode through spawn_agent"
```

---

## Task 7: Factory per-mode sandbox and delivery

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (`worker_tool_registry:90-99`, `start_with_provider_turn_gate:409-564`)
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (in-file tests) or `yi-agent-rs/crates/yi-agent/tests/`

**Step 1: Write the failing test**

Add a test asserting the read-only registry omits `write`/`edit` and the coding registry includes them:

```rust
#[test]
fn read_only_workers_get_no_write_tools() {
    let directory = tempfile::TempDir::new().unwrap();
    let factory = DaemonAgentWorkerFactory::new(
        /* provider stub used by sibling tests */,
        Arc::new(ToolRegistry::default()),
        AgentConfig::default(),
        directory.path().join("runtime.sock"),
    )
    .with_sandbox(yi_agent_tools::SandboxMode::WorkspaceWrite, Vec::new());

    let workspace = WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: directory.path().to_path_buf(),
        path: directory.path().to_path_buf(),
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
    };

    let read_only = factory.worker_tool_registry(&workspace, TaskWorkspaceMode::ReadOnly);
    let names: Vec<_> = read_only.schemas().into_iter().map(|schema| schema.name).collect();
    assert!(!names.iter().any(|name| name == "write" || name == "edit"));
}
```

(Adapt the provider/config construction to whatever the sibling tests in this file use; the key assertions are on `worker_tool_registry`.)

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent read_only_workers_get_no_write_tools`
Expected: FAIL to compile — `worker_tool_registry` takes one argument.

**Step 3: Implement**

**7a. `worker_tool_registry`** (`subagent_runtime.rs:90`): take a mode and pick the sandbox:

```rust
    fn worker_tool_registry(
        &self,
        workspace: &WorkerWorkspace,
        workspace_mode: TaskWorkspaceMode,
    ) -> ToolRegistry {
        let mut tools = (*self.tools).clone();
        let (sandbox, writable_roots) = match workspace_mode {
            TaskWorkspaceMode::Coding => (
                self.sandbox,
                git_dir_for_worktree(&workspace.path).into_iter().collect(),
            ),
            TaskWorkspaceMode::ReadOnly => (yi_agent_tools::SandboxMode::ReadOnly, Vec::new()),
        };
        yi_agent_tools::register_builtin_tools_with_sandbox(
            &mut tools,
            workspace.path.clone(),
            sandbox,
            writable_roots,
        );
        tools
    }
```

Add `use yi_agent_core::TaskWorkspaceMode;` to the imports (or `yi_agent_core::subagent::task::TaskWorkspaceMode`).

**7b. `start_with_provider_turn_gate`** (`subagent_runtime.rs:409`): read the mode and use it for the registry and the delivery branch.

Capture the mode before the async move:

```rust
        let workspace_mode = request.workspace_mode;
        let worker_tools = Arc::new(self.worker_tool_registry(&workspace, workspace_mode));
```

In the `AgentEvent::Done` / `None` arm, replace the delivery block (`subagent_runtime.rs:549-562`) with:

```rust
                                            if workspace_mode == TaskWorkspaceMode::Coding {
                                                if let Some(service) = workspace_service.as_ref() {
                                                    match service.inspect_delivery(&workspace_for_delivery) {
                                                        Ok(delivery) => reporter.report_delivery(delivery),
                                                        Err(error)
                                                            if !assistant_report.trim().is_empty()
                                                                && is_empty_delivery_error(&error) =>
                                                        {
                                                            reporter.report_completed(assistant_report.trim())
                                                        }
                                                        Err(error) => reporter.report_failure(error.to_string()),
                                                    }
                                                } else {
                                                    reporter.report_completed(assistant_report.trim());
                                                }
                                            } else {
                                                reporter.report_completed(assistant_report.trim());
                                            }
                                            break 'run;
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent read_only_workers_get_no_write_tools`
Expected: PASS.

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: PASS. In particular `daemon_worker_reports_text_completion_without_a_workspace_delivery` and the sandbox-boundary tests must stay green.

**Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat(app): run read-only workers without write tools or commit delivery"
```

---

## Task 8: Sync project documentation

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md:24`
- Modify: `docs/bug-list.md:16`

**Step 1: Add the feature entry**

Append to `docs/project-management/subagent-runtime.md`:

```markdown
- [x] Subagent 默认只读、按需 worktree — child 默认 `mode: "read_only"`，原地以只读 sandbox 运行（不注册 `write`/`edit`）并走文本结果收口；父显式 `spawn_agent(mode: "coding")` 才建 worktree 并走 commit 交付与审核；mode 落 `tasks.workspace_mode`（schema v9）并在重启后恢复；非 git session 降级为只读原地模式，coding child 以 `coding_requires_git_repository` 失败。代码：`yi-agent-rs/crates/yi-agent-core/src/subagent/{task.rs,worker.rs,supervisor.rs}`、`yi-agent-rs/crates/yi-agent-store/src/{repository.rs,runtime.rs,ipc.rs}`、`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`，验证：`cargo test -p yi-agent-core --test subagent_supervisor children_default_to_read_only && cargo test -p yi-agent-store --test runtime_coordinator read_only_child_runs_in_place && cargo test -p yi-agent-store --test runtime_ipc daemon_spawn_agent_defaults_to_read_only && cargo test -p yi-agent --bin yi-agent read_only_workers_get_no_write_tools`
```

**Step 2: Update the module index count**

In `docs/project-management/README.md:24`, bump the `subagent-runtime` row from `14 / 21` to `15 / 22`:

```markdown
| subagent-runtime | 15 / 22 | [详情](./subagent-runtime.md) |
```

**Step 3: Update `bug-list.md`**

Rewrite line 16 to record the default-read-only fix and add the non-git entry:

```markdown
- [ ] 当前启动subagent runtime 就会创建worktree。太多了怎么清理。（大幅缓解：subagent 现默认只读、不建 worktree，只有显式 `mode: "coding"` 的 child 才建；已集成 delivery 验收后自动回收 worktree/branch/workspace 行，见 `runtime.rs` `recycle_accepted_delivery`。剩余：coding child 已交付但父未集成/未 merge，以及失败/取消/驳回的 coding 任务仍不回收）
- [x] 非 git 目录下 subagent session 不可用（修复：非 git session 降级为只读原地模式，root 不建 worktree；coding child 以 `coding_requires_git_repository` 明确失败。见 `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` `prepare_task_workspace`、`yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` `DaemonWorkspaceService`，验证：`cargo test -p yi-agent-store --test runtime_coordinator coding_child_fails_clearly_without_a_git_repository`）
```

**Step 4: Verify formatting and commit**

Run: `cd yi-agent-rs && cargo fmt --all --check`
Expected: no output (formatting clean; docs are unaffected but this confirms the tree is fmt-clean before the final commit).

```bash
git add docs/project-management/subagent-runtime.md \
        docs/project-management/README.md \
        docs/bug-list.md
git commit -m "docs: record subagent read-only default and non-git degradation"
```

---

## Final verification (before finishing the branch)

Run the four crate suites in sequence (never concurrently; `ps aux | grep cargo` first):

```bash
cd yi-agent-rs
cargo test -p yi-agent-core
cargo test -p yi-agent-tools
cargo test -p yi-agent-store
cargo test -p yi-agent --bin yi-agent
```

Expected: all `0 failed`. Pay special attention to:
- `cargo test -p yi-agent-store --test runtime_coordinator` — the 9 worktree-recycling tests stay green (recycling only applies to coding children, which own a `task_workspaces` row).
- `cargo test -p yi-agent --bin yi-agent` — the sandbox-boundary and text-completion tests stay green.

Then run `superpowers:finishing-a-development-branch`.
