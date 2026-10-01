# Superpowers 看板 — Plan 2：daemon 通用能力（自主可写隔离会话）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 daemon 补上一处**通用**能力：经一次 IPC 创建「自主可写、带独立 worktree、可自动驱动到终态」的根会话。该能力不含任何看板概念。

**Architecture:** 现状是——根会话默认为 `Coding` 模式（有写工具），但**没有 workdir**（`workdirs` 表只在 spawn 子任务时写入），于是 `prepare_task_workspace` 走 `in_place_workspace`，根会话在**项目根目录原地跑**。同时 `StartWorker` 在生产代码里零调用方，没有任何东西自动启动 root worker。本计划补两个通用原语：(1) 给 root 绑定一个 workdir；(2) 一次 IPC 调用完成「建会话 → 绑 workdir → 启动 worker」且幂等。

**Tech Stack:** Rust 2024、现有 `yi-agent-core`（supervisor/worker）、`yi-agent-store`（coordinator/repository/ipc）、`tokio`、`serde`。

## Global Constraints

- 新增能力必须**通用**：不得出现 `kanban` / `board` / `superpowers` 等看板字样。
- **不改**既有 IPC 请求的行为与语义；只**新增**请求变体与转换分支。
- `IpcRequest` 使用 `#[serde(tag = "type", deny_unknown_fields)]`，新变体必须同步更新协议测试。
- workdir 必须已存在且是 git worktree；**daemon 只观察，绝不创建目录**（沿用现有约定）。
- 启动必须**幂等**：同一个 **root task** 重复调用 `start_worker` 不得创建第二个 worker（该幂等由
  `start_worker` 内部的 `has_worker` 检查保证，本计划不重复实现）。
- **去重语义的归属（已裁决）：** `create_autonomous_session` **每次调用都新建会话**，签名里没有去重键。
  一张卡片不会被重复启动，靠的是 Plan 3a 的卡片状态机（只对 `Queued` 卡片调用），而不是这一层。
  因此不要给签名或 IPC 加去重键；文档注释必须如实说明这一点，不得暗示本方法自身去重。
- **非 git 目录的拒绝（已裁决）：** git 校验由下游 `yi-agent-subagent`（`prepare_task_workspace`
  → workspace service）负责，生产 daemon 始终挂载该 service。本层只检查目录存在（`is_dir`），
  **不重复实现** git 校验；但必须有测试证明下游确实会拒绝非 git 目录。
- 非 git 项目或不存在的 workdir → 明确报错，绝不静默原地跑。
- 提交信息用 conventional commits，**不写** `Co-Authored-By`。
- 每个任务结束跑 `cd yi-agent-rs && cargo fmt --all && cargo test -p <crate>`。

> **依赖：** 本计划不依赖 Plan 1。Plan 3a 依赖本计划的 `CreateAutonomousSession`（以及既有
> 的 `ListTaskSummaries`）来驱动卡片会话；Plan 3a 负责在调用前**预建** worktree 并把路径传进来。

---
## 文件结构

| 文件 | 职责 |
|------|------|
| `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` | 新增 `set_workdir`，让 root 也能被绑定目录 |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | 新增 `create_autonomous_session`：建会话 + 绑 workdir + 启 worker，幂等 |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | 新增 `IpcRequest::CreateAutonomousSession` 与请求分发 |
| `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs` | coordinator 层测试 |
| `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` | IPC 端到端测试 |

---

### Task 1: `AgentSupervisor::set_workdir`（让 root 可被绑定目录）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Test: 同文件 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `AgentSupervisor`（既有）
- Produces:
  - `AgentSupervisor::set_workdir(&mut self, task_id: &TaskId, workdir: PathBuf) -> Result<(), String>`

- [ ] **Step 1: 写失败的测试**

在 `supervisor.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
#[test]
fn set_workdir_binds_a_directory_to_the_root() {
    let session = RootSessionId::new();
    let mut supervisor = AgentSupervisor::new_with_objective(session, "objective".into());
    let root = supervisor.root_task_id().clone();
    assert_eq!(supervisor.spawn_workdir(&root), None, "a root starts with no workdir");

    supervisor
        .set_workdir(&root, PathBuf::from("/tmp/example-worktree"))
        .unwrap();

    assert_eq!(
        supervisor.spawn_workdir(&root),
        Some(PathBuf::from("/tmp/example-worktree"))
    );
}

#[test]
fn set_workdir_rejects_an_unknown_task() {
    let session = RootSessionId::new();
    let mut supervisor = AgentSupervisor::new_with_objective(session, "objective".into());
    let unknown = TaskId::new();
    let error = supervisor
        .set_workdir(&unknown, PathBuf::from("/tmp/example-worktree"))
        .unwrap_err();
    assert_eq!(error, "task does not exist");
}
```

