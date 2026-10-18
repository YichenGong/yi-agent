# 项目组长 Agent P3-B/C：写动作与移动端缺口设计

状态：已实现

上级设计：[2026-10-17-project-lead-agent-design.md](./2026-10-17-project-lead-agent-design.md)（§6.1、§6.2、§8、§9.1、§9.2、§10、决策 6/12/13）
前序分期：[P1](./2026-10-17-project-lead-agent-p1.md) 已实现、[P2](./2026-10-17-project-lead-agent-p2.md) 已实现、[P3-A](./2026-10-18-project-lead-agent-p3-view-design.md) 已实现（视野工具）。

## 1. 本设计覆盖的范围

上级 spec §11 把 P3 定为三块：①视野工具（= P3-A，已另开设计）；②权限留痕/撤销；③移动端远程。
本设计覆盖**②（下称 P3-B）与 ③ 的真实缺口（下称 P3-C）**，一份 spec、两个章节，但**风险等级不同**：

- **P3-B 是后端改写**：给组长新增两个写工具，直接触碰「谁能在无人值守时改别人的会话」这条底线。
- **P3-C 是前端小修**：修一个真 bug + 加一处视觉区隔。

两者虽同属 P3，但一个高风险、一个低风险。计划里会**分开成独立 Task 组**，B 的每个 Task 都要单独过验证；C 的 Task 可以一次做完。

**③ 里有一条不属于本轮**：决策 12「组长起真 thread（带 `created_by` 标记）」至今**未实现**。
它才是 §9.1 里 `created_by` 的真实归属，见 §10「范围外」。

## 2. 要解决的问题

### 2.1 P3-B：§8 的两个写入目标都不存在

上级 §8 写：「后台循环可写：取消 / 打回 / 改 objective」，且每个动作必须留痕、可撤销、
不绕过状态机（引用了 `review_agent` 强制 feedback/reason、`accept` 只由 Git ancestry 观察）。

现实（逐条核过代码）：

| §8 的前提 | 今天的现实 | 证据 |
|---|---|---|
| `review_agent` 强制 feedback/reason | 该工具**被测试强制不得注册** | `yi-agent-subagent/src/lib.rs:2343`（「交付即完成：验收是父自己的 git 动作，运行时不再有交互式 review 工具」） |
| `accept` 只由 Git ancestry 观察完成 | 同上，无此运行时状态机 | 同上 |
| 可「改 objective」 | thread **没有** objective 字段；`objective` 只存在于 legacy 自主 root session | `yi-agent-store/src/runtime.rs:60`；`ThreadMeta` 无该字段（`thread_store.rs`） |

结论：P3-B **不是「给写动作补留痕」，而是先重建「组长能写什么」这份清单**。
在"交付即完成"的世界里，没有 review 状态机可走，也没有 objective 可改。

### 2.2 组长今天的写能力：零

组长（`role: lead` 的 thread）今天只有：

- `ask_lead`（**收**信，单向或阻塞）——`lead.rs:202`；
- `lead_remember` / `lead_recall`（读写自己的账本）——`lead_memory.rs`；
- 以及**所有 thread 都有的**子代理工具 `spawn_agent` / `cancel_agent` / `send_message` / `inspect_agent`
  （但这些操作的是**它自己 spawn 的子 worker**，不是「项目里的其它 thread」）。

**它对「项目里的其它 thread」没有任何写能力**——这正是 P3-B 要补的。

### 2.3 P3-C：组长行在手机上长按会弹出无意义的会话菜单

侧栏组长行（`ThreadSidebar.tsx:772`）带 `data-lead-row` **也**带了 `data-thread-row`。
移动端长按由 `useLongPress` 按 `data-thread-row` 判定落点为「会话行」（`ThreadSidebar.tsx:294`），
于是长按组长行会弹出「重命名 / 置顶 / 删除」（`ThreadSidebar.tsx:669/680/691`）。
后两项对组长**无意义**（组长是项目级固定入口，置顶与删除不适用），这是一个**真 bug**。

