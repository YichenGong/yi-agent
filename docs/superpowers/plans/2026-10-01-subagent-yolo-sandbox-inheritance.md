# Subagent 继承父的有效沙箱 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `coding` 子任务的 OS 沙箱跟随父 agent 的有效沙箱——父线程切 YOLO 后派发的 coding 子任务以 `DangerFullAccess`（无沙箱）运行，父普通态时维持今天的 `workspace-write`；`read_only` 子任务恒 `ReadOnly`。

**Architecture:** 在 spawn 那一刻对"父的有效沙箱"取快照，作为一个不透明字符串沿 IPC → store（落库/恢复）→ supervisor → worker factory 逐层传递；由 `yi-agent-subagent` 统一把它解析成 `SandboxMode` 并 clamp（coding 下限 `WorkspaceWrite`）。`read_only` child 不带该值。整棵子树按"子继承直接父任务的有效沙箱"逐层传递。

**Tech Stack:** Rust（core / store / subagent / runtime / app-server / CLI bin）、SQLite（rusqlite，schema v12）、serde、Tauri 桌面端经 `yi-agent app-server` 复用同一条 daemon 链路。

**Spec:** `docs/superpowers/specs/2026-10-01-subagent-yolo-sandbox-inheritance-design.md`

## Global Constraints

- 有效沙箱字符串取值固定为 `"read-only"` / `"workspace-write"` / `"danger-full-access"`（kebab-case，与 `SandboxMode` 的 `ValueEnum` 名一致；注意它与 `ChildWriteMode` 的 snake_case `"read_only"` 不同，勿混用）。
- `read_only` child 恒 `ReadOnly`，忽略继承值；继承值只对 `coding` child 生效。
- `coding` child 的沙箱 = `clamp(继承值, 下限 WorkspaceWrite)`；缺省（`None`）回退到 factory 的 `cfg.sandbox`。
- `read_only` 子任务仍不能 `spawn_agent(mode:"coding")`（沿用既有拒绝，本改动不放宽）。
- `yi-agent-store` 只依赖 `yi-agent-core`，**不得**依赖 `yi-agent-tools`；跨层只传不透明字符串。
- 模型可见的 `spawn_agent` schema **不新增参数**；继承是自动的。
- 每个 crate 单独跑测试（遵守 `CLAUDE.md`：跑前 `ps aux | grep cargo`，禁止 workspace 全量 `cargo test`）。
- 提交信息使用英文、conventional commits；同一 PR 内同步 `docs/project-management/*`。

---

### Task 1: core 新增 `InheritedSandbox` 类型

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`（在 `ChildWriteMode` 定义之后，约 `:464` `impl Default for ChildWriteMode` 之后追加）
- Test: 同文件 `#[cfg(test)] mod tests` 内追加

**Interfaces:**
- Consumes: 无。
- Produces:
  - `yi_agent_core::subagent::task::InheritedSandbox`（`pub enum`，`Copy`）
  - `InheritedSandbox::as_str(&self) -> &'static str` → `"read-only" | "workspace-write" | "danger-full-access"`
  - `InheritedSandbox::parse(value: &str) -> Option<Self>`
  - 需在 `yi-agent-rs/crates/yi-agent-core/src/lib.rs:26` 的 `pub use subagent::task::{...}` 里补导出 `InheritedSandbox`（与 `ChildWriteMode` 同处）。

- [ ] **Step 1: Write the failing test**

在 `task.rs` 末尾 `#[cfg(test)] mod tests`（若无则新建该模块）加入：

```rust
#[cfg(test)]
mod inherited_sandbox_tests {
    use super::InheritedSandbox;

    #[test]
    fn round_trips_every_variant() {
        for variant in [
            InheritedSandbox::ReadOnly,
            InheritedSandbox::WorkspaceWrite,
            InheritedSandbox::DangerFullAccess,
        ] {
            assert_eq!(InheritedSandbox::parse(variant.as_str()), Some(variant));
        }
    }

    #[test]
    fn rejects_unknown_and_snake_case_spellings() {
        assert_eq!(InheritedSandbox::parse("read_only"), None);
        assert_eq!(InheritedSandbox::parse("nope"), None);
        assert_eq!(InheritedSandbox::parse(""), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && ps aux | grep -c '[c]argo' ; cargo test -p yi-agent-core --lib inherited_sandbox_tests`
Expected: 编译失败（`InheritedSandbox` 未定义）。

- [ ] **Step 3: Write minimal implementation**

在 `task.rs` 的 `impl Default for ChildWriteMode { ... }` 之后追加：

