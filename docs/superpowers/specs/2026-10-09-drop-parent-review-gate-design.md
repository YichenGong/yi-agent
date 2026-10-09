# 删除父审门（Drop Parent-Review Gate）设计

**状态**：待实现
**日期**：2026-10-09
**影响 crate**：`yi-agent-core`、`yi-agent-store`、`yi-agent-subagent`、`yi-agent`（TUI/CLI）、`yi-agent-app-server`

## 1. 背景

子任务（`ChildWriteMode::Coding`）在自己的 git worktree 上跑，交付一个 commit
（`DeliveryReport.commit`）。当前状态机在"交付完成"与"终态"之间插了一个等待态：

```
Running --WorkerDelivered--> AwaitingParentReview --ReviewAccepted--> Completed
```

`AwaitingParentReview` 的语义是"把 delivery 交给直接父任务，停在这里等裁决"。
它有三条出边：accept → `Completed`、rework → 派生新 attempt、reject → `Blocked`。

### 1.1 该状态的真实处境

- worker 已停止：进入该状态时 supervisor 立即 `workers.remove`（`supervisor.rs:966-975`）。
- 无自驱、无超时出边：watchdog 事件只在 `Running` 合法，故它不会因"跑太久"自动离开。
- 它**不是**终态（`task.rs:226` 的 `is_terminal` 不含它），因为它还在等外部决定。

### 1.2 运行时自动集成验收已经不存在

原设计（`docs/superpowers/specs/2026-09-26-subagent-worktree-recycling-design.md`）
用一条运行时校验兜底"父真的合了"：

```
git merge-base --is-ancestor <child.commit> <parent HEAD>
```

并在 `reconcile_worker_events` 里扫描 `AwaitingParentReview` 任务、通过即 `accept_review`。
但 commit `71c28abe`（"drop automatic delivery acceptance, worktree reclaim, and daemon gc"）
已整体删除这套：

- reconcile 不再扫描集成交付；
- `reclaim_session_worktrees` / `reclaim_idle_worktrees` / TTL sweep / daemon-tick reclaim 全删；
- `cleanup_accepted` / `is_merged_into` / `ChildWorktree` 等 worktree 原语全删。

核实当前代码：`runtime.rs` 中无 `is_ancestor` / `merge-base` 调用（剩余 "ancestor"
字样属 recovery 路径）；`RuntimeCoordinator::accept_review`（`runtime.rs:2662`）**没有生产调用者**，
只剩测试在调。故"是否合入"如今完全依赖父 agent 自觉（prompt 里的 `git merge --no-ff <commit>`，
`agent.rs:197-200`）。

### 1.3 现存的 review 入口只有两处

1. **TUI 人工命令** `/accept` `/rework` `/reject`（`tui/app.rs:2604-2630, 3175+`）。
2. **agent 工具** `review_agent`（`yi-agent-subagent/src/lib.rs:1927-2003`），
   发给直接父，仅支持 `rework` / `reject`。

## 2. 问题

`AwaitingParentReview` 是一个"停着等外部输入"的停驻态，却被下游一律折叠成
"running / 活跃"——因为各处只用一个"是否终态"的布尔回答两个不同的问题
（"还能不能动" vs "现在跑不跑"）：

| 位置 | 表现 |
| --- | --- |
| `desktop/src/lib/subagents.ts:27-31` | `TERMINAL_STATES` 四态，`isFinished` 判 false → rail 显示"工作中" + 过期 `lastStep`（`SubagentRail.tsx:71`、`SubagentTrace.tsx:118`） |
| `tui/app.rs:3409` | 同一份四态列表 → `active: !is_terminal_task_state` → `/agents --active` 收它 |
| `repository.rs:4008` `task_summaries(active_only)` | 只排除四态 → 计入"活跃" |
| `repository.rs:3297` `active_children_for_thread` | 同一份 `TERMINAL_TASK_STATES` → 算作会话活子任务 |
| `supervisor.rs:1271-1282` `max_direct_children` | `!is_terminal()` 计入名额 → 卡住父的直接子名额 |

## 3. 决策

