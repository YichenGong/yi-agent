# 删除父审门（Drop Parent-Review Gate）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除 `AwaitingParentReview` 父审状态，子任务交付即落 `Completed`，同时整条 review 决策面（accept/rework/reject）退出代码库。

**Architecture:** 状态机 `Running --WorkerDelivered--> Completed` 取代 `Running --WorkerDelivered--> AwaitingParentReview --ReviewAccepted--> Completed`。core 先删枚举与超管方法，store 跟随删持久化/IPC 面并加 schema v14 迁移把历史行改写为 `completed`，最后清工具与前端。父仍在交付上报那一刻经 mailbox 收到 `MessageKind::Completed(delivery)`，只是不再需要回审。

**Tech Stack:** Rust（edition 2024，workspace 14 crate）、rusqlite/SQLite、serde、tokio；依据 spec `docs/superpowers/specs/2026-10-09-drop-parent-review-gate-design.md`。

> **行号基准（2026-10-14 rebase 核验）**：本分支已 rebase 到 `main`（`065a4e0d`）。
> 自本计划原始基点（`40cd506e`）以来 `main` 前进 214 个提交，但**后端 6 文件与全部测试文件
> 未被触碰**（`task.rs`、`supervisor.rs`、`mailbox.rs`、`repository.rs`、`runtime.rs`、`ipc.rs`、
> `yi-agent-subagent/src/lib.rs`，逐文件 `git diff 40cd506e..main` 为空），故 Task 1-3、Task 5 及
> spec 的行号引用**仍然精确**。只有 **Task 4 的前端 3 文件漂移**：`app-server/src/server.rs`
> （+1040）、`tui/app.rs`（+175）、`main.rs`（+1）。下方 Task 4 的行号已按实测值回写；执行时仍以
> 编译器错误 + `grep -n` 现查为准。

## Global Constraints

- **严禁在 `main` 上改代码或提交**：先 `git worktree add .worktrees/<branch> -b <type>/<name>`；改完在 worktree 内跑测试 + `cargo fmt --all`。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）；commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。
- **不要并发跑多个 `cargo test`**：每次 cargo 前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留；优先按 crate 跑，**不要** `cargo test --workspace`。
- **删除 review 事件枚举后，历史 `events` 行会因未知 kind 硬报错**（`RuntimeEvent::parse` → `RepositoryError::UnknownEventKind`，`repository.rs:214`）；schema v14 迁移**必须**把这些 kind 改写成 `task_completed`。
- **不可误删**：`delivery_diff`（`runtime.rs:3119`）与 `ReadTaskDiff` / `TaskDiff`（TUI diff 视图、CLI `/diff`、父查看子交付共用）；`from_hydrated_review_root` / `insert_hydrated_review_child` / `hydrated_review_task`（application-root hydration 复用，`runtime.rs:1048`）；`MessageKind::Completed(delivery)` 投递；`hydrated_application_root_state`（`runtime.rs:4033`）。
- Rust 注释中文、讲「为什么」；测试名英文 snake_case。

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs` | 状态机、事件、`DeliveryState`、`IntegrationValidation` | 改 |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` | 超管 review 方法、`children_needing_attention` | 改 |
| `yi-agent-rs/crates/yi-agent-core/src/subagent/mailbox.rs` | `ReworkInstruction`、review 消息种类 | 改 |
| `yi-agent-rs/crates/yi-agent-store/src/repository.rs` | 持久化、schema v14、`RuntimeEvent`、review repo 函数 | 改 |
| `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` | coordinator review 方法、reconcile 分支、hydration | 改 |
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | review IPC 面 | 改 |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | `review_agent` 工具 | 改 |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | `/accept` `/rework` `/reject` | 改 |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | CLI review 动作 | 改 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `DELEGATION_TOOLS` | 改 |

依赖方向：`task.rs` → `supervisor.rs` → `runtime.rs`/`repository.rs` → `ipc.rs` → 工具/前端。

---

### Task 1: core 状态机去 review

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/mailbox.rs`
- Test: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`、`task.rs` 底部 `#[cfg(test)]`

