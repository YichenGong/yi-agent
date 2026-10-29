# 父任务终结后禁止派生 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让「父任务处于已定终态」成为派生新子任务时的一道显式拒绝，堵住 `2026-10-02` 级联不变式的反向缺口。

**Architecture:** 判定复用 core 里已有的 `classify_terminal`（`Settled` / `RecoveryRequired` / `NonTerminal` 三分类），在 `AgentSupervisor::spawn_with_objective` 加权威闸门；在 `RuntimeCoordinator::spawn_child_with_objective` 的 `take_fork_messages` 之前加一层预检，防止拒绝路径白白消耗 fork 上传。库里的 `state_json` 是裸标签（`completed`），需要一条与 `task_state_label` 互逆的解析路径，放在 core 里复用，不在 store 另立清单。

**Tech Stack:** Rust 2024 edition，workspace crates: `yi-agent-core`（状态机/超管）、`yi-agent-store`（runtime coordinator/repository/IPC），`thiserror`，`rusqlite`，`cargo test`。

## Global Constraints

- 禁止在 `main` 分支直接改代码；所有实现必须在 worktree 的新分支上，完成后 `git merge --no-ff` 回 `main`。worktree 路径：`.worktrees/feat-settled-parent-spawn-gate`，分支：`feat/settled-parent-spawn-gate`。
- 提交前必须跑 `cargo fmt --all`（在 `yi-agent-rs/` 下），保证 `just fmt-check` 通过。
- Commit message 用 conventional commits（`feat:` / `fix:` / `test:` / `docs:`），首行 <=72 字符，**不得**写 `Co-Authored-By`。
- 测试按 crate 跑，**不要** `cargo test --workspace`（易 OOM / 死锁）。跑前先 `ps aux | grep cargo` 确认无残留 cargo 进程。
- 判定必须用 `classify_terminal(...) == TerminalKind::Settled`，**禁止**用 `TaskState::is_terminal()`（它把 `RecoveryRequired` 也算终态，会误拒可恢复的父任务）。
- 已定终态恰好 8 个：`completed`、`completed_no_changes`、`blocked`、`stalled`、`timed_out`、`budget_exhausted`、`failed`、`cancelled`。
- `RecoveryRequired` 必须保持可派生（父可 resume 后继续派生）。
- 不处理存量孤儿数据（实测为 0），不碰 `resource_leases` 的租约泄漏（独立缺陷）。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` | `SpawnError` 新变体、`spawn_with_objective` 闸门、把 `task_state_label` / `classify_terminal` 提为 `pub(crate)` 或被复用的宿主 | 修改 |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs` | `TaskState::from_state_label` —— 与标签互逆的解析入口 | 修改 |
| `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs` | 闸门行为测试（8 个终态拒绝、`recovery_required` 放行、非终态放行、无半个子任务） | 修改 |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | `RuntimeCoordinatorError::ParentSettled` 变体、预检逻辑 | 修改 |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | `ipc_error_message` 新增 `ParentSettled` 可读消息 | 修改 |
| `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs` | fork 上传不被消耗、无幽灵任务行 | 修改 |
| `docs/project-management/subagent-runtime.md` | 勾选新增条目 + 验证命令 | 修改 |

`task.rs` 承载 `TaskState` 的定义，把「标签 ↔ 状态」这对互逆操作放在同一个文件里，避免称呼散落两处；`supervisor.rs` 的 `classify_terminal` 同理与它同源。

---

### Task 1: `TaskState` 标签解析入口

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`（在 `is_terminal` 之后新增方法）
- Test: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`（文件内 `#[cfg(test)]` 模块；若无则新建）

**Interfaces:**
- Consumes: 无（本任务独立）
- Produces:
  - `impl TaskState { pub fn from_state_label(label: &str) -> Option<TaskState> }`
  - 接受库中实际出现的全部 14 个标签，返回对应 `TaskState`；未知标签返回 `None`
  - 与 `supervisor.rs` 的 `task_state_label(&TaskState) -> &'static str` 互为逆运算（对该函数能产出的每个标签）

**为什么需要它：** `state_json` 在库里存的是裸标签，而 `TaskState` 没有 `#[serde(rename_all)]`，serde 反序列化期望的是 `"Completed"` 这类 PascalCase 带引号形态，`serde_json::from_str::<TaskState>("\"completed\"")` 会失败。因此必须显式写标签映射。

- [ ] **Step 1: 写失败测试**

在 `task.rs` 末尾追加（若已有 `#[cfg(test)] mod tests`，追加到该模块内）：