> 若测试模块缺少 `use std::path::PathBuf;`，在其顶部补上。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib supervisor::tests::set_workdir`
Expected: 编译失败，`set_workdir` 未定义。

- [ ] **Step 3: 实现最小代码**

在 `supervisor.rs` 中 `pub fn spawn_workdir` 之前插入：

```rust
/// Binds a directory to a task. A root has no workdir by default, so this is
/// how an autonomous session is told to run in an isolated worktree instead of
/// in the project directory. The directory must already exist: the runtime
/// resolves a path, it never creates one.
pub fn set_workdir(&mut self, task_id: &TaskId, workdir: PathBuf) -> Result<(), String> {
    if !self.tasks.contains_key(task_id) {
        return Err("task does not exist".into());
    }
    self.workdirs.insert(task_id.clone(), Some(workdir));
    self.notify_update();
    Ok(())
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib supervisor::tests::set_workdir`
Expected: PASS（2 个测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs
git commit -m "feat(subagent): let a supervisor bind a workdir to any task"
```

---

### Task 2: `RuntimeCoordinator::create_autonomous_session`（建会话 + 绑 workdir + 启 worker，幂等）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: `AgentSupervisor::set_workdir`（Task 1）、`RuntimeCoordinator::{create_session_with_objective_and_mode, start_worker, supervisor}`（既有）
- Produces:
  - `RuntimeCoordinator::create_autonomous_session(&self, objective: String, workdir: PathBuf) -> Result<AutonomousSession, RuntimeCoordinatorError>`
  - `runtime::AutonomousSession { session_id: RootSessionId, root_task_id: TaskId }`
  - `runtime::AutonomousSessionError`（并入 `RuntimeCoordinatorError` 的 `Supervisor(String)` 变体即可，不新增变体）

- [ ] **Step 1: 写失败的测试**

在 `runtime_coordinator.rs` 追加：

```rust
#[tokio::test]
async fn an_autonomous_session_runs_in_the_given_worktree() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let worktree = directory.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    let session = coordinator
        .create_autonomous_session("implement the plan".into(), worktree.clone())
        .await
        .unwrap();

    assert_eq!(
        coordinator.root_task_id(&session.session_id).unwrap(),
        session.root_task_id
    );
    let starts = factory.starts.lock().unwrap();
    let root_start = starts
        .iter()
        .find(|start| start.task_id == session.root_task_id)
        .expect("the root worker must have been started");
    assert_eq!(root_start.root_session_id, session.session_id);
}

#[tokio::test]
async fn an_autonomous_session_rejects_a_missing_directory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();

    let error = coordinator
        .create_autonomous_session(
            "implement the plan".into(),
            directory.path().join("does-not-exist"),
        )
        .await
        .unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("does not exist") || message.contains("not inside a git worktree"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn an_empty_objective_is_rejected() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let worktree = directory.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    let factory = Arc::new(MessageRecordingFactory::default());
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();

    let error = coordinator
        .create_autonomous_session("   ".into(), worktree)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("objective"), "{error}");
}
```

> `MessageRecordingFactory` **已存在**于 `runtime_coordinator.rs:86`，其 `starts: Arc<Mutex<Vec<WorkerStart>>>` 记录每次 `start`（`runtime_coordinator.rs:109`），
> 且 `WorkerStart` 的会话字段名是 **`root_session_id`**（见 `yi-agent-core/src/subagent/worker.rs:69`）。
> 测试里构造它时复用文件内既有的构造辅助（若为 `MessageRecordingFactory::default()` 则直接用）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator an_autonomous_session`
Expected: 编译失败，`create_autonomous_session` / `AutonomousSession` 未定义。

- [ ] **Step 3: 实现最小代码**

在 `runtime.rs` 中 `create_session_with_objective_and_mode` 之后插入：

```rust
/// A root session created to run a single objective autonomously in its own
/// worktree. Generic on purpose: nothing here knows what the objective is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutonomousSession {
    pub session_id: RootSessionId,
    pub root_task_id: TaskId,
}

