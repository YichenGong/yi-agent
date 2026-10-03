# 父任务终结时级联取消子代理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让「父任务进入已定终态 ⇒ 其子树全部进入终态并释放资源」成为超管的结构不变式，不再依赖每个调用点记得传参。

**Architecture:** 在 `AgentSupervisor` 内引入收口方法 `reduce_task`，替换 supervisor.rs 内全部 21 处直接 `.reduce(` 调用；`reduce_task` 归约后若任务落入「已定终态」，就报警（仅正常完成且有活子时）并递归取消子树；daemon 侧把返回的受影响 id 追加进状态变更列表，复用既有落库 + lease 释放路径。

**Tech Stack:** Rust（cargo workspace，`yi-agent-core` / `yi-agent-store`）、tokio、tracing、内置测试框架。

## Global Constraints

- **已定终态**（触发级联）恰好 8 个：`Completed`、`CompletedNoChanges`、`Blocked`、`Stalled`、`TimedOut`、`BudgetExhausted`、`Failed`、`Cancelled`。
- **必须排除**的终态：`RecoveryRequired`（`TaskState::is_terminal()` 会把它算作终态，级联判断**不能**直接用 `is_terminal()`）。
- 报警只在**父是正常完成**（`Completed` / `CompletedNoChanges`）却有活子任务时发出，用 `tracing::warn!`；其余终态静默级联。
- 级联范围**只限该任务的子树**，不影响同目录其它会话、不碰 `tasks` 中无亲缘关系的任务。
- 不改 UI 文案，不做用户可见通知，不追溯处理历史孤儿数据。
- cargo 不在默认 PATH：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`。
- 所有命令在 worktree 根执行（本仓库 Rust 代码在 `yi-agent-rs/` 子目录下；`Cargo.toml` 位于 `yi-agent-rs/`）。

---

### Task 1: 级联收口 `reduce_task` + 递归取消子树

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`（新增 `settle_terminal` / `reduce_task`，并替换 21 处 `.reduce(`）
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Consumes: 既有 `AgentTask::reduce(event, now) -> Result<TransitionResult, TaskReduceError>`、`AgentTask::state() -> &TaskState`、`AgentSupervisor::children_of(&TaskId) -> &[TaskId]`、`tasks: HashMap<TaskId, AgentTask>`。
- Produces:
  - `fn classify_terminal(state: &TaskState) -> TerminalKind`（枚举 `NonTerminal` / `Settled` / `RecoveryRequired`）
  - `fn settle_terminal(&mut self, task_id: &TaskId) -> Vec<TaskId>`
  - `fn reduce_task(&mut self, task_id: &TaskId, event: TaskEvent) -> Result<Vec<TaskId>, String>`

- [ ] **Step 1: 写失败测试（父失败 → 子级联终态）**

在 `tests/subagent_supervisor.rs` 末尾追加：

```rust
#[test]
fn failing_a_parent_cascades_its_live_child_to_terminal() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    supervisor.start_task(&root).unwrap();
    supervisor.start_task(&child).unwrap();

    supervisor.fail_task(&root, "worker crashed").unwrap();

    assert!(
        supervisor.task(&child).unwrap().state().is_terminal(),
        "the live child must be cascaded to terminal, got {:?}",
        supervisor.task(&child).unwrap().state()
    );
    assert!(
        matches!(
            supervisor.task(&child).unwrap().state(),
            TaskState::Cancelled(_)
        ),
        "cascade must cancel the child, got {:?}",
        supervisor.task(&child).unwrap().state()
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor failing_a_parent_cascades_its_live_child_to_terminal`
Expected: FAIL（子任务仍停在 `Running`，断言 `the live child must be cascaded to terminal` 失败）

- [ ] **Step 3: 加终态分类 helper**

在 `supervisor.rs` 内（`impl AgentSupervisor` 之外、文件级）加入：

```rust
/// Whether a state should trigger the parent-terminal cascade. Deliberately
/// narrower than `TaskState::is_terminal`: `RecoveryRequired` is terminal for
/// bookkeeping but is a *parked, resumable* state, so cascading on it would
/// kill children the parent could still need after a resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalKind {
    NonTerminal,
    Settled,
    RecoveryRequired,
}

fn classify_terminal(state: &TaskState) -> TerminalKind {
    match state {
        TaskState::Completed
        | TaskState::CompletedNoChanges
        | TaskState::Blocked(_)
        | TaskState::Stalled(_)
        | TaskState::TimedOut(_)
        | TaskState::BudgetExhausted(_)
        | TaskState::Failed(_)
        | TaskState::Cancelled(_) => TerminalKind::Settled,
        TaskState::RecoveryRequired(_) => TerminalKind::RecoveryRequired,
        _ => TerminalKind::NonTerminal,
    }
}
```