```rust
/// The OS sandbox a task inherits from its parent at spawn time.
///
/// Lives in `core` so `yi-agent-store` (which does not depend on
/// `yi-agent-tools`) can carry it across IPC as an opaque string and the
/// `yi-agent-subagent` layer can resolve it into a `SandboxMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InheritedSandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl InheritedSandbox {
    /// Kebab-case, matching `SandboxMode`'s `ValueEnum` names and the CLI
    /// spellings (`--sandbox danger-full-access`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }
}
```

在 `yi-agent-rs/crates/yi-agent-core/src/lib.rs:26` 的 `pub use subagent::task::{` 列表中追加 `InheritedSandbox,`。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib inherited_sandbox_tests`
Expected: PASS（2 个测试）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent-core/src/subagent/task.rs crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): add InheritedSandbox for subagent sandbox inheritance"
```

---

### Task 2: core — `WorkerStart` 字段与 supervisor 逐任务登记

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`（`WorkerStart` 结构 `:66`，`new` `:137`，`with_*` 构造器 `:162` 一带）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`（结构字段 `:88`，三个构造函数 `:123/:161/:214`，`spawn_with_objective` `:1048`，`insert_hydrated_review_child` `:236`，`insert_recovered_child` `:271`，`WorkerStart` 组装 `:575`，及其它 `workspace_modes` 登记点 `:252/:323/:354`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/mod.rs`（若需 re-export，可不改）
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Consumes: `InheritedSandbox`（Task 1）。
- Produces:
  - `WorkerStart.inherited_sandbox: Option<InheritedSandbox>`
  - `WorkerStart::with_inherited_sandbox(self, InheritedSandbox) -> Self`
  - `AgentSupervisor::set_inherited_sandbox(&mut self, &TaskId, InheritedSandbox)`
  - `AgentSupervisor::inherited_sandbox(&self, &TaskId) -> Option<InheritedSandbox>`

- [ ] **Step 1: Write the failing test**

在 `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs` 末尾追加：

```rust
#[test]
fn worker_start_carries_inherited_sandbox_from_supervisor() {
    use yi_agent_core::subagent::supervisor::AgentSupervisor;
    use yi_agent_core::subagent::task::{ChildWriteMode, InheritedSandbox};
    use yi_agent_core::subagent::worker::SpawnRequest;

    let session = yi_agent_core::subagent::task::RootSessionId::new();
    let mut supervisor = AgentSupervisor::new_with_objective(session, "root".into());
    let root = supervisor.root_task_id().clone();
    supervisor.set_inherited_sandbox(&root, InheritedSandbox::DangerFullAccess);
    assert_eq!(
        supervisor.inherited_sandbox(&root),
        Some(InheritedSandbox::DangerFullAccess)
    );

    let child = supervisor
        .spawn_with_objective(
            root.clone(),
            SpawnRequest::new("child".into(), ChildWriteMode::Coding, None),
        )
        .expect("child spawns");
    // A child inherits its parent's value until the caller overrides it.
    assert_eq!(supervisor.inherited_sandbox(&child), None);
    supervisor.set_inherited_sandbox(&child, InheritedSandbox::DangerFullAccess);
    assert_eq!(
        supervisor.inherited_sandbox(&child),
        Some(InheritedSandbox::DangerFullAccess)
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor worker_start_carries_inherited_sandbox_from_supervisor`
Expected: 编译失败（`set_inherited_sandbox` 不存在）。

- [ ] **Step 3: Write minimal implementation**

在 `worker.rs` 的 `WorkerStart` 结构体，`workspace_mode` 字段之后加：

```rust
    /// The OS sandbox this task inherits from its parent at spawn time. `None`
    /// means "no inheritance": the factory falls back to its configured mode.
    pub inherited_sandbox: Option<InheritedSandbox>,
```

在 `worker.rs` 顶部 `use` 中把 `InheritedSandbox` 引入（与 `ChildWriteMode` 同一 `use` 行）：

```rust
use super::task::{AttemptId, ChildWriteMode, InheritedSandbox, ...};
```

在 `WorkerStart::new` 的构造器字面量里，`workspace_mode: ChildWriteMode::default(),` 之后加：

```rust
            inherited_sandbox: None,
```

在 `with_workspace_mode` 之后加：

```rust
    pub fn with_inherited_sandbox(mut self, inherited_sandbox: InheritedSandbox) -> Self {
        self.inherited_sandbox = Some(inherited_sandbox);
        self
    }
```

在 `supervisor.rs`：`AgentSupervisor` 结构体 `workspace_modes` 之后加：

```rust
    inherited_sandboxes: HashMap<TaskId, InheritedSandbox>,
```

在三个构造函数（`new_with_objective`/`from_recovered_root`/`from_hydrated_review_root`，即 `workspace_modes: HashMap::new(),` 出现的每一处）之后加：

```rust
            inherited_sandboxes: HashMap::new(),
```

在 `spawn_with_objective`（`:1048`）里 `self.workspace_modes.insert(...)` 之后加：

```rust
        // A child starts with no recorded inheritance; the caller (coordinator)
        // seeds it from the persisted task row before the worker starts.
        self.inherited_sandboxes.remove(&task_id);
```

在 `insert_hydrated_review_child` 与 `insert_recovered_child` 里，`self.workspace_modes.insert(task_id.clone(), workspace_mode);` 之后各加：

```rust
        self.inherited_sandboxes.remove(&task_id);
```

在 `set_workspace_mode`/`workspace_mode` 附近加：

```rust
    pub fn set_inherited_sandbox(&mut self, task_id: &TaskId, sandbox: InheritedSandbox) {
        self.inherited_sandboxes.insert(task_id.clone(), sandbox);
    }

    pub fn inherited_sandbox(&self, task_id: &TaskId) -> Option<InheritedSandbox> {
        self.inherited_sandboxes.get(task_id).copied()
    }
```

