# 子 Agent TUI MVP 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在现有 TUI 中跑通自然语言委派、实时子任务卡片、真实 commit delivery，以及接受/返工/拒绝的完整可用闭环。

**Architecture:** 前台 TUI Agent 通过带 capability 的 typed IPC 挂接为 daemon 的持久化根任务；daemon 继续拥有任务状态、子 worker、SQLite 和资源调度。应用层注入受信任的 Git workspace service，为根任务和子任务创建独立 worktree，并在审核确认后只向直接父 worktree 集成固定 commit。TUI 订阅 runtime snapshot/event，只维护只读 projection，并把它渲染为不进入 LLM 上下文的结构化 history cell。

**Tech Stack:** Rust、Tokio、Ratatui/Crossterm、Unix-domain IPC、SQLite/rusqlite、Git worktree、现有 `Agent`/`AgentSupervisor`/`WorktreeService`。

---

## 计划约束

- 只在 `.worktrees/feat-subagent-core` 和 `feat/subagent-core` 工作，绝不 merge `main`。
- 每个检查点由 fresh implementer 按 TDD 完成并 commit；随后依次通过 fresh specification review、另一名 fresh code-quality review，以及父 Agent 验证。
- 每次 Cargo 命令前运行 `ps aux | rg '[c]argo|[r]ustc|yi_agent' || true`。
- Cargo 命令串行执行；不运行 `cargo test --workspace`。
- 每次 commit 前在 `yi-agent-rs/` 运行 `cargo fmt --all`、`just fmt-check` 和 `git diff --check`。
- MVP 完成前不更新 `docs/project-management/`；完整 milestone 验收后再统一更新。

## 文件职责

- `crates/yi-agent-core/src/subagent/worker.rs`：workspace assignment、外部根 capability、worker 权限/交付接口。
- `crates/yi-agent-core/src/subagent/supervisor.rs`：外部根挂接和 worker 权限决定投递。
- `crates/yi-agent-store/src/repository.rs`：workspace、外部根幂等键、review preview、权限和恢复的 SQLite 权威状态。
- `crates/yi-agent-store/src/runtime.rs`：workspace admission、外部根生命周期、delivery、integration 和恢复协调。
- `crates/yi-agent-store/src/ipc.rs`：版本化 attach/report/permission/review typed IPC。
- `crates/yi-agent-tools/src/worktree.rs`：根/子 worktree、固定 delivery 检查、直接父集成和安全回滚。
- `crates/yi-agent/src/subagent_runtime.rs`：应用 workspace service、每任务工具、delivery 和权限桥接。
- `crates/yi-agent/src/tui/subagents.rs`：连接、projection、任务卡片状态和 review input mode。
- `crates/yi-agent/src/tui/cell.rs`、`history.rs`、`app.rs`、`statusbar.rs`：卡片渲染、原地更新、交互和状态栏。
- `crates/yi-agent/src/main.rs`：TUI runtime bootstrap、有效 workspace 和前台根 Agent 生命周期。

### Task 1：关闭当前 review checkpoint 的两个持久化缺口

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

- [ ] **Step 1：为双重失败后的 durable resident lease 写失败测试**

```rust
#[tokio::test]
async fn failed_recovery_transition_still_releases_the_durable_resident_lease() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let factory = Arc::new(MessageRecordingFactory::default());
    let (coordinator, session, _parent, child, _delivery) =
        delivered_child_coordinator(&database, factory.clone()).await;
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER fail_rework_delivery_ack
         BEFORE UPDATE OF delivered_at ON mailbox_messages
         WHEN OLD.kind = 'rework' AND NEW.delivered_at IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected acknowledgement failure'); END;
         CREATE TRIGGER fail_recovery_transition
         BEFORE UPDATE OF state_json ON tasks
         WHEN NEW.state_json = 'recovery_required'
         BEGIN SELECT RAISE(ABORT, 'injected recovery transition failure'); END;",
    ).unwrap();

    assert!(coordinator.rework_review(&child, "preserve this feedback").await.is_err());
    let repository = RuntimeRepository::open(&database).unwrap();
    assert!(!repository.has_active_lease_prefix(&child, "resident:").unwrap());
    drop(repository);

    connection.execute_batch(
        "DROP TRIGGER fail_rework_delivery_ack;
         DROP TRIGGER fail_recovery_transition;",
    ).unwrap();
    drop(connection);
    drop(coordinator);
    let mut repository = RuntimeRepository::open(&database).unwrap();
    repository.recover_inflight_tasks().unwrap();
    drop(repository);
    let restarted = RuntimeCoordinator::open(&database, factory).unwrap();
    restarted.resume_task(&session, &child).await.unwrap();
}
```

- [ ] **Step 2：为 factory 错误证据写失败测试**