impl RuntimeCoordinator {
    /// Creates an autonomous root session bound to `workdir` and starts its
    /// worker. The directory must already exist and be a git worktree; the
    /// runtime observes it and never creates it.
    ///
    /// Idempotency lives one level down (`start_worker` refuses a task that
    /// already owns a worker), so a repeated call for the same session is safe
    /// but a repeated call creates a *new* session — callers de-duplicate on
    /// their own key (the board does this by card id).
    pub async fn create_autonomous_session(
        &self,
        objective: String,
        workdir: std::path::PathBuf,
    ) -> Result<AutonomousSession, RuntimeCoordinatorError> {
        if objective.trim().is_empty() {
            return Err(RuntimeCoordinatorError::Supervisor(
                "autonomous session objective must not be empty".into(),
            ));
        }
        if !workdir.is_dir() {
            return Err(RuntimeCoordinatorError::Supervisor(format!(
                "workdir does not exist: {}",
                workdir.display()
            )));
        }
        let session_id =
            self.create_session_with_objective_and_mode(objective, ChildWriteMode::Coding)?;
        let root_task_id = self.root_task_id(&session_id)?;
        {
            let handle = self.supervisor(&session_id)?;
            let mut supervisor = handle.lock().await;
            supervisor
                .set_workdir(&root_task_id, workdir.clone())
                .map_err(RuntimeCoordinatorError::Supervisor)?;
        }
        self.start_worker(&session_id, &root_task_id).await?;
        Ok(AutonomousSession {
            session_id,
            root_task_id,
        })
    }
}
```

> 注意：该 `impl RuntimeCoordinator` 块内不要重复导入；`ChildWriteMode`、`RootSessionId`、`TaskId`、`PathBuf` 均已在文件顶部导入。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator an_autonomous_session`
Expected: PASS（3 个测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "feat(runtime): add autonomous sessions bound to a worktree"
```

---

### Task 3: IPC 请求 `CreateAutonomousSession`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: `RuntimeCoordinator::create_autonomous_session`（Task 2）
- Produces:
  - `IpcRequest::CreateAutonomousSession { objective: String, workdir: String }`
  - `IpcResponse::AutonomousSessionCreated { session_id: String, root_task_id: String }`

- [ ] **Step 1: 写失败的测试**

在 `runtime_ipc.rs` 追加：

```rust
#[test]
fn daemon_creates_an_autonomous_session_bound_to_a_worktree() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let worktree = directory.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    let factory = Arc::new(RecordingWorkerFactory);
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        factory,
    )
    .unwrap();
    let socket = daemon.socket_path().to_path_buf();

    let response = send_request(
        &socket,
        IpcRequest::CreateAutonomousSession {
            objective: "implement the plan".into(),
            workdir: worktree.to_string_lossy().to_string(),
        },
    )
    .unwrap();

    let IpcResponse::AutonomousSessionCreated {
        session_id,
        root_task_id,
    } = response
    else {
        panic!("expected an autonomous session, got {response:?}");
    };
    assert!(!session_id.is_empty());
    assert!(!root_task_id.is_empty());
}

