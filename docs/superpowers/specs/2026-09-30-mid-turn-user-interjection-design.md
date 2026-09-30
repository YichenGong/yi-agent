# 轮次中途追加用户输入（mid-turn interjection）设计

**目标：** 让用户在 agent 执行一个周期（turn）的过程中提交的输入，**在系统下一次发出
API 请求之前**就进入上下文，而不是等整个周期跑完、`Done` 之后才被当作新一轮的开场白。

**状态：** 设计已确认，待写实现计划。

**范围：** core 提供 mid-turn 注入通道；TUI 与 desktop（app-server）两端接入。

---

## 1. 问题

### 1.1 现状：追加输入必须等整个周期结束

一次 `agent.run()`（`yi-agent-core/src/agent.rs:394`）内部是一个
`run_loop`（`:523`），循环里可以发生任意多次 provider 请求（`let req =
ProviderRequest`，`agent.rs:590`）。这个循环**没有任何用户输入入口**：
在 `agent.rs` 内检索 `input_rx` / `user_input` / `pending_input` / `interject`，
**零匹配**。消息向量 `messages` 只由模型自身输出推动增长
（`tool_results`、`CONTINUE_AFTER_TRUNCATION`、`COMPLETION_AUDIT_PROMPT`）。

三个前端因此都只能在周期结束后接受新输入：

| 前端 | 位置 | 行为 | 后果 |
|---|---|---|---|
| TUI | `main.rs:1321` 通道 + `main.rs:1600` | driver 的 `input_rx.recv()`（`main.rs:1381`）在 `agent.run(text).await`（`main.rs:1600`）**之外**；运行中不消费任何输入 | 忙时提交只能缓冲，等 `Done` 后转正 |
| TUI（缓存） | `tui/app.rs:398`、`tui/app.rs:1613` | 忙时 `PendingQueue::submit` 返回 `Queued`，**只进预览**，发送推迟到回合结束 | 见 `docs/superpowers/specs/2026-09-27-tui-pending-queue-design.md` §3.2 |
| desktop | `app-server/src/server.rs:837` | 活跃时第二个 `turn/start` 被拒绝：`RpcError::turn_in_progress` = `-32012`（`protocol.rs:99-100`）；desktop 侧回滚乐观气泡（`desktop/src/App.tsx:311-316`） | 用户必须等本轮结束 |

（子 agent 侧另有一处同类语义：`subagent_runtime.rs:620-626` 收到 mailbox 消息后
`agent_cancellation.cancel()`，靠 `:735` 的 `retrying_provider = true` 重启一次
`retry_current_session()`。即"中途输入"目前是**取消 + 重启**实现，不是注入。
本设计**不**统一该路径，见 §8。）

### 1.2 期望行为

输入到达后：**不打断**当前正在进行的流式输出或工具执行；在**下一次 provider 请求
之前**并入上下文；并消除"输入刚好撞上 `EndTurn` 判定"这个竞态，使"追加"不会退化成
"开新一轮"。

---

## 2. 决策清单

每条决策都记录理由，便于日后回溯。标注 **【已核实】** 的陈述有 `file:line` 证据。

### D1 时机：请求前注入 + `EndTurn` 判定前再查一次

注入**不**打断进行中的流式输出或工具执行。drain 发生在三个点（见 §4）。其中
"`EndTurn` 判定前再查一次"（`agent.rs:841` 的 `tool_uses.is_empty()` 分支内）用于消除
竞态：否则输入若在模型给出无工具调用的回复之后到达，仍会退化为新轮。

**排除**"到达即打断流/工具"方案：需处理 partial 丢弃、工具半途中断、会话配对回滚
（`safe_cancel_truncate_len`，`agent.rs:464`），风险面最大。

### D2 / D10 / D11 TUI 忙时缓冲：单一职责

- **D2**：忙时一律投注入路径；投递失败则沿用现有 `Rejected` 语义（文本退回输入框 +
  history 提示行），不做静默丢弃。