**Interfaces:**
- Consumes: 无（本任务是最内层）。
- Produces:
  - `TaskState` 无 `AwaitingParentReview` 变体；`WorkerDelivered` 归约目标为 `TaskState::Completed`。
  - `TaskEvent` 无 `ReviewAccepted` / `ReviewRework` / `ReviewRejected`。
  - `AgentTask` 无 `delivery: DeliveryState` 字段与 `delivery()` 方法；`AgentTask::hydrated_review_task(...)` 签名去掉 `delivery` 参数。
  - `MessageKind` 无 `Rework` / `ReviewRejected`；`ReworkInstruction` 删除。

- [ ] **Step 1: 写失败测试（core）**

在 `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs` 的 `#[cfg(test)]` 内，新增（替换现有 `only_parent_with_successful_integration_can_accept_review` / `root_task_cannot_accept_its_own_review` / `review_events_require_the_current_delivery` / `retry_and_rework_create_attempts_without_erasing_evidence`）：

```rust
/// 交付直接落终态：不再有中间审阅态。
#[test]
fn worker_delivery_completes_the_child_directly() {
    let (mut task, workspace) = task_with_workspace();
    let now = Utc::now();
    let attempt_id = task.active_attempt_id().clone();
    task.reduce(
        TaskEvent::AdmissionGranted {
            attempt_id: attempt_id.clone(),
        },
        now,
    )
    .unwrap();
    let delivery = coding_delivery(workspace);
    task.reduce(
        TaskEvent::WorkerDelivered {
            attempt_id,
            delivery: delivery.clone(),
        },
        now,
    )
    .unwrap();
    assert_eq!(task.state(), &TaskState::Completed);
    assert!(task.state().is_terminal());
    assert_eq!(
        task.active_attempt().delivery.as_ref(),
        Some(&delivery),
        "attempt 仍保留交付报告，供父读取 commit"
    );
}
```

同时删除对 `TaskState::AwaitingParentReview`、`TaskEvent::ReviewAccepted/Rework/Rejected`、`IntegrationValidation::passed/failed`、`DeliveryState::*` 的测试断言（编译错误会逐条指出）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent::task`
Expected: 编译失败，报 `no variant named AwaitingParentReview` 等（或新测试 `worker_delivery_completes_the_child_directly` 断言失败）。

- [ ] **Step 3: 改 `task.rs`**

删除 `TaskState::AwaitingParentReview(DeliveryId)` 变体、`can_transition_to` 中相关两处（`Running -> ...` 列里的 `Self::AwaitingParentReview(_)`、`AwaitingParentReview -> ...` 整条边）。

删除 `TaskEvent::{ReviewAccepted, ReviewRework, ReviewRejected}` 声明（`task.rs:833-845`）及 `attempt_id()` 中三处匹配臂（`task.rs:899-901`）。

删除 `TaskReduceError::{DeliveryMismatch, ReviewRequiresParent, ReviewActorMismatch, IntegrationNotValidated}`（`task.rs:929-951`）与 `IntegrationId`（`task.rs:51`）、`IntegrationValidation`（`task.rs:325-352`）、`DeliveryState` 枚举（`task.rs:246-268`）、`require_review_delivery`（`task.rs:1153`）。

删除归约中三个 review 事件分支（`task.rs:1040-1094`）。

改 `WorkerDelivered` 分支（`task.rs:1020-1033`）为：

```rust
        TaskEvent::WorkerDelivered { delivery, .. } => {
            delivery
                .validate_for(task)
                .map_err(|reason| TaskReduceError::InvalidDeliveryEvidence { reason })?;
            task.active_attempt_mut().delivery = Some(delivery.clone());
            transition(task, TaskState::Completed, now)?;
        }
