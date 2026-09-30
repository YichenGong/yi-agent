# 子 Agent 观察入口设计（TUI + 桌面端）

**目标：** 让用户能"进入"任意子 agent，实时看到它在做什么——它说了什么、调用了哪些工具、
拿到了什么结果、现在跑到哪一步；并且可以在不打断主对话的前提下对它做两项有限干预
（发消息、取消）。

**状态：** 已设计，待实现。

**范围：** 观察（轨迹的实时订阅与回放）+ 少量干预（发消息、取消）。不含验收/打回/拒绝，
不含思考（reasoning）展示，不含暂停并编辑，不含跨项目聚合。

**相关文档：**
[Subagent Runtime Architecture Design](./2026-08-09-subagent-architecture-design.md)（任务树与 daemon 架构）、
[Mac 桌面端子 Agent 委派对齐 TUI 设计](./2026-09-30-desktop-subagent-delegation-design.md)（桌面端 attach 项目 runtime）、
`docs/project-management/subagent-runtime.md`（现有观察能力：`/agents`、`/agent`、`/events`）、
`docs/project-management/desktop.md`（P3 路线图：「子 agent 任务树可视化」）。

---

## 1. 问题

### 1.1 现象

用户能起子 agent，但看不到它在干什么。这个缺口在需求阶段就已被登记：

> **目前没有路径进入 subagent 内部看它的内容与进度，CLI 与 App 都缺这条入口。**
> CLI：`/agent <task_id>`（`yi-agent-rs/crates/yi-agent/src/tui/app.rs:1870`
> `daemon_agent_detail_at`）走 `IpcRequest::InspectTask`（`app.rs:1873`），只返回一次性元数据
> 快照 … App：app-server 无任何 `agent/` 方法 … 目标为两级入口：slash command 默认给进度
> 摘要，并可进一步钻进只读完整轨迹。
>
> —— `docs/bug-list.md`

### 1.2 根因（已核实）

**一、daemon 有订阅骨架，但流里没有子 agent 的轨迹。**

`IpcRequest::SubscribeEvents { after_event_id }`（`yi-agent-store/src/ipc.rs:323`）＋
`SubscriptionFilters { task_ids, kinds }`（`ipc.rs:331`）已经提供了带游标的持久订阅：
事件有自增 `event_id`、订阅首帧即快照、断线可用游标续传、每个事件帧都有身份校验
（`ipc.rs:652`）、溢出即关连接（`ipc.rs:1508`）。但流里只有状态机事实——
`TaskEvent`（`yi-agent-core/src/subagent/task.rs:760`：admission / permission / delivered /
review / cancel / budget_exhausted）与 `RuntimeEvent`。**子 agent 的助手文本与工具调用
从不外流。**

**二、轨迹在 worker 侧被就地丢弃。**

子 worker 消费 `AgentEvent` 的那个循环（`yi-agent-subagent/src/lib.rs:653-768`）已经把轨迹
握在手里，却只用于三件别的事：`AssistantText` 拼进 `assistant_report`（`:659`）当最终报告、
成功 `ToolResult` 只用来 `report_meaningful_progress`（`:668`）喂 watchdog、其余忽略。
**消息流、工具调用与工具结果全部落地即弃。**

**三、两个前端的现状不对称。**

TUI 已有 `/agents`（任务树）、`/agent <task_id>`（元数据快照）、`/events`、`/diff`、
`/mailbox`；桌面端 app-server 的命名空间只有 `thread/` `turn/` `item/` `workspace/`
`config/read`，**没有任何 `agent/` 方法**，`desktop/src/components/` 里也没有子 agent 入口。

### 1.3 需求确认（本次设计逐项与用户确认）

| 决策点 | 结论 |
| --- | --- |
| 交互能力 | 只读 + 少量干预（发消息、取消）；**不做**验收/打回/拒绝 |
| 观察内容 | 档 2：助手文本流 + 每次工具调用与结果，可滚动看完整轨迹；**不做** reasoning |
| 实时性 | 实时推（订阅式），不是轮询快照 |
| 留存语义 | 落库 + 写入前聚合 + 保留上限；可断线续看、任务结束后仍可回看 |
| 打开时行为 | 先回灌历史，再接实时推 |
| 分组口径 | 按对话分组；**本期把对话标记打进底层**（协议改动） |
| 终端入口 | Ctrl+P 加第三个页签「子 agent」+ 可钻入 |
| 桌面入口 | 主对话旁的「子 agent 暂留区」，点卡片进入详情，可下钻 |