**删除父审门，子任务交付即 `Completed`。**

用户明确取舍：**不需要"父审 + 返工 + 集成验证"这套能力**。
保留的边界是"父仍负责合入"（选项 a）：prompt 里 `git merge --no-ff <commit>` 指令保留，
父 agent 用自家 `bash` 合、解决冲突、自行验证——只是**运行时不再介入、不再验证**。

（用户已确认：交付落 `Completed`，非 `CompletedNoChanges`。）

### 3.1 目标状态机

```
Running --WorkerDelivered--------> Completed          (有交付)
Running --WorkerCompletedNoChanges--> CompletedNoChanges  (无交付，不变)
```

### 3.2 为什么这个改动现在安全

"直接 done 会丢工作"的原风险来自旧设计里两条联动：运行时 ancestor 校验 + TTL 回收
（未合入的交付被保护在 `AwaitingParentReview` 里，永不回收，
见 `2026-09-26-worktree-ttl-reclaim-design.md:207`）。这两条都已在 `71c28abe` 删除，
**当前没有自动回收会删未合入的 worktree**，故删除状态不再引入工作丢失风险。

## 4. 变更设计

### 4.1 core —— `subagent/task.rs`

删除：

- `TaskState::AwaitingParentReview(DeliveryId)` 及迁移表、`is_terminal` 中的相关行。
- `TaskEvent::{ReviewAccepted, ReviewRework, ReviewRejected}` 及其 `attempt_id()` 分支。
- `DeliveryState` 枚举整体（`task.rs:246-268`）——它纯是 review 生命周期；
  `AgentTask.delivery` 字段与 `.delivery()` 访问器（唯一非测试消费者是 review 归约）。
- `IntegrationValidation` 结构体（`task.rs:326+`）——见 §4.3，已无消费者。

改：

- `TaskEvent::WorkerDelivered` 的归约（`task.rs:1020-1033`）从
  `transition(AwaitingParentReview)` 改为 `transition(Completed)`；`delivery` 记录
  改落在 `attempt` 上（保留 attempt-level delivery，供父读取 commit）。

### 4.2 core —— `subagent/supervisor.rs`

删除：

- `accept_review`、`rework_review`、`reject_review`、`reduce_rework_review`、
  `stage_review_with_persistence`、`require_review_delivery`、
  `tasks_awaiting_parent_review`、`ReviewPersistenceError`。
- `children_needing_attention`（`supervisor.rs:1192-1204`）中的
  `active_attempt().delivery.is_some()` 分支——子一交付即终态，`is_terminal()` 已覆盖。

保留（**关键**，不可误删）：

- `MessageKind::Completed(delivery)` 投递（`supervisor.rs:843-856`）：父仍在子交付
  上报的**同一时刻**收到通知，带 commit sha。删 review 只删"必须回头裁决"的动作，
  不删"父知道子做了什么"这条信息流。

### 4.3 store —— `runtime.rs` / `repository.rs`

改：

- `record_delivery_for_review`（`repository.rs:1540`）→ `record_delivery`：
  落 `state_json = 'completed'`（不再 `awaiting_parent_review`），保留
  `delivery_json`、`attempts` 更新、`deliveries` 表写入、以及给父的 `mailbox_messages` 插入。
- `reconcile_worker_events` 中 `AwaitingParentReview` 的独立分支（`runtime.rs:3527-3535`）
  删除——`Completed` 走普通终态落库路径。

删除：

- `accept_review`、`approve_review`、`preview_review`、`confirm_review`、
  `rework_review`、`reject_review`、`review_child_authorized`、`review_context`、
  `ReviewDecision`、`ReviewPreview`、`REVIEW_CONFIRMATION_TTL`、
  `review_parent_notification`、`daemon_parent_integration_actor_json`。
- repository 的 `accept_delivery_review`、`rework_delivery_review`、
  `reject_delivery_review`、`delivery_has_review` 及 `reviews` 表的新行写入。
- `RuntimeEvent::{ReviewApproved, ReviewAccepted, ReviewRework, ReviewRejected}`
  及其 `kind()` / `parse()` 分支（`repository.rs:180-213`）。