```rust
#[tokio::test]
async fn startup_failure_retains_the_concrete_factory_error() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(StartupErrorFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let task = coordinator.root_task_id(&session).unwrap();
    let attempt = RuntimeRepository::open(&database)
        .unwrap()
        .active_attempt_id(&task)
        .unwrap();

    assert!(coordinator.start_worker(&session, &task).await.is_err());
    let terminal = RuntimeRepository::open(&database)
        .unwrap()
        .attempt_terminal_json(&attempt)
        .unwrap();
    assert!(terminal.contains("provider bootstrap failed"));
}
```

- [ ] **Step 3：运行 RED**

```bash
cargo test -p yi-agent-store --test runtime_coordinator failed_recovery_transition_still_releases_the_durable_resident_lease -- --exact
cargo test -p yi-agent-store --test runtime_coordinator startup_failure_retains_the_concrete_factory_error -- --exact
```

Expected: 第一项因 `resident:*` 仍为 active 失败；第二项因 terminal JSON 未保存具体错误失败。

- [ ] **Step 4：增加窄 lease 释放接口并保留错误证据**

```rust
pub fn release_process_leases_for_task(
    &mut self,
    task: &TaskId,
) -> Result<usize, RepositoryError> {
    Ok(self.connection.execute(
        "UPDATE resource_leases
         SET state = 'released', released_at = CURRENT_TIMESTAMP
         WHERE task_id = ?1 AND state = 'active'
           AND resource_key NOT LIKE 'workspace:%'
           AND resource_key NOT LIKE 'worktree:%'",
        [task.to_string()],
    )?)
}
```

rework acknowledgement fallback 无论 `RecoveryRequired` transition 是否成功，都调用该接口；两项都失败时返回包含两项原因的 `Supervisor` 错误。factory 启动失败证据使用：

```rust
let evidence = serde_json::to_string(&serde_json::json!({
    "reason": "worker_start_failed",
    "error": error,
}))
.expect("worker startup evidence is serializable");
```

- [ ] **Step 5：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent-store --test runtime_coordinator
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent-store/src/repository.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "fix: close recovery admission persistence gaps"
```

Expected: suite 全部通过；随后完成规格复审、质量复审和父 Agent 复验。

### Task 2：定义并持久化每任务 workspace assignment

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

- [ ] **Step 1：写 workspace round-trip 和 migration RED**

```rust
#[test]
fn task_workspace_round_trips_every_git_identity_field() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let mut repository = RuntimeRepository::open(&database).unwrap();
    let (session, task, attempt) = persisted_root(&mut repository);
    let workspace = test_workspace(&session, &task);
    repository.record_task_workspace(&task, &attempt, &workspace).unwrap();
    assert_eq!(repository.task_workspace(&task).unwrap(), workspace);
}

#[test]
fn v6_database_migrates_to_workspace_and_attachment_tables() {
    let database = legacy_v6_database();
    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(repository.schema_version().unwrap(), 7);
}
```

- [ ] **Step 2：运行 RED**

```bash
cargo test -p yi-agent-store --test repository_decisions task_workspace_round_trips_every_git_identity_field -- --exact
cargo test -p yi-agent-store --test runtime_ipc v6_database_migrates_to_workspace_and_attachment_tables -- --exact
```

Expected: `WorkerWorkspace`、repository 方法和 schema v7 尚不存在。

- [ ] **Step 3：加入稳定 workspace 类型和 schema v7**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerWorkspace {
    pub lease_id: WorkspaceLeaseId,
    pub repository_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub parent_branch: String,
    pub base_commit: String,
}
```

给 `WorkerStart` 增加 `pub workspace: Option<WorkerWorkspace>`，并让 `with_workspace` 同时设置 assignment 和 `workspace_lease_id`。schema v7 新增：

```sql
CREATE TABLE task_workspaces (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id),
    attempt_id TEXT NOT NULL REFERENCES attempts(id),
    lease_id TEXT NOT NULL UNIQUE,
    repository_root TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    branch TEXT NOT NULL UNIQUE,
    parent_branch TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE application_root_attachments (
    idempotency_key TEXT PRIMARY KEY,
    root_session_id TEXT NOT NULL,
    root_task_id TEXT NOT NULL REFERENCES tasks(id),
    capability_digest TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    detached_at TEXT
);
```

实现 `record_task_workspace`、`task_workspace` 和 `task_workspace_optional`；读取时拒绝空路径、空 branch 和空 base。

- [ ] **Step 4：把 workspace 暴露给 typed task detail**

```rust
pub struct IpcTask {
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub objective: String,
    pub state: String,
    pub attempt: u32,
    pub current_wait: Option<String>,
    pub workspace: Option<WorkerWorkspace>,
    pub delivery: Option<DeliveryReport>,
}

#[serde(default, skip_serializing_if = "Option::is_none")]
pub workspace: Option<WorkerWorkspace>,
```

同时扩展 repository 的 `PersistedTask` 和 subscription snapshot query，使 fresh/resync snapshot 直接携带卡片所需的 parent、objective、attempt、wait、workspace 和 delivery。`InspectTask` 读取相同权威字段；旧任务没有 assignment 时返回 `None`，不得伪造当前目录。