```rust
#[cfg(test)]
mod state_label_tests {
    use super::*;

    #[test]
    fn every_label_the_supervisor_emits_parses_back() {
        // 这 8 个是「已定终态」，闸门必须认出来。
        for label in [
            "completed",
            "completed_no_changes",
            "blocked",
            "stalled",
            "timed_out",
            "budget_exhausted",
            "failed",
            "cancelled",
        ] {
            let state = TaskState::from_state_label(label)
                .unwrap_or_else(|| panic!("settled label {label} must parse"));
            assert!(
                !matches!(state, TaskState::RecoveryRequired(_)),
                "{label} must not parse as RecoveryRequired"
            );
        }
    }

    #[test]
    fn recovery_required_parses_as_recovery_not_settled() {
        let state = TaskState::from_state_label("recovery_required")
            .expect("recovery_required must parse");
        assert!(
            matches!(state, TaskState::RecoveryRequired(_)),
            "recovery_required must stay recovery: a resumed parent still spawns"
        );
    }

    #[test]
    fn non_terminal_labels_parse_to_non_terminal_states() {
        for label in [
            "queued",
            "running",
            "waiting_for_resource",
            "waiting_for_permission",
            "waiting_for_children",
            "paused",
        ] {
            let state = TaskState::from_state_label(label)
                .unwrap_or_else(|| panic!("non-terminal label {label} must parse"));
            assert!(
                !state.is_terminal(),
                "{label} must not parse as terminal"
            );
        }
    }

    #[test]
    fn unknown_labels_are_none_not_a_panic() {
        assert!(TaskState::from_state_label("").is_none());
        assert!(TaskState::from_state_label("not_a_state").is_none());
        assert!(TaskState::from_state_label("Completed").is_none());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib state_label_tests
```

Expected: 编译失败，`no function or associated item named 'from_state_label' found for enum 'TaskState'`

- [ ] **Step 3: 写最小实现**

在 `task.rs` 的 `impl TaskState` 块内、`is_terminal` 之后追加：

```rust
    /// Parses the bare label stored in `tasks.state_json` back into a state.
    ///
    /// The store keeps bare labels (`completed`), while `TaskState`'s serde shape
    /// is PascalCase (`"Completed"`), so serde cannot read a persisted row. This
    /// is the inverse of the supervisor's `task_state_label`, and exists so callers
    /// outside the state machine (the runtime coordinator) can classify a
    /// persisted parent without keeping a second copy of the terminal vocabulary.
    ///
    /// Payload-bearing variants get a stand-in payload: only the variant identity
    /// matters to callers, and inventing a reason string here would be a lie the
    /// caller could accidentally persist.
    pub fn from_state_label(label: &str) -> Option<Self> {
        match label {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "waiting_for_resource" => Some(Self::WaitingForResource(ResourceWait(String::new()))),
            "waiting_for_permission" => {
                // A uuid newtype: `String::new()` does not exist here.
                Some(Self::WaitingForPermission(PermissionRequestId::new()))
            }
            "waiting_for_children" => Some(Self::WaitingForChildren(WaitSelector::AnyChild)),
            "paused" => Some(Self::Paused(PauseReason(String::new()))),
            "completed" => Some(Self::Completed),
            "completed_no_changes" => Some(Self::CompletedNoChanges),
            "blocked" => Some(Self::Blocked(BlockReason(String::new()))),
            "stalled" => Some(Self::Stalled(WatchdogEvidence {
                last_meaningful_event_id: None,
                last_meaningful_at: Utc::now(),
                elapsed_secs: 0,
                current_wait: None,
            })),
            "timed_out" => Some(Self::TimedOut(TimeoutKind::WallClock)),
            "budget_exhausted" => Some(Self::BudgetExhausted(BudgetKind::WallTime)),
            "failed" => Some(Self::Failed(TaskFailure(String::new()))),
            "cancelled" => Some(Self::Cancelled(CancelReason(String::new()))),
            "recovery_required" => Some(Self::RecoveryRequired(RecoveryEvidence(String::new()))),
            _ => None,
        }
    }
```

这两个构造有坑，别想当然：

- `PermissionRequestId` 由 `uuid_id!` 宏生成（`task.rs:51`），是 UUID newtype，只有 `new()` / `Default` / `FromStr`，**没有** `String::new()` 形态。
- `WaitSelector` 的变体是 `AnyChild` / `AllChildren`（`task.rs:91-94`），**不是** `Any`。