- **D10**：**复用现有 `input_tx`（`main.rs:1321`，容量 16），由 driver 分流**——不新增
  第二条"TUI 直达 core"的路径，也不要求 `Agent` 对外暴露可跨重建持有的句柄。
  **【已核实】** 该设计可行且无句柄陈旧问题：`input_tx` 属于 driver（不是 `Agent`），
  driver 内的 5 处 `Agent::new`（`main.rs:1367/1389/1411/1437/1498`）都不换它，因此
  它天然跨重建存活。driver 在运行期间把收到的文本调 `agent.interject(...)`；空闲时按
  今天的方式当新 prompt。
- **D11**：`PendingQueue` 更名 `DeliveredInterjections`（见 §3.4），保留三态
  `SubmitOutcome`（`Sent`→投出/`Rejected`→退回），预览能力不变。

**代价（明示）**：`agent.interject()` 需要 `&Agent`，而 driver 的运行循环里"够得到
`agent` 的时刻"是**每次 `agent.run()` 返回之后**（`main.rs:1600` 的 `match agent.run(text).await`）。
因此实现必须：

1. 在 `main.rs:1602`（`let mut stream = Box::pin(stream);`）之后**立即取一次
   `let inbox = agent.inbox_handle();`**，再把 `inbox` 与 `cancel_token` 一起交给转发循环；
2. 把 `inbox`/`cancel_token` 放进同一个 `tokio::select!`（`main.rs:1606` 的
   `event = stream.next()` 与 `main.rs:1616` 的 `_ = interrupt_rx.recv()` 旁边）；
3. **整个 loop 必须改成 `while let Some(ev) = ...` 并持有 `agent`**：今天
   `agent.cancel()` 在 `select!` 分支内被调用（`main.rs:1618`），说明该处本来就持有
   `&mut agent`，不需要额外借用技巧。

若 `inbox_handle()` 返回 `None`，该条走 `Rejected` 并复用现有提示行。

**排除**：把投递改经第二条 core 直达通道（会与 `input_tx` 形成并行路径，重演
`docs/superpowers/specs/2026-09-27-tui-pending-queue-design.md` §1.2 花力气消掉的
"双 FIFO 时钟错位"）。

**说明：** driver 在 `run()` 返回后取句柄，故"attach/重建 agent 与第一次 `run()` 之间"
的窗口内投递会走 `Rejected`。该窗口不是新增缺陷（今天 TUI 的 `in_flight` 也存在同类
时钟差异，见同设计 §1.3），本设计只需在该窗口返回 `Rejected` 并复用现有提示行。

### D3 注入消息形态：带前缀标记

注入文本包一层说明后作为 user 消息进入 `messages`，与 `agent.rs:313` 的
`CONTINUE_AFTER_TRUNCATION`、`agent.rs:315` 的 `COMPLETION_AUDIT_PROMPT` 同风格。
前缀明确"这是对当前任务的修订，不要重复已完成的工作"，避免模型当作新任务从头开始。

**排除**裸文本（模型易误判为新任务）、前缀+已观测事实提醒（每轮固定开销，收益接近）。

### D4 轮次归属：算同一轮，新增独立 item

注入并入当前 `turn_id`：`active_turn_id`（`app-server/src/session.rs:23`）不动，因此
不触发 `-32012` 分支（`server.rs:837`），本轮仍只落一行 `TurnLine::Turn`
（`server.rs:1261` 的落盘合成仍是 `user-{turn_id}`，只装本轮首条 prompt）。

中途追加在协议上表现为**独立 item**：`Item::UserInterjection { id, text }`。
**【已核实】** `protocol.rs:206-224` 现有 `Item` 仅有 `UserMessage` / `AgentMessage` /
`ToolCall` 三个变体，故这是纯新增。

### D5 打断时：未消费文本经事件回推前端

Esc / `turn/interrupt` 触发取消时，把 inbox 中尚未被注入的文本**回推**给前端：
TUI 塞回输入框、desktop 按 tag 精确撤回乐观气泡。零丢失，且语义不漂（这些文本本意是
"对当前任务的修订"，不应漂成下一轮的开场白）。

### D6 事件形态：新增独立事件，`Cancelled` 零改动

