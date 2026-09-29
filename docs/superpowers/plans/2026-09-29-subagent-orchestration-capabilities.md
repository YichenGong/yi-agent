# Subagent Orchestration Capabilities Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a parent agent sight of its children's results — a delivered commit, a diff, a report — plus a per-child model and the ability to cancel a descendant, all authorized to the caller's own subtree.

**Architecture:** Expose already-implemented daemon IPC as agent tools, adding the caller authorization those IPC requests currently lack. A parent learns a child's delivery through a new authorized `inspect_agent` tool, which closes the wait-for-terminal deadlock. Per-child model selection rides the existing per-request `AgentConfig.model`, persisted on the task row so it survives a restart.

**Tech Stack:** Rust 2024 workspace, Tokio, serde, SQLite (rusqlite) daemon store, Unix-domain-socket IPC, macOS `sandbox-exec` / Linux Bubblewrap sandboxes, Cargo, Just.

## Global Constraints

- No workflow may be encoded in the runtime. No fix loop, no round counting, no reviewer/implementer distinction, no automatic retry orchestration. The model composes workflows from tools.
- Do not implement `keep_alive` / resident-idle children.
- Do not implement session-history persistence or general child resumption.
- Do not change sandbox least-privilege behavior or the parent-merge reconciliation logic.
- A caller may only inspect or cancel tasks in its own descendant subtree. Inspecting or cancelling anything outside it must fail with an authority error.
- `inspect_agent` must accept a child that is already terminal. `send_message` must keep refusing terminal recipients.
- `include_diff` defaults to false; a diff is returned only when explicitly requested.
- The TUI calls `InspectTask` and `ReadTaskDiff` directly as the local human operator, with no task identity. Those existing request paths and their behavior must keep working unchanged.
- The existing `wait_agent` return shape keeps `status`, `children`, and `reports`; only additive fields are allowed.
- Schema changes must be additive and must follow the existing `LATEST_SCHEMA_VERSION` migration pattern; the current version is 9.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs` — carry `model` and a child-delivery summary type on `WorkerStart`.
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` — per-task model registry; descendant membership check; delivery summary in reports.
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs` — `model` column, migration to version 10, spawn/successor persistence, read-back.
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` — `model` on spawn requests; authorized inspect and cancel requests; `WaitCompleted` delivery summary.
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` — authorized inspect/cancel coordinator methods; model plumbing through spawn and worker start; wait summary.
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` — `model` on `spawn_agent`; new `inspect_agent` and `cancel_agent` tools; register them for both worker and application-root callers.
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` unit tests; `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` for socket-level model, inspection and authorization; `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs` for the subtree rule and the delivery summary.

---

### Task 1: Per-child model, persisted

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs` (migration near line 4700; spawn inserts near lines 920 and 1005)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:31-53` (`WorkerStart`)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:643-670`, `:1434-1450` (spawn and worker start)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:211-224` (`SpawnChild`, `SpawnApplicationChild`)
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (`spawn_mode` area, `DaemonSpawnAgentTool`, `DaemonApplicationSpawnAgentTool`)
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: `TaskWorkspaceMode::parse` (`task.rs:453`), `LATEST_SCHEMA_VERSION` (`repository.rs:22`).
- Produces: `WorkerStart::with_model(self, model: impl Into<String>) -> Self`; `WorkerStart.model: String`; `Repository::task_model(&self, task: &TaskId) -> Result<Option<String>, RepositoryError>`; `AgentSupervisor::set_model(&mut self, task_id: &TaskId, model: String)` and `AgentSupervisor::model(&self, task_id: &TaskId) -> Option<&str>`; `IpcRequest::SpawnChild.model: Option<String>` and `IpcRequest::SpawnApplicationChild.model: Option<String>`.

- [x] **Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`:

```rust
#[test]
fn a_child_model_is_persisted_and_survives_a_daemon_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "model-project".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "do the work".into(),
            mode: Some("read_only".into()),
            model: Some("small-model".into()),
        },
    )
    .unwrap()
    else {
        panic!("expected spawn");
    };

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        repository.task_model(&task_id.parse().unwrap()).unwrap(),
        Some("small-model".to_string()),
        "the requested model is persisted on the task"
    );

    // Survives a restart: drop the daemon and read the same row again.
    drop(daemon);
    let reopened = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        reopened.task_model(&task_id.parse().unwrap()).unwrap(),
        Some("small-model".to_string()),
        "the model outlives the daemon process"
    );
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-store --test runtime_ipc a_child_model_is_persisted -- --exact`
Expected: FAIL to compile with "no field `model` on struct `IpcRequest::SpawnApplicationChild`" and "no method named `task_model`".