- [ ] **Step 5：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent-core --lib subagent::worker
cargo test -p yi-agent-store --test repository_decisions
cargo test -p yi-agent-store --test runtime_ipc
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent-core/src/subagent/worker.rs crates/yi-agent-store/src/repository.rs crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/tests/repository_decisions.rs crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat: persist task workspace assignments"
```

Expected: 三组测试通过；随后完成两阶段 review 和父 Agent 复验。

### Task 3：接通根/子 worktree provisioning 与每任务工具目录

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

- [x] **Step 1：写 root/child provisioning RED**

```rust
#[test]
fn root_and_child_worktrees_are_distinct_and_begin_at_the_recorded_parent_head() {
    let repo = committed_repository();
    let service = WorktreeService::new();
    let root_path = repo.path().join(".worktrees/yi-root");
    let child_path = repo.path().join(".worktrees/yi-child");
    let root = service.create_root(repo.path(), "feat/yi-root", &root_path).unwrap();
    let child = service
        .create_child(&root.path, &root.base_commit, "feat/yi-child", &child_path)
        .unwrap();
    assert_ne!(root.path, child.path);
    assert_eq!(child.parent_branch, root.branch);
    assert_eq!(child.base_commit, git_head(&root.path));
    assert_eq!(git_head(repo.path()), root.base_commit);
}
```

coordinator 测试还要断言 factory start 前 workspace 已持久化，且 `WorkerStart.workspace.path` 与 repository 完全一致。

- [x] **Step 2：运行 RED**

```bash
cargo test -p yi-agent-tools --test subagent_worktree root_and_child_worktrees_are_distinct_and_begin_at_the_recorded_parent_head -- --exact
cargo test -p yi-agent-store --test runtime_coordinator worker_receives_its_persisted_workspace_before_provider_start -- --exact
```

Expected: `create_root` 和 workspace service injection 尚不存在。

- [x] **Step 3：定义 workspace service 边界**

```rust
pub trait AgentWorkspaceService: Send + Sync {
    fn prepare_root(
        &self,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError>;

    fn prepare_child(
        &self,
        parent: &WorkerWorkspace,
        root_session_id: &RootSessionId,
        task_id: &TaskId,
        attempt_id: &AttemptId,
    ) -> Result<WorkerWorkspace, WorkerError>;
}
```

提供 `UnavailableWorkspaceService`，只允许非 coding mock 测试显式使用；它返回 `WorkerError::Startup("coding workspace service is unavailable")`，不得回退到当前目录。

- [x] **Step 4：实现 Git service 并接入 coordinator**

`WorktreeService::create_root` 复用 parent-base 验证。`DaemonWorkspaceService` 使用稳定命名：

```rust
fn branch_name(session: &RootSessionId, task: &TaskId, root: bool) -> String {
    if root {
        format!("feat/yi-agent-{}-root", short(session.as_ref()))
    } else {
        format!("feat/yi-agent-{}-{}", short(session.as_ref()), short(task.as_ref()))
    }
}

fn short(value: &str) -> String {
    value.chars().filter(|ch| *ch != '-').take(8).collect()
}
```

root/child task 先以 queued 持久化，再 provision 并记录 assignment，最后才 admit worker。provisioning 失败在 provider 之前持久化为 failed，并保留 Git evidence。

- [x] **Step 5：让 worker tools/recovery 使用 assignment.path**

`DaemonAgentWorkerFactory::start_with_provider_turn_gate` 必须读取 `request.workspace`，并为其 path 重建 builtin tool registry。`recovery_context` 改为接收对应 `WorkerStart`；删除全局 workspace 复用。

- [x] **Step 6：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent-tools --test subagent_worktree
cargo test -p yi-agent-store --test runtime_coordinator
cargo test -p yi-agent --bin yi-agent subagent_runtime::tests
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent-core/src/subagent/worker.rs crates/yi-agent-tools/src/worktree.rs crates/yi-agent-tools/tests/subagent_worktree.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/tests/runtime_coordinator.rs crates/yi-agent/src/subagent_runtime.rs crates/yi-agent/src/main.rs
git commit -m "feat: provision isolated runtime worktrees"
```

Expected: user checkout HEAD/status 在测试前后不变；随后完成两阶段 review 和父 Agent 复验。

### Task 4：挂接前台 TUI 根任务并注册自然语言委派工具

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Create: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`

- [x] **Step 1：写 attach 幂等、capability 与显式启动 RED**

```rust
#[test]
fn application_root_attach_is_idempotent_and_returns_the_same_workspace() {
    let daemon = test_daemon_with_workspace_service();
    let first = attach_root(daemon.socket_path(), "tui-start-1");
    let second = attach_root(daemon.socket_path(), "tui-start-1");
    assert_eq!(first.session_id, second.session_id);
    assert_eq!(first.root_task_id, second.root_task_id);
    assert_eq!(first.workspace, second.workspace);
    assert!(!first.message_capability.is_empty());
}

#[test]
fn delegation_tool_rejects_a_capability_from_another_attached_root() {
    let daemon = test_daemon_with_workspace_service();
    let first = attach_root(daemon.socket_path(), "tui-a");
    let second = attach_root(daemon.socket_path(), "tui-b");
    assert_eq!(spawn_as(&first, second.message_capability), IpcErrorCode::AuthorityDenied);
}
```

TUI unit test证明 disconnected 状态只产生 `RuntimeStartPrompt`，用户确认前 launcher 调用数为 0。

- [x] **Step 2：运行 RED**

```bash
cargo test -p yi-agent-store --test runtime_ipc application_root_attach_is_idempotent_and_returns_the_same_workspace -- --exact
cargo test -p yi-agent --bin yi-agent tui::subagents::tests -- --nocapture
```

Expected: attach IPC 和 TUI runtime model 尚不存在。

- [x] **Step 3：增加外部根任务 typed IPC**

```rust
IpcRequest::AttachApplicationRoot { idempotency_key: String },
IpcRequest::ActivateApplicationRoot {
    session_id: String,
    root_task_id: String,
    capability: String,
    objective: String,
},
IpcRequest::DetachApplicationRoot {
    session_id: String,
    root_task_id: String,
    capability: String,
},
```

```rust
IpcResponse::ApplicationRootAttached {
    session_id: String,
    root_task_id: String,
    message_capability: String,
    workspace: WorkerWorkspace,
},
IpcResponse::ApplicationRootActivated,
IpcResponse::ApplicationRootDetached,
```

repository 使用 `sha2 = "0.10"` 只保存 capability SHA-256 digest。重复 idempotency key 返回同一记录；`Activate` 原子记录首轮 objective 并把 queued root 变为 running；`Detach` 进入 pause/recovery 边界，不得记录 completed。

- [x] **Step 4：实现 TUI runtime bootstrap 与工具注册**

```rust
pub enum RuntimeStartupChoice { Start, ContinueWithoutDelegation }

pub struct AttachedRoot {
    pub session_id: String,
    pub task_id: String,
    pub capability: String,
    pub workspace: WorkerWorkspace,
}

pub enum TuiRuntimeMode {
    Attached(AttachedRoot),
    Disabled { reason: String },
}
```

用户选择后再返回 mode。`main.rs` 使用 attachment workspace 重建 PermissionChecker 和 builtin tool registry，并调用 `register_application_subagent_tools` 注册三个 daemon proxy tools。第一条输入送给 Agent 前先 `Activate`，退出时 `Detach`。

- [x] **Step 5：验证自然语言 Agent 可见工具且无 `/delegate`**

```rust
#[test]
fn attached_tui_root_exposes_subagent_tools_without_a_delegate_command() {
    let names = attached_root_registry()
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"spawn_agent".to_string()));
    assert!(names.contains(&"send_message".to_string()));
    assert!(names.contains(&"wait_agent".to_string()));
    assert!(!SlashCommand::all().iter().any(|command| command.name() == "delegate"));
}
```

- [x] **Step 6：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent-core --lib subagent::supervisor
cargo test -p yi-agent-store --test runtime_ipc
cargo test -p yi-agent --bin yi-agent tui::subagents::tests
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent-core/src/subagent/supervisor.rs crates/yi-agent-store/Cargo.toml crates/yi-agent-store/src/repository.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/tests/runtime_ipc.rs crates/yi-agent/src/subagent_runtime.rs crates/yi-agent/src/tui/subagents.rs crates/yi-agent/src/tui/mod.rs crates/yi-agent/src/main.rs
git commit -m "feat: attach TUI sessions to the subagent runtime"
```