---

## 2. 架构

三层，每层一个原则。

```text
worker（子 agent 的 agent loop）
  └─ 轨迹来源：AssistantText / ToolCall / ToolResult / 终态
     │  写入前聚合（≥512B 或 ≥200ms 或结构边界才 flush）
     ▼
daemon（yi-agent-store）
  ├─ task_trace_events 表（新，独立于审计 events 表）+ 保留策略
  ├─ reconcile 每轮抽干 worker 缓冲并落库（复用既有 10ms 节奏，不新开线程）
  └─ IPC 面：ReadTaskTrace（快照）/ SubscribeTrace（带游标续传）
     ▼
前端
  ├─ TUI：Ctrl+P →「子 agent」页签 → 只读轨迹视图（可逐级下钻）
  └─ 桌面：子 agent 暂留区 → 详情（摘要 → 完整轨迹）+ 发消息/取消
```

### 2.1 记录层：给子 agent 装行车记录仪

**源头。** 在 `yi-agent-subagent/src/lib.rs` 消费 `AgentEvent` 的循环里挂一个每 worker 一份
的 `TraceSink`。它接收轨迹事实，在**写入前聚合**，然后把聚合结果交给 reporter 转交 daemon。

聚合规则（决定 DB 增长量级）：

- `AssistantText` 分片在缓冲里合并，满足任一条件才 flush 成一行：累计 ≥512 字节、
  距上次 flush ≥200ms、或遇到结构边界（工具调用开始 / 工具结果 / 本轮结束）。
  **绝不按 token 落行。**
- `ToolCall` / `ToolResult` 各成一行，记录工具名、入参摘要、是否错误、输出头部（截断）。
  重复的工具重试（`ToolRetry`）聚合为计数，不逐条落行。
- 终态（完成 / 预算耗尽 / 取消 / 失败）各写一条 `state_note`，让轨迹能自我解释"为什么停了"。

### 2.2 存储层：独立表，不复用审计 `events`

**为什么必须独立（这是设计中被推翻的一版）。** 曾考虑直接复用 `events` 表与
`event_records_after`，读代码后否掉，三条硬理由：

1. `event_records_after` 用 `RuntimeEvent::parse(kind)` 解析（`repository.rs:3904`），而
   `RuntimeEvent` 是**封闭枚举**（`repository.rs:72`）。塞入轨迹意味着每加一种轨迹 kind
   都要改这个枚举，全仓所有 `match RuntimeEvent` 跟着动。
2. 现有订阅者（`/events`、订阅快照、replay floor）会开始读到轨迹噪声。
3. 生命周期互相打架：`events` 是审计事实，必须持久不可删；轨迹要**可裁剪**。同一张表
   里放两种生命周期，迟早互相拖累。

**改为新增表**（schema v12）：

```sql
CREATE TABLE task_trace_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    kind TEXT NOT NULL,          -- assistant_text | tool_call | tool_result | state_note
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX task_trace_task_id_idx ON task_trace_events(task_id, id);
```

轨迹 kind 用**独立小枚举**解析，不进 `RuntimeEvent`。迁移沿用既有
`if current_version < N` 块（`repository.rs:4391` 起的范式），`LATEST_SCHEMA_VERSION` 11 → 12。

**保留策略**（数值先定，实现时可调）：每任务保留最近 **2000 行**（插入时按 task 裁剪，
环形语义）；终态任务的轨迹在终态后 **24 小时**清理，挂在 daemon 既有的分钟 tick
（`ipc.rs:752` 附近的 `last_schedule_minute` 分支）里。