```

删除 `AgentTask.delivery` 字段（`task.rs:640, 536, 578` 等构造点）、`delivery()` 方法（`task.rs:713`）、`start_next_attempt` 的 `clear_delivery` 参数与 `self.delivery` 赋值（`task.rs:734-752`）。

改 `hydrated_review_task`（`task.rs:632`）签名为去掉 `delivery: Option<DeliveryReport>` 参数，并删其中的 `DeliveryState::ReadyForReview` 匹配（`task.rs:655-659`）。

- [ ] **Step 4: 改 `supervisor.rs`**

删除 `tasks_awaiting_parent_review`（`supervisor.rs:507-516`）、`accept_review`（1740）、`rework_review`（1768）、`stage_review_with_persistence`（1783）、`reduce_rework_review`（1810）、`reject_review`（1849）、`ReviewPersistenceError`（141）。

简化 `children_needing_attention`（`supervisor.rs:1192-1204`）：

```rust
    /// Children whose outcome the caller must act on: finished children, or
    /// children that produced a completion report. A delivered child is now
    /// terminal on delivery, so `is_terminal()` covers the old review wait.
    fn children_needing_attention(&self, caller: &TaskId) -> Vec<TaskId> {
        self.children_of(caller)
            .iter()
            .filter(|child| {
                self.task(child).is_some_and(|task| {
                    task.state().is_terminal()
                        || self.completion_reports.contains_key(*child)
                })
            })
            .cloned()
            .collect()
    }