- [ ] **Step 4: 加 `settle_terminal`（报警 + 递归取消）**

在 `impl AgentSupervisor` 内加入：

```rust
/// After `task_id` has been reduced, enforce the tree invariant: a task in a
/// settled terminal state must not leave live descendants behind. Returns only
/// ids whose state actually changed, so callers persist exactly what moved.
fn settle_terminal(&mut self, task_id: &TaskId) -> Vec<TaskId> {
    let Some(state) = self.tasks.get(task_id).map(|task| task.state().clone()) else {
        return Vec::new();
    };
    if classify_terminal(&state) != TerminalKind::Settled {
        return Vec::new();
    }
    if matches!(state, TaskState::Completed | TaskState::CompletedNoChanges) {
        let live: Vec<TaskId> = self
            .collect_subtree(task_id)
            .into_iter()
            .filter(|id| {
                self.tasks
                    .get(id)
                    .is_some_and(|task| classify_terminal(task.state()) == TerminalKind::NonTerminal)
            })
            .collect();
        if !live.is_empty() {
            tracing::warn!(
                ?task_id,
                live_children = live.len(),
                "task reached a successful terminal state while live descendants remained; cascading cancellation"
            );
        }
    }
    let mut changed = Vec::new();
    for id in self.collect_subtree(task_id) {
        if id == *task_id {
            continue;
        }
        let Some(task) = self.tasks.get_mut(&id) else {
            continue;
        };
        if classify_terminal(task.state()) != TerminalKind::NonTerminal {
            continue;
        }
        if let Some(worker) = self.workers.get(&id) {
            worker.cancel();
        }
        let attempt_id = task.active_attempt_id().clone();
        if task
            .reduce(
                TaskEvent::CancelRequested {
                    attempt_id,
                    reason: CancelReason("parent reached a terminal state".into()),
                },
                chrono::Utc::now(),
            )
            .is_ok()
        {
            changed.push(id);
        }
    }
    if !changed.is_empty() {
        self.notify_update();
    }
    changed
}

fn collect_subtree(&self, task_id: &TaskId) -> Vec<TaskId> {
    let mut out = Vec::new();
    self.collect_subtree_into(task_id, &mut out);
    out
}

fn collect_subtree_into(&self, task_id: &TaskId, out: &mut Vec<TaskId>) {
    out.push(task_id.clone());
    for child in self.children_of(task_id) {
        self.collect_subtree_into(child, out);
    }
}
```

- [ ] **Step 5: 加 `reduce_task` 收口方法**

```rust
/// The single entry point every state-changing reduce must go through. It
/// performs the reduction, then enforces the parent-terminal cascade. Returns
/// every task id whose state changed (the reduced task plus any cascade
/// victims), so the runtime coordinator can persist and release each one.
pub fn reduce_task(&mut self, task_id: &TaskId, event: TaskEvent) -> Result<Vec<TaskId>, String> {
    {
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| "task does not exist".to_string())?;
        task.reduce(event, chrono::Utc::now())
            .map_err(|error| error.to_string())?;
    }
    let mut changed = vec![task_id.clone()];
    changed.extend(self.settle_terminal(task_id));
    self.notify_update();
    Ok(changed)
}
```

借用在第一个代码块结束时释放，之后 `settle_terminal` 才能可变借用其它任务——不要试图在一个借用里同时归约目标任务与级联子任务。

调用方要注意：`reduce` 原本由调用方校验 attempt（`reduce` 内部会比对 `event.attempt_id()` 与 `active_attempt_id()`），本方法不改这一语义，事件构造仍需带正确的 `attempt_id`（既有 21 处调用均已如此）。

- [ ] **Step 6: 替换全部 21 处 `.reduce(` 调用**

把 `supervisor.rs` 内每一处 `task.reduce(EVENT, chrono::Utc::now())` 改成 `self.reduce_task(&id, EVENT)`。逐处处理，注意：

- 上下文已持有 `task`（`get_mut` 结果）的地方，改成先取到 `id`、`drop` 掉借用，再 `self.reduce_task(&id, ...)`。
- `reduce` 返回值被使用的地方（如下）保持语义：
  - `rework_review`（约 1662 行）：`.reduce(...)?.new_attempt`
  - `retry_task`（约 1769 行）：`.reduce(...)?.new_attempt`
  - `reconcile_worker_events` 内 `Delivered` 分支：需要 `attempt_id` 与 `parent_id`，在归约前先取出。