**写入节奏。** daemon 的 listener 循环与订阅 producer 循环本来就每轮调一次
`reconcile_worker_events()`（`ipc.rs:752`、`ipc.rs:1385`），且 `WouldBlock` 时休眠 10ms。
轨迹的"抽干 → 落库"挂进 `RuntimeCoordinator::reconcile_worker_events()`
（`runtime.rs:2854`，与既有 worker 事件抽干并列），**不新开线程、不引入新轮询**。

### 2.3 传输层：一条可订阅、可续看的通道

**新增两个 IPC 请求**（`ipc.rs` 的 `IpcRequest`）：

- `ReadTaskTrace { task_id }` → `TraceSnapshot { rows, high_water_id }`：一次性回灌。
- `SubscribeTrace { task_ids, after_id }` → 与 `SubscribeEvents` 同构的事件帧流
  （带 `event_id`、首帧快照、身份校验、溢出关闭）。

**复用而不新造**：带 `event_id` 的响应信封与校验、producer 线程与 10ms 轮询、
`write_frame_until` 分片写、`SubscriptionQueueClose::Overflowed` 语义，全部照搬现有
订阅路径的形状，只把事件载荷换成轨迹类型。

**打开时的无缝隙续接**（照搬现有 `Subscription` 首帧即快照的范式）：先 `ReadTaskTrace`
拿到截至当下最大 `id`，再以该 id 作 `after_id` 建 `SubscribeTrace`。

**按需订阅**（控制并发流数量）。并发上限是 `DEFAULT_GLOBAL_RESIDENT_SUBAGENTS = 16`
（`yi-agent-core/src/subagent/scheduler.rs:134`），即同时可能有十几条轨迹在产生。因此：

- **列表**：只订阅轻量状态（起没起、跑到第几步、当前工具名），数据源是**已经存在**的
  `TaskEvent`，一条几十字节。
- **完整轨迹**：**只为当前打开的那个 agent** 建立订阅；退回列表即断开。这天然把并发流
  控制在 1 条。用户同一时刻只能看一条轨迹，为屏外的十几个 agent 付渲染成本没有收益。

### 2.4 对话标记：让"按对话分组"成立

**约束（必须点明）。** 桌面端 root 是**按 cwd attach 的，不是按 thread**：同一 cwd 下多个
thread 共享同一个 `Arc<AttachedProjectRuntime>` 与同一个 `root_task_id`
（`yi-agent-app-server/src/server.rs:135`，`ProjectRuntimes` 缓存 `server.rs:71`；
`project-management/desktop.md` 明写"同目录两个 thread 共享同一个"）。所以"按 thread 分组"
**在当前协议下不可能**——两个 thread 的子 agent 挂在同一棵树上，daemon 分不出谁是谁。

**本期改动**（经用户确认，做进底层）：

- `tasks` 表加一列 `thread_id TEXT`（nullable；schema v12 与轨迹表同批）。
- `SpawnApplicationChild { thread_id: Option<String>, ... }`（`ipc.rs:190`）携带来源标记，
  沿 `runtime.rs:1153 spawn_child_with_objective` 落到 `tasks.thread_id`。
- **由父继承**：daemon 建孙任务时读取父的 `thread_id` 作为缺省，因此"某对话名下的全部
  子 agent"是一次筛选即可得到的集合。
- TUI 传 `None`，其全部子 agent 落在同一桶（语义即"终端会话"）。
- 缺省为空时行为与今天完全一致；标记只是附加信息，不改变任何准入/调度/审核语义。

### 2.5 呈现层：两个入口，一套观感

**TUI。** 现有运行时弹窗 `RuntimePopup`（`tui/app.rs:707`，已是 tab 枚举）加第三个页签
`Agents`：列表显示目标摘要 + 状态 + 当前在做什么；回车进入该 agent 的**只读轨迹视图**
（复用主对话那套 `HistoryCell` 渲染：助手文本 / 工具调用 / 工具结果 / 状态行，可滚动）；
若该 agent 自己还有子任务，可在其轨迹视图内继续下钻。Esc 逐级退回。`/agent <task_id>`
保留，直接跳到同一视图（两入口共用）。