在 `WorkerStart` 组装处（`:575` 的 `.with_workspace_mode(...)` 之后）加：

```rust
        .maybe_with_inherited_sandbox(self.inherited_sandbox(task_id))
```

并在 `worker.rs` 的 `WorkerStart` 加一个可选构造器：

```rust
    pub fn maybe_with_inherited_sandbox(mut self, value: Option<InheritedSandbox>) -> Self {
        self.inherited_sandbox = value;
        self
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor`
Expected: PASS（含既有用例不回归）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent-core/src/subagent/worker.rs crates/yi-agent-core/src/subagent/supervisor.rs crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "feat(core): thread inherited sandbox through WorkerStart and supervisor"
```

---

### Task 3: store — schema v12、落库与恢复读取

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`（`LATEST_SCHEMA_VERSION:18`、`PersistedRecoveredTask:345`、`create_task_with_attempt_and_objective:897`、`create_child_task_with_attempt_and_objective:985`、`recovered_tasks:3374`、`migrate:4125`）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `yi_agent_core::InheritedSandbox`（Task 1）。
- Produces:
  - 新列 `tasks.inherited_sandbox TEXT`（可空）
  - `Repository::task_inherited_sandbox(&self, &TaskId) -> Result<Option<InheritedSandbox>, RepositoryError>`
  - `PersistedRecoveredTask.inherited_sandbox: Option<InheritedSandbox>`
  - `create_task_with_attempt_and_objective` / `create_child_task_with_attempt_and_objective` 各新增末位参数 `inherited_sandbox: Option<InheritedSandbox>`

- [ ] **Step 1: Write the failing test**

在 `repository.rs` 的测试模块追加（沿用该文件既有的临时库 helper；若测试以 `Repository::open` 类构造函数开头，照抄相邻用例的建库写法）：

```rust
#[test]
fn inherited_sandbox_round_trips_and_defaults_to_none() {
    let mut repo = test_repository(); // 复用本文件既有 helper
    let root = yi_agent_core::RootSessionId::new();
    let attempt = yi_agent_core::subagent::task::AttemptId::new();
    let task = yi_agent_core::subagent::task::TaskId::new();
    repo.create_task_with_attempt_and_objective(
        &task,
        &root,
        &attempt,
        1,
        "queued",
        "obj",
        yi_agent_core::ChildWriteMode::Coding,
        None,
        Some(yi_agent_core::InheritedSandbox::DangerFullAccess),
    )
    .unwrap();
    assert_eq!(
        repo.task_inherited_sandbox(&task).unwrap(),
        Some(yi_agent_core::InheritedSandbox::DangerFullAccess)
    );

    let task2 = yi_agent_core::subagent::task::TaskId::new();
    let attempt2 = yi_agent_core::subagent::task::AttemptId::new();
    repo.create_task_with_attempt_and_objective(
        &task2,
        &root,
        &attempt2,
        1,
        "queued",
        "obj",
        yi_agent_core::ChildWriteMode::Coding,
        None,
        None,
    )
    .unwrap();
    assert_eq!(repo.task_inherited_sandbox(&task2).unwrap(), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib inherited_sandbox_round_trips_and_defaults_to_none`
Expected: 编译失败（缺 `task_inherited_sandbox` / 参数不匹配）。

- [ ] **Step 3: Write minimal implementation**

1) `repository.rs:18` 改：`const LATEST_SCHEMA_VERSION: i64 = 12;`

2) `migrate()` 末尾（`if current_version < 11 { ... }` 之后、`Ok(())` 之前）追加：

```rust
    if current_version < 12 {
        let transaction = connection.unchecked_transaction()?;
        let has_column = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('tasks')
                WHERE name = 'inherited_sandbox'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_column {
            // NULL means "no inheritance recorded": the worker factory falls
            // back to its configured sandbox, preserving pre-change behavior.
            transaction.execute_batch("ALTER TABLE tasks ADD COLUMN inherited_sandbox TEXT;")?;
        }
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (12)", [])?;
        transaction.commit()?;
    }
```

3) `PersistedRecoveredTask`（`:345`）在 `workspace_mode` 之后加：

```rust
    pub inherited_sandbox: Option<InheritedSandbox>,
```

4) 两个 INSERT 方法各加末位参数 `inherited_sandbox: Option<InheritedSandbox>`，并把 SQL 列与占位符扩展为 `..., workspace_mode, model, inherited_sandbox) VALUES (...?10)`。以 `create_child_task_with_attempt_and_objective` 为例，改后：

```rust
        transaction.execute(
            "INSERT INTO tasks (id, root_session_id, parent_id, depth, state_json, contract_version, active_attempt_id, delivery_json, workspace_mode, model, inherited_sandbox)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10)",
            params![
                task.to_string(),
                root.to_string(),
                parent.to_string(),
                depth,
                state,
                attempt.to_string(),
                delivery_json,
                workspace_mode.as_str(),
                model,
                inherited_sandbox.map(|value| value.as_str()),
            ],
        )?;
```