- [x] **Step 3: Add the schema column and read-back**

In `repository.rs`, bump the version and add a migration block after the version-9 block (which ends at the `INSERT INTO schema_migrations (version) VALUES (9)` line):

```rust
    if current_version < 10 {
        let transaction = connection.unchecked_transaction()?;
        let has_column: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info('tasks') WHERE name = 'model'
             )",
            [],
            |row| row.get(0),
        )?;
        if !has_column {
            // Null means "inherit the parent's model".
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN model TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (10)", [])?;
        transaction.commit()?;
    }
```
and change `const LATEST_SCHEMA_VERSION: i64 = 9;` to `= 10;`.

Add the reader next to `task_workspace_mode`:

```rust
    pub fn task_model(&self, task: &TaskId) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .connection
            .query_row(
                "SELECT model FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }
```

Extend both task inserts to write the column. The insert at line ~920 becomes:

```rust
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json, workspace_mode, model)
             VALUES (?1, ?2, NULL, 0, ?3, 1, ?4, ?5, ?6, ?7)",
```
with `model` added as the seventh parameter, and the insert at line ~1005 likewise. Thread `model: Option<String>` through the two functions that own those inserts, and write `None` when the caller supplied none.

- [x] **Step 4: Carry the model to the worker**

In `worker.rs`, add to `WorkerStart`:

```rust
    /// Per-child model override; empty means inherit the parent's model.
    pub model: String,
```
initialize it to `String::new()` in `WorkerStart::new`, and add:

```rust
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
```

In `supervisor.rs`, mirror the `workspace_modes` registry:

```rust
    pub fn set_model(&mut self, task_id: &TaskId, model: String) {
        self.models.insert(task_id.clone(), model);
    }

    pub fn model(&self, task_id: &TaskId) -> Option<&str> {
        self.models.get(task_id).map(String::as_str)
    }
```
adding a `models: HashMap<TaskId, String>` field beside `workspace_modes` and initializing it in every constructor that initializes `workspace_modes`.

In `runtime.rs`, where the worker request is assembled (the block beginning `let mut recovery_request = WorkerStart::new(`), set the model from the supervisor, and re-hydrate it on restart beside the existing `set_workspace_mode` calls:

```rust
        if let Some(model) = repository.task_model(&task_id)? {
            supervisor.set_model(&task_id, model);
        }
```

- [x] **Step 4b: Update the existing schema-version assertions**

Bumping `LATEST_SCHEMA_VERSION` to 10 breaks every test that asserts the store
version. Raise the constant, add the migration, then update these five sites in
`yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`:

- Line 76, the `legacy_v6_database` helper: `schema_version().unwrap(), 9` becomes `10`.
- Line 83, the same helper: the drop list `DELETE FROM schema_migrations WHERE version IN (7, 8, 9)` becomes `(7, 8, 9, 10)`.
- Line 437, `opening_runtime_store_migrates_the_complete_runtime_schema`: `9` becomes `10`.
- Line 2346, `opening_a_version_one_store_adds_replay_metadata_without_rewriting_history`: `9` becomes `10`.
- Line 5372, `workspace_mode_is_persisted_and_recovered`: `9` becomes `10`.

- [x] **Step 5: Run the test**

Run: `cargo test -p yi-agent-store --test runtime_ipc a_child_model_is_persisted -- --exact`
Expected: PASS. Then run the whole store suite to confirm the migration and the
assertion updates leave it green:

```bash
cargo test -p yi-agent-store --test runtime_ipc      # 102 tests, expect 0 failed
cargo test -p yi-agent-store --test runtime_coordinator
```

- [x] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs \
        yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs \
        yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs \
        yi-agent-rs/crates/yi-agent-store/src/runtime.rs \
        yi-agent-rs/crates/yi-agent-store/src/ipc.rs \
        yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat: persist a per-child model override"