新增 `AgentEvent::InterjectionsReturned { items }`（取消时回推），**不**给
`Cancelled` 加负载。**【已核实】** `AgentEvent::Cancelled` 全仓 24 处引用：core 生产
构造 5 处（`agent.rs:549/612/669/726/1062`）、core 测试断言 5 处
（`:2225/2425/2527/2656/3047`）、app-server 生产 1 处（`translate.rs:308`）、app-server
测试 3 处、TUI 4 处（`history.rs:452`、`app.rs:355/374/1280`）、TUI 测试 1 处
（`app.rs:3873`）、subagent 1 处（`subagent_runtime.rs:707`）、headless 4 处
（`main.rs:1125/1138/1168/2212`）。给 `Cancelled` 加负载需改约 19 处，其中 10 处是测试；
新增独立事件则**既有 24 处一处不动**。

**代价（明示）**：`InterjectionsReturned` 必须先于 `Cancelled` 发出。TUI 把 `Cancelled`
当作回合结束判据（`app.rs:355`/`:374`/`:1280` 的 `Done | Cancelled | Error`），顺序写错
会导致回推内容被跳过——**静默丢话**。类型系统无法阻止，必须由测试钉住（§7 测试 ③）。

### D7 desktop 协议：新增 `turn/interject`

新增独立方法，而非让 `turn/start` 在活跃时"变脸"成注入：

- `turn/start` 的 `-32012` 契约**完全保留**（`server.rs:837` 不动，唯一测试
  `server.rs:2138`/`:2162` 不动）——真正的并发冲突仍然拒绝。
- `turn/interject` 活跃时返回 `{ turn_id: <当前活跃 id>, interjection_id }`；
  不活跃时返回新错误码 `-32013 not running`。**【已核实】** 全仓 `32013` 匹配数为 0，
  该码未被占用。
- desktop 在 turn 活跃时改调 `turn/interject`（状态已可从 `statuses` 读到，
  `App.tsx:328-336`）。

**排除**：`turn/start` 双语义（隐式契约，且会让唯一那条 `-32012` 测试失去意义）、
`-32012` 附带自愈提示（多一轮往返，错误码当控制流）。

### D8 事件回执：`InterjectionAccepted`

新增 `AgentEvent::InterjectionAccepted { seq, text, tag }`，在文本**真正 push 进
`session` 之后**发出（不 是投递时）。预览区的"待生效条数"由它减数。

**排除**：预览区改为无条数的"已追加，等待生效"（砍掉用户唯一关心的信息——"我那几句
话到底进去没有"，而这正是本 bug 的原始抱怨）；TUI 轮询 core 原子计数（引入事件流之外
的第二真相来源，方向与本次改动相反）。

**代价（明示）**：从"投递成功"到"被消费"之间有可见延迟（最坏等到下次请求前），预览区
必须表达这层含义，否则用户会重复发送。

### D9 去重与回执对账：`seq` + 可选 `tag`

- core 单调分配 `seq`，`InterjectionAccepted` 与 `InterjectionsReturned` 都携带。
  前端据此**精确**知道哪几条生效、哪几条退回，无需按内容猜测。
- `tag` 为调用方自带的稳定对账标识：app-server 传入在 RPC 入口铸好的
  `interjection_id`（复用 `translate.rs:122` 的 `item-<turn_id>-<n>` 命名空间）；
  TUI 传 `None`（它不需要跨进程对账）。

### D12 / D13 drain 位置：compact 之后、计入 turn

- **D12**：drain 放在 auto-compact 块（`agent.rs:553-558`）**之后**。否则注入文本会被
  `maybe_auto_compact`（定义 `agent.rs:1175`）的 `replace_messages`
  （调用 `agent.rs:1195`）整体替换掉——用户刚打的字只剩摘要，不可接受。
- **D13**：drain 放在 `turn += 1`（`agent.rs:560`）**之后**，即注入消耗一个 turn。
  `max_turns` 默认 `Some(100)`（`agent.rs:114`），界继续有效，避免无限追加把循环
  永久续下去。**【已核实】** `turn += 1` 位于 compact 与 max_turns 检查（`:561`）之间，
  故 A1/A2 物理上就是 drain 写在 `:560` 上边还是下边的区别。
- 另确保 drain 在 `last_logged = messages.len()`（`agent.rs:587`）**之前**，令注入文本
  出现在本轮的请求增量 debug 日志里。