`create_task_with_attempt_and_objective` 同理（parent 为 `NULL`，参数序号相应调整）。**同时更新所有内部调用方**：`create_child_task_with_attempt`（legacy wrapper）传 `None`；其余调用点在 Task 4 统一改。

5) 新增读取：

```rust
    /// Reads a task's inherited sandbox. `None` means no inheritance was
    /// recorded (old rows, or a task spawned before this feature).
    pub fn task_inherited_sandbox(
        &self,
        task: &TaskId,
    ) -> Result<Option<InheritedSandbox>, RepositoryError> {
        let value = self
            .connection
            .query_row(
                "SELECT inherited_sandbox FROM tasks WHERE id = ?1",
                params![task.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match value {
            Some(value) => InheritedSandbox::parse(&value).map(Some).ok_or_else(|| {
                RepositoryError::UnknownEventKind {
                    kind: format!("invalid inherited_sandbox in store: {value}"),
                }
            }),
            None => Ok(None),
        }
    }
```

6) `recovered_tasks`：SELECT 列表末尾加 `tasks.inherited_sandbox`，`row.get` 元组加一个 `Option<String>`（索引顺延），并在 `PersistedRecoveredTask { ... }` 里加：

```rust
                    inherited_sandbox: inherited_sandbox
                        .as_deref()
                        .map(|value| {
                            InheritedSandbox::parse(value).ok_or_else(|| {
                                RepositoryError::UnknownEventKind {
                                    kind: format!(
                                        "invalid inherited_sandbox in store: {value}"
                                    ),
                                }
                            })
                        })
                        .transpose()?,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib`
Expected: PASS（含新用例与既有迁移/往返用例）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent-store/src/repository.rs
git commit -m "feat(store): persist and recover inherited sandbox on tasks (schema v12)"
```

---

### Task 4: store — IPC 字段、解析与协调器透传

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`SpawnChild:179`、`SpawnApplicationChild:190`、handler `:2511/:2538`、`parse_workspace_mode:2994` 旁）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`create_session_with_objective:643`、`create_session_with_objective_and_mode:653`、`spawn_application_child:1021`、`spawn_child_with_objective:1153`、`spawn_child_and_admit:1255`、恢复路径 `:400-410`/`:507`/`:1315`）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`、`yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: Task 3 的 `task_inherited_sandbox` 与新增参数。
- Produces:
  - IPC `SpawnChild` / `SpawnApplicationChild` 新增 `#[serde(default)] sandbox: Option<String>`
  - `parse_inherited_sandbox(Option<String>) -> Result<Option<InheritedSandbox>, IpcError>`
  - 协调器方法新增 `inherited_sandbox: Option<InheritedSandbox>` 参数（`create_session_with_objective_and_mode` 新增末位参数，兼容 wrapper 传 `None`）

- [ ] **Step 1: Write the failing test**

在 `tests/runtime_ipc.rs` 追加（沿用该文件既有的 daemon/IPC helper）：

```rust
#[test]
fn spawn_child_rejects_invalid_inherited_sandbox() {
    // 复用本文件既有的 send_request helper 与一个已 attach 的 root。
    let response = send_spawn_child_with_sandbox(Some("read_only".into())); // 注意：snake_case 非法
    assert!(
        matches!(response, IpcResponse::Error { .. }),
        "snake_case must be rejected; got {response:?}"
    );
}

#[test]
fn spawn_child_accepts_and_persists_inherited_sandbox() {
    let response = send_spawn_child_with_sandbox(Some("danger-full-access".into()));
    let IpcResponse::TaskSpawned { task_id } = response else {
        panic!("expected TaskSpawned, got {response:?}");
    };
    // 通过存储读取校验落库（复用既有的 repo 句柄或查询）。
    assert_eq!(
        read_task_inherited_sandbox(&task_id),
        Some(yi_agent_core::InheritedSandbox::DangerFullAccess)
    );
}
```

（若本文件没有可直接复用的 helper，则把这两个用例改写成该文件既有 `SpawnChild` 用例的同构版本，只改 `mode`/`sandbox` 字段。）

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc spawn_child_`
Expected: 编译失败（IPC 无 `sandbox` 字段）。

- [ ] **Step 3: Write minimal implementation**

1) `ipc.rs` 两个请求变体各加字段（`workdir` 之后）：

```rust
        #[serde(default)]
        sandbox: Option<String>,
```

2) handler（`:2511` / `:2538`）在 `let workspace_mode = parse_workspace_mode(mode)?;` 之后加：

```rust
            let inherited_sandbox = parse_inherited_sandbox(sandbox)?;