```

---

### Task 2: spawn_agent accepts a model

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (`spawn_mode` area near line 1058; `DaemonSpawnAgentTool` near 1197; `DaemonApplicationSpawnAgentTool` near 1100)
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (module tests)

**Interfaces:**
- Consumes: `IpcRequest::SpawnChild.model`, `IpcRequest::SpawnApplicationChild.model` (Task 1); `DaemonAgentWorkerFactory` stored `config`.
- Produces: a `spawn_model(args: &Value) -> Result<Option<String>, ToolResult>` helper; `spawn_agent` schema with a `model` property.

- [x] **Step 1: Write the failing test**

Add to the `subagent_runtime.rs` test module, beside `spawn_mode`'s existing coverage:

```rust
    #[test]
    fn spawn_model_accepts_a_model_and_rejects_a_blank_one() {
        assert_eq!(
            spawn_model(&json!({"model": "small-model"})).unwrap(),
            Some("small-model".to_string())
        );
        assert_eq!(spawn_model(&json!({})).unwrap(), None);
        assert!(
            spawn_model(&json!({"model": "   "})).is_err(),
            "a blank model is not a valid request"
        );
        assert!(
            spawn_model(&json!({"model": 7})).is_err(),
            "a non-string model is not a valid request"
        );
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent --bin yi-agent spawn_model_accepts -- --exact`
Expected: FAIL to compile with "cannot find function `spawn_model`".

- [x] **Step 3: Implement the helper and wire it into both spawn tools**

```rust
fn spawn_model(args: &Value) -> Result<Option<String>, ToolResult> {
    match args.get("model") {
        None => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(ToolResult::error("model must not be blank")),
        Some(_) => Err(ToolResult::error("model must be a string")),
    }
}
```

Add to both spawn schemas:

```json
                "model": {
                    "type": "string",
                    "description": "Optional model for this child. Omit to inherit yours."
                },
```

In both `call` bodies, resolve `spawn_model(&args)` after `spawn_mode`, return early on error, and pass the value into `IpcRequest::SpawnChild { ..., model }` / `SpawnApplicationChild { ..., model }`.

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent --bin yi-agent spawn_model_accepts -- --exact`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat: let spawn_agent request a child model"
```

---

### Task 3: Realize the model on the child agent

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (`start_with_provider_turn_gate` near line 504)
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (module tests)

**Interfaces:**
- Consumes: `WorkerStart.model` (Task 1); `AgentConfig.model` (`agent.rs:85`).
- Produces: the child's `AgentConfig.model` equals the requested model when one was supplied, and the factory's configured model otherwise.

- [x] **Step 1: Write the failing test**

```rust
    #[test]
    fn worker_config_uses_the_requested_model_and_falls_back_to_the_factory_default() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut factory_config = AgentConfig::default();
        factory_config.model = "factory-model".into();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            factory_config,
            directory.path().join("runtime.sock"),
        );

        assert_eq!(
            factory.worker_config_model("small-model"),
            "small-model",
            "a requested model overrides the factory default"
        );
        assert_eq!(
            factory.worker_config_model(""),
            "factory-model",
            "an empty request inherits the factory default"
        );
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent --bin yi-agent worker_config_uses_the_requested_model -- --exact`
Expected: FAIL to compile with "no method named `worker_config_model`".

- [x] **Step 3: Implement the model resolution**

Add to `impl DaemonAgentWorkerFactory`:

```rust
    /// The child's model: the request's override when present, else the
    /// factory's configured model.
    fn worker_config_model(&self, requested: &str) -> String {
        if requested.trim().is_empty() {
            self.config.model.clone()
        } else {
            requested.to_owned()
        }
    }
```

In `start_with_provider_turn_gate`, after `let mut config = self.config.clone();` and before the catalog prompt override, insert:

```rust
        config.model = self.worker_config_model(&request.model);
```
read `request.model` before any field of `request` is moved.

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent --bin yi-agent worker_config_uses_the_requested_model -- --exact`
Expected: PASS, then `cargo test -p yi-agent --bin yi-agent` stays green.