此外上级 §9.1 要求组长在侧栏是「IM 式独立头像」，今天侧栏**没有任何头像组件**（`grep avatar` 为空）。

## 3. P3-B 工具清单

新增两个写工具，**只对组长注册**（门控见 §6）：

| 工具 | 语义 | 目标状态要求 | 对目标转录的可见后果 |
|---|---|---|---|
| `lead_interrupt_thread({ thread_id, reason })` | 打断目标 thread 的**当前活跃轮** | 必须有活跃轮（否则报错） | 该轮以 `interrupted` 收尾 |
| `lead_reject_thread({ thread_id, reason })` | 「打回」：向目标投递一条「组长的指令 + 理由」 | 任意 | 转录里多一条用户消息（经注入路径） |

**明确不做**（§8 里不存在的东西）：

- **不做 `lead_set_objective`**：thread 没有 objective 字段，没有可改的对象。
- **不走 review 状态机**：`review_agent` 不存在，无可走的路径。

### 3.1 「打回」的语义：非阻塞注入

打回**不打断**目标当前正在跑的轮。具体映射（复用既有原语，不新造机制）：

- **目标有活跃轮** → 走该 thread 的 `interject_tx`（`InterjectionRequest` struct 见 `session.rs:61`，
  投递端 `interject_tx` 见 `session.rs:139`）。
  这与「用户在 thread 跑动中打字」**完全同一路径**：消息在 turn 边界经 `inject_pending`
  折进转录（`yi-agent-core/src/agent.rs:876`），带 `INTERJECTION_PREFIX` 前缀。
  即「组长的纠偏」与「用户的中途补充」在目标 thread 眼里同构——这是刻意的：**不引入第二套注入语义**。
- **目标无活跃轮（idle / 卡在审批）** → 走该 thread 的 `prompt_tx` 起**一轮新 turn**，
  prompt 即「组长的指令 + 理由」。见 §4 的偏差记录。

### 3.2 「取消」的语义：复用既有打断原语

`lead_interrupt_thread` 不新造打断机制，等价于用户按 `Esc`：

- 取目标 `session.active_turn_id`，`session.interrupt_tx.try_send(turn_id)`
  （与 `rpc/turn.rs:99` 的 `turn/interrupt` 逐字同源：幂等 level 信号、`try_send` 避免阻塞）。
- 目标无活跃轮时**报错**（不是 no-op）：让组长知道"这一下没打着"，而不是以为打出效果了。

### 3.3 撤销：追加式 supersede

§8 要求的「可撤销」，本设计选**追加式**：

- 组长要撤回一条打回，就再调用一次 `lead_reject_thread`，消息文本形如
  「撤回上一条打回（理由：…）；按原要求继续」。
- 转录 append-only，靠对话历史自然表达"先打回、后撤回"。**零新机制、零新存储**。
- 不引入「打回账本 + 状态位」：今天**没有任何消费者**需要机器判定"某条打回是否已被撤销"
  （YAGNI）；若将来出现该消费者，再补结构化账本。

### 3.4 留痕：纯转录

§8 要求「谁、何时、为什么」可查。本设计**不新增审计存储**，因为两处转录天然满足：

| 留痕点 | 内容 | 谁看得到 |
|---|---|---|
| **目标 thread 转录** | 一条用户消息（打回文本）或一轮 `interrupted` | 从 thread 入口看（上级 §5.4 的既有承诺） |
| **组长转录** | 组长那次 `lead_reject_thread` / `lead_interrupt_thread` 的工具调用 | 从组长 IM 入口看 |

这与本设计"注入即留痕"的整体选择同构（§3.1）。**刻意不采用的替代**：让组长"顺便"调用
`lead_remember` 记账——那**依赖 LLM 每次记得写**，正是上级 §7.7 明确反对的模式
（「靠机制保证远比靠提示词自律可靠」）。若将来需要机器可查的干预日志，正确做法是在
**工具实现内部强制落盘**，而不是靠提示词。