Expected: attach/bootstrap 测试通过；随后完成两阶段 review 和父 Agent 复验。

**Completion evidence (2026-08-11):**

- Commits: `e5096c1`, `db90804`, `47f22c7`, `efb2035`, `4240e8d`, `2de830a`, `d497408`.
- Verified commands:
  - `cargo test -p yi-agent-core --lib subagent::supervisor` → 9 passed.
  - `cargo test -p yi-agent-store --test runtime_ipc` → 70 passed.
  - `cargo test -p yi-agent --bin yi-agent tui::subagents::tests -- --nocapture` → 3 passed.
  - `cargo test -p yi-agent --bin yi-agent runtime_start_prompt -- --nocapture` → 2 passed.
  - `cargo test -p yi-agent --bin yi-agent tests::build_tui_root_tools_registers_subagent_tools_for_attached_runtime -- --exact --nocapture` → 1 passed.
  - `cargo test -p yi-agent --bin yi-agent subagent_runtime::tests::worker_wait_proxy_rejects_unbound_capability -- --exact --nocapture` → 1 passed.
  - `cargo fmt --all && just fmt-check && git diff --check` passed.
- Fresh final rereview result: no Critical / Important / Minor findings; ready to proceed to Task 5.

### Task 5：产生真实 commit delivery 并执行受信任的直接父集成

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

- [ ] **Step 1：写 delivery 与 pinned integration RED**

