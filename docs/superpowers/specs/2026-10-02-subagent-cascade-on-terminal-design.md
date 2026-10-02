# 父任务终结时级联取消子代理

日期：2026-10-02
状态：设计已确认，待实施

## 1. 背景

### 1.1 问题

任务树里「父死子死」目前只是**调用点的约定**，不是**结构保证**。

所有把任务推入终态的路径，最终都经过 `task.rs` 的 `transition()`；而它只做三件事——校验转移合法性、写状态、关闭自己的 attempt：

```rust
fn transition(task: &mut AgentTask, next: TaskState, now) -> Result<(), TaskReduceError> {
    task.state.can_transition_to(next.clone())?;
    let terminal_reason = terminal_reason_for(&next);
    task.state = next;
    if let Some(reason) = terminal_reason {
        task.pause_request = None;
        task.close_active_attempt(reason, now);
    }
    Ok(())
}
```

它**不看 `children`**。级联真正发生，只因为某些调用方记得传 `recursive=true`；而有的调用点写死 `false`（`WorkerEvent::Cancelled => cancel_task_tree(&task_id, false)`）。

### 1.2 漏掉的入口（实测）

| 路径 | 是否连坐子任务 |
|---|---|
| 父 worker 失败 `WorkerEvent::Failed` | 否 |
| 看门狗超时 `timeout_task` | 否 |
| 预算耗尽 `exhaust_task_budget` | 否 |
| 父被取消 `WorkerEvent::Cancelled`（写死 `recursive: false`） | 否 |
| 用户删会话 | 是（另一处补丁，仅此一个入口） |

### 1.3 后果

父 worker 崩溃 → 父变 Failed → 子代理仍在跑 → 跑完把结果投递到**已死的父**的邮箱 → 无人接收 → 子任务卡在待审、占着 worktree lease 与并发额度，直到 daemon 重启才被扫掉。

### 1.4 已有的能力（本次复用）

- `reconcile_worker_events` 返回的 `changed` 列表，daemon 会逐个**落库终态并释放 resident lease**（`runtime.rs`：`for (task_id, attempt, state, event, terminal_json) in updates` → `transition_task_and_attempt*` + `release_resident_lease`）。
- `collect_cancellation_targets` 已有递归收集子树的能力。
- 事件→状态→终态副作用的映射在 `reconcile` 里已经维护：新出现的终态只要进了 `changed` 就自动被处理。

**结论**：只要级联出的子任务出现在超管返回的受影响列表里，持久化与 lease 释放**无需新代码**。

## 2. 目标

把「父任务进入已定终态 ⇒ 其全部子任务也进入终态」做成**结构不变式**，不再依赖每个调用点记得传参。

非目标：不改终态本身集合的定义，不改 UI 文案，不做用户可见的通知（见 §6）。

## 3. 设计

### 3.1 核心规则

任务进入**已定终态**时，级联取消其全部非终态子任务（递归至叶）。

**已定终态**（8 个）：`completed`、`completed_no_changes`、`blocked`、`stalled`、`timed_out`、`budget_exhausted`、`failed`、`cancelled`。

### 3.2 排除恢复类终态

`recovery_required` / `recovery_gated` / `recovery_attested` **不触发级联**。

理由：注释写明 `recovery_required` 是「等待显式恢复的暂存态，不是终结」，daemon 重启后父可 resume。若在此级联，父亲恢复时会发现孩子已被杀，破坏可恢复性。恢复类终态让整棵子树**一致地被停放**。

（注意：这与 `reclaim_orphaned_tasks` 对「孤儿」的处理并不矛盾——那是 daemon 启动时对**无人认领**的清理，与「父主动终结时的一致性」是两件事。）

### 3.3 报警

当**父是正常完成**（`completed` / `completed_no_changes`）却仍有活子任务时，记 `tracing::warn!`。

这是异常：按现有模型父本该等孩子。留痕而非静默。其余终态（失败、取消、超时等）属正常级联，不报警。

### 3.4 实现位置与接线

**收口点选在超管层，而不是 reducer。**

`transition()` 看不到 `children`（`children` 归超管所有），而 `reduce` 被直接调用于 `AgentTask`，绕过了超管。因此引入一个超管级包装方法（暂名 `reduce_task`）：

```
fn reduce_task(&mut self, task_id, event) -> Result<Vec<TaskId>, String>
    1. scoped 借用：对目标任务执行 reduce（借用在此结束）
    2. 读回状态；若为「已定终态」→ 调用 settle_terminal
    3. settle_terminal：正常完成且有活子 → warn；递归取消子树；
       返回受影响 id（含目标自身）
    4. 非终态 → 只返回目标自身（若有变化）
```

**接线方式：把超管里 21 处直接 `.reduce(` 调用改为 `self.reduce_task(`。** 这是机械替换，且全部落在 supervisor.rs 内。之所以不选「只接 worker 事件 + 看门狗两个入口」，是因为终态还能从别的路径产生——例如子任务的评审完成（`ReviewAccepted` → `Completed`），而子任务自身也可能带子任务（Root → Child → Leaf）。只接两个入口会漏，收口点必须覆盖全部 21 处。

**daemon 侧对接**：

- `reconcile_worker_events`：`reduce_task` 返回的受影响 id 并入 `changed`，daemon 现有逻辑自动落库 + 释放 lease。
- 其余超管公开方法（`fail_task` / `timeout_task` / `exhaust_task_budget` / `cancel_task_tree` 等）：改为返回受影响 id 列表，daemon 各调用点一并落库 + 释放 lease。
- `cancel_task_tree(recursive=true)` 已处理整棵子树 → 走 `reduce_task` 后自然幂等（子树已在取消中），不重复处理。
- 删会话（上一处补丁）不改。

**借用约束**：`settle_terminal` 会变更其它任务（子任务），因此必须先结束对目标任务的 `get_mut` 借用，再进入级联。实现时按此顺序，避免借用冲突。

### 3.5 数据流

```
worker 事件 / 看门狗判定
        ↓
supervisor 落定该任务终态
        ↓
settle_terminal：正常完成且有活子 → warn；递归取消子树
        ↓
返回受影响 id（父 + 级联出的子）
        ↓
daemon：落库终态 + release_resident_lease（现有逻辑）
```

## 4. 测试

**先写红测试**（父有在跑的子任务）：

1. 父失败 → 子任务全部终态、lease 释放。
2. 父超时/预算耗尽 → 同上。
3. 父正常完成且有活子 → 子任务被取消，且产生一条 warn 日志。
4. **`recovery_required` 不级联** → 子任务保留、仍可恢复。

**变异验证**：临时去掉级联逻辑，上述测试必须失败（证明测试真的在测这个行为）。

## 5. 风险

- **最大风险**：把某个「故意不级联」的路径误改成级联。实施前逐个核对收口处的调用点。
- `AwaitingParentReview` 的子任务：父正常终结时按本设计会被取消；这与现有「父必须审阅孩子交付」的流程如何交互，需在实施时确认并补测试。
- 级联范围只限该任务的子树，不影响同目录其它会话（与上一处补丁的隔离原则一致）。

## 6. 明确不做

- **用户可见的通知**：本次报警只写 `tracing::warn!`（开发者日志，Desktop UI 看不到）。要做 UI 提示需另立设计。
- **历史数据**：已有的孤儿任务不追溯处理。