其余 payload 类型（`ResourceWait`、`BlockReason`、`TaskFailure`、`CancelReason`、`RecoveryEvidence`）都是 `pub struct X(pub String)` 元组结构，`String::new()` 可用。`Utc` 已在文件顶部引入。

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib state_label_tests
```

Expected: `4 passed`

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(core): parse a persisted task state label back into TaskState

state_json keeps bare labels but TaskState's serde shape is PascalCase, so
serde cannot read a persisted row. This inverse of task_state_label lets the
runtime coordinator classify a parent without a second terminal vocabulary."
```

---

### Task 2: 超管派生闸门

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs:33-40`（`SpawnError`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs:1277-1281`（`spawn_with_objective` 开头）
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Consumes: `classify_terminal`、`task_state_label`（均在本文件，`TerminalKind` 现为私有，本任务内即可用）
- Produces:
  - `SpawnError::ParentSettled { state: &'static str }`
  - `spawn_with_objective` 在父为 `Settled` 时返回该错误，且**不插入**任何任务

- [ ] **Step 1: 写失败测试**

追加到 `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs` 末尾：

```rust
/// Drives a task to a settled terminal state through the public reducer.
fn settle_task(supervisor: &mut AgentSupervisor, task: &TaskId, event: TaskEvent) {
    supervisor
        .reduce_task(task, event)
        .unwrap_or_else(|error| panic!("reduce must succeed: {error}"));
}

fn attempt_of(supervisor: &AgentSupervisor, task: &TaskId) -> yi_agent_core::subagent::task::AttemptId {
    supervisor.task(task).unwrap().active_attempt_id().clone()
}

#[test]
fn a_completed_parent_cannot_spawn() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let parent = supervisor.root_task_id().clone();
    supervisor.start_task(&parent).unwrap();
    let attempt = attempt_of(&supervisor, &parent);
    settle_task(
        &mut supervisor,
        &parent,
        TaskEvent::WorkerCompletedNoChanges { attempt_id: attempt },
    );
    // WorkerCompletedNoChanges lands on CompletedNoChanges, NOT Completed —
    // verified by running it. Both are Settled, so the label is the other one.
    assert_eq!(
        supervisor.task(&parent).unwrap().state(),
        &TaskState::CompletedNoChanges
    );

    let before = supervisor.children_of(&parent).len();
    let error = supervisor.spawn(parent.clone()).unwrap_err();
    assert_eq!(error, SpawnError::ParentSettled { state: "completed_no_changes" });
    assert_eq!(
        supervisor.children_of(&parent).len(),
        before,
        "a refused spawn must not leave half a child behind"
    );
}

#[test]
fn a_failed_parent_cannot_spawn() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let parent = supervisor.root_task_id().clone();
    supervisor.start_task(&parent).unwrap();
    let attempt = attempt_of(&supervisor, &parent);
    settle_task(
        &mut supervisor,
        &parent,
        TaskEvent::WorkerFailed {
            attempt_id: attempt,
            failure: yi_agent_core::subagent::task::TaskFailure::new("boom"),
        },
    );
    assert_eq!(
        supervisor.spawn(parent.clone()).unwrap_err(),
        SpawnError::ParentSettled { state: "failed" }
    );
}

#[test]
fn a_cancelled_parent_cannot_spawn() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let parent = supervisor.root_task_id().clone();
    supervisor.start_task(&parent).unwrap();
    let attempt = attempt_of(&supervisor, &parent);
    settle_task(
        &mut supervisor,
        &parent,
        TaskEvent::CancelRequested {
            attempt_id: attempt,
            reason: yi_agent_core::subagent::task::CancelReason("test".into()),
        },
    );
    assert_eq!(
        supervisor.spawn(parent.clone()).unwrap_err(),
        SpawnError::ParentSettled { state: "cancelled" }
    );
}

#[test]
fn a_recovery_required_parent_can_still_spawn() {
    // The whole reason the gate uses classify_terminal instead of is_terminal:
    // recovery_required is terminal for bookkeeping but is a parked, resumable
    // state, and a resumed parent still needs to spawn.
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let parent = supervisor.root_task_id().clone();
    supervisor.start_task(&parent).unwrap();
    let attempt = attempt_of(&supervisor, &parent);
    settle_task(
        &mut supervisor,
        &parent,
        TaskEvent::RuntimeInterrupted {
            attempt_id: attempt,
            evidence: RecoveryEvidence("interrupted".into()),
        },
    );
    assert!(
        matches!(
            supervisor.task(&parent).unwrap().state(),
            TaskState::RecoveryRequired(_)
        ),
        "precondition: parent must be recovery_required"
    );
    let child = supervisor
        .spawn(parent.clone())
        .expect("a recovery-required parent must still be able to spawn");
    assert_eq!(supervisor.task(&child).unwrap().state(), &TaskState::Queued);
}

#[test]
fn a_running_parent_can_still_spawn() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let parent = supervisor.root_task_id().clone();
    supervisor.start_task(&parent).unwrap();
    supervisor
        .spawn(parent.clone())
        .expect("a running parent must spawn normally");
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor -- a_completed_parent_cannot_spawn a_failed_parent_cannot_spawn a_cancelled_parent_cannot_spawn
```