```rust
#[tokio::test]
async fn coding_child_done_reports_the_exact_clean_commit_for_review() {
    let harness = CodingWorkerHarness::new();
    let child = harness.start_child_that_commits("feature.txt", "delivered\n").await;
    let detail = harness.wait_for_review(&child).await;
    let delivery: DeliveryReport = serde_json::from_str(&detail.delivery_json).unwrap();
    assert_eq!(delivery.commit, git_head(&harness.child_workspace(&child)));
    assert_ne!(delivery.commit, delivery.base_ref);
    assert_eq!(git_status(&harness.child_workspace(&child)), "");
}

#[test]
fn reviewed_head_change_prevents_daemon_integration() {
    let harness = ReviewedDeliveryHarness::new();
    let preview = harness.preview_accept();
    harness.advance_child_head();
    assert_eq!(harness.confirm_accept(preview.token), IpcErrorCode::Conflict);
    assert_eq!(git_head(harness.parent()), harness.parent_head_before_review());
}
```

- [ ] **Step 2：运行 RED**

```bash
cargo test -p yi-agent --bin yi-agent coding_child_done_reports_the_exact_clean_commit_for_review -- --exact
cargo test -p yi-agent-store --test runtime_ipc reviewed_head_change_prevents_daemon_integration -- --exact
```

Expected: worker 仍报告 `CompletedWithoutDelivery`，accept 仍只通知 parent。

- [ ] **Step 3：扩展 workspace service 的检查与集成接口**

先用向后兼容字段扩展 delivery；旧 SQLite JSON 反序列化为空列表：

```rust
pub struct DeliveryReport {
    pub id: DeliveryId,
    pub commit: String,
    pub base_ref: String,
    pub workspace: WorkspaceLeaseId,
    pub evidence: String,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub known_limitations: Vec<String>,
}
```

`inspect_delivery` 使用 `git diff --name-only <base>..<head>` 填充去重、排序后的 repository-relative `changed_files`；不能接受绝对路径或 `..`。MVP worker 没有主动报告限制时使用空 `known_limitations`。

```rust
pub trait AgentWorkspaceService: Send + Sync {
    fn inspect_delivery(
        &self,
        workspace: &WorkerWorkspace,
        evidence: String,
    ) -> Result<DeliveryReport, WorkerError>;

    fn integrate_delivery(
        &self,
        parent: &WorkerWorkspace,
        child: &WorkerWorkspace,
        delivery: &DeliveryReport,
    ) -> Result<IntegrationValidation, WorkerError>;
}
```

`DaemonWorkspaceService` 固定 clean HEAD；integration 重新检查 parent/child cleanliness 和 fixed HEAD，再执行 `merge --no-ff <delivery.commit>` 与 `git diff --check`。失败 evidence 包含命令、exit code、stderr 和 status，且不 reset。

- [ ] **Step 4：worker Done 生成真实 delivery**

正常 tool result 时递增 `successful_tool_results: u64`。Done 分支使用稳定 JSON evidence：

```rust
let evidence = serde_json::json!({
    "checks": ["git diff --check"],
    "successful_tool_results": successful_tool_results,
})
.to_string();

match request.workspace.as_ref() {
    Some(workspace) if request.parent_task_id.is_some() => {
        match workspace_service.inspect_delivery(workspace, evidence) {
            Ok(delivery) => reporter.report_delivery(delivery),
            Err(error) => reporter.report_failure(error.to_string()),
        }
    }
    Some(_) => reporter.report_completed_without_delivery(),
    None => reporter.report_failure("coding worker has no persisted workspace"),
}
```

给 `WorkerStart` 增加 `parent_task_id: Option<TaskId>`，coordinator 从 task tree 填充。

- [ ] **Step 5：增加 review preview/confirm 并执行 integration**

```rust
IpcRequest::PreviewReview {
    task_id: String,
    decision: IpcReviewDecision,
},
IpcRequest::ConfirmReview {
    task_id: String,
    decision: IpcReviewDecision,
    confirmation_token: String,
},
IpcResponse::ReviewPreview {
    confirmation_token: String,
    task_id: String,
    delivery: DeliveryReport,
    parent_workspace: WorkerWorkspace,
    child_workspace: WorkerWorkspace,
    expires_in_secs: u64,
},
IpcResponse::ReviewAccepted,
```

confirmation digest 覆盖 task、decision、delivery ID/head、parent workspace/head 和 child workspace/head。Confirm Accept 先集成，再把成功 `IntegrationValidation` 传给 `accept_review`。Rework/Reject 也用 preview/confirm；旧的无确认 `Review` mutation 返回 `ConfirmationRequired`。