**桌面。** 主对话区旁加**可折叠的子 agent 暂留区**，按当前对话列出其子 agent 卡片
（目标、状态、当前步骤）；点卡片进入详情——**两级视图：先摘要，再展开完整轨迹**，可下钻。
两个动作：**发消息**、**取消**。与主对话**并列可切换**，不是弹窗（不打断主对话）。

**干预动作落在已有能力上。** `SendUserMessage`（`ipc.rs:263`，TUI 已有 `/message` 命令）与
`CancelTask`/`PreviewCancel`+`ConfirmCancel` 均已存在。本期只把它们接到观察入口，
**不新造控制语义、不新增权限模型**。取消沿用既有预览 token 后确认的受控路径。

---

## 3. 不做（明确非目标）

1. **不在观察面做验收 / 打回 / 拒绝。** 验收仍由父 agent 用真实合并证明（现有
   `subagent-runtime.md` 的设计决定：approve 刻意不提供）。
2. **不做 reasoning / thinking 展示。** core 的 `ProviderEvent` / `AgentEvent` 无 thinking
   变体、provider 未解析 thinking block，属独立工作（另开条目）。
3. **不做暂停并编辑式中途干预。** 只提供发消息与取消。
4. **不做跨项目聚合观察。** 只看当前 attached 项目 / 当前会话的子树。
5. **不做轨迹导出 / 分享 / 搜索。**
6. **不做子 agent 权限审批入口。** 子 agent 走 `SandboxMode::WorkspaceWrite` 沙箱边界
   （`yi-agent-subagent/src/lib.rs:69`），是结构性放行而非交互式审批；被沙箱拒绝就是轨迹里
   一条失败的工具结果（`ToolResult.is_error`，已存在）。
7. **大输出 / diff 沿用现有 64 KiB 截断惯例**（`inspect_agent` 的 diff 截断）。

---

## 4. 验证策略

每层都要可跑的判据，不靠肉眼。

**数据层**

- schema v12 迁移可在旧库上执行（含 `LATEST_SCHEMA_VERSION` 断言）。
- 轨迹写入、聚合（同 512B/200ms 窗口的连续分片合并为一行）、每任务 2000 行上限、
  终态 24h 清理各有用例。

**续看（最容易出错，重点钉死）**

- 订阅断开后以游标重连，**不丢不重**：反射式断言"回放 + 增量"的拼接等于单次全量。

**分组（本期新增标记的核心断言）**

- 同一项目、两个 thread 各自 spawn 子 agent，各自只列出**自己**的子 agent。
- 孙任务继承父的 `thread_id`。

**按需订阅**

- 未打开的 agent **不产生**轨迹推送（断言只有打开的那条流存在）。

**TUI**

- 页签列表内容、钻入渲染（文本流 + 工具卡 + 状态行）、逐级返回、发消息与取消各有用例。

**桌面**

- 暂留区的分组与状态、钻入两级视图、发消息与取消各有用例，外加 `tsc --noEmit` 与构建。

---

## 5. 落地分期（可分段验收）

1. **数据层打底**：轨迹表 + `thread_id` 标记（schema v12）+ `ReadTaskTrace` / `SubscribeTrace`。
2. **源头接入**：worker 侧 `TraceSink` + 聚合，接进 reconcile。
3. **TUI 入口**：Ctrl+P 第三页签 + 只读轨迹视图 + 下钻 + 两个动作。
4. **桌面入口**：暂留区 + 详情两级视图 + 两个动作。

前三步各自独立可验；第四步依赖前两步，可与第三步并行推进。

---

## 6. 已知取舍

- **留存上限与"看完整轨迹"冲突。** 每任务 2000 行的上限意味着超长任务的最早部分会被裁掉。
  这是为 DB 增长设的上界；若实际任务尺度更大，实现时可上调，但必须显式设一个上界。
- **聚合带来的时延。** 文本流最迟 200ms 才可见（聚合窗口），不是逐字蹦。这换来的是行数
  从 token 级降到块级，是刻意的取舍。
- **`thread_id` 是新增协议字段。** 它只做分组，不参与任何准入与调度；但对老库需迁移，
  对老客户端需容忍字段缺失（`#[serde(default)]`）。