### D14 审计关卡：追加优先

结束竞态的 drain（D1）放在 `tool_uses.is_empty()`（`agent.rs:841`）内、
`verification_pending && !audit_attempted`（`agent.rs:842`）**之前**：用户明确追加了要求，
应优先于系统强制审计，而不是被审计插到前面。

### D15 回推顺序不变量

五处 cancel 点（`agent.rs:549/612/669/726/1062`）在发 `Cancelled` 之前先发
`InterjectionsReturned`。见 D6 的代价说明。

---

## 3. 架构

### 3.1 core：`Inbox` 与 `InboxHandle`

```rust
pub struct Interjection { pub seq: u64, pub text: String, pub tag: Option<String> }

pub enum InterjectError { Full, NotRunning }

impl Agent {
    /// 投递一条中途追加。返回 `seq` 供前端对账。
    pub fn interject(&self, text: String, tag: Option<String>) -> Result<u64, InterjectError>;
    /// 本次 run 的投递句柄。driver 在 `run()` 返回后立即取,放进转发循环。
    /// 返回 `None` 表示当前没有活跃 run,调用方按 `NotRunning` 处理。
    pub fn inbox_handle(&self) -> Option<InboxHandle>;
}
```

不变量：

1. `Inbox` 有界（容量与 TUI 现有 `PendingQueue::CAPACITY` = 16 对齐，`queued.rs:28`），
   FIFO，满则 `Full`。
2. `seq` 单调递增，永不复用；前端以 `seq` 为唯一对账键。
3. **句柄的生命周期就是一次 `run()`**（这是 D10 的落法）：`start_run`（`agent.rs:409`）
   每次重建 cancel token（`agent.rs:409`），`InboxHandle` 与它同生命周期。driver 在
   `agent.run()` 返回后取句柄（`main.rs:1602` 之后），交给转发循环使用；`run()` 结束
   即失效，下一次 `run()` 重新取。**因此不存在"句柄陈旧/需在 5 处重建点轮换"的问题**——
   这是选择 D10（复用 `input_tx` 分流）而非"TUI 直接持句柄"的核心理由：后者才需要在
   5 处 `Agent::new` 重建点逐个轮换句柄，漏一处即静默丢消息。

### 3.2 新增事件

```rust
AgentEvent::InterjectionAccepted { seq: u64, text: String, tag: Option<String> },
AgentEvent::InterjectionsReturned { items: Vec<Interjection> },
```

两者都是纯新增；**`Cancelled` 保持 `agent.rs:269` 原样**。

### 3.3 app-server

- 新增 `turn/interject`：参数 `{ threadId, input: [{type:"text",text}] }`。
- 响应 `{ turn_id, interjection_id }`；`interjection_id` 作为 `tag` 传入 core。
- 新错误码 `-32013 not running`（`32013` 当前未被占用）。
- 注入生效时 translator 以 `interjection_id` 发 `Item::UserInterjection`。
- `turn/start` 与 `-32012` 路径不动（D7）。

**投递路径遵循 D10：不新增"主循环直达 agent"的旁路。** 主循环够不到 driver 持有的
`agent`，但**它有 `threads: HashMap<thread_id, ThreadSession>`，而 `ThreadSession` 已经
持有通往 driver 的发送端**（`prompt_tx`、`interrupt_tx`，`session.rs:27/29`）。因此新增
同类的 `interject_tx`，与 `interrupt_tx` 完全对称：

- `ThreadSession` 新增 `interject_tx: mpsc::Sender<InterjectionRequest>`；
- `run_thread_driver` 新增 `interject_rx: mpsc::Receiver<..>` 参数（与既有
  `interrupt_rx` 并列，`server.rs:1084` 的签名处）；
- driver 在**已有**的两处内层 `select!`（`server.rs:1191` 与 `server.rs:1246`
  的 `Some(target) = interrupt_rx.recv(), if !cancel_sent` 分支旁）加一路
  `interject_rx.recv()` → 调 `agent.interject(text, Some(interjection_id))`。

这与 TUI 侧（`input_tx` → driver → `agent.interject`）**是同一个形状**，两端因此共享
同一条设计，而不是各写一套。

### 3.4 TUI