```

并把 `inherited_sandbox` 传入协调器调用（`spawn_child_and_admit` / `spawn_application_child`）。

3) 在 `parse_workspace_mode` 之后加：

```rust
/// Resolves the optional inherited sandbox carried by a spawn request. An
/// omitted value means "no inheritance"; an explicit but unknown value is
/// rejected (`invalid_params` at the boundary).
fn parse_inherited_sandbox(
    sandbox: Option<String>,
) -> Result<Option<yi_agent_core::InheritedSandbox>, IpcError> {
    match sandbox.as_deref() {
        None => Ok(None),
        Some(value) => yi_agent_core::InheritedSandbox::parse(value)
            .map(Some)
            .ok_or_else(|| {
                IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "sandbox must be 'read-only', 'workspace-write', or 'danger-full-access', got {value}"
                    ),
                ))
            }),
    }
}
```

4) `runtime.rs` 协调器签名各加末位参数 `inherited_sandbox: Option<yi_agent_core::InheritedSandbox>`：

- `create_session_with_objective_and_mode(objective, workspace_mode, inherited_sandbox)`；`create_session_with_objective` wrapper 传 `None`。在函数体内 `supervisor.set_workspace_mode(&root_id, workspace_mode);` 之后，若有 `Some` 则 `supervisor.set_inherited_sandbox(&root_id, value);`；`create_task_with_attempt_and_objective(...)` 调用补末位参数。
- `spawn_child_with_objective(..., inherited_sandbox)`：在 `supervisor.spawn_with_objective(...)` 之后、`create_child_task_with_attempt_and_objective(...)` 之前，`supervisor.lock().await.set_inherited_sandbox(&child, value)`（若 `Some`）；INSERT 补末位参数。
- `spawn_child_and_admit` 与 `spawn_application_child`：透传该参数。

5) 恢复路径：`runtime.rs:400-410` / `:507` 一带读取 `recovered.workspace_mode` 处，同时读 `recovered.inherited_sandbox` 并 `supervisor.set_inherited_sandbox(&task_id, value)`（若 `Some`），确保重启后 coding child 仍继承。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc && cargo test -p yi-agent-store --test runtime_coordinator`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/tests
git commit -m "feat(store): carry inherited sandbox across spawn IPC and recovery"
```

---

### Task 5: subagent — 解析、clamp 与两个 spawn 工具下发

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（factory 字段与 `with_sandbox:88`、`worker_tool_registry:126`、`start_with_provider_turn_gate` 组装 `:529`/`:556`、`DaemonSpawnAgentTool:1207`/`impl:1589`、`DaemonApplicationSpawnAgentTool:1213`/`impl:1259`、`register_application_subagent_tools:1111`、`register_attached_root_tools:1097`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 既有 `#[cfg(test)] mod tests`（`:2165` 一带）

**Interfaces:**
- Consumes: `WorkerStart.inherited_sandbox`（Task 2）、`yi_agent_tools::{SandboxController, SandboxMode}`。
- Produces:
  - `fn sandbox_mode_from(inherited: InheritedSandbox) -> SandboxMode`
  - `fn resolve_effective_sandbox(base: SandboxMode, inherited: Option<InheritedSandbox>) -> SandboxMode`（clamp ≥ `WorkspaceWrite`）
  - `fn sandbox_mode_str(mode: SandboxMode) -> &'static str`
  - `DaemonAgentWorkerFactory::worker_tool_registry(&self, workspace: &WorkerWorkspace, mode: ChildWriteMode, inherited: Option<InheritedSandbox>) -> ToolRegistry`
  - `DaemonSpawnAgentTool { ..., sandbox: SandboxMode }`
  - `DaemonApplicationSpawnAgentTool { ..., controller: SandboxController }`
  - `register_application_subagent_tools(registry, runtime_socket, session_id, caller_task_id, capability, controller: SandboxController)`
  - `register_attached_root_tools(registry, runtime_socket, root, controller: SandboxController)`

- [ ] **Step 1: Write the failing test**

在 `lib.rs` 测试模块追加：

```rust
#[test]
fn effective_sandbox_clamps_read_only_up_to_workspace_write() {
    use yi_agent_tools::SandboxMode;
    assert_eq!(
        resolve_effective_sandbox(SandboxMode::WorkspaceWrite, Some(yi_agent_core::InheritedSandbox::ReadOnly)),
        SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        resolve_effective_sandbox(SandboxMode::WorkspaceWrite, Some(yi_agent_core::InheritedSandbox::DangerFullAccess)),
        SandboxMode::DangerFullAccess
    );
    assert_eq!(
        resolve_effective_sandbox(SandboxMode::WorkspaceWrite, None),
        SandboxMode::WorkspaceWrite
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib effective_sandbox_clamps`
Expected: 编译失败（`resolve_effective_sandbox` 未定义）。

- [ ] **Step 3: Write minimal implementation**

在 `lib.rs`（`git_writable_roots_for_worktree` 附近，`fn` 区域）加：

```rust
fn sandbox_mode_from(inherited: yi_agent_core::InheritedSandbox) -> yi_agent_tools::SandboxMode {
    use yi_agent_core::InheritedSandbox;
    use yi_agent_tools::SandboxMode;
    match inherited {
        InheritedSandbox::ReadOnly => SandboxMode::ReadOnly,
        InheritedSandbox::WorkspaceWrite => SandboxMode::WorkspaceWrite,
        InheritedSandbox::DangerFullAccess => SandboxMode::DangerFullAccess,
    }
}

/// Coding children clamp up to at least `WorkspaceWrite` so they can always
/// write their worktree and commit; a `ReadOnly` inheritance cannot make a
/// coding child unable to deliver.
fn resolve_effective_sandbox(
    base: yi_agent_tools::SandboxMode,
    inherited: Option<yi_agent_core::InheritedSandbox>,
) -> yi_agent_tools::SandboxMode {
    use yi_agent_tools::SandboxMode;
    let target = inherited.map(sandbox_mode_from).unwrap_or(base);
    match target {
        SandboxMode::ReadOnly => SandboxMode::WorkspaceWrite,
        other => other,
    }
}

fn sandbox_mode_str(mode: yi_agent_tools::SandboxMode) -> &'static str {
    use yi_agent_tools::SandboxMode;
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    }
}
```