- `reconcile_worker_events` 的 `changed` 列表：把 `reduce_task` 返回的所有 id 追加进去（取代原来只 push `task_id`）。这使级联出的子任务自动进入 daemon 的落库 + lease 释放路径。
- `cancel_task_tree` 保持现状（已处理整棵子树）；可改为内部走 `reduce_task` 以求统一，但不是必须。

- [ ] **Step 7: 运行该测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor failing_a_parent_cascades_its_live_child_to_terminal`
Expected: PASS

- [ ] **Step 8: 跑整个 core 测试套件**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core`
Expected: PASS（若 `a_coding_delivery_to_a_terminal_parent_does_not_wedge_reconciliation` 失败，见 Task 2）

- [ ] **Step 9: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "feat(core): cascade child cancellation when a parent task settles"
```

---

### Task 2: 对齐既有测试「父终态时子交付」的新语义

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs:66-110`（`a_coding_delivery_to_a_terminal_parent_does_not_wedge_reconciliation`）

**Interfaces:**
- Consumes: `AgentSupervisor::spawn/start_task/fail_task/start_worker/reconcile_worker_events`。
- Produces: 无新接口。

**背景（必读）：** 该测试当前断言「父已终态时，子任务的交付仍被记录（停在 `AwaitingParentReview`）」。本设计刻意反转这一点：**父已终态 ⇒ 子任务不该还在跑，值得交付的东西也不再有观众**。测试必须改写为新不变式，而不是删掉——它原本防护的是「投递到已终态父不能 wedge 整轮 reconcile」，这个防护在竞争窗口下依然成立（子任务在被级联取消前可能已投递）。

- [ ] **Step 1: 改写测试断言**

把断言区（约 95-109 行）

```rust
    assert!(
        reconciled.is_ok(),
        "a delivery to a terminal parent must not wedge reconciliation, got {reconciled:?}"
    );
    assert!(
        matches!(
            supervisor.task(&child).unwrap().state(),
            TaskState::AwaitingParentReview(_)
        ),
        "the child's delivery must still be recorded, got {:?}",
        supervisor.task(&child).unwrap().state()
    );
```

改为

```rust
    assert!(
        reconciled.is_ok(),
        "a delivery to a terminal parent must not wedge reconciliation, got {reconciled:?}"
    );
    assert!(
        supervisor.task(&child).unwrap().state().is_terminal(),
        "a child of a settled parent must not keep running, got {:?}",
        supervisor.task(&child).unwrap().state()
    );
```

并把该测试的文档注释里 "must not throw away the child's completion fact" 一段改写为：级联设计下，已终态父的活子任务会被取消；本测试守护的是「无论投递是否被接受，reconcile 都不得 wedge」。

- [ ] **Step 2: 运行该测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor a_coding_delivery_to_a_terminal_parent_does_not_wedge_reconciliation`
Expected: PASS

- [ ] **Step 3: 全量 core 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "test(core): align terminal-parent delivery test with cascade semantics"
```

---

### Task 3: 报警行为与恢复类不级联（TDD）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/Cargo.toml`（dev-dependencies 增补，见 Step 1）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Consumes: Task 1 的 `reduce_task` / `settle_terminal`；`interrupt_worker_for_recovery(&mut self, ...) -> Result<...>`（归约到 `RecoveryRequired`）。
- Produces: 无新接口（仅行为）。

- [ ] **Step 1: 加测试期日志捕获辅助**

`tracing-subscriber` 已在 dev-dependencies，但缺 `fmt` feature。把 core 的 `[dev-dependencies]` 中该行改为：

```toml
tracing-subscriber = { version = "0.3", default-features = false, features = ["std", "registry", "fmt", "env-filter"] }
```

在测试文件加入（若 `tracing` 尚未在 dev-deps 中可用，用 `yi_agent_core` 已重导出的 `tracing`；否则在 dev-dependencies 加 `tracing = "0.1"`，它是 lib 依赖，测试可直接 `use tracing`）：

```rust
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct CaptureWarns(Arc<Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureWarns {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        struct V(String);
        impl tracing::field::Visit for V {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut v = V(String::new());
        event.record(&mut v);
        self.0.lock().unwrap().push(v.0);
    }
}
```