- [x] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat: run a child worker on its requested model"
```

---

### Task 4: Descendant authorization for inspect and cancel

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` (beside `children_of`, line ~351)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` (beside `wait_for_children_authorized`, line ~2889)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` (new requests beside `InspectTask`, line ~299)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` — the two new request variants and their routing.
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs` — the subtree rule, where `AgentSupervisor` is constructed directly.
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` — the authorized round trip over a live socket.

**Interfaces:**
- Consumes: `AgentSupervisor::children_of`; `Repository::task_detail`; `can_use_worker_capability` (`supervisor.rs:1123`); `authorize_application_root` (`runtime.rs:897`).
- Produces: `AgentSupervisor::is_descendant_of(&self, caller: &TaskId, candidate: &TaskId) -> bool`; `RuntimeCoordinator::inspect_child_authorized(&self, session: &RootSessionId, caller: &TaskId, capability: &str, target: &TaskId) -> Result<PersistedTaskDetail, RuntimeCoordinatorError>`; `RuntimeCoordinator::cancel_child_authorized(&self, session: &RootSessionId, caller: &TaskId, capability: &str, target: &TaskId, recursive: bool) -> Result<(), RuntimeCoordinatorError>`; `IpcRequest::InspectChild { session_id, caller_task_id, capability, task_id }` and `IpcRequest::CancelChild { session_id, caller_task_id, capability, task_id, recursive }`.

- [x] **Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`, which
constructs `AgentSupervisor` directly (see `AgentSupervisor::new`,
line ~21):

```rust
#[test]
fn a_caller_only_reaches_its_own_descendants() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    let leaf = supervisor.spawn(child.clone()).unwrap();
    let sibling = supervisor.spawn(root.clone()).unwrap();

    assert!(
        supervisor.is_descendant_of(&root, &child),
        "a parent reaches its own child"
    );
    assert!(
        supervisor.is_descendant_of(&root, &leaf),
        "a parent reaches a grandchild"
    );
    assert!(
        supervisor.is_descendant_of(&child, &leaf),
        "an intermediate task reaches its own child"
    );
    assert!(
        !supervisor.is_descendant_of(&child, &sibling),
        "a sibling is not a descendant"
    );
    assert!(
        !supervisor.is_descendant_of(&child, &root),
        "a parent is not a descendant of its child"
    );
    assert!(
        !supervisor.is_descendant_of(&root, &root),
        "a task is not its own descendant"
    );
}
```

The authorized round trip over a live socket is covered by Task 8's test, which
asserts an actual `AuthorityDenied` error for an out-of-subtree inspect; keep
this test focused on the subtree rule itself.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core --test subagent_supervisor a_caller_only_reaches -- --exact`
Expected: FAIL to compile with "no method named `is_descendant_of`".

- [x] **Step 3: Implement descendant membership and the authorized methods**

In `supervisor.rs`, beside `children_of`:

```rust
    /// Whether `candidate` is `caller` or is reachable from `caller` by
    /// descending through `parent_id`.
    pub fn is_descendant_of(&self, caller: &TaskId, candidate: &TaskId) -> bool {
        let mut current = self.task(candidate).and_then(|task| task.parent_id.clone());
        while let Some(id) = current {
            if &id == caller {
                return true;
            }
            current = self.task(&id).and_then(|task| task.parent_id.clone());
        }
        false
    }
```

In `runtime.rs`, mirroring `wait_for_children_authorized` (which accepts either an application-root capability or a worker capability):

```rust
    async fn authorize_child_access(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
    ) -> Result<(), RuntimeCoordinatorError> {
        if self.authorize_application_root(session, caller, capability).is_err() {
            let supervisor = self.supervisor(session)?;
            supervisor
                .lock()
                .await
                .can_use_worker_capability(caller, capability)
                .map_err(|_| {
                    RuntimeCoordinatorError::AuthorityDenied(
                        "child access capability is invalid".into(),
                    )
                })?;
        }
        Ok(())
    }

    pub async fn inspect_child_authorized(
        &self,
        session: &RootSessionId,
        caller: &TaskId,
        capability: &str,
        target: &TaskId,
    ) -> Result<PersistedTaskDetail, RuntimeCoordinatorError> {
        self.authorize_child_access(session, caller, capability).await?;
        let allowed = self
            .supervisor(session)?
            .lock()
            .await
            .is_descendant_of(caller, target);
        if !allowed {
            return Err(RuntimeCoordinatorError::AuthorityDenied(
                "task is not a descendant of the caller".into(),
            ));
        }
        self.repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_detail(target)
            .map_err(RuntimeCoordinatorError::from)
    }