改 `worker_tool_registry`（`:126`）签名与内部：

```rust
    fn worker_tool_registry(
        &self,
        workspace: &WorkerWorkspace,
        workspace_mode: ChildWriteMode,
        inherited: Option<yi_agent_core::InheritedSandbox>,
    ) -> ToolRegistry {
        let mut tools = (*self.tools).clone();
        let (sandbox, writable_roots) = match workspace_mode {
            ChildWriteMode::Coding => {
                let mut writable_roots = vec![workspace.path.clone()];
                writable_roots.extend(git_writable_roots_for_worktree(&workspace.path));
                (resolve_effective_sandbox(self.sandbox, inherited), writable_roots)
            }
            ChildWriteMode::ReadOnly => (yi_agent_tools::SandboxMode::ReadOnly, Vec::new()),
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

在 `start_with_provider_turn_gate`（`:529`）计算一次并复用：

```rust
        let workspace_mode = request.workspace_mode;
        let effective_sandbox = match workspace_mode {
            ChildWriteMode::Coding => {
                resolve_effective_sandbox(self.sandbox, request.inherited_sandbox)
            }
            ChildWriteMode::ReadOnly => yi_agent_tools::SandboxMode::ReadOnly,
        };
        let worker_tools = Arc::new(self.worker_tool_registry(
            &workspace,
            workspace_mode,
            request.inherited_sandbox,
        ));
```

在 `:556` 注册 `DaemonSpawnAgentTool` 时加：

```rust
            worker_tools.register(Arc::new(DaemonSpawnAgentTool {
                runtime_socket: runtime_socket.clone(),
                session_id: request.root_session_id.to_string(),
                caller_task_id: request.task_id.to_string(),
                caller_capability: request.message_capability.clone(),
                sandbox: effective_sandbox,
            }));
```

`DaemonSpawnAgentTool`（`:1207`）加字段 `sandbox: yi_agent_tools::SandboxMode`；其 `impl Tool::call`（`:1589`）在构造 `IpcRequest::SpawnChild {...}` 时加 `sandbox: spawn_sandbox(&args, self.sandbox)?`（仅 coding 携带）：

```rust
/// The `sandbox` IPC field for a spawn: only a coding child inherits the
/// caller's effective sandbox; a read-only child carries nothing.
fn spawn_sandbox(
    args: &Value,
    effective: yi_agent_tools::SandboxMode,
) -> Result<Option<String>, ToolResult> {
    match spawn_mode(args)? {
        ChildWriteMode::Coding => Ok(Some(sandbox_mode_str(effective).to_string())),
        ChildWriteMode::ReadOnly => Ok(None),
    }
}
```

`DaemonApplicationSpawnAgentTool`（`:1213`）加字段 `controller: yi_agent_tools::SandboxController`；其 `impl Tool::call`（`:1259`）构造 `IpcRequest::SpawnApplicationChild {...}` 时加：

```rust
                sandbox: spawn_sandbox(&args, self.controller.effective())?,
```

`register_application_subagent_tools`（`:1111`）加参数 `controller: yi_agent_tools::SandboxController` 并传给 `DaemonApplicationSpawnAgentTool`；`register_attached_root_tools`（`:1097`）加参数并透传。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent`
Expected: PASS（含既有 `worker_tool_registry` 用例——需同步给它们补 `None` 第三参）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): resolve, clamp and forward inherited sandbox to spawn tools"
```

---

### Task 6: runtime/app-server — 一份共享控制器同时驱动 root 工具与继承

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`（新增 `build_tool_setup_with_controller`，并让 `build_tool_setup_with_switch` 复用它）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`RuntimeTooling:61`、`build_runtime_tooling:183`、`wrap_for_delegation` 调用点 `:190-201`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 既有测试模块

**背景（本任务同时修复一处既有不一致）：** `build_runtime_tooling` 目前用 `build_tool_setup_in` 重建 root 工具集，其内部 `YoloSwitch::new(cfg.yolo)` 是**静态快照**——委派启用后 root 自己的 bash 沙箱不再跟随运行期 yolo。本任务让 root 工具集与 spawn 工具**共用同一个**基于线程实时 `YoloSwitch` 的 `SandboxController`，消除"两份真相"。

**Interfaces:**
- Consumes: Task 5 的 `register_attached_root_tools(..., controller)`。
- Produces: `bootstrap::build_tool_setup_with_controller(cfg, naked, workspace, controller) -> Result<ToolSetup>`

- [ ] **Step 1: Write the failing test**

在 `server.rs` 测试模块追加（复用该文件既有的 mock provider / runtime 注入方式）：