- [ ] **Step 2: 写「正常完成有活子 → 报警」测试**

```rust
#[tokio::test]
async fn a_completed_parent_with_a_live_child_warns_and_cascades() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    let factory = HandleCapturingWorkerFactory::default();
    supervisor.start_worker(&factory, &child).await.unwrap();

    let captured = CaptureWarns::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    // The root completes normally while the child is still running.
    supervisor.start_task(&root).unwrap();
    supervisor
        .reduce_task(&root, TaskEvent::WorkerCompletedNoChanges {
            attempt_id: supervisor.task(&root).unwrap().active_attempt_id().clone(),
        })
        .unwrap();

    assert!(
        supervisor.task(&child).unwrap().state().is_terminal(),
        "the live child must be cascaded, got {:?}",
        supervisor.task(&child).unwrap().state()
    );
    assert!(
        captured.0.lock().unwrap().iter().any(|m| m.contains("live descendants")),
        "a successful terminal with a live child must warn, got {:?}",
        captured.0.lock().unwrap()
    );
}
```

（`reduce_task` 是 `pub`，可从集成测试调用。若 `WorkerCompletedNoChanges` 需要 root 处于 `Running`，上面已 `start_task`。）

- [ ] **Step 3: 写「恢复类终态不级联」测试**

```rust
#[test]
fn a_recovery_required_parent_does_not_cascade() {
    let mut supervisor = AgentSupervisor::new(RootSessionId::new());
    let root = supervisor.root_task_id().clone();
    let child = supervisor.spawn(root.clone()).unwrap();
    supervisor.start_task(&root).unwrap();
    supervisor.start_task(&child).unwrap();

    let attempt_id = supervisor.task(&root).unwrap().active_attempt_id().clone();
    supervisor.reduce_task(
        &root,
        TaskEvent::RuntimeInterrupted {
            attempt_id,
            evidence: RecoveryEvidence("safe checkpoint grace deadline elapsed".into()),
        },
    ).unwrap();

    assert!(
        matches!(supervisor.task(&root).unwrap().state(), TaskState::RecoveryRequired(_)),
        "fixture requires a recovery-required root, got {:?}",
        supervisor.task(&root).unwrap().state()
    );
    assert!(
        !supervisor.task(&child).unwrap().state().is_terminal(),
        "recovery-required must not cascade, got {:?}",
        supervisor.task(&child).unwrap().state()
    );
}
```

- [ ] **Step 4: 运行两个测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_supervisor a_completed_parent_with_a_live_child_warns_and_cascades a_recovery_required_parent_does_not_cascade`
Expected: PASS

- [ ] **Step 5: 变异验证（证明测试真的在测级联）**

临时在 `settle_terminal` 第一行插入 `return Vec::new();`，重跑 Step 4 的两个测试。
Expected: `a_completed_parent_with_a_live_child_warns_and_cascades` 与 Task 1 的失败测试均 FAIL。
然后**撤销**该临时改动。

- [ ] **Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/Cargo.toml yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "test(core): cover cascade warning and recovery-required exclusion"
```

---

### Task 4: daemon 接线——看门狗/失败路径也落库级联结果

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（`record_watchdog_terminal` 约 1950-1985；`fail_task` 调用点约 1506；`cancel_task` 约 2040）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: core 的 `reduce_task`（若 supervisor 的 `timeout_task` / `exhaust_task_budget` / `stall_task` / `fail_task` 改为返回 `Vec<TaskId>`，则走它们；**推荐**改这些方法返回受影响 id，保持 daemon 现有落库循环不变）。
- Produces: supervisor 的 `fail_task` / `stall_task` / `timeout_task` / `exhaust_task_budget` 返回类型由 `Result<(), String>` 变为 `Result<Vec<TaskId>, String>`。

- [ ] **Step 1: 改 supervisor 四个方法返回受影响 id**

`fail_task`（约 1445 行）改为：

```rust
pub fn fail_task(&mut self, task_id: &TaskId, message: String) -> Result<Vec<TaskId>, String> {
    let attempt_id = self
        .tasks
        .get(task_id)
        .ok_or_else(|| "task does not exist".to_string())?
        .active_attempt_id()
        .clone();
    self.reduce_task(
        task_id,
        TaskEvent::WorkerFailed {
            attempt_id,
            failure: TaskFailure::new(message),
        },
    )
}
```