- [ ] **Step 6：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent-tools --test subagent_worktree
cargo test -p yi-agent-store --test runtime_coordinator
cargo test -p yi-agent-store --test runtime_ipc
cargo test -p yi-agent --bin yi-agent subagent_runtime::tests
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent-core/src/subagent/task.rs crates/yi-agent-core/src/subagent/worker.rs crates/yi-agent-tools/src/worktree.rs crates/yi-agent-tools/tests/subagent_worktree.rs crates/yi-agent-store/src/repository.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/tests/runtime_coordinator.rs crates/yi-agent-store/tests/runtime_ipc.rs crates/yi-agent/src/subagent_runtime.rs
git commit -m "feat: deliver and integrate subagent commits"
```

Expected: 真实 Git 测试通过且 user checkout 不变；随后完成两阶段 review 和父 Agent 复验。

### Task 6：把 daemon child 权限请求接入现有 TUI 权限交互

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

- [ ] **Step 1：写 child permission wait/resolve RED**

```rust
#[tokio::test]
async fn daemon_child_waits_for_the_durable_tui_permission_decision() {
    let harness = PermissionWorkerHarness::new();
    let child = harness.start_child_requesting_bash().await;
    let request = harness.wait_for_permission_event(&child).await;
    assert_eq!(harness.task_state(&child), "waiting_for_permission");
    assert!(!harness.tool_executed());
    harness.resolve_from_tui(&request, IpcPermissionDecision::AllowOnce);
    harness.wait_for_tool().await;
    assert!(harness.tool_executed());
    assert_eq!(harness.permission_state(&request), "allowed");
}
```

- [ ] **Step 2：运行 RED**

```bash
cargo test -p yi-agent --bin yi-agent daemon_child_waits_for_the_durable_tui_permission_decision -- --exact
```

Expected: daemon worker 没有 PermissionChecker/decision bridge。

- [ ] **Step 3：增加 worker permission channel**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerPermissionRequest {
    pub id: PermissionRequestId,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerPermissionDecision {
    pub id: PermissionRequestId,
    pub decision: PermissionDecision,
}
```

`WorkerEvent` 增加 `PermissionRequested`；`WorkerHandle` 增加按 request ID 等待/投递决定的 channel。supervisor 只有在 repository 成功持久化 request 后才 reducer 到 `WaitingForPermission`；resolve 先持久化，再投递给精确 worker。

- [ ] **Step 4：桥接 Agent permission event 与 typed IPC**

`DaemonAgentWorkerFactory` 为 child workspace 创建 PermissionChecker 和 decision channel，并调用 `Agent::with_permission`。收到 `AgentEvent::PermissionRequest` 时报告完整 tool/input/kind/prefix JSON；收到 worker decision 后转发给 Agent channel。

```rust
pub enum IpcPermissionDecision {
    AllowOnce,
    AlwaysAllowTool,
    AlwaysAllowPrefix { prefix: String },
    Deny,
}
```

runtime reducer 仍使用 allow/deny 权威状态；audit payload 保存完整选择。prefix 必须匹配 pending request 的建议，不能由 client 扩大。

- [ ] **Step 5：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent-core --lib subagent::worker
cargo test -p yi-agent-core --lib subagent::supervisor
cargo test -p yi-agent-store --test runtime_coordinator
cargo test -p yi-agent-store --test runtime_ipc
cargo test -p yi-agent --bin yi-agent subagent_runtime::tests
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent-core/src/subagent/worker.rs crates/yi-agent-core/src/subagent/supervisor.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/tests/runtime_coordinator.rs crates/yi-agent-store/tests/runtime_ipc.rs crates/yi-agent/src/subagent_runtime.rs crates/yi-agent/src/tui/subagents.rs
git commit -m "feat: route subagent permissions through the TUI"
```

Expected: allow/deny、错误 request ID 和 security-reserved 测试通过；随后完成两阶段 review 和父 Agent 复验。

### Task 7：实现 runtime projection、内嵌任务卡片和审核操作

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Test: the same files' `#[cfg(test)]` modules

- [ ] **Step 1：写 projection、原地更新和上下文隔离 RED**

```rust
#[test]
fn runtime_events_upsert_one_cell_without_entering_agent_context() {
    let mut projection = SubagentProjection::default();
    let mut history = HistoryState::new();
    projection.replace_snapshot(snapshot(vec![task_snapshot("child-1", "queued")]));
    history.upsert_runtime_task(projection.card("child-1").unwrap().clone(), 80);
    projection.apply_event(task_started_event("child-1"));
    history.upsert_runtime_task(projection.card("child-1").unwrap().clone(), 80);
    assert_eq!(history.runtime_cell_count("child-1"), 1);
    assert_eq!(history.agent_context_messages(), Vec::<String>::new());
}

#[test]
fn resync_discards_the_stale_projection_before_rendering_replacement() {
    let mut projection = projection_with_task("stale-child");
    projection.begin_resync();
    projection.replace_snapshot(snapshot(vec![task_snapshot("fresh-child", "running")]));
    assert!(projection.card("stale-child").is_none());
    assert!(projection.card("fresh-child").is_some());
}
```

- [ ] **Step 2：写焦点和窄终端 RED**