- `PendingQueue` → `DeliveredInterjections`：结构体仍持有 `items: Vec<String>` 与
  `in_flight`（`queued.rs:22-25`），保留预览能力与三态 `SubmitOutcome`；
  忙时 `submit` 由"入队"改为"投递"。
- 预览的"待生效条数" = 已投递、未见 `InterjectionAccepted`/`InterjectionsReturned` 的
  `seq` 集合大小（纯推导，不新增状态源）。
- **预览标题改为 `⌛ 已送达，待生效 (N)`**：这直接回应 D8 明示的代价（用户可能因看不到
  反馈而重复发送）。改标题需同步改两处既有断言（`queued.rs:302` 的 `"⌛ 排队中 (1)"`、
  `queued.rs:336` 的 `"⌛ 排队中 (10)"`）。**保留原标题亦可**（`queued.rs:115`），但会在
  "投递已到达 core、尚未注入"的窗口里误导用户以为还没发出——所以本设计选改标题，并把
  两处断言的改动计入成本。

**改名成本（明示）**：`PendingQueue` 这个名字在改造后会"说谎"（不再排队，而是记账
待回执）。决策是改名为 `DeliveredInterjections` 并重写文档注释，而不是删掉类型把状态
并入 `app.rs`。**【已核实】** 两者测试成本几乎相同：`queued.rs` 共 14 个测试，渲染类 7 个
（`:167/188/286/293/306/314/328`，只依赖 `&[String]`，两种方案都不动）、语义类 7 个
（`:204/212/221/238/252/261/274`，两种方案都要改）；`app.rs` 触及队列的测试 6 个
（`:4490/7006/7089/7139/7246/7297`）。区别只在"保留有名字的类型（需改名）" vs
"删类型、状态并入视图层"。

---

## 4. 数据流

### 4.1 正常注入

```
前端 interject(text, tag)
  → Inbox（FIFO，满则 Full）
  → run_loop 循环顶部 drain：compact(agent.rs:553) 之后、
    turn += 1(agent.rs:560) 之后、last_logged(agent.rs:587) 之前
  → 包前缀 → messages.push + session.push
  → 下一轮 req(agent.rs:590) 带上它
  → 发 InterjectionAccepted { seq, text, tag }
```

### 4.2 结束竞态（D1）

```
tool_uses.is_empty() (agent.rs:841)
  → 先 drain 一次
    ├─ 有内容 → 注入 + continue（不发 Done）
    └─ 无内容 → 进审计关卡 (agent.rs:842) → … → 发 Done(EndTurn)
```

### 4.3 取消回推（D5 / D6 / D15）

```
cancel 点（agent.rs:549/612/669/726/1062）
  → 发 InterjectionsReturned { items: <全部未消费, 含 tag> }
  → 发 Cancelled（原样, agent.rs:269 不变）
TUI：文本回输入框
desktop：按 tag 精确撤回乐观气泡
```

### 4.4 desktop 时序

```
turn 活跃时用户提交
  → desktop 调 turn/interject（而非 turn/start）
  → app-server 铸 interjection_id 作为 tag → core.interject(...)
  → 注入生效 → Item::UserInterjection → desktop 渲染"中途追加"气泡
不活跃时用户提交
  → 仍走 turn/start（-32012 语义不受影响）
```

---

## 5. 错误处理与边界

| 情形 | 行为 |
|---|---|
| inbox 满 | `interject` → `Full`；TUI → `Rejected`（文本退回输入框 + 提示行，沿用 `tui/app.rs:1613` 附近现有样式）；desktop → RPC 错误，走已有的乐观气泡回滚（`App.tsx:311-316`） |
| agent 未运行 | TUI：`inbox_handle()` 为 `None` 的窗口内 → `Rejected`（见 D2 代价）；desktop → `-32013` |
| 与 auto-compact 交互 | drain 在 compact 之后（D12），注入文本不会被 `replace_messages`（`agent.rs:1195`）摘要掉 |
| 与审计关卡交互 | 结束竞态 drain 在审计关卡之前（D14） |
| 与 `max_turns` 交互 | 注入计入 turn（D13）；默认界 100（`agent.rs:114`） |
| 取消时仍有未消费 | `InterjectionsReturned` 回推（D5/D15），不静默丢弃 |
| 异常路径未发 `InterjectionAccepted` | `InterjectionsReturned` 兜底，不会永久挂在"待生效" |