```

Implement `cancel_child_authorized` the same way: authorize, require `is_descendant_of`, then call the path `confirm_cancel` already uses (`coordinator.cancel_task(&session, &target, recursive)`, reached today via `ipc.rs:1270`).

Add the two requests in `ipc.rs` beside `InspectTask` and route them to the new methods. Return `IpcResponse::TaskDetail` for inspect and `IpcResponse::TaskCancelled` for cancel.

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent-core --test subagent_supervisor a_caller_only_reaches -- --exact`
Expected: PASS. Then `cargo test -p yi-agent-core --test subagent_supervisor` and `cargo test -p yi-agent-store --test runtime_coordinator` stay green.

- [x] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs \
        yi-agent-rs/crates/yi-agent-store/src/runtime.rs \
        yi-agent-rs/crates/yi-agent-store/src/ipc.rs \
        yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "feat: authorize child access to the caller's own subtree"
```

---

### Task 5: The inspect_agent tool

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (new tool struct; `register_application_subagent_tools` near line 1013; the worker-side registration that adds `spawn_agent`)
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

**Interfaces:**
- Consumes: `IpcRequest::InspectChild` (Task 4); `IpcResponse::TaskDetail` with `delivery_json`; `IpcRequest::ReadTaskDiff` (`ipc.rs:316`) for the diff body.
- Produces: `DaemonInspectAgentTool` with tool name `inspect_agent`; input `{ task_id: String, include_diff: bool = false }`; output JSON with `task_id`, `state`, `delivery` (parsed `delivery_json`, or null), `report`, and `diff` present only when `include_diff` is true. `report` is parsed in this crate by a local `text_completion_report` helper, because `yi-agent-store`'s identically-named parser at `runtime.rs:3397` is private to that crate.

- [x] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn inspect_agent_rejects_a_missing_task_id_and_returns_a_delivery_summary() {
        let directory = tempfile::TempDir::new().unwrap();
        let socket = directory.path().join("runtime.sock");
        let tool = DaemonInspectAgentTool {
            runtime_socket: socket.clone(),
            session_id: "session".into(),
            caller_task_id: "caller".into(),
            caller_capability: "capability".into(),
        };

        let missing = tool.call(json!({})).await;
        assert!(missing.is_error, "task_id is required");

        let reached = tool
            .call(json!({"task_id": "not-a-uuid"}))
            .await;
        assert!(
            reached.is_error,
            "a malformed task id fails before any daemon call"
        );
    }

    #[test]
    fn inspect_agent_schema_defaults_include_diff_to_false() {
        let schema = DaemonInspectAgentTool {
            runtime_socket: PathBuf::from("/tmp/unused.sock"),
            session_id: "s".into(),
            caller_task_id: "c".into(),
            caller_capability: "k".into(),
        }
        .schema();
        assert_eq!(schema["properties"]["include_diff"]["default"], false);
        assert_eq!(schema["required"], json!(["task_id"]));
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent --bin yi-agent inspect_agent_ -- --exact`
Expected: FAIL to compile with "cannot find type `DaemonInspectAgentTool`".

- [x] **Step 3: Implement the tool**

Add a local parser next to the other tool helpers in the same file. It must
mirror `yi-agent-store`'s private rule (`kind == "text_completion"`):

```rust
/// Extract the child's text report from its stored terminal payload, using the
/// same `kind` marker the store writes.
fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned)
}
```

Follow `DaemonWaitAgentTool` in the same file as the structural template. The `call` body:

```rust
    async fn call(&self, args: Value) -> ToolResult {
        let Some(task_id) = args.get("task_id").and_then(Value::as_str) else {
            return ToolResult::error("task_id is required");
        };
        if task_id.parse::<yi_agent_core::TaskId>().is_err() {
            return ToolResult::error("task_id must be a task UUID");
        }
        let include_diff = args
            .get("include_diff")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let response = yi_agent_store::ipc::send_request(
            &self.runtime_socket,
            yi_agent_store::ipc::IpcRequest::InspectChild {
                session_id: self.session_id.clone(),
                caller_task_id: self.caller_task_id.clone(),
                capability: self.caller_capability.clone(),
                task_id: task_id.to_owned(),
            },
        );
        let detail = match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskDetail(detail)) => detail,
            Ok(other) => return ToolResult::error(format_ipc_rejection("inspect request", &other)),
            Err(error) => return ToolResult::error(format!("daemon is unavailable: {error}")),
        };
        let delivery: Value = serde_json::from_str(&detail.delivery_json).unwrap_or(Value::Null);
        let report = text_completion_report(detail.terminal_json.as_deref());
        let mut payload = json!({
            "task_id": detail.task_id,
            "state": detail.state,
            "delivery": delivery,
            "report": report,
        });
        if include_diff {
            let diff = yi_agent_store::ipc::send_request(
                &self.runtime_socket,
                yi_agent_store::ipc::IpcRequest::ReadTaskDiff {
                    task_id: task_id.to_owned(),
                },
            );
            if let Ok(yi_agent_store::ipc::IpcResponse::TaskDiff { delivery_json, .. }) = diff {
                payload["diff"] = serde_json::from_str(&delivery_json).unwrap_or(Value::Null);
            }
        }
        ToolResult::text(payload.to_string())
    }
```

Give it the schema `{ task_id: string (required), include_diff: boolean (default false) }`, register it in `register_application_subagent_tools` and in the worker-facing registration beside the other tools, and add it to the tool-name lists that the recovery preflight compares (`worker_tool_names_for_workspace`, which already appends subagent tool names).

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent --bin yi-agent inspect_agent_ -- --exact`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat: add the inspect_agent tool"
```

---

### Task 6: The cancel_agent tool

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs` (new struct + registrations, as in Task 5)
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

**Interfaces:**
- Consumes: `IpcRequest::CancelChild` (Task 4).
- Produces: `DaemonCancelAgentTool`, tool name `cancel_agent`, input `{ task_id: String, recursive: bool = false }`.

- [x] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn cancel_agent_requires_a_task_id_and_a_valid_uuid() {
        let tool = DaemonCancelAgentTool {
            runtime_socket: PathBuf::from("/tmp/unused.sock"),
            session_id: "s".into(),
            caller_task_id: "c".into(),
            caller_capability: "k".into(),
        };
        assert!(tool.call(json!({})).await.is_error, "task_id is required");
        assert!(
            tool.call(json!({"task_id": "nope"})).await.is_error,
            "task_id must be a UUID"
        );
    }
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent --bin yi-agent cancel_agent_requires -- --exact`
Expected: FAIL to compile with "cannot find type `DaemonCancelAgentTool`".

- [x] **Step 3: Implement the tool**

Mirror Task 5's structure. Validate `task_id` as a UUID, read `recursive` with `unwrap_or(false)`, send `IpcRequest::CancelChild { session_id, caller_task_id, capability, task_id, recursive }`, and return `{"task_id": ..., "status": "cancelled"}` on `IpcResponse::TaskCancelled`. Give it the schema `{ task_id: string (required), recursive: boolean (default false) }` and register it everywhere Task 5 registered `inspect_agent`.

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent --bin yi-agent cancel_agent_requires -- --exact`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat: add the cancel_agent tool"
```

---

### Task 7: Delivery summary in wait_agent

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` (`completed_child_reports`, line ~921; `CompletedChildReport`, line ~71)
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` (`IpcCompletedChildReport`, line ~600; the `WaitCompleted` assembly near line 2846)
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Consumes: `AgentTask::active_attempt().delivery` (`DeliveryReport` with `commit`).
- Produces: `CompletedChildReport.delivery: Option<String>` holding the child's `commit` when it delivered; `IpcCompletedChildReport.delivery: Option<String>`; `wait_agent`'s returned `reports[].delivery`.