```rust
#[test]
fn review_shortcuts_only_apply_to_the_focused_review_card() {
    let mut ui = tui_with_review_card("child-1");
    ui.focus_input();
    assert_eq!(ui.handle_key(key('a')), ReviewAction::None);
    ui.focus_task("child-1");
    assert_eq!(ui.handle_key(key('A')), ReviewAction::PreviewAccept("child-1".into()));
}

#[test]
fn review_card_renders_within_a_forty_column_terminal() {
    let lines = review_card().lines(40);
    assert!(lines.iter().all(|line| line.width() <= 40));
    assert!(rendered_text(&lines).contains("A 接受"));
}
```

- [ ] **Step 3：运行 RED**

```bash
cargo test -p yi-agent --bin yi-agent tui::subagents::tests
cargo test -p yi-agent --bin yi-agent tui::history::tests::runtime_events_upsert_one_cell_without_entering_agent_context -- --exact
cargo test -p yi-agent --bin yi-agent tui::cell::tests::review_card_renders_within_a_forty_column_terminal -- --exact
```

Expected: projection 和 runtime cell 尚未实现。

- [ ] **Step 4：实现 projection 和异步订阅桥**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeConnectionState { Starting, Connected, Disconnected, Resyncing }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCard {
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub objective: String,
    pub state: String,
    pub attempt: u32,
    pub current_wait: Option<String>,
    pub progress: Option<String>,
    pub workspace: Option<WorkerWorkspace>,
    pub delivery: Option<DeliveryReport>,
    pub expanded: bool,
}
```

独立 subscription thread 把 `RuntimeUiEvent::{Snapshot, Event, ResyncRequired, Disconnected}` 发送到 TUI channel。projection 只按递增 event ID 应用；snapshot replace 先清空 cards。render/input loop 不做阻塞 socket I/O。

- [ ] **Step 5：实现 HistoryCell 与 review input mode**

`HistoryCell` 增加 `RuntimeTask { card: TaskCard }`，`HistoryState` 按 task ID upsert。`is_foldable` 和 `toggle_fold` 支持任务卡片。

```rust
pub enum ReviewInputMode {
    Rework { task_id: String },
    Reject { task_id: String },
}
```

只有 card focus 下路由大写 `A/R/X/D`。Escape 清除 mode；Enter 对空文本不发送。A/R/X 先 `PreviewReview`，渲染 confirmation cell，再由用户确认 `ConfirmReview`。

- [ ] **Step 6：状态栏和 Slash fallback 复用同一 client**

`StatusBarState` 增加 connection/resident/queued。`/agents`、`/review`、`/accept`、`/rework`、`/reject` 不再输出“接入中”，全部调用与卡片相同的 `RuntimeActionClient`，不得各自拼 IPC payload。

- [ ] **Step 7：运行 GREEN、格式化并提交**

```bash
cargo test -p yi-agent --bin yi-agent tui::subagents::tests
cargo test -p yi-agent --bin yi-agent tui::cell::tests
cargo test -p yi-agent --bin yi-agent tui::history::tests
cargo test -p yi-agent --bin yi-agent tui::app::tests
cargo test -p yi-agent --bin yi-agent tui::slash::tests
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent/src/tui/subagents.rs crates/yi-agent/src/tui/cell.rs crates/yi-agent/src/tui/history.rs crates/yi-agent/src/tui/app.rs crates/yi-agent/src/tui/statusbar.rs crates/yi-agent/src/tui/slash.rs
git commit -m "feat: show and review subagents in the TUI"
```

Expected: 焦点、重同步、窄终端和 Slash fallback 测试通过；随后完成两阶段 review 和父 Agent 复验。

### Task 8：跑通真实 daemon/Git 的 TUI MVP 端到端闭环

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/subagent_mvp_e2e_tests.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/subagents.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/worktree.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`

- [ ] **Step 1：写完整 accept path RED**

测试使用 `TempDir` Git repository、真实 `Daemon`/Unix socket、脚本化 root/child provider、真实 file/shell tools 和 Ratatui `TestBackend`：

```rust
#[test]
fn natural_language_tui_delegation_delivers_and_integrates_a_child_commit() {
    let mut harness = TuiSubagentMvpHarness::new();
    let user_head = harness.user_checkout_head();
    harness.confirm_runtime_start();
    harness.submit("实现 feature.txt；把文件实现委派给一个子 Agent");
    harness.run_until_review_card();
    let child = harness.only_child();
    assert!(harness.screen().contains("等待父任务审核"));
    assert!(harness.delivery(&child).commit.len() >= 7);
    harness.focus_review(&child);
    harness.press('A');
    harness.confirm_review();
    harness.run_until_accepted();
    assert!(harness.root_workspace().join("feature.txt").exists());
    assert!(harness.root_head_has_two_parents());
    assert_eq!(harness.user_checkout_head(), user_head);
    assert_eq!(harness.user_checkout_status(), "");
    assert_eq!(harness.task_state(&child), "completed");
    assert!(!harness.has_active_resident_lease(&child));
}
```

- [ ] **Step 2：写 rework、reject、restart 和 failure RED**