```rust
#[tokio::test]
async fn delegation_tooling_uses_the_threads_live_yolo_switch() {
    // 该测试断言：启用委派后，root 工具集与 spawn 工具读同一个开关。
    // 复用既有测试的 bring-up helper 拿到 thread 的 YoloSwitch 与 built agent；
    // 翻转 switch 后，registry 中 bash 工具的 sandbox 模式应变为 danger-full-access。
    let (registry, switch) = build_delegation_tooling_with_switch(); // 见既有接入方式
    switch.set(true);
    let bash = registry.get("bash").expect("bash registered");
    let _ = bash; // 断言点：registry 的 sandbox 由 switch 驱动（用可观测的 controller 断言）
    // 具体断言按本文件既有对 SandboxController 的观察手段实现。
    unimplemented!("替换为本文件既有可观测断言方式");
}
```

> 说明：本文件的既有测试用注入 factory 的方式构造 agent。若直接观察 controller 不便，则改为断言 `build_tool_setup_with_controller` 的单元行为：传入一个 `switch`，翻转后 `ToolSetup` 内 bash 的 sandbox 生效模式随之改变。

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server delegation_tooling_uses_the_threads_live_yolo_switch`
Expected: 编译失败 / FAIL。

- [ ] **Step 3: Write minimal implementation**

1) `bootstrap.rs`：抽出：

```rust
pub fn build_tool_setup_with_switch(
    cfg: &RuntimeConfig,
    naked: bool,
    workspace: &Path,
    switch: yi_agent_core::autonomy::YoloSwitch,
) -> Result<ToolSetup> {
    let controller =
        yi_agent_tools::SandboxController::new(switch, cfg.sandbox, cfg.sandbox_promotable);
    build_tool_setup_with_controller(cfg, naked, workspace, controller)
}