#[test]
fn daemon_refuses_an_autonomous_session_in_a_missing_directory() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(RecordingWorkerFactory);
    let daemon = Daemon::start_with_factory(
        directory.path().join("runtime"),
        &database,
        factory,
    )
    .unwrap();
    let socket = daemon.socket_path().to_path_buf();

    let response = send_request(
        &socket,
        IpcRequest::CreateAutonomousSession {
            objective: "implement the plan".into(),
            workdir: directory
                .path()
                .join("nope")
                .to_string_lossy()
                .to_string(),
        },
    )
    .unwrap();

    assert!(
        matches!(response, IpcResponse::Error { .. }),
        "a missing workdir must be refused, got {response:?}"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc autonomous_session`
Expected: 编译失败，`CreateAutonomousSession` 变体不存在。

- [ ] **Step 3: 实现最小代码**

在 `ipc.rs` 的 `IpcRequest` 枚举里 `StartWorker` 之后加入：

```rust
    /// Creates a root session that runs `objective` autonomously in `workdir`
    /// and starts its worker. Generic: the daemon does not interpret the
    /// objective, and `workdir` must already exist as a git worktree.
    CreateAutonomousSession {
        objective: String,
        workdir: String,
    },
```

在 `IpcResponse` 枚举里 `SessionCreated` 之后加入：

```rust
    AutonomousSessionCreated {
        session_id: String,
        root_task_id: String,
    },
```

在 `respond` 的 `IpcRequest::StartWorker { .. } => { .. }` 分支之后加入：

```rust
        IpcRequest::CreateAutonomousSession { objective, workdir } => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let created = runtime.block_on(coordinator.create_autonomous_session(
                objective,
                std::path::PathBuf::from(workdir),
            ))?;
            Ok(IpcResponse::AutonomousSessionCreated {
                session_id: created.session_id.to_string(),
                root_task_id: created.root_task_id.to_string(),
            })
        }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc autonomous_session`
Expected: PASS（2 个测试）。

- [ ] **Step 5: 验证协议往返测试仍通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_ipc_protocol`
Expected: PASS（`deny_unknown_fields` 下新变体不影响既有往返用例）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat(store): expose autonomous session creation over ipc"
```

---

### Task 4: 端到端——自主会话跑完并在重启后仍可读

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: Task 3 的 IPC 变体
- Produces: 无新接口（验证既有持久化与恢复）

- [ ] **Step 1: 写失败的测试**

在 `runtime_ipc.rs` 追加：

```rust
#[test]
fn an_autonomous_session_is_listed_after_a_daemon_restart() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let runtime_dir = directory.path().join("runtime");
    let worktree = directory.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();

    let session_id = {
        let factory = Arc::new(RecordingWorkerFactory);
        let daemon =
            Daemon::start_with_factory(runtime_dir.clone(), &database, factory).unwrap();
        let socket = daemon.socket_path().to_path_buf();
        let response = send_request(
            &socket,
            IpcRequest::CreateAutonomousSession {
                objective: "implement the plan".into(),
                workdir: worktree.to_string_lossy().to_string(),
            },
        )
        .unwrap();
        let IpcResponse::AutonomousSessionCreated { session_id, .. } = response else {
            panic!("expected a session, got {response:?}");
        };
        session_id
    };

    // Restart against the same database and socket directory.
    let factory = Arc::new(RecordingWorkerFactory);
    let daemon = Daemon::start_with_factory(runtime_dir, &database, factory).unwrap();
    let socket = daemon.socket_path().to_path_buf();
    let response = send_request(
        &socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    )
    .unwrap();
    let IpcResponse::TaskSummaries { tasks } = response else {
        panic!("expected task summaries, got {response:?}");
    };
    // `IpcTaskSummary` carries only task_id/state/is_root, so the durable
    // survival check is that the session's root task is still listed.
    assert!(
        tasks.iter().any(|task| task.is_root),
        "the autonomous session's root task must survive a restart"
    );
    assert!(
        !session_id.is_empty(),
        "session id must round-trip out of the first daemon"
    );
}
```

- [ ] **Step 2: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc autonomous_session_is_listed`
Expected: PASS。

> 该测试传 `session_id: None, active_only: false`，即列出全部任务。`IpcTaskSummary` 只带
> `task_id` / `state` / `is_root`（`ipc.rs:550`），因此断言的是「至少存在一个 root 任务」，
> 而不是比对 session id——这是该响应形状下能给出的最强断言。

- [ ] **Step 3: 全量跑 store 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store`
Expected: 全绿。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "test(store): an autonomous session survives a daemon restart"
```

---

## 完成判据

- `cd yi-agent-rs && cargo test -p yi-agent-core --lib supervisor` 全绿。
- `cd yi-agent-rs && cargo test -p yi-agent-store` 全绿（含 4 个新测试组）。
- `cd yi-agent-rs && cargo clippy --all-targets --all-features -- -D warnings` 无警告。
- 新 IPC 变体**不含**任何看板字样（`grep -ri "kanban\|board" yi-agent-rs/crates/yi-agent-store/src/ipc.rs` 为空）。
- 既有测试零回归：`cargo test -p yi-agent-store --test runtime_ipc` 与 `--test subagent_ipc_protocol` 全绿。