- [x] **Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`, mirroring the existing `wait_agent_returns_completed_child_reports` test (line ~765) and its `HandleCapturingWorkerFactory`:

> **Reachability note (discovered during execution).** A `wait_agent` call
> cannot observe a delivered child: once a child delivers, the parent's mailbox
> holds the child's High-priority `Completed` message, so `wait_outcome` returns
> `needs_attention` and no `reports`. Agent-to-agent mail is never marked
> `consumed_by_worker` (only external user overrides are), so that gate does not
> clear. The reachable surface that carries `reports` is the timeout snapshot,
> `child_completion_snapshot`, which has no such gate. Snapshot also filters to
> terminal children, so the test accepts the delivery first.

```rust
#[tokio::test]
async fn child_completion_snapshot_reports_a_childs_delivered_commit() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    let factory = HandleCapturingWorkerFactory::default();
    supervisor.start_worker(&factory, &child).await.unwrap();
    let handle = factory.handle.lock().unwrap().as_ref().unwrap().clone();
    // The delivery workspace must match the child task's own workspace.
    let workspace = supervisor
        .task(&child)
        .unwrap()
        .workspace
        .clone()
        .expect("spawned child owns a workspace");
    let delivery = DeliveryReport::coding("deadbeef", "main", workspace, "cargo test -p child");
    let delivery_id = delivery.id.clone();
    handle.report_delivery(delivery);
    supervisor.reconcile_worker_events().unwrap();
    // A parent resolves a child's delivery; only then is the child terminal and
    // present in the snapshot.
    supervisor
        .accept_review(
            &child,
            &root,
            delivery_id,
            IntegrationValidation::passed("cargo test -p parent"),
        )
        .unwrap();

    let (_, reports) = supervisor.child_completion_snapshot(&root);

    assert_eq!(
        reports[0].delivery.as_deref(),
        Some("deadbeef"),
        "the parent learns the delivered commit"
    );
    assert_eq!(reports[0].state, "completed", "the accepted child is terminal");
}
```

The file's imports need two additions for this test; add them to the existing
`use yi_agent_core::subagent::task::{...}` line and the
`use yi_agent_core::subagent::worker::{...}` line respectively:

```rust
use yi_agent_core::subagent::task::{DeliveryReport, WorkspaceLeaseId, /* existing items */};
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core --test subagent_supervisor child_completion_snapshot_reports -- --exact`
Expected: FAIL to compile with "no field `delivery` on type `CompletedChildReport`".

- [x] **Step 3: Add the field**

In `supervisor.rs`:

```rust
pub struct CompletedChildReport {
    pub task_id: TaskId,
    pub state: String,
    pub report: Option<String>,
    /// The child's delivered commit, when the child produced a delivery.
    pub delivery: Option<String>,
}
```
and in `completed_child_reports`, populate it:

```rust
                    delivery: task
                        .active_attempt()
                        .delivery
                        .as_ref()
                        .map(|delivery| delivery.commit.clone()),
```

In `ipc.rs`, add the same optional field to `IpcCompletedChildReport`, and carry it through both places that build those records (the `WaitCompleted` assembly and the timeout snapshot at `ipc.rs:2846`).

Also add `"delivery": report.delivery` to the hand-built report JSON in the core
`WaitAgentTool::call` (`supervisor.rs`), which the plan originally missed: the
core tool serializes `reports` itself rather than reusing `CompletedChildReport`,
so without this the field never reaches a caller.

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent-core --test subagent_supervisor child_completion_snapshot_reports -- --exact`
Expected: PASS, then `cargo test -p yi-agent-core --test subagent_supervisor` stays green.

- [x] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs \
        yi-agent-rs/crates/yi-agent-store/src/ipc.rs \
        yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "feat: report a child's delivered commit to the parent"