---

## 6. 影响面

| 文件 | 改动 |
|---|---|
| `yi-agent-core/src/agent.rs` | `Interjection`/`Inbox`/`InterjectError`、`Agent::interject`、`Agent::inbox_handle`、两个新事件、`INTERJECTION_PREFIX` 常量、三处 drain、五处 cancel 点加回推 |
| `yi-agent-app-server/src/protocol.rs` | `Item::UserInterjection`、`RpcError::not_running`(`-32013`) |
| `yi-agent-app-server/src/session.rs` | `ThreadSession` 新增 `interject_tx`（与 `prompt_tx`/`interrupt_tx` 并列） |
| `yi-agent-app-server/src/server.rs` | `turn/interject` 分支写入 `interject_tx`；`run_thread_driver` 新增 `interject_rx` 参数并在两处内层 `select!`（`server.rs:1191`/`:1246`）加一路 |
| `yi-agent-app-server/src/translate.rs` | `InterjectionAccepted` → `Item::UserInterjection` |
| `yi-agent/src/main.rs` | 运行循环在 `main.rs:1602` 后取 `inbox_handle()`、转发 `select!`（`main.rs:1606`/`:1616`）加一路、`agent_tx`(256, `main.rs:1320`) 转发两个新事件 |
| `yi-agent/src/tui/queued.rs` | `PendingQueue` → `DeliveredInterjections`、忙时投递、`seq` 对账 |
| `yi-agent/src/tui/app.rs` | `submit` 忙时改投递、`Accepted` 减数、`Returned` 回输入框、`Cancelled` 前先处理回推（`app.rs:355/374/1280` 三处判据的顺序敏感） |
| `yi-agent/src/tui/history.rs` | 追加行渲染 |
| `desktop/src/App.tsx` | 活跃时改调 `turn/interject`；`Returned` 时按 tag 撤回气泡 |
| `desktop/src/lib/protocol.ts` | 新方法与新 item 类型 |

`docs/project-management/` 与 `README.md` 索引计数同步更新（CLAUDE.md 要求，同一 PR 内）。

---

## 7. 测试计划（TDD，先红后绿）

**core**

1. 注入在下次请求前生效：断言第二次 `ProviderRequest.messages` 含前缀 + 用户文本。
2. 结束竞态：`EndTurn` 判定前 drain 到内容时**不发** `Done`，而是续行（断言事件序里
   无 `Done`、有 `InterjectionAccepted`）。
3. **顺序不变量**（D6 代价）：`InterjectionsReturned` 出现在 `Cancelled` **之前**，
   且 `tag` 原样透传。
4. inbox 满返回 `Full`。
5. 注入计入 `max_turns`。

**app-server**

6. 活跃时 `turn/interject` 返回 `{ turn_id, interjection_id }`，且最终产出
   `Item::UserInterjection`。
7. 不活跃时返回 `-32013`。
8. 回归：`server.rs:2138` 的 `-32012` 测试**保持通过**（证明 D7 未削弱既有契约）。

**TUI**

9. 忙时提交不再产生"第二条 new-prompt"路径（今天 `app.rs:1613` 的入队改为投递）。
10. 收到 `InterjectionAccepted` 后预览条数减一。
11. 收到 `InterjectionsReturned` 后文本回到输入框。

**验证命令（计划阶段细化，此处给出目标）**

```
cargo test -p yi-agent-core --lib agent::tests::
cargo test -p yi-agent-app-server
cargo test -p yi-agent --bin yi-agent -- tui::
cd desktop && npm test
```

---

## 8. Non-goals

- **不改 `Cancelled`**（D6）。
- **不统一 subagent mailbox 与 inbox**：`subagent_runtime.rs:620-626` + `:735` 的
  "取消 + 重启"路径保持原样，留作独立议题。
- **不支持打断进行中的流式输出或工具执行**（D1 已排除）。
- **不改 headless 单 prompt 模式**（无排队需求）。
- **不做队列编辑/撤销**：延续既有 YAGNI 决策。