## 4. 与上级 spec 的偏差（显式记录）

### 4.1 打回 idle 目标会「立刻起一轮」——偏离决策 13 / §6.2

上级 **决策 13**：组长「目标**由用户在 IM 里给**；后台只围绕该意图盯梢推进，**不自创目标**」。
上级 **§6.2**：唤醒事件只有四类（`ask_lead` / 终态或卡住 / 用户在 IM 说话 / 低频兜底 tick）。

**本设计选择**：`lead_reject_thread` 打到 idle 目标时，**立刻起一轮**让它生效（否则打回要等到
目标"自然"起下一轮，可能永远不生效）。

**为什么这不是完全越界**：打回本身由一次**唤醒事件**触发（某 thread `ask_lead`、卡住、
或用户在组长 IM 里发话），不是组长凭空自创任务；组长只是把"这次唤醒的结论"**落成一轮**。

**它确实打开的风险**：`打回 → 目标重跑 → 目标又 ask_lead → 组长又打回 …` 的乒乓循环，
无人值守地烧钱。**本期不做循环防护**——记为**已知限制**（§9），理由：暂无证据表明该循环是
常态，先上线观察；若实测出现，再加"每 thread 每次唤醒至多一次打回起轮"的硬闸（计数需外部
事件重置）。

### 4.2 「不绕过状态机」在新世界里无从遵循

§8 第 3 条「不绕过状态机」引用的是**已不存在**的 review 状态机（§2.1）。
本设计的对应物是：**取消**复用 `turn/interrupt`、**打回**复用 interjection
——即"不新造绕过既有 turn 生命周期的旁路"。这是对 §8 第 3 条精神（不施加旁路写）
在现实现下的**重述**，不是字面遵循。

## 5. 控制路径：独立反向通道

### 5.1 为什么不能直接写

`threads: HashMap<String, ThreadSession>` 是**主循环独占的 `&mut`**（上级 §6.1 硬约束）。
组长的工具在 **driver 侧**、装配期只拿到 `project_key`，天然碰不到那份内存。
这与 P2 的后台 tick 是同一个约束（`server.rs:2969` 的注释），因此解法也同款。

### 5.2 新通道 `LeadControlInbox`

照抄 `lead_inbox`（`lead.rs:94`）的先例，新建一条**方向相反**的通道：

```
组长工具 (lead_interrupt_thread / lead_reject_thread)
   └─ send(LeadControlCmd) ──► LeadControlInbox (broadcast, 容量 64)
                                     │
        主循环 select! 订阅 ─────────┘   ← 只有它持有 &mut threads
```

- `LeadControlCmd { project_key, kind: Interrupt | Reject, thread_id, reason }`——
  纯进程内结构（非 wire 类型，与 `LeadEnvelope` 同款，不涉 `serde`）。
- 容量 64 与 `lead_inbox` / `theme_tool` 同类通道一致。
- **不复用 `lead_inbox`**：方向相反（一个是 thread→lead，一个是 lead→thread），
  挤进同一通道会让两种语义后续长歪（这正是 danger 表里「通道语义混杂」的成因）。
- **不把目标 thread 的 `prompt_tx` / `interrupt_tx` 直接注入工具**：那要维护一份随
  thread 增删变化的注册表，且**绕开主循环对 `threads` 的独占**——与 §6.1 直接冲突。

### 5.3 主循环消费

在主循环 `select!` 里（与 `lead_rx` 分支并列，`server.rs:3012` 一带）新增一个分支：

1. 收到 `LeadControlCmd`；
2. 按 `thread_id` 在 `threads` 里查目标 session；
3. `Interrupt` → 取 `active_turn_id`，`interrupt_tx.try_send(turn_id)`；
   **无活跃轮则回错**（§3.2）；
4. `Reject` → 有活跃轮走目标 `interject_tx`，否则走目标 `prompt_tx` 起一轮（§3.1）。