Expected: 编译失败，`no variant named 'ParentSettled' found for enum 'SpawnError'`

- [ ] **Step 3: 加错误变体**

`supervisor.rs` 的 `SpawnError`（第 33-40 行）改为：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpawnError {
    #[error("parent task does not exist")]
    ParentNotFound,
    #[error("leaf tasks cannot spawn descendants")]
    MaximumDepthReached,
    #[error("an agent may have at most {limit} direct children")]
    DirectChildLimitReached { limit: usize },
    #[error("a settled parent task (state: {state}) cannot spawn descendants")]
    ParentSettled { state: &'static str },
}
```

- [ ] **Step 4: 加闸门**

`supervisor.rs:1277-1281` 的 `spawn_with_objective` 开头，在 `ParentNotFound` 之后、`can_spawn_child` 之前插入：

```rust
    pub fn spawn_with_objective(
        &mut self,
        parent_id: TaskId,
        request: SpawnRequest,
    ) -> Result<TaskId, SpawnError> {
        let parent = self
            .tasks
            .get(&parent_id)
            .ok_or(SpawnError::ParentNotFound)?;
        // A settled parent is finished: any child spawned now falls outside the
        // terminal cascade (which only fires on the transition itself) and its
        // result has nowhere to go, because deliver_message refuses a terminal
        // recipient. Refuse the spawn instead of building a dead subtree.
        //
        // classify_terminal, not is_terminal: recovery_required is terminal for
        // bookkeeping but parked and resumable, and a resumed parent keeps
        // spawning.
        if let TerminalKind::Settled = classify_terminal(parent.state()) {
            return Err(SpawnError::ParentSettled {
                state: task_state_label(parent.state()),
            });
        }
        let child_depth = parent
            .depth
            .can_spawn_child()
            .map_err(|_| SpawnError::MaximumDepthReached)?;
```

注意借用：`classify_terminal(parent.state())` 与 `task_state_label(parent.state())` 都只在返回前读取 `parent`，`return` 结束借用，后续 `parent` 使用不受影响。

- [ ] **Step 5: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor -- a_completed_parent_cannot_spawn a_failed_parent_cannot_spawn a_cancelled_parent_cannot_spawn a_recovery_required_parent_can_still_spawn a_running_parent_can_still_spawn
```

Expected: `5 passed`

- [ ] **Step 6: 跑整个 core 测试，确认没有既有测试依赖「从终态父派生」**

```bash
ps aux | grep -v grep | grep cargo || echo "no cargo running"
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor
```

Expected: 全绿。若有既有测试失败，说明它依赖了本次要禁用的行为——先读该测试确认它测的是旧行为还是别的，**不要**为了让测试变绿而放宽闸门。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(core): refuse to spawn from a settled parent task

spawn_with_objective never inspected parent state, so a finished parent could
still create children. Those fall outside the terminal cascade and cannot
deliver back, producing a subtree that is dead on arrival.

The gate uses classify_terminal so recovery_required stays spawnable."
```

---

### Task 3: 变异验证闸门确实在生效

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`（临时改、验证后立刻改回）

**Interfaces:**
- Consumes: Task 2 的闸门与测试
- Produces: 无代码产物，只产出「测试真的锁住了取舍」这一结论

- [ ] **Step 1: 临时把闸门换成 `is_terminal()`**

把 Task 2 Step 4 插入的 `if let TerminalKind::Settled = ...` 改为：

```rust
        if parent.state().is_terminal() {
            return Err(SpawnError::ParentSettled {
                state: task_state_label(parent.state()),
            });
        }
```

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor -- a_recovery_required_parent_can_still_spawn
```

Expected: **FAIL** —— `a recovery-required parent must still be able to spawn`。若这条测试仍通过，说明它没有真正锁定取舍，必须修测试而不是继续。

- [ ] **Step 2: 还原闸门**

把 `if parent.state().is_terminal() {` 改回 `if let TerminalKind::Settled = classify_terminal(parent.state()) {`，恢复 `return Err(...)` 的原有缩进块。

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor -- a_recovery_required_parent_can_still_spawn
```

Expected: `1 passed`

- [ ] **Step 3: 确认工作区干净（没有把临时改动留下）**

```bash
cd .. && git status --porcelain
```

Expected: 无输出

---

### Task 4: runtime 层预检，保护 fork 上传

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:85-88`（`RuntimeCoordinatorError` 变体）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`spawn_child_with_objective` 内，`let supervisor = self.supervisor(session)?;` 之后、`take_fork_messages` 之前）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: `TaskState::from_state_label`（Task 1）、`classify_terminal`（Task 2）
- Produces:
  - `RuntimeCoordinatorError::ParentSettled { state: String }`
  - 父为 `Settled` 时，`spawn_child_with_objective` 在消耗 fork 上传之前返回该错误

**为什么需要这一层：** `take_fork_messages`（`runtime.rs:1180`）会 `uploads.remove(token)`。若只在 supervisor 拦截，fork 上传会先被消耗，用户重试时 token 已失效。因此预检必须排在它之前。

- [ ] **Step 1: 写失败测试**

追加到 `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs` 末尾。该文件顶部已有 `RuntimeCoordinator::open(&database, Arc::new(RecordingFactory))`、`coordinator.create_session()`、`coordinator.root_task_id(&session)` 的用法（见 `coordinator_persists_worker_failure_reported_by_the_factory`），照抄即可：

```rust
#[tokio::test]
async fn a_settled_parent_cannot_spawn_and_the_gate_runs_before_the_fork_token() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let coordinator = RuntimeCoordinator::open(&database, Arc::new(RecordingFactory)).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();

    // Settle the root: a cancelled root is one of the 8 settled terminal states.
    coordinator.cancel_task(&session, &root, false).await.unwrap();
    assert_eq!(coordinator.task_state(&root).unwrap(), "cancelled");

    let before = coordinator.children_of(&session, &root).unwrap().len();

    // A bogus fork token is the simplest possible ordering probe: the real
    // upload machine is irrelevant here. If the fork path ran first it would
    // report "fork upload token is unknown"; seeing ParentSettled proves the
    // pre-check is ahead of take_fork_messages.
    let error = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "child".into(),
            yi_agent_core::ChildWriteMode::ReadOnly,
            None,
            None,
            None,
            None,
            Some("definitely-not-a-real-fork-token".into()),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(
            &error,
            RuntimeCoordinatorError::ParentSettled { state } if state == "cancelled"
        ),
        "expected ParentSettled(cancelled); a burned or first-checked fork token \
         would surface as ForkUpload instead. got {error:?}"
    );

    // No ghost task row: the refusal happened before anything was persisted.
    assert_eq!(
        coordinator.children_of(&session, &root).unwrap().len(),
        before,
        "a refused spawn must not leave a queued ghost child"
    );
}
```

若该文件缺少 `RecordingFactory`，改用 `StaticWorkspaceFactory` 之外本文件已有的任意工厂（搜 `impl AgentWorkerFactory for`）。若 `coordinator.children_of` 不存在，用 `coordinator.task_state(&root)` 之外的等价公开查询；先在文件内搜 `children_of` 确认，没有则删掉这两处长度断言，改为断言第二次调用仍返回 `ParentSettled`（重复上面的调用再断言一次）。

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator -- a_settled_parent_cannot_spawn
```

Expected: 编译失败，`no variant named 'ParentSettled' found for enum 'RuntimeCoordinatorError'`

- [ ] **Step 3: 加错误变体**

`runtime.rs` 的 `RuntimeCoordinatorError`，在 `Spawn(#[from] SpawnError)`（第 87 行）之后插入：

```rust
    #[error("a settled parent task (state: {state}) cannot spawn descendants")]
    ParentSettled { state: String },
```

- [ ] **Step 4: 加预检**

`runtime.rs` 的 `spawn_child_with_objective` 中，在 `let supervisor = self.supervisor(session)?;` 之后、`take_fork_messages` 之前插入：

```rust
        // Refuse a settled parent here, before `take_fork_messages` consumes the
        // upload: the supervisor enforces the same rule, but by then the token is
        // already gone and the caller's retry would fail with an unhelpful
        // "unknown token" instead of the real reason.
        //
        // An unparseable state falls through to the supervisor's authoritative
        // check rather than refusing: the pre-check is a guard, not the verdict,
        // and wrongly refusing a live parent is worse than one wasted upload.
        let parent_state = self
            .repository
            .lock()
            .expect("runtime repository mutex poisoned")
            .task_state(parent)?;
        if let Some(state) = TaskState::from_state_label(&parent_state)
            && classify_terminal(&state) == TerminalKind::Settled
        {
            return Err(RuntimeCoordinatorError::ParentSettled {
                state: parent_state,
            });
        }
```

这需要 `classify_terminal` 与 `TerminalKind` 能从 store crate 访问。当前二者在 `supervisor.rs` 里是文件私有（第 1960、1966 行）。

最小改动是就地放开可见性，**不要**把它们搬到 `task.rs`：

```rust
// supervisor.rs:1960 附近
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    NonTerminal,
    Settled,
    RecoveryRequired,
}

pub fn classify_terminal(state: &TaskState) -> TerminalKind {
```

`yi-agent-core/src/subagent/mod.rs:4` 已经是 `pub mod supervisor;`，`ipc.rs` 也早已从 `yi_agent_core::subagent::supervisor::...` 引用 `SpawnError` / `WaitMode`，所以这条路径本来就是通的，不需要改 `lib.rs` 的 re-export。

`runtime.rs` 顶部按既有风格用完整路径导入（不要加进 `lib.rs` 的 re-export 列表，那里只放被多个 crate 广泛使用的类型）：

```rust
use yi_agent_core::subagent::supervisor::{classify_terminal, TerminalKind};
```



- [ ] **Step 5: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator -- a_settled_parent_cannot_spawn
```

Expected: `1 passed`

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(store): pre-check a settled parent before consuming a fork upload

The supervisor gate alone would let take_fork_messages remove the upload
first, so a refused spawn would burn the token and the caller's retry would
fail on an unknown token instead of the real reason."
```

---

### Task 5: IPC 让模型看到可读原因

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:2969`（`ipc_error_message`）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs`

**Interfaces:**
- Consumes: `RuntimeCoordinatorError::ParentSettled`（Task 4）
- Produces: `ipc_error_message` 对 `ParentSettled` 返回 `Some(...)`，让 `format_ipc_rejection` 输出 `daemon rejected spawn: invalid_state: a settled parent task (state: completed) cannot spawn descendants`

- [ ] **Step 1: 写失败测试**

`ipc_error_message` 是私有函数，外部集成测试看不到它，必须放在 `ipc.rs` 内部的单元测试里。文件里已有现成范式：`mod coding_requires_git_mapping_tests`（`ipc.rs:5279`）就以 `ipc_error_message(&error).expect("message")` 直接断言文案。

在该模块的 `}` 之前追加：

```rust
    #[test]
    fn a_settled_parent_rejection_carries_a_readable_message() {
        let error = IpcError::Runtime(RuntimeCoordinatorError::ParentSettled {
            state: "completed".into(),
        });
        let message = ipc_error_message(&error)
            .expect("a settled-parent rejection must not reach the model as a bare code");
        assert!(message.contains("settled parent"), "{message}");
        assert!(message.contains("completed"), "{message}");
        assert_eq!(ipc_error_code(&error), IpcErrorCode::InvalidState, "{message}");
    }
```

（`IpcError`、`RuntimeCoordinatorError`、`IpcErrorCode` 在该模块内已通过 `use super::*` 或文件顶部的 `use` 可见，照同模块既有测试的可见性处理。）

- [ ] **Step 2: 运行测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib a_settled_parent_rejection_carries_a_readable_message
```

Expected: FAIL，`called Option::unwrap() on a None value` / `a settled-parent rejection must not reach the model as a bare code`（`ipc_error_message` 的兜底返回 `None`）

- [ ] **Step 3: 加消息分支**

`ipc.rs:2969` 的 `ipc_error_message`，在 `DirectChildLimitReached` 分支之后插入：

```rust
        IpcError::Runtime(
            error @ RuntimeCoordinatorError::ParentSettled { .. },
        ) => Some(error.to_string()),
```

`RuntimeCoordinatorError` 的 `#[error(...)]` 已含状态，`to_string()` 即得完整句子。

- [ ] **Step 4: 运行测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib a_settled_parent_rejection_carries_a_readable_message
```

Expected: `1 passed`

- [ ] **Step 5: 确认错误码落到 InvalidState**

`ParentSettled` 是独立变体，不会命中 `ipc_error_code` 里 `Spawn(_)` 那一组的兜底。若 Step 1 的 `assert_eq!(ipc_error_code(&error), IpcErrorCode::InvalidState)` 失败，在 `ipc_error_code` 的 `Spawn(_)` 那一支旁边加上 `RuntimeCoordinatorError::ParentSettled { .. }`。

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib a_settled_parent_rejection_carries_a_readable_message
cd yi-agent-rs && cargo test -p yi-agent-store --lib
```

Expected: 两条都全绿。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A
git commit -m "feat(store): surface a readable reason when a settled parent is refused

Without a message branch the caller saw only the invalid_state code; the tool
layer renders 'daemon rejected spawn: <code>: <message>' and needs the text."
```

---

### Task 6: 端到端验收与文档

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`

**Interfaces:**
- Consumes: Task 1-5 全部产出
- Produces: 项目进度记录中的可验证条目

- [ ] **Step 1: 全量回归（按 crate，不要 --workspace）**

```bash
ps aux | grep -v grep | grep cargo || echo "no cargo running"
cd yi-agent-rs && cargo test -p yi-agent-core
cd yi-agent-rs && cargo test -p yi-agent-store
```

Expected: 两个 crate 全绿。

- [ ] **Step 2: 手工确认错误文案**

用真实 daemon 验证拒绝文案与 `recovery_required` 放行。若不便起 daemon，用 Task 2/Task 4 的测试断言代替，并在文档里写明用的是哪条测试。

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator -- a_settled_parent_cannot_spawn --nocapture
```

Expected: 通过，且错误文案含 `a settled parent task (state: completed) cannot spawn descendants`。

- [ ] **Step 3: 更新项目进度文件**

在 `docs/project-management/subagent-runtime.md` 的 feature 列表中追加一条（保持该文件既有的超长单行格式：问题 → 做法 → 代码位置 → 验证命令）：

```markdown
- [x] 已定终态父任务拒绝派生 — `spawn_with_objective` 此前只校验 depth 与 direct-child 上限，从不看 `parent.state()`，因此已完成的父仍能派生新子任务；这类子任务落在终态级联之外（级联只在状态转换那一刻触发），且 `deliver_message` 拒绝向终态 recipient 投递，结果是一棵注定无人消费的死子树。修复用同文件已有的 `classify_terminal`（`Settled` / `RecoveryRequired` / `NonTerminal` 三分类）作闸门，**不用** `is_terminal()`——后者把可恢复的 `RecoveryRequired` 也算终态，会误拒 resume 后仍需派生的父。runtime 层另加一层预检，排在 `take_fork_messages` 之前，避免拒绝路径白白消耗 fork 上传。代码：`yi-agent-rs/crates/yi-agent-core/src/subagent/{supervisor.rs,task.rs}`、`yi-agent-rs/crates/yi-agent-store/src/{runtime.rs,ipc.rs}`，验证：`cargo test -p yi-agent-core --test subagent_supervisor -- a_recovery_required_parent_can_still_spawn a_completed_parent_cannot_spawn`、`cargo test -p yi-agent-store --test runtime_coordinator -- a_settled_parent_cannot_spawn`、`cargo test -p yi-agent-store --lib a_settled_parent_rejection_carries_a_readable_message`
```

- [ ] **Step 4: 同步 README 计数**

读 `docs/project-management/README.md` 模块索引表，把 `subagent-runtime` 一行的「完成 / 总计」计数各加 1。**不要**凭记忆改数字，先 `grep subagent-runtime docs/project-management/README.md` 看实际值。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all --check
cd .. && git add -A
git commit -m "docs: record the settled-parent spawn gate

Adds the verifiable entry with the reason classify_terminal was chosen over
is_terminal, and points at the tests that lock that choice."
```

- [ ] **Step 6: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/settled-parent-spawn-gate -m "merge: refuse to spawn from a settled parent task"
git worktree remove .worktrees/feat-settled-parent-spawn-gate
git branch -d feat/settled-parent-spawn-gate
git log --oneline -3
```

Expected: 合并成功，worktree 与分支已清理。

---

## 实测验证记录

本计划的关键代码在起草阶段已用一次性 probe worktree 跑通（跑完即删，未留痕）：

| 验证项 | 结果 |
|---|---|
| Task 1 `from_state_label` 编译 | 通过 |
| Task 1 四条单测 | 4 passed |
| Task 2 `ParentSettled` 变体 + 闸门编译（含借用） | 通过，无借用冲突 |
| Task 2 五条测试 | 5 passed |
| Task 3 变异验证 | 改成 `is_terminal()` 后 `probe_a_recovery_required_parent_can_still_spawn` 由绿转红，错误为 `ParentSettled { state: "recovery_required" }` |

由此发现并已修正两处计划错误：

1. `WorkerCompletedNoChanges` 落到 `TaskState::CompletedNoChanges`，标签是 `completed_no_changes`，**不是** `Completed` / `completed`。原计划断言写错了。
2. `WaitSelector::AnyChild`（非 `Any`）、`PermissionRequestId::new()`（UUID newtype，非 `String::new()`）。

环境注意：`cargo` 不在默认 PATH，执行前先
`export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`。

---

## Self-Review

**1. Spec coverage**

| Spec 章节 | 覆盖任务 |
|---|---|
| §3.1 复用 `classify_terminal`，不用 `is_terminal` | Task 2 Step 4 + Task 3 变异验证 |
| §3.2 `SpawnError::ParentSettled` | Task 2 Step 3 |
| §3.3 判定在 `ParentNotFound` 之后、`can_spawn_child` 之前 | Task 2 Step 4 |
| §3.4 两层拦截 + 不另立清单 + 不用 `hydrated_application_root_state` | Task 1（互逆解析）+ Task 4（预检排在 fork 之前，且复用 Task 1） |
| §3.5 恢复路径不受影响 | Task 2 的 `a_recovery_required_parent_can_still_spawn`；`insert_recovered_child` 是本计划不碰的独立 API |
| §3.6 IPC 接线（message 必须加，code 兜底） | Task 5 |
| §4.1 core 测试 1-5 | Task 2 Step 1（5 条测试一一对应） |
| §4.2 store 测试 6-7（仅用公开 API，不读私有 `fork_uploads`） | Task 4 Step 1 |
| §4.3 变异验证 | Task 3 |
| §5.1 不做存量清扫 | Global Constraints + Task 6 文档条目不提清扫 |
| §5.2 不改结构不变式 | 计划未触碰 `settle_terminal` / `reduce_task` |
| §7 验收命令 | Task 6 Step 1-2 |

无遗漏。§6 的租约泄漏明确排除，未安排任务（符合 spec）。

**2. Placeholder scan**

无 placeholder。Task 4 的测试最初依赖未核实的 helper，现已改为只用已核实的真实 API：`RuntimeCoordinator::open` / `create_session` / `root_task_id`（`runtime_coordinator.rs:1792` 起的既有用法）、`cancel_task(&session, &task, recursive)`（`runtime.rs:2389`）、`task_state`（`runtime.rs:3169`）。

fork 顺序用一个假 token 就能证明，不需要真的建一次上传：`begin_fork_upload` 要求 application-root capability（`runtime.rs:1085`，经 `authorize_child_access` 鉴权），为它搭 fixture 只会让测试变脆。假 token 下若 fork 路径先跑，错误是 `ForkUpload("fork upload token is unknown")`；看到 `ParentSettled` 即证明预检在前。

其余步骤均含可直接粘贴的代码与可执行的命令。

**3. Type consistency**

- `SpawnError::ParentSettled { state: &'static str }` ↔ `task_state_label` 返回 `&'static str`：一致。
- `RuntimeCoordinatorError::ParentSettled { state: String }` ↔ `repository.task_state()` 返回 `String`：一致（store 侧持有所有权，故用 `String`；core 侧 `Copy` 约束要求 `&'static str`，两侧刻意不同且有理由）。
- `TaskState::from_state_label` 返回 `Option<TaskState>`；Task 4 用 `if let Some(state) = ... && classify_terminal(&state) == TerminalKind::Settled`：签名为 `&TaskState`，一致。
- Task 2 测试用 `supervisor.children_of(&parent).len()`：`children_of` 返回 `&[TaskId]`（`supervisor.rs:539`），`.len()` 可用。
- Task 4 的 `TaskState` 已在 `runtime.rs:29` 的 import 列表中，无需新增。
- Task 1 的 `PermissionRequestId::new()`（UUID newtype，非 `String`）与 `WaitSelector::AnyChild`（非 `Any`）已按真实定义修正，见 Task 1 Step 3 的说明。
- Task 4 不再移动 `classify_terminal`，只把 `supervisor.rs` 里两个符号改为 `pub`；`subagent::supervisor` 本就是 `pub mod`，路径成立。