/// Same as [`build_tool_setup_with_switch`], but with an externally-owned
/// controller so the caller can share one controller with other tools (e.g.
/// the subagent spawn tools) and with the live YOLO switch.
pub fn build_tool_setup_with_controller(
    cfg: &RuntimeConfig,
    naked: bool,
    workspace: &Path,
    controller: yi_agent_tools::SandboxController,
) -> Result<ToolSetup> {
    if naked {
        return Ok(ToolSetup {
            tools: Arc::new(yi_agent_core::ToolRegistry::new()),
            catalog: None,
            system_prompt: None,
            mcp: None,
        });
    }
    let mut registry = yi_agent_core::ToolRegistry::new();
    let prompt = build_prompt_setup(cfg)?;
    if let Some(svc) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }
    yi_agent_tools::register_builtin_tools_with_controller(
        &mut registry,
        workspace.to_path_buf(),
        controller.clone(),
        cfg.sandbox_writable_roots.clone(),
    );
    let process_manager = yi_agent_tools::ProcessManager::with_controller(
        workspace.to_path_buf(),
        controller,
        cfg.sandbox_writable_roots.clone(),
    );
    // ...（把原 `build_tool_setup_with_switch` 余下逻辑原样搬到此函数）
    Ok(ToolSetup { /* ...原样... */ })
}
```

（实施时保持原函数体逻辑逐行不变，仅拆分出 controller 参数化的入口；把 `process_manager` 等仍需要的部分完整搬入。）

2) `server.rs` `build_runtime_tooling`（`:183`）改为先建共享 controller：

```rust
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    attached: &yi_agent_subagent::attach::AttachedProjectRuntime,
    yolo: yi_agent_core::autonomy::YoloSwitch,
) -> Result<RuntimeTooling, String> {
    // One controller, one truth: it backs the root's builtin tools AND the
    // subagent spawn tools, and it reads the thread's live YOLO switch.
    let controller = yi_agent_tools::SandboxController::new(
        yolo.clone(),
        cfg.sandbox,
        cfg.sandbox_promotable,
    );
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &attached.workspace_root,
        controller.clone(),
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    yi_agent_subagent::register_attached_root_tools(
        &mut registry,
        attached.socket_path.clone(),
        &attached.attached_root,
        controller,
    );
    let permission = yi_agent_runtime::bootstrap::load_permission_checker_with_switch(
        &attached.workspace_root,
        yolo,
    )
    .map_err(|error| error.to_string())?;
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent-runtime/src/bootstrap.rs crates/yi-agent-app-server/src/server.rs
git commit -m "fix(app-server): back root and subagent spawn tools with one live sandbox controller"
```

---

### Task 7: CLI/TUI — 传控制器，保持两路径一致

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`register_attached_root_tools` 调用点 `:616`、`:721`）
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`（若 TUI 侧工厂/工具集在此装配）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`（`:28` re-export、`:116` 测试调用）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs` 既有测试

**Interfaces:**
- Consumes: `register_attached_root_tools(..., controller)`（Task 5）、`SandboxController`。
- Produces: TUI 侧 root 委派工具同样携带基于启动时 `cfg.yolo` 的 controller。

- [ ] **Step 1: Write the failing test**

在 `tui/subagents.rs` 测试模块更新既有调用并断言 controller 生效：

```rust
#[test]
fn tui_registers_delegation_tools_with_a_controller() {
    use yi_agent_core::autonomy::YoloSwitch;
    use yi_agent_tools::{SandboxController, SandboxMode};
    let switch = YoloSwitch::new(true);
    let controller = SandboxController::new(switch, SandboxMode::WorkspaceWrite, true);
    assert_eq!(controller.effective(), SandboxMode::DangerFullAccess);
    // 调用 register_attached_root_tools 时传入该 controller（编译即证明签名接线）。
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui_registers_delegation_tools_with_a_controller`
Expected: 编译失败（调用点缺参数）。

- [ ] **Step 3: Write minimal implementation**

在 `main.rs:616` 与 `:721` 两处 `register_attached_root_tools(...)` 调用前构造 controller（用该路径已持有的 `cfg`）：

```rust
    let delegation_controller = yi_agent_tools::SandboxController::new(
        yi_agent_core::autonomy::YoloSwitch::new(config.yolo),
        config.sandbox,
        config.sandbox_promotable,
    );
    crate::tui::subagents::register_attached_root_tools(
        &mut registry,
        socket_path.clone(),
        &attached_root,
        delegation_controller,
    );
```

（`config` / `registry` / `socket_path` / `attached_root` 用该调用点既有变量名；两处同样处理。）`tui/subagents.rs:116` 的测试调用补一个 `SandboxController::new(YoloSwitch::new(false), SandboxMode::WorkspaceWrite, false)`。

- [ ] **Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::subagents`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && git add crates/yi-agent/src/main.rs crates/yi-agent/src/tui/subagents.rs crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat(tui): pass a sandbox controller to delegation tool registration"
```

---

### Task 8: 文档同步

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/project-management/permission.md`（若已登记 yolo 条目，补一句子任务传播）

- [ ] **Step 1: 更新 `subagent-runtime.md`**

新增一条 feature 行，含代码位置与验证命令，例如：

```markdown
- [x] coding 子任务继承父的有效沙箱 — spawn 时快照、逐层下传、`read_only` 恒只读 —
  `crates/yi-agent-core/src/subagent/task.rs`（`InheritedSandbox`）、
  `crates/yi-agent-store/src/repository.rs`（schema v12 `inherited_sandbox`）、
  `crates/yi-agent-subagent/src/lib.rs`（`resolve_effective_sandbox` clamp）；
  验证：`cargo test -p yi-agent-core --lib inherited_sandbox_tests`、
  `cargo test -p yi-agent-store --lib inherited_sandbox_round_trips_and_defaults_to_none`、
  `cargo test -p yi-agent-subagent effective_sandbox_clamps_read_only_up_to_workspace_write`
  — [设计](../superpowers/specs/2026-10-01-subagent-yolo-sandbox-inheritance-design.md)
```

- [ ] **Step 2: 更新 `README.md` 计数**

把 `subagent-runtime` 模块的「完成 / 总计」计数按新增条目 +1 同步。

- [ ] **Step 3: 更新 `permission.md`**

在 yolo 条目下补一句：YOLO 开启后，其派发的 `coding` 子任务同样以 `DangerFullAccess` 运行；`read_only` 子任务不受影响。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/subagent-runtime.md docs/project-management/README.md docs/project-management/permission.md
git commit -m "docs: record coding subagent sandbox inheritance and YOLO propagation"
```

---

## Self-Review

**1. Spec coverage：**
- 语义规则（编码收敛）→ Task 5 `resolve_effective_sandbox`。
- `InheritedSandbox` 类型（core）→ Task 1。
- IPC 字段 + 解析 → Task 4。
- tasks 列 + 落库 + 恢复 → Task 3。
- supervisor per-task map + WorkerStart → Task 2。
- factory 解析 + clamp + 两个 spawn 工具下发 + D1 逐层 → Task 5。
- app-server/TUI 装配共享控制器 → Task 6 / Task 7。
- 错误处理（非法值 / 缺省 / 旧行）→ Task 3（`None` 回退）、Task 4（`invalid_params`）。
- 文档 → Task 8。
- **发现并纳入**：`build_runtime_tooling` 静态开关的既有不一致 → Task 6（消除"两份真相"风险，spec §8 已列为风险）。

**2. Placeholder scan：** Task 6/7 的测试给出了断言意图与编译级接线证明，但个别观察手段需按该文件既有 helper 落地（已注明"复用本文件既有方式"）。实施者在这些点应直接沿用相邻用例的接入写法，不得留 `unimplemented!` 提交。

**3. Type consistency：** `InheritedSandbox`（core）↔ 字符串（IPC/DB）↔ `SandboxMode`（tools）的转换集中在 Task 5 的 `sandbox_mode_from` / `resolve_effective_sandbox` / `sandbox_mode_str`；`worker_tool_registry` 第三参类型在各任务中一致为 `Option<InheritedSandbox>`；`register_attached_root_tools` 末参在 Task 5/6/7 一致为 `SandboxController`。

## 未决 / 需用户裁定

- **Task 6 属扩展范围**：它修复一处既有不一致（委派启用后 root bash 沙箱不跟随运行期 YOLO）。这是设计过程中发现的问题，纳入是因为它直接决定"父的有效沙箱"是否存在两份真相。若你希望严格控制范围，可去掉 Task 6 中的 `build_tool_setup_with_controller` 收口，仅保留 spawn 工具用线程实时 `YoloSwitch` 构造 controller（此时 root 自身 bash 仍为静态，继承语义依旧成立）。