**结果如何回给组长**：工具 `call()` 需要一个"发出去了 / 失败了"的有界回应。
复用 `LeadReplies` 同款形状（`lead.rs:38`）——`LeadControlCmd` 带一个 `request_id`，
工具登记 `oneshot`、主循环处理完回填。这样工具的返回值是**真实的执行结果**，
而不是"已投递"的空话（与 `ask_lead blocking` 的收口同理，但它不是 2 分钟往返，
而是本地一步操作，毫秒级）。

### 5.4 冷 thread（不在 `threads` 里）的处理

目标 thread 若**不在** `threads`（进程重启后未被打开过的冷 thread），主循环查不到它。

**本设计的选择**：工具返回明确错误「目标会话当前未打开，无法下达指令」，
**不做冷启动**。理由：给**另一个 thread** 冷启动一条 turn 需要复刻 `deliver_to_lead`
的整套冷启动恢复（`server.rs:5369`：载入转录 → `replace_messages` → 恢复用量），
那是为**组长**这一特例写的；把它泛化到任意目标属于新增能力，超出 P3-B。
记为**已知限制**（§9）。实践中同一进程内的活跃项目，thread 基本都温着。

## 6. 注册与边界

两个写工具与 `lead_remember` / `lead_recall` **同一处**注册、**同一条件**门控：
`build_runtime_tooling` 里 `if crate::lead_memory::is_lead_thread(&store, thread_id)` 的分支内
（`server.rs` 现 `:1069`）。

- 普通 thread 与子 worker 一律看不到（上级 §7.5）。
- 两个都**不**设 `read_only`（它们写），但 `requires_confirmation: false`——
  与 `lead_remember` 同理由：后台无人值守时若需批准，功能就废了（上级 §7.3 硬约束 2）。
  审计性由 §3.4 的两处转录保证。
- `project_key` 与 `ask_lead` 同源（装配期注入），保证"组长管哪个项目"口径一致。
- **工具只接受本项目 store 内的 `thread_id`**：主循环按 `thread_id` 在 `threads` 里查，
  天然只在当前进程的项目里找；不读项目外路径。

## 7. P3-C：移动端缺口

### 7.1 ① 组长行长按菜单（bug 修复）

**问题**：`useLongPress.resolveTarget`（`ThreadSidebar.tsx:290`）先看 `data-thread-row`，
组长行带着它，于是弹会话菜单。

**修法**：在 `resolveTarget` 里**先判 `data-lead-row`**，命中即返回 `null`（组长行不参与
长按菜单）。**不动 `data-thread-row`**——它还被选中逻辑与既有样式规则使用
（`ThreadSidebar.tsx:772` 的注释明说"带 `data-thread-row` 以便移动端长按与既有样式规则照旧生效"，
此处**有意改掉**该注释所称的行为，并同步更新注释）。

组长是项目级固定入口：重命名/置顶/删除对它都不适用，故**不提供任何组长专属长按菜单**
（连"重命名"也不给——组长标题由服务端 `find_lead` 语义决定，改名无意义）。

### 7.2 ② IM 式头像（纯前端）

上级 §9.1 要求组长是「IM 式独立头像」。

**范围**：只做**视觉区隔**，不碰 wire：

- 侧栏组长行左侧加一个固定头像位（圆形 + 与普通会话区分的配色/图标），
  与既有的 `data-lead-row` 标记配合。
- `ChatView` 顶部（组长会话打开时）显示组长头像与「组长」标签。
- **不新增头像图片资源**：用 CSS 画一个简洁的字符/图标头像（避免引入二进制资源与
  打包路径问题）。

### 7.3 明确不做：`created_by` 字段

§9.1 提到「wire 侧 `thread_summary_json` 新增 `role` 与 `created_by` 字段」。
`role` 已上线（`server.rs:3516`）；`created_by` **全仓不存在**。

它的真实归属是**决策 12「组长起真 thread（带 `created_by` 标记）」**——那是一条**独立能力**，
至今未实现（组长没有创建侧栏 thread 的路径）。`created_by` 今天**没有生产者**，
现在加它只会是一个恒为 `"user"` 的死字段。