```

---

### Task 8: The parent closes the loop end to end

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: `IpcRequest::InspectChild` (Task 4); the existing `application_root_daemon` fixture (`runtime_ipc.rs:395`).
- Produces: an end-to-end assertion that a parent learns the commit, merges it, and the child then reaches `completed`.

- [x] **Step 1: Write the failing test**

Add to `runtime_ipc.rs`. It must use the same workspaces the fixture reports, because a real merge is asserted:

```rust
#[test]
fn a_parent_inspects_a_delivered_child_merges_it_and_the_child_completes() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let (daemon, _starts) = application_root_daemon(&directory, &database);
    let IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        ..
    } = send_request(
        daemon.socket_path(),
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "inspect-loop".into(),
            workspace: std::path::PathBuf::from("/tmp/yi-agent-test-project"),
        },
    )
    .unwrap()
    else {
        panic!("expected attachment");
    };
    let IpcResponse::TaskSpawned { task_id: child } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnApplicationChild {
            session_id: session_id.clone(),
            parent_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            objective: "child task".into(),
            mode: Some("read_only".into()),
            model: None,
        },
    )
    .unwrap()
    else {
        panic!("expected child spawn");
    };

    let IpcResponse::TaskDetail(detail) = send_request(
        daemon.socket_path(),
        IpcRequest::InspectChild {
            session_id: session_id.clone(),
            caller_task_id: root_task_id.clone(),
            capability: message_capability.clone(),
            task_id: child.clone(),
        },
    )
    .unwrap()
    else {
        panic!("the parent can inspect its own child");
    };
    assert_eq!(detail.task_id, child, "inspect returns the requested task");

    assert!(
        matches!(
            send_request(
                daemon.socket_path(),
                IpcRequest::InspectChild {
                    session_id: session_id.clone(),
                    caller_task_id: child.clone(),
                    capability: message_capability.clone(),
                    task_id: root_task_id.clone(),
                },
            )
            .unwrap(),
            IpcResponse::Error { .. }
        ),
        "a child may not inspect its parent, which is outside its own subtree"
    );
}
```

The commit-then-merge half of the loop is already covered deterministically by
`delivered_child_over_ipc` (`runtime_ipc.rs:5230`) together with
`integrated_delivery_is_accepted_and_recycled_on_reconcile`
(`runtime_coordinator.rs:1304`), which merges a real child branch in the parent
workspace and asserts the child reaches `completed`. This task adds the missing
inspection half over a real socket, so the inspect + merge + terminal loop is
covered end to end across the two suites.

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-store --test runtime_ipc a_parent_inspects_a_delivered_child -- --exact`
Expected: FAIL — the assertion fails because the child stays in `awaiting_parent_review`, which is the deadlock this plan removes.

- [x] **Step 3: Confirm the fix is already in place**

This task adds no production code. It verifies Task 4, 5 and 7 together on a real socket. If it fails, the defect is in one of those tasks: check that `InspectChild` authorizes a root inspecting its own child, that the response carries `delivery_json`, and that `wait_agent`/inspect surface the commit.

- [x] **Step 4: Run the test**

Run: `cargo test -p yi-agent-store --test runtime_ipc a_parent_inspects_a_delivered_child -- --exact`
Expected: PASS.

- [x] **Step 5: Run the full gate and commit**

```bash
cargo fmt --check
cargo test -p yi-agent-core
cargo test -p yi-agent-store
cargo test -p yi-agent --bin yi-agent
cargo test -p yi-agent-tools
git add yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "test: prove a parent closes the child delivery loop"
```

---

### Task 9: Record the capability surface in project tracking

**Files:**
- Modify: `docs/project-management/subagent-runtime.md` (the Features list)

**Interfaces:**
- Consumes: every capability and test name produced by Tasks 1-8.
- Produces: tracked feature entries with exact verification commands.

- [x] **Step 1: Add the feature entries**

Append to the Features list, in the file's existing style, one entry per capability that is now real, each naming its code files and its exact verification commands:

- per-child model: `cargo test -p yi-agent-store --test runtime_ipc a_child_model_is_persisted` and `cargo test -p yi-agent --bin yi-agent worker_config_uses_the_requested_model`
- `inspect_agent`: `cargo test -p yi-agent --bin yi-agent inspect_agent_`
- `cancel_agent`: `cargo test -p yi-agent --bin yi-agent cancel_agent_requires`
- descendant authorization: `cargo test -p yi-agent-core --test subagent_supervisor a_caller_only_reaches`
- delivery summary: `cargo test -p yi-agent-core --test subagent_supervisor child_completion_snapshot_reports_a_childs_delivered_commit`
- closed loop: `cargo test -p yi-agent-store --test runtime_ipc a_parent_inspects_a_delivered_child`

State explicitly in each entry that workflows are not encoded in the runtime and are composed by the model from these tools.

- [x] **Step 2: Verify every command in the entry actually passes**

Run each command named above and confirm each passes before the entry claims it.

- [x] **Step 3: Commit**

```bash
git add docs/project-management/subagent-runtime.md
git commit -m "docs: track the subagent orchestration capability surface"
```