- `hydrated_application_root_state` / `hydrated_review_state` 中的
  `"awaiting_parent_review"` 分支（`runtime.rs:4055, 4120`）。

删除 review-only 的 mailbox 消息种类及其处理：

- `MessageKind::{Rework(ReworkInstruction), ReviewRejected(MessageId)}`、`ReworkInstruction`。
- 其 hydration 分支（`runtime.rs:703` 附近）、优先级表项、persisted-message 路径。

保留（**不可误删**）：

- `delivery_diff`（`runtime.rs:3119`）与 `IpcRequest::ReadTaskDiff` /
  `IpcResponse::TaskDiff`：这是 TUI diff 视图、`/diff` action、以及父查看子交付的工具
  （`yi-agent-subagent/src/lib.rs:1852`、`tui/app.rs:3062,3123`、`main.rs:468`）共用的通用能力，
  **与 review 无关**。

### 4.4 数据迁移（schema v14）

新增一条迁移（`LATEST_SCHEMA_VERSION: 13 → 14`，`repository.rs:18`），并在
`repository.rs:4873` 的 `assert_eq!(LATEST_SCHEMA_VERSION, 13)` 同步改为 14：

1. `UPDATE tasks SET state_json = 'completed' WHERE state_json = 'awaiting_parent_review';`
2. `UPDATE attempts SET state = 'completed' WHERE state = 'awaiting_parent_review';`
3. `DELETE FROM mailbox_messages WHERE kind IN ('rework', 'review_rejected');`
   （review-only 消息，删除枚举后无法解码；一并清掉避免 hydration 报未知 kind）
4. `UPDATE events SET kind = 'task_completed' WHERE kind IN
   ('review_approved', 'review_accepted', 'review_rework', 'review_rejected');`
   ——**必须做**：`RuntimeEvent::parse` 对未知 kind 直接返回
   `RepositoryError::UnknownEventKind`（`repository.rs:214`），且 events 读取路径
   （`repository.rs:4200/4266/4304`）无容错，残留旧 kind 会让 `ReadTaskEvents`
   直接失败。映射到 `task_completed` 是语义最接近的既有 kind。
5. 历史 `reviews` 表行保留不动（表结构不变，只是不再写新行）。

迁移后需删除 `PersistedReviewHydrationTask` 查询（`repository.rs:3815-3830`，
按 `kind IN ('rework','review_rejected')` 找 review 会话）及其调用点——review-only
消息已在步骤 3 清空，该查询不再有对象。

**命名易混点（不可误删）**：`AgentSupervisor::from_hydrated_review_root`、
`insert_hydrated_review_child`、`AgentTask::hydrated_review_task` 虽带 "review" 之名，
但被 **application-root hydration**（`runtime.rs:1048` 起，
`application_root_hydration_tasks`）复用，是通用 hydration 构造器，**必须保留**
（可选：改名去 "review"，但非必需）。

真正 review-only、可删的是：`review_hydration_tasks` 查询、
`hydrated_review_state`（`runtime.rs:4106`）、
以及 `runtime.rs:449/466-653` 那段 `review_tasks` 专属 hydration 与 mailbox 回放循环。
`hydrated_application_root_state`（`runtime.rs:4033`）保留，仅删其
`"awaiting_parent_review"` 分支。

### 4.5 IPC —— `store/src/ipc.rs`

删除请求变体：`Review`、`PreviewReview`、`ConfirmReview`、`ReviewChild`。
删除响应变体：`ReviewPreview`、`ReviewApproved`、`ReviewReworkRequested`、
`ReviewRejected`、`ChildReviewAccepted`。
删除类型：`IpcReviewDecision`、`ChildReviewDecision` 及其 `From` 实现与分派分支
（`ipc.rs:334-342, 395-402, 545-560, 642-647, 1719-1756, 3552-3559, 3782+`）。

### 4.6 工具与前端

- 删 `DaemonReviewAgentTool`（`review_agent`，`yi-agent-subagent/src/lib.rs:1927-2003`）
  及其注册点（`lib.rs:658, 1413`）与工具名断言（`lib.rs:199, 223`）。