```

worker 事件循环里 `AwaitingParentReview` 的 worker 摘除分支（`supervisor.rs:966-975`）保留 `Paused`、去掉 `AwaitingParentReview`：

```rust
                if self.tasks.get(&id).is_some_and(|task| {
                    task.state().is_terminal() || matches!(task.state(), TaskState::Paused(_))
                }) {
```

- [ ] **Step 5: 改 `mailbox.rs`**

删除 `ReworkInstruction`（`mailbox.rs:23`）、`MessageKind::{Rework, ReviewRejected}`（`mailbox.rs:36-37`）、`priority()` 中两项（`mailbox.rs:49-50`）、`is_rework`（`mailbox.rs:229-233`）、`pending_worker_inputs` 中 `Rework` 分支（`mailbox.rs:270-276`）。

- [ ] **Step 6: 更新 core 集成测试**

在 `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`：
- 改 `child_completion_snapshot_reports_a_childs_delivered_commit`（第 24 行）：删 `accept_review` 调用与 `IntegrationValidation` import，`reconcile_worker_events()` 后直接断言 `reports[0].state == "completed"`、`delivery == Some("deadbeef")`。
- 改 `a_parent_that_settles_cascades_a_child_already_awaiting_review`（第 1425 行）：交付后子已是 `Completed`（终态），故断言文案改为"已完成的子不因父 settle 被再次取消"，或改测"父 settle 时子的终态保持"；把 `AwaitingParentReview` 断言换成 `Completed`。
- 删除仅针对 review 的用例（若编译报未使用 import，一并清）。

- [ ] **Step 7: 跑 core 测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent && cargo test -p yi-agent-core --test subagent_supervisor`
Expected: PASS。

- [ ] **Step 8: Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add yi-agent-rs/crates/yi-agent-core
git commit -m "refactor(core): 子任务交付即完成，删除父审状态与 review 事件"
```

---

### Task 2: store 持久化与 schema v14

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs`

**Interfaces:**
- Consumes: Task 1 的 `TaskState`（无 `AwaitingParentReview`）。
- Produces:
  - `RuntimeRepository::record_delivery(&mut self, task: &TaskId, attempt: &AttemptId, delivery: &DeliveryReport) -> Result<i64, RepositoryError>`（原 `record_delivery_for_review` 改名，落 `completed`）。
  - `LATEST_SCHEMA_VERSION = 14`。
  - `RuntimeEvent` 无 `ReviewApproved` / `ReviewAccepted` / `ReviewRework` / `ReviewRejected`。

- [ ] **Step 1: 写失败测试**

在 `yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs` 新增：

```rust
/// 交付落库即 `completed`，不再是 awaiting_parent_review。
#[test]
fn record_delivery_commits_the_task_as_completed() {
    let mut repo = /* 既有测试构造器，参照本文件其它用例 */;
    let (task, attempt, delivery) = /* 既有 fixture */;
    repo.record_delivery(&task, &attempt, &delivery).unwrap();
    assert_eq!(repo.task_state(&task).unwrap(), "completed");
}

/// v13 库里的 awaiting_parent_review 行迁移后变 completed，
/// 且旧 review_* 事件 kind 被改写，避免 parse 报未知 kind。
#[test]
fn migration_v14_folds_review_state_into_completed() {
    // 建一个 v13 库，插入 state_json='awaiting_parent_review' 的 task、
    // attempts.state='awaiting_parent_review'、一条 kind='review_accepted' 的 events 行、
    // 一条 kind='rework' 的 mailbox_messages 行；
    // 重新 open（触发迁移）后断言：
    //   tasks.state_json == 'completed'
    //   attempts.state == 'completed'
    //   events.kind == 'task_completed'
    //   mailbox_messages 中 kind IN ('rework','review_rejected') 为 0 行
    //   ReadTaskEvents 能成功解析（不再 UnknownEventKind）
}
```

（`/* ... */` 处用本文件既有的 `open_temp_repository()` 风格构造器；实现时照抄相邻用例的建库与 fixture 写法，不引入新 helper。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test repository_decisions migration_v14`
Expected: FAIL（`LATEST_SCHEMA_VERSION` 仍为 13；`record_delivery` 未定义）。

- [ ] **Step 3: 改 `repository.rs` 的迁移**

`repository.rs:18` 改 `const LATEST_SCHEMA_VERSION: i64 = 14;`，并在 `migrate()` 末尾（`repository.rs:4821` 的 v13 块之后、`Ok(())` 之前）追加：

```rust
    // v14：父审状态下线。历史行改写为 `completed`，并清掉 review-only 的
    // mailbox 消息与事件 kind——否则删除枚举后 `RuntimeEvent::parse` 会对
    // 残留 kind 直接报 UnknownEventKind，让 ReadTaskEvents 整条读取失败。
    if current_version < 14 {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(
            "UPDATE tasks SET state_json = 'completed'
                 WHERE state_json = 'awaiting_parent_review';
             UPDATE attempts SET state = 'completed'
                 WHERE state = 'awaiting_parent_review';
             DELETE FROM mailbox_messages
                 WHERE kind IN ('rework', 'review_rejected');
             UPDATE events SET kind = 'task_completed'
                 WHERE kind IN ('review_approved', 'review_accepted',
                                'review_rework', 'review_rejected');",
        )?;
        transaction.execute("INSERT INTO schema_migrations (version) VALUES (14)", [])?;
        transaction.commit()?;
    }
```

同步改版本断言（`repository.rs:4873`）：`assert_eq!(LATEST_SCHEMA_VERSION, 14);`

- [ ] **Step 4: 改 `record_delivery` 与状态字符串**

`repository.rs:1542` 的 `record_delivery_for_review` 改名 `record_delivery`，函数体两处状态字符串改 `completed`：

```rust
        let changed = transaction.execute(
            "UPDATE tasks
             SET state_json = 'completed', delivery_json = ?1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ?2 AND active_attempt_id = ?3 AND state_json = 'running'",
            params![delivery_json, task.to_string(), attempt.to_string()],
        )?;
        if changed == 0 {
            return Err(RepositoryError::TaskNotReadyForDelivery {
                task: task.to_string(),
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = 'completed'
             WHERE id = ?1 AND task_id = ?2",
            params![attempt.to_string(), task.to_string()],
        )?;
```

（`deliveries` 表写入、`mailbox_messages` 的 `'completed'` 插入、`TaskDelivered` + `MailboxMessageQueued` 事件全部保留不动。）

- [ ] **Step 5: 删 review 持久化函数与事件变体**

删除 `accept_delivery_review`（1632）、`rework_delivery_review`（1846）、`reject_delivery_review`（2023）、`delivery_has_review`（1831）、`review_hydration_tasks`（3814）及其 `PersistedReviewHydrationTask`（426）；删除 `RuntimeEvent::{ReviewApproved, ReviewAccepted, ReviewRework, ReviewRejected}` 声明、`kind()`（180-183）、`parse()`（210-213）四处；删除 `IntegrationValidation` 的 import（`repository.rs:10`）。

**保留** `application_root_hydration_tasks`（3894）与 `PersistedReviewHydrationTask` 若仍被它使用——若该结构体只服务 `review_hydration_tasks`，改名/内联；若 `application_root_hydration_tasks` 也返回它，则保留结构体只删 `review_hydration_tasks`。

- [ ] **Step 6: 跑 store repository 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test repository_decisions`
Expected: PASS。

- [ ] **Step 7: Commit**

```bash
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs
git commit -m "refactor(store): 交付落 completed，加 schema v14 迁移并删 review 持久化"
```

---

### Task 3: store runtime 与 IPC 去 review

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`、`runtime_ipc.rs`

**Interfaces:**
- Consumes: Task 2 的 `record_delivery`、`LATEST_SCHEMA_VERSION=14`。
- Produces:
  - `RuntimeCoordinator` 无 `accept_review` / `approve_review` / `preview_review` / `confirm_review` / `rework_review` / `reject_review` / `review_child_authorized` / `review_context`；`ReviewDecision` / `ReviewPreview` 删除。
  - **保留** `delivery_diff`（`runtime.rs:3119`）。
  - `IpcRequest` 无 `Review` / `PreviewReview` / `ConfirmReview` / `ReviewChild`；`IpcResponse` 无 `ReviewPreview` / `ReviewApproved` / `ReviewReworkRequested` / `ReviewRejected` / `ChildReviewAccepted`；`IpcReviewDecision` / `ChildReviewDecision` 删除。

- [ ] **Step 1: 写失败测试**

在 `runtime_coordinator.rs` 新增（并替换现有的 `accept_review` / `ReviewDecision` 用例）：

```rust
/// 子任务交付后 reconcile 直接落 completed，无需任何 review 调用。
#[tokio::test]
async fn a_delivered_child_reconciles_directly_to_completed() {
    // 复用本文件既有的 daemon fixture：spawn child → report_delivery →
    // 等待 task_state(&child) == "completed"（不再是 awaiting_parent_review）。
}
```

在 `runtime_ipc.rs`：删除 `PreviewReview` / `ConfirmReview` / `ReviewChild` / `ReadTaskDiff` 之外的 review 用例；保留并确保 `ReadTaskDiff` 仍返回 diff（回归保护，因为 `delivery_diff` 不可删）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator a_delivered_child_reconciles_directly_to_completed`
Expected: FAIL（仍落 `awaiting_parent_review`）。

- [ ] **Step 3: 改 `runtime.rs`**

删除：`accept_review`（2662）、`preview_review`（2738）、`confirm_review`（2789）、`approve_review`（2839）、`rework_review`（2938）、`reject_review`（3019）、`review_child_authorized`（3391）、`review_context`（3161）、`ReviewDecision`（147）、`ReviewPreview`（154）、`REVIEW_CONFIRMATION_TTL`（47）、`review_parent_notification`（4179）、`review_persistence_error`（3984）、`daemon_parent_integration_actor_json`（3971）、`hydrated_review_state`（4106）。

改 reconcile 收敛：删除 `deliveries` 分支（`runtime.rs:3527-3535`）与收集处（`3441` 附近的 `let mut deliveries`）、落库处（`3616-3620`）——交付现在由 `WorkerDelivered → Completed` 走普通终态路径。`reconcile_worker_events` 里 `TaskState::Completed` 落 `("completed", RuntimeEvent::TaskCompleted, ...)`：确认 `Completed` 已在 match 臂中（若原先只处理 `CompletedNoChanges`，补上 `Completed` 臂，`terminal_json` 传 `None`）。

改 hydration：删除 `review_hydration_tasks` 调用与 `runtime.rs:449, 466-653` 段（review 专属 hydration + mailbox 回放循环）。`hydrated_application_root_state`（4033）保留，仅删其 `"awaiting_parent_review"` 分支（4055-4062）。`AgentTask::hydrated_review_task` 三处调用（485/613/1064）适配 Task 1 的新签名。
**保留** `delivery_diff`（3119）与其 helper；删 review-only 后确认它仍编译（它只依赖 `task_detail` + git）。

- [ ] **Step 4: 改 `ipc.rs`**

删除请求变体 `Review`（330）、`PreviewReview`（336）、`ConfirmReview`（340）、`ReviewChild`（395）；响应变体 `ReviewPreview`、`ReviewApproved`、`ReviewReworkRequested`、`ReviewRejected`、`ChildReviewAccepted`（`ipc.rs:545-560` 一带）；类型 `IpcReviewDecision`（657）、`ChildReviewDecision`（665 后）及其 `From` 实现（642-647）；分派臂 `ipc.rs:3548-3561`、`3782-3800`；helper `preview_review`（1719）、`confirm_review`（1738）。
**保留** `ReadTaskDiff` / `TaskDiff` 与其分派（`ipc.rs:3855`）。

- [ ] **Step 5: 跑 store 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add yi-agent-rs/crates/yi-agent-store
git commit -m "refactor(store): 删除 review 决策面与 IPC，交付走普通完成路径"
```

---

### Task 4: 工具与前端去 review

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 3 的 IPC 面（无 review 变体）。
- Produces: `review_agent` 工具消失；TUI/CLI 无 `/accept` `/rework` `/reject`；`DELEGATION_TOOLS` 为五工具。

- [ ] **Step 1: 写失败测试**

在 `yi-agent-subagent/src/lib.rs` 的工具列表断言处（`lib.rs:2186` / `2550` 一带），把期望列表去掉 `"review_agent"`，例如：

```rust
    assert_eq!(
        names,
        vec![
            "spawn_agent", "send_message", "wait_agent",
            "inspect_agent", "cancel_agent",
        ]
    );
```

在 `yi-agent-app-server/src/server.rs`：把 `DELEGATION_TOOLS`（`server.rs:9273-9280`，原计划写 8233-8240，rebase 后已漂移 +1040）改为五项（去掉 `"review_agent"`），相关断言同步。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib review_agent`
Expected: 编译失败或断言失败（`review_agent` 仍在）。

- [ ] **Step 3: 改 `yi-agent-subagent/src/lib.rs`**

删除 `DaemonReviewAgentTool` 结构体（1677）、其 `Tool` 实现（1927-2003）、两处注册（658, 1413）、工具名断言中的 `review_agent`（199, 223）、`review_agent_validates_its_arguments_before_touching_the_daemon` 测试（2560）。删除 `ReviewChild` / `ChildReviewDecision` 的 import 与调用（1987-1998）。

- [ ] **Step 4: 改 TUI 与 CLI**

`tui/app.rs`：删 `/accept` `/rework` `/reject` 分派（`SlashCommand::{Accept,Rework,Reject}` 臂在 2779-2817）、helper `parse_accept_args` / `parse_rework_args` / `parse_reject_args` / `parse_review_text_args`（2970-3025）、`daemon_accept*` / `daemon_rework*` / `daemon_reject*` / `daemon_review_decision_at`（3351-3472）、相关测试（`review_command_parsers_accept_confirmation_tokens_and_freeform_text` 4332、`accept/rework/reject_control_previews_then_confirms_review_from_tui` 4453/4480/4515）。

`main.rs`：删 `AgentAction::{Accept, Rework, Reject}` 及其 `ConfirmReview` 调用（517-575）、结果分支 `ReviewApproved/ReviewReworkRequested/ReviewRejected`（621-623）、`preview_review` helper（`show_review_preview`，632-665）与 `ReviewPreview` 处理。

- [ ] **Step 5: 跑受影响 crate 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib && cargo test -p yi-agent --lib`
Expected: PASS。

- [ ] **Step 6: Commit**

```bash
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add yi-agent-rs/crates/yi-agent-subagent yi-agent-rs/crates/yi-agent yi-agent-rs/crates/yi-agent-app-server
git commit -m "refactor: 删除 review_agent 工具与 accept/rework/reject 命令"
```

---

### Task 5: e2e 断言改写与全量验证

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`（若有残留 review 用例）

**Interfaces:**
- Consumes: Task 1-4 全部。
- Produces: 无新接口；仅测试断言对齐。

- [ ] **Step 1: 改写成功条件**

`subagent_runtime_e2e.rs:392, 548`：把 `detail.state == "awaiting_parent_review"` 改为 `detail.state == "completed"`（panic 文案同步改 "task never reached completed"）。
`subagent_real_e2e.rs:280`：同样改 `"awaiting_parent_review"` → `"completed"`；删除该文件里 review 相关的 await/断言与 prompt 里"local human reviewer will accept"一类要求（若测试流程依赖接受动作，改为断言子任务 `completed` 即可）。

- [ ] **Step 2: 确认全仓编译**

Run: `cd yi-agent-rs && cargo build --workspace`
Expected: 编译通过（无残留对已删类型的引用）。

- [ ] **Step 3: 分 crate 跑测试**

先确认无并发 cargo：

```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo "clean"
```

然后按序跑（**不并行**）：

```bash
cd yi-agent-rs
cargo test -p yi-agent-core
cargo test -p yi-agent-store
cargo test -p yi-agent-subagent
cargo test -p yi-agent-app-server
```

Expected: 全部 PASS。

- [ ] **Step 4: 格式与 lint**

```bash
cd yi-agent-rs
cargo fmt --all
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: 无改动、无警告。

- [ ] **Step 5: 变异验证（证明测试真的锁住了行为）**

临时把 Task 1 的 `WorkerDelivered` 目标改回 `TaskState::AwaitingParentReview`（若枚举已删，改为临时断言 `state == AwaitingParentReview` 的等价物，或临时把落库字符串改回 `awaiting_parent_review`），跑 `cargo test -p yi-agent-core --lib subagent::task && cargo test -p yi-agent-store --test repository_decisions`。
Expected: 相关测试 FAIL。验证后**还原**改动。

- [ ] **Step 6: 自审 diff 并 Commit**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git diff --stat
grep -rn "awaiting_parent_review\|AwaitingParentReview\|review_agent\|ReviewAccepted\|ReviewRework\|ReviewRejected\|IntegrationValidation\|DeliveryState" yi-agent-rs/crates desktop/src --include='*.rs' --include='*.ts' --include='*.tsx' | grep -v '/target/'
```

Expected: 除 Migration v14 字符串、`hydrated_review_task` / `from_hydrated_review_root` / `insert_hydrated_review_child` 等保留名字、以及测试断言的注释外，无残留功能性引用。

```bash
cargo fmt --all --manifest-path yi-agent-rs/Cargo.toml
git add -A
git commit -m "test(e2e): 子任务成功条件改为 completed，收尾 review 门下线"
```

---

## Self-Review

**1. Spec coverage**（对照 `2026-10-09-drop-parent-review-gate-design.md`）：

| Spec 章节 | 覆盖任务 |
| --- | --- |
| §4.1 `task.rs` | Task 1 Step 3 |
| §4.2 `supervisor.rs` | Task 1 Step 4 |
| §4.3 `runtime.rs` / `repository.rs` | Task 2 Step 4-5、Task 3 Step 3 |
| §4.4 迁移 v14 | Task 2 Step 3 |
| §4.5 IPC | Task 3 Step 4 |
| §4.6 工具与前端 | Task 4 |
| §5 连带收益 | Task 1 Step 4（worker 摘除）、名额释放随终态自然生效 |
| §7 测试策略 | 各任务 Step 1 红测试 + Task 5 变异验证 |
| §9 明确不做 | 无任务（prompt 不动、reviews 表不动） |

**2. Placeholder scan**：Task 2 Step 1、Task 3 Step 1 的测试用 `/* ... */` 指代本文件既有 fixture——实现时**必须**照抄相邻用例的建库写法，不得留占位。其余步骤均含实际代码。

**3. Type consistency**：`record_delivery`（Task 2 生产）在 Task 3 Step 3 消费；`LATEST_SCHEMA_VERSION=14`（Task 2）在 Task 2 Step 3 断言；`hydrated_review_task` 新签名（Task 1 生产：去掉 `delivery` 参数）在 Task 3 Step 3 三处调用点消费（485/613/1064）——注意 `hydrated_review_task` 与 `from_hydrated_review_root` / `insert_hydrated_review_child` 均为**保留**名字。