```rust
#[test]
fn focused_review_card_rework_reaches_the_successor_attempt_once() {
    assert_rework_path(TuiSubagentMvpHarness::new());
}

#[test]
fn focused_review_card_reject_retains_the_unmerged_child_worktree() {
    assert_reject_path(TuiSubagentMvpHarness::new());
}

#[test]
fn restarted_daemon_resyncs_cards_without_replaying_an_accepted_delivery() {
    assert_restart_path(TuiSubagentMvpHarness::new());
}

#[test]
fn dirty_checkout_and_permission_denial_remain_actionable_in_the_tui() {
    assert_failure_cards(TuiSubagentMvpHarness::new());
}
```

每个 helper 都断言 SQLite state/event、Git HEAD/status 和 TUI 文本，不能只检查一层。

- [ ] **Step 3：运行 RED 并记录第一个真实断点**

```bash
cargo test -p yi-agent --bin yi-agent tui::subagent_mvp_e2e_tests -- --nocapture
```

Expected: 至少一条测试在真实组件边界失败；不得放宽断言。

- [ ] **Step 4：逐个修复 E2E 缺口**

每次只修当前确定性失败并保留回归。workspace/Git 失败只修改 `worktree.rs`、`repository.rs` 或 `runtime.rs`；IPC correlation 失败只修改 `ipc.rs`；worker delivery/permission 失败只修改 `worker.rs`、`supervisor.rs` 或 `subagent_runtime.rs`；TUI projection/input 失败只修改 `subagents.rs`、`cell.rs`、`history.rs`、`app.rs` 或 `statusbar.rs`；bootstrap 失败只修改 `main.rs`。不得加入测试专用生产分支、跳过真实 Git，或把 `CompletedWithoutDelivery` 当 coding success。每个修复后重跑精确测试，直到全部 GREEN。

- [ ] **Step 5：运行 MVP 聚焦验证矩阵**

每条命令前检查进程并串行运行：

```bash
cargo test -p yi-agent-core --lib
cargo test -p yi-agent-core --test subagent_contract_mailbox
cargo test -p yi-agent-tools --lib
cargo test -p yi-agent-tools --test subagent_worktree
cargo test -p yi-agent-store --test repository_decisions
cargo test -p yi-agent-store --test runtime_coordinator
cargo test -p yi-agent-store --test runtime_ipc
cargo test -p yi-agent --bin yi-agent tui::subagent_mvp_e2e_tests
cargo test -p yi-agent --bin yi-agent tui::
```

Expected: 全部通过，无残留 `cargo`、`rustc` 或 `yi_agent_*` 进程。

- [ ] **Step 6：格式化、提交并完成最终 MVP review**

```bash
cargo fmt --all
just fmt-check
git diff --check
git add crates/yi-agent/src/tui/subagent_mvp_e2e_tests.rs crates/yi-agent/src/tui/mod.rs crates/yi-agent/src/tui/app.rs crates/yi-agent-core crates/yi-agent-tools crates/yi-agent-store crates/yi-agent
git commit -m "test: prove the subagent TUI MVP workflow"
```

提交前检查 staged diff，排除无关文件。随后进行中文 MVP 逐项 specification review、独立 code-quality review、父 Agent 完整聚焦矩阵复跑和一次用户可见 TUI smoke test。只报告可验证入口，保持分支未 merge。

## 规格覆盖自检

| 中文规格要求 | 实施与验证任务 |
|---|---|
| 普通对话中由根 Agent 自主调用 `spawn_agent` | Task 4 工具挂接；Task 8 scripted root provider E2E |
| TUI 内明确启动 runtime，拒绝后仍可单 Agent 对话 | Task 4 startup model；Task 8 failure path |
| 根/子独立 worktree，用户 checkout 只读 | Tasks 2-3；Task 8 Git HEAD/status 断言 |
| coding child 产生真实固定 commit delivery | Task 5 delivery tests；Task 8 accept path |
| 接受只集成到直接父 worktree，固定 HEAD 防 TOCTOU | Task 5 preview/confirm/integration tests |
| 返工恰好一次、拒绝保留现场 | Task 1 recovery prerequisite；Tasks 5 和 8 |
| 子 Agent 权限进入 TUI 并先持久化后唤醒 | Task 6；Task 8 permission denial path |
| 卡片原地更新且不进入 LLM 上下文 | Task 7 projection/history tests |
| review card 焦点、输入隔离和窄终端 | Task 7 rendering/input tests |
| 断线、resync、restart 后从 durable snapshot 恢复 | Tasks 2、4、7；Task 8 restart path |
| 不 merge `main`，MVP 后继续完整 handoff 范围 | 所有 Git tests 与本计划“计划约束/MVP 后续” |

自检结论：中文规格的每项 MVP 要求都有明确生产边界、RED/GREEN 测试和端到端证据；未发现缺失项、占位符或互相矛盾的类型名称。

## MVP 后续

本计划完成只证明 TUI MVP，不代表完整 handoff objective 完成。随后继续 root→child→leaf 两级 E2E、完整侧边栏、自然语言 schedule 执行、shutdown hang、checkpoints 1-20 审计、strict Clippy 和最终项目管理文档更新。所有要求通过且用户明确授权前，仍不得 merge。