---

## 9. 风险与未决

1. **顺序不变量靠测试而非类型**（D6）：若未来有人调整 `Cancelled` 周边代码，回推可能被
   跳过。缓解：测试 ③ 明确断言顺序。
2. **运行循环改动的借用复杂度**：TUI 侧要把 `inbox_handle()` 与 `cancel_token` 一起放进
   转发 `select!`（`main.rs:1606`/`:1616`），app-server 侧要在两处内层 `select!`
   （`server.rs:1191`/`:1246`）各加一路。**【已核实】** 两处 `select!` 的分支内**都已在
   调用 `agent` 的方法**（TUI 的 `main.rs:1618` `agent.cancel()`；app-server 的
   `cancel_token.cancel()`），因此"循环里持有 `agent`"不是新增约束，而是现状。实现时
   仍需确认 `while let Some(ev) = stream.next().await` 的写法不影响既有 break 语义。
3. **语义 vs 计数的取舍**：`DeliveredInterjections` 会保留一个"名字→语义"的转换成本；
   若实现中发现"记账"逻辑更自然地位于视图层，可回退到"删类型"方案（§3.4 已论证二者
   成本相当）。

---

## 10. 附：本设计引用的行号如何取得

§1–§6 中所有行号均以 worktree 基点 `656673c` 为准，且**逐条经 `sed`/`grep` 原样读出并
对照**。已核验的行号如下（每条都确认输出与引用内容一致）：

- **core `agent.rs`**：82（`run`）、87（`max_turns`）、114（默认 100）、269（`Cancelled`）、
  313/315（两个常量）、394（`pub async fn run`）、409（`start_run`）、414（重建 cancel
  token）、464（`safe_cancel_truncate_len`）、523（`run_loop`）、535（`turn`）、547（cancel
  关）、549/612/669/726/1062（五处 cancel 点）、553（compact 关）、560（`turn += 1`）、
  561（max_turns 关）、587（`last_logged`）、590（`req`）、841（`tool_uses.is_empty()`）、
  842/843（审计关卡）、1073（`verification_pending = true`）、1175（`maybe_auto_compact`
  定义）、1195（`replace_messages` 调用）；测试断言 2225/2425/2527/2656/3047。
- **TUI `main.rs`**：1320（`agent_tx` 容量 256）、1321（`input_tx` 容量 16）、1381
  （`input_rx.recv()` 分支）、1600（`agent.run`）、1602（`Box::pin(stream)`）、1606
  （`stream.next()`）、1616（`interrupt_rx.recv()`）、1618（`agent.cancel()`）；
  `Agent::new` 的 5 处：1367/1389/1411/1437/1498。
- **TUI `queued.rs`**：22（`PendingQueue`）、28（`CAPACITY`）、102（渲染函数）、115（标题
  行）；14 个测试按行切分：渲染类 167/188/286/293/306/314/328，语义类 204/212/221/238/
  252/261/274。
- **TUI `app.rs`**：355/374/1280（三处 `Done | Cancelled | Error` 判据）、398 与 1613
  （两处 `input_tx.try_send`）、3873（测试构造）、队列测试 4490/7006/7089/7139/7246/7297。
- **app-server**：`protocol.rs` 99（`turn_in_progress`）、206（`Item`）；`session.rs` 23
  （`active_turn_id`）、27（`prompt_tx`）、29（`interrupt_tx`）；`server.rs` 451/459 与
  608/616（`driver_turn_tx`，说明"主循环持有通往 driver 的发送端"已成惯例）、837
  （`-32012`）、1084（`run_thread_driver` 签名）、1191 与 1246（两处内层 `select!`）、
  1261（`user-{turn_id}` 落盘）、2138（`-32012` 唯一测试）；`translate.rs` 122（item id
  命名空间）、308（`Cancelled` 分支）。
- **desktop**：`App.tsx` 311-316（乐观气泡回滚）。
- **subagent**：`subagent_runtime.rs` 620-626（mailbox 取消）、735（`retrying_provider`）。

行号会随代码演进漂移，引用时应以符号名（函数名、常量名）为准；本清单的作用是让
实现阶段能快速判断"引用是否已过期"。