**故本设计不做**：把 `created_by` 与"组长起 thread"一起留给未来某一轮（§10）。

## 8. 测试与验证

### P3-B

- 纯逻辑单测（不起 app-server）：
  - `LeadControlCmd` 的构造与 `project_key` 路由（同项目命中、异项目丢弃）。
  - 工具门控：非组长装配后注册表里**没有** `lead_interrupt_thread` / `lead_reject_thread`
    （沿用既有 `only_the_lead_sees_the_memory_tools` 形状）。
- 集成测试（`#[tokio::test]`，走既有 Harness）：
  - 组长调 `lead_reject_thread` → 目标 thread 转录里出现该文本（active 与 idle 两条路径）。
  - 组长调 `lead_interrupt_thread` → 目标活跃轮以 `interrupted` 收尾；
    无活跃轮时工具返回错误。
  - 冷 thread 目标 → 工具返回"未打开"错误（§5.4）。
- 不动 daemon 隔离不变量（上级 §10）：新通道纯进程内，不新增跨 root capability。

### P3-C

- 前端单测（既有 vite/vitest 体系）：
  - 组长行长按**不**弹会话菜单（`resolveTarget` 对 `data-lead-row` 返回 `null`）。
  - 普通行长按**仍**弹菜单（回归保护）。
  - 组长行渲染出头像位、`ChatView` 顶部显示组长标签。

验证命令：

```
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc -- \
  two_conversation_roots_do_not_share_children_or_reach_across \
  ending_one_conversation_reclaims_only_its_own_root
cd yi-agent-rs && cargo fmt --all -- --check
cd desktop && npm test
```

## 9. 已知限制（本期接受）

| 限制 | 说明 | 将来修的位置 |
|---|---|---|
| **无循环防护** | 打回→重跑→再 ask_lead→再打回 的乒乓循环无人值守烧钱（§4.1） | 加"每 thread 每次唤醒至多一次打回起轮"的硬闸 |
| **冷 thread 不可下达指令** | 目标不在 `threads` 时工具报"未打开"，不做冷启动（§5.4） | 泛化 `deliver_to_lead` 的冷启动恢复 |
| **撤销非结构化** | 只有追加式 supersede，无机器可查的"打回是否已撤销"（§3.3） | 需要该消费者时再补账本 |
| **留痕非强制落盘** | 依赖两处转录，无独立审计日志（§3.4） | 需要机器查询时在工具内强制落盘 |

## 10. 范围外

- **不做 `lead_set_objective`**：无字段可改（§3）。
- **不做 review 状态机相关动作**：状态机不存在（§2.1）。
- **不做组长创建 thread**（决策 12）与其 `created_by` 字段：独立能力，另开一轮（§7.3）。
- **不做实时状态投影**：属 P3-A 的已知限制，与本文无关。
- **不改 TUI**（上级 §9.3）。
- **不新增跨 root capability**、不改 daemon 会话隔离模型（上级 §10）。
- **不做头像图片资源**：纯 CSS 头像（§7.2）。

## 11. 危险点与对策

| 危险点 | 对策 |
|---|---|
| 组长无人值守地改别人会话 | 只有两个动作（取消/打回）；取消需目标有活跃轮；打回的后果是一条转录消息（可被用户看见与覆盖） |
| 打回打开烧钱循环 | 记为已知限制（§9），先观察；必要时加硬闸 |
| 工具绕开主循环独占 `threads` | 独立反向通道 + 主循环消费（§5），守住 §6.1 |
| 工具给错对象 | 与账本工具同门控（`is_lead_thread`）+ 门控测试（§6、§8） |
| 组长写不动、或以为写成功了 | 用 `oneshot` 回填真实执行结果（§5.3）；无活跃轮/冷 thread 都**报错**，不静默 no-op |
| 组长行在手机上弹无意义菜单 | P3-C ① 修（§7.1） |
| 新增 UI 资源导致打包问题 | 头像纯 CSS，不引入二进制资源（§7.2） |