- 删 CLI 的 review 动作：`AgentAction::{Accept, Rework, Reject}` 及其
  `ConfirmReview` / `PreviewReview` 调用与结果分支（`main.rs:516-575, 620-622, 634-644`）。
- 删 TUI 的 `/accept`、`/rework`、`/reject` 及其 helper
  （`tui/app.rs:2604-2630, 2794-2819, 3175-3250`）。
- app-server：改 `DELEGATION_TOOLS`（`server.rs:8233-8240`）去掉 `"review_agent"`，
  六工具列表变五工具；相关断言同步。
- 桌面端：已确认**无任何 review UI/RPC**，无需改动。

## 5. 连带收益

- **级联冲突消解**：`2026-10-02-subagent-cascade-on-terminal-design.md:133` 记录的未决问题
  （"父正常终结时 `awaiting_parent_review` 的子会被取消，与'父必须审阅孩子交付'冲突"）
  随该状态消失而自动消解——子交付即终态，不在级联窗口内。
- **子名额及时释放**：`max_direct_children`（`supervisor.rs:1271`）不再把停驻态计入，
  子一跑完就腾出名额。

## 6. 影响面

源码（8 文件；括号为该文件相关命中数）：

| 文件 | 命中 |
| --- | --- |
| `yi-agent-core/src/subagent/task.rs` | 45 |
| `yi-agent-store/src/repository.rs` | 35 |
| `yi-agent-core/src/subagent/supervisor.rs` | 31 |
| `yi-agent-store/src/runtime.rs` | 28 |
| `yi-agent-store/src/ipc.rs` | 17 |
| `yi-agent-subagent/src/lib.rs` | 8 |
| `yi-agent/src/tui/app.rs` | 3 |
| `yi-agent/src/main.rs` | 3 |

测试（6 文件）：

- `yi-agent-core/tests/subagent_supervisor.rs`
- `yi-agent-store/tests/{runtime_coordinator, runtime_ipc, repository_decisions, subagent_runtime_e2e}.rs`
- `yi-agent/tests/subagent_real_e2e.rs`

其中"把到达 `awaiting_parent_review` 当成功条件"的断言
（`subagent_runtime_e2e.rs:392,548`、`subagent_real_e2e.rs:280`）改为断言 `completed`。

## 7. 测试策略

1. **core 单测（TDD 先行）**：
   - `WorkerDelivered` → `Completed`，且 attempt 上保留 delivery。
   - `AwaitingParentReview` 已从状态机消失：`can_transition_to` 无该目标；
     `is_terminal()` 语义不变。
   - 父在下一次唤醒收到 `MessageKind::Completed(delivery)`。
   - `max_direct_children`：一个已交付的子不再占名额。
2. **store 单测**：`record_delivery` 落 `completed`；迁移把历史 `awaiting_parent_review`
   行改 `completed` 并清 review-only 消息。
3. **e2e**：把两处成功条件从 `awaiting_parent_review` 改 `completed`；删 review 流程断言。
4. **变异验证**：临时恢复 `AwaitingParentReview` 目标，相关测试必须失败。

## 8. 风险

- **照用户的取舍，交付正确性不再被自动保证**：父若忘记 merge，子仍显示 `completed`，
  无信号。这是接受的代价，如实记录在 spec 中。
- 历史库中处于 `awaiting_parent_review` 的行（若存在）迁移后变 `completed`；
  未合入的分支仍留在磁盘（无回收删除它们），不会丢。
- 删除 IPC 变体会破坏仍在使用旧 daemon 的客户端；本项目 daemon 与客户端同仓同版本，
  接受。

## 9. 明确不做

- 不改父的 prompt（`agent.rs:197-200` 的 `git merge --no-ff <commit>` 指令保留）。
- 不改 `MessageKind::Completed` 的投递时机（仍是交付上报那一刻）。
- 不引入"子任务完成后自动 merge 到父分支"的自动化（用户未要求）。
- 不动 `reviews` 表结构（保留空表，便于未来需要时复用）。
- 不追溯清理历史分支/worktree。
- 不新增 UI 提示。