`reduce_watchdog_event`（约 1487 行）末行由
`task.reduce(event(attempt_id), chrono::Utc::now()).map_err(|error| error.to_string())?;`
改为 `self.reduce_task(task_id, event(attempt_id))`，返回类型 `Result<Vec<TaskId>, String>`；
`stall_task` / `timeout_task` / `exhaust_task_budget` 随之改为返回 `Result<Vec<TaskId>, String>`。
（原方法末尾各自的 `self.notify_update(); Ok(())` 可删，`reduce_task` 已负责通知。）

- [ ] **Step 2: daemon 侧把返回的 id 一并落库 + 释放 lease**

`record_watchdog_terminal`（约 1950-1985）改为：

```rust
let affected = match terminal {
    WatchdogTerminal::Stalled => supervisor.stall_task(task, evidence),
    WatchdogTerminal::TimedOut(kind) => supervisor.timeout_task(task, kind),
    WatchdogTerminal::BudgetExhausted(kind) => supervisor.exhaust_task_budget(task, kind),
}.map_err(...)?;
drop(supervisor); // 结束借用
for id in &affected {
    // 既有 repository.transition_task_and_attempt_with_terminal(...) 逻辑，按各终态映射
    self.release_resident_lease(id);
}
```

`runtime.rs:1506` 的 `let _ = supervisor.fail_task(task, error.to_string());` 同样改为遍历返回值逐个落库 + 释放 lease（或并入既有批量落库点）。

- [ ] **Step 3: 写 store 层测试（父超时 → 子终态 + lease 释放）**

在 `runtime_coordinator.rs` 新增测试，复用该文件既有的会话/任务 fixture 与 `has_active_lease_prefix` 断言。骨架：

```rust
#[tokio::test]
async fn a_watchdog_timed_out_parent_cascades_and_releases_its_childs_lease() {
    // 复用本文件既有 fixture：建立 coordinator、root = <parent>、child = <child>，
    // 令 child 处于 Running 且已通过 workspace lease 认领了其 workspace。
    let (coordinator, parent, child) = /* 既有 fixture 构造 */;

    assert!(
        coordinator.has_active_lease_prefix(&child, "workspace:"),
        "fixture requires the child to hold a workspace lease"
    );

    coordinator
        .record_watchdog_terminal(&parent, WatchdogTerminal::TimedOut(TimeoutKind::Idle))
        .await
        .unwrap();

    assert_eq!(coordinator.task_state(&child).unwrap(), "cancelled");
    assert!(
        !coordinator.has_active_lease_prefix(&child, "workspace:"),
        "the cascaded child must release its workspace lease"
    );
}
```

- [ ] **Step 4: 运行 store 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-store/src/runtime.rs yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs
git commit -m "feat(store): persist and release cascaded children on terminal paths"
```

---

### Task 5: 全量验证与文档

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`（注释）
- Modify: `docs/superpowers/specs/2026-10-02-subagent-cascade-on-terminal-design.md`（状态更新）

- [ ] **Step 1: 全量测试**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo test --workspace`
Expected: PASS（记录通过数）

- [ ] **Step 2: clippy**

Run: `cd yi-agent-rs && cargo clippy --workspace --all-targets 2>&1 | tail -n 20`
Expected: 新增代码零告警

- [ ] **Step 3: 在 `reduce_task` 上补文档注释**

说明这是唯一的归约入口、级联不变式、以及 `RecoveryRequired` 为何排除。

- [ ] **Step 4: 更新 spec 状态行**

把设计文档头部「状态：设计已确认，待实施」改为「状态：已实施（见 plan 2026-10-02-subagent-cascade-on-terminal）」。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs docs/superpowers/specs/2026-10-02-subagent-cascade-on-terminal-design.md
git commit -m "docs: record cascade invariant and mark spec implemented"
```

---

## 风险提示（实施者必读）

- **最大的坑**：把某个「故意不级联」的调用点误改成级联。替换 21 处 reduce 时，凡是**非状态转移**的调用（例如仅触发 `AdmissionGranted`、`PermissionRequested`、`PauseAcknowledged`、`MessageConsumed` 这类不可能进终态的事件）也应走 `reduce_task`——它们不会触发级联（`settle_terminal` 会直接返回空），但统一走一个入口可保证不漏。
- `reconcile_worker_events` 的 `Delivered` 分支不得因为级联而丢掉「投递到已终态父」的容错：归约与通知的顺序保持现状。
- `AwaitingParentReview` 的子任务在父正常终结时会被取消；这是本设计的既定行为（见 spec §3），已由 Task 2 的测试锚定。
