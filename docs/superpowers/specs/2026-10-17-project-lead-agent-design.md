# 项目组长 Agent（Project Lead）设计

日期：2026-10-17
状态：P1 已实现并合入（见下），P2/P3 待实现
模块：`yi-agent-app-server`、`yi-agent-subagent`、`desktop`、移动端远程
关联：`docs/project-management/yi-agent-app-server.md`、`docs/project-management/subagent-runtime.md`、
`docs/project-management/desktop.md`

## 1. 背景与诉求

现在的结构是「一个项目目录 → 若干 thread，每个 thread 一个独立 Agent」。
每个 thread 各自 attach 自己的 application root（attach key `thread:<id>`，
`yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs:15`），各自拥有独立的
直接子 agent 名额，互相之间**刻意不可达**。

新的诉求是再抽象出一层：

1. 与「代表某个项目的组长 Agent」对话。组长前后台分离：前台对话几乎实时响应，
   后台同时管理很多个 thread，每个 thread 并行工作。
2. thread 内通过一个函数与组长对话，且从 thread 入口仍能正常看到执行进度。
3. 用户照旧能像现在这样自己起 thread。

换句话说：一个项目 = 一个组长 = 它名下的全部 thread；
「文件夹代表一组 memory」这一抽象由「组长可检索本项目的全部 thread」落地。

## 2. 需求澄清（已确认的决策）

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | 用户侧入口 | **不变**。用户照旧自己起 thread；thread 内新增「向组长发言」函数。组长另有 IM 式独立头像与对话框 |
| 2 | 发起方 | **两者都要**：用户能起 thread，组长也能起 thread，两种在组长眼里等价 |
| 3 | 前后台 async | **组长有常驻后台循环**，前台对话是读同一份状态的投影，故前台秒回、不被后台 LLM 拖慢 |
| 4 | thread→组长 函数语义 | **两者都要**：默认单向投递，显式 `blocking: true` 才等回信 |
| 5 | memory | **结构抽象 + 一个受控的决策日志**。「文件夹 = 一组 memory」主要靠组长可检索既有数据（代码、`CLAUDE.md`、`.jsonl` 转录、SQLite task/trace）成立；此外在 `<project>/.yi-agent/lead_memory/memory.md` 放一份 lead 自己维护的 **`memory.md`**，职责严格收窄为「不可再生」的信息（见 §7.3） |
| 6 | 后台权限边界 | **可写，但留痕、可撤销**，且走既有 delivery/review 状态机（不绕过） |
| 7 | 前端范围 | **桌面 + 移动端远程**；TUI 本期只要求「能起 thread、thread 内能发言」 |
| 8 | 归属规则 | **自动全量归属**：thread 的 cwd 在项目目录下即自动归该项目组长，零认领、零额外状态 |
| 9 | 组长身份 | **组长是一个特殊 thread**（带 `role: lead` + 绑定项目目录），复用现有转录/持久化/driver/前端 |
| 10 | 前台实时性 | **前台抢占**：后台 turn 在工具边界让位，前台消息优先 |
| 11 | 组长视野 | **索引机械计算 + 内容按需拉取**：状态/时间/待裁决由 app-server 现算，内容由组长用工具按需读 |
| 12 | 组长起 thread | **起真 thread**（与用户起的同构，带 `created_by` 标记），不占 `MAX_DIRECT_CHILDREN` |
| 13 | 组长目标 | **由用户在 IM 里给**；后台只围绕该意图盯梢推进，不自创目标 |
| 14 | 唤醒条件 | **事件驱动 + 低频兜底 tick** |
| 15 | 项目纪律 | **靠 system prompt 注入**（`<workdir>/AGENTS.md`），不在 lead 派活时重复强调；该注入曾于 2026-08-15 被 merge 静默丢弃，已恢复（§7.7） |

## 3. 概念模型

- **Project Lead（组长）**：绑定项目目录的特殊 thread，`ThreadMeta.role = "lead"`。
  一个项目目录至多一个 lead。
- **归属（membership）**：thread 的 `cwd` 落在该项目目录内即自动归属；不新增归属存储。
- **Lead 目标**：最近一次你在 IM 里交代的意图。后台循环不发明目标。
- **Lead 起的 thread**：与用户起的 thread 完全同构，仅 `ThreadMeta.created_by` 区分
  发起人（`user` / `lead`）；不占用 `MAX_DIRECT_CHILDREN`（thread 不是 task）。

## 4. 架构决定：app-server 做频道中枢

daemon 的隔离不变量**一个字不动**：`two_conversation_roots_do_not_share_children_or_reach_across`
（`yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs:5335`）与
`ending_one_conversation_reclaims_only_its_own_root`（同文件 `:5444`）保持不变。

被否掉的方案：

- **给 thread 一张指向组长 root 的 capability**：会让两个会话 root 互相可达，
  与上述不变量正面冲突，等于给 daemon 开跨 root 后门。
- **让组长当项目 root、每个 thread 成为其直接子任务**：表面免费（mailbox /
  `send_message` / `inspect_agent` 直接可用），但 ① 直接子名额默认 4
  （`MAX_DIRECT_CHILDREN`，`YI_AGENT_DIRECT_CHILD_LIMIT` 可覆盖），组长只能管 4 个 thread；
  ② thread 不是 task——thread 有独立 driver 与交互式 turn 模型，task 是一次性
  worker + attempt + delivery 模型，合并会导致语义糊掉。

**选定方案**：thread 与 lead 的全部通信经 app-server。app-server 本就持有
thread → 项目 → 组长 的映射（root attach 由它负责），是最自然的频道。

## 5. 通信通道：thread → 组长

### 5.1 工具

每个归组 thread 注册新工具 `ask_lead`：

- `ask_lead({ question, blocking?: bool, timeout_ms?: u64 })`
- `blocking` 缺省 `false`：投递即返回，不阻塞 thread 的 turn。
- `blocking: true`：driver 停下等回信，带可配超时（默认值见 §5.3）。
- 超时/无回信时返回可读文本（如「组长暂未回应，请自行继续」），thread 不永久挂死。

### 5.2 阻塞路径复用审批形状

driver 遇 `ask_lead(blocking)` 时不直接回答，而是：发一个反向请求给 app-server，
把 `oneshot::Sender` 登记进路由表，然后 await；应答者（lead）回填后 driver 继续。

这与审批完全同形，依据 `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:6478`：
`pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>`（登记表在
`server.rs:2232` 建立，由 driver 参数传入）。
差别只在「应答者从用户点按钮换成 lead 回话」。**选同一形状是刻意的**——审批路径
已有超时、发起方断连收尾、跨客户端广播等成熟边界处理，可整套复刻。

### 5.3 排队的降级

lead 是**一个** thread、**一个** agent，turn 在同 thread 内串行
（driver 循环 `server.rs:6516`）。当 lead 正忙，阻塞的 `ask_lead` 必须在有界时间内
收口，而不是无限等待。默认超时 120 秒（与 TUI `wait_agent` 的默认 2 分钟一致，
`docs/project-management/subagent-runtime.md`「TUI wait_agent 有界等待」）。

### 5.4 回信落到 thread

- 单向投递：进入该 thread 的下个 turn 边界注入。
- 阻塞回信：直接作为 `ask_lead` 的 tool result 进入 thread 上下文。
- 两种情形都在该 thread 的转录里留下可见条目（用户从 thread 入口看得到进度）。

## 6. 前后台 async

### 6.1 后台循环的位置

**lead 的常驻后台循环住在 app-server 主循环（项目级）**，不寄生于 driver。

原因（两条硬事实）：

1. driver 在 `prompt_rx` 关闭时 `break` 退出（`server.rs:6516` 取到 `None`，
   `server.rs:6526` 处 `break`）；
2. 冷 thread 根本没有 driver——driver 只在 `thread/start`（`server.rs:3779`）与
   `thread/resume`（`server.rs:7557`）时 spawn。

项目级主循环天然看得到全部冷热 thread，因此是唯一正确的位置。对 warm thread
走既有 `ThreadSession.prompt_tx` 注入；对冷 thread 走 resume 路径（按需拉起）。

### 6.2 唤醒条件（事件驱动 + 低频兜底）

即时唤醒事件：

1. 有 thread 调 `ask_lead`；
2. 有 thread 到达终态 / 交付待审 / 卡住；
3. 用户在 lead 的 IM 里说话；
4. 一个低频兜底 tick（扫描上述事件是否被漏掉）。

形状与既有 daemon 的 `reconcile` + 分钟 tick 一致，因此判定逻辑（什么算「卡住」、
什么算「待裁决」）可以复用同一套机械判据，零 LLM 成本。

### 6.3 前台抢占

lead 的后台 turn 跑在「可让位」模式下：在每个工具调用边界检查是否有前台请求，
有则收口本轮并优先跑前台 turn，前台跑完后续后台。

复用两套既有原语：

- **mid-turn 注入**：`Agent` 的 inbox + `INTERJECTION_PREFIX`
  （`yi-agent-rs/crates/yi-agent-core/src/agent.rs:412`），
  以及 `InterjectionRequest`
  （`yi-agent-rs/crates/yi-agent-app-server/src/session.rs:49`）。
- **打断语义**：`Esc` 只打断本轮、不退出（`2026-08-10-esc-interrupt-only`），
  配合 turn checkpoint 落盘（`2026-10-03-turn-checkpoint-persistence`），
  被让位的后台进度不丢。

「前台是投影」由此成立：投影读同一份状态，秒回不需要等后台的 LLM 调用。

### 6.4 上下文预算与压缩

lead 是长寿命 thread，会反复被后台唤醒，因此它比普通 thread 更早撞上上下文上限。

**兜底已存在**：`maybe_auto_compact` 在每轮 THINK 之前检查
（`yi-agent-rs/crates/yi-agent-core/src/agent.rs:909`），阈值 = 有效窗口 ×
`compact_ratio / 100`（默认 80%，`yi-agent-rs/crates/yi-agent-runtime/src/config.rs:304`）。
压缩保留最多 20,000 token 的真实 user 输入 + 12,000 token 的完整工具后缀，其余交给
provider 生成 handoff 摘要（`yi-agent-rs/crates/yi-agent-core/src/compact.rs:261`）。
所以「撑爆窗口」这个字面问题不存在，lead 不会死。

**但 lead 撞的是压缩质量问题**：`plan_compaction` 的保留规则对 topic 无感知，而 lead
最需要的恰是「thread A 卡在哪 / thread B 的交付我还没看」这类中间区记忆，会被随机压掉；
且压缩有连锁放大——lead 越活跃、注入越多、压得越频繁、记忆切得越碎。
另有一个加速机制：`inject_pending` 被刻意放在 max_turns 检查**之后**
（`agent.rs:947`），所以每插一条消息就消耗一个 turn，注入越频繁，turn 数与上下文增长越快。

**对策（核心原则）**：区分「可再生的」与「不可再生的」。

- **可再生信息绝不依赖对话历史**：名下每个 thread 的状态是每轮**现算的投影**（§7.1）。
  它被压缩掉毫无损失，因为下一轮唤醒会重新算出来。这条成立后，压缩对 lead 的伤害
  结构性归零——压掉的全是可再生的。
- **不可再生的信息落盘**：决策与理由写进 `memory.md`（§7.3），不靠历史承载。

配套措施：

1. **注入预算上限**：后台唤醒只注入「有变化 / 需关注」的条目，不注入全量索引。
2. **变化优先**：未变化的 thread 只给一行状态，不展开。
3. **lead 单独配置压缩阈值**：长寿命 thread 不应与普通 thread 共用一套 ratio。
4. **压缩后无需重建态势**：态势本来就每轮现算，这是前三条的必然结果。

## 7. Lead 的视野（memory 的落法）

### 7.1 索引：app-server 机械计算

app-server 从既有数据（`ThreadMeta` + `ThreadStatus` + `.jsonl` 尾部 + delivery 状态）
现算一份轻量索引，每个 thread 一条：标题 / 状态 / 最后一条结论 / 是否有待裁决事项。

**零 LLM 成本、无幻觉，不新增存储。**

### 7.2 内容：lead 按需拉取

新增 lead 侧工具族：

- `lead_list_threads`：读索引（§7.1）。
- `lead_read_thread`：读指定 thread 的转录片段（增量/截断，受既有 trace 裁剪口径约束）。
- `lead_remember`：向 `memory.md` 追加一条带时间戳与来源的条目（§7.3）。
- `lead_recall`：检索 `memory.md` 的相关片段（按需，不灌全量）。

**绝不预灌全量上下文**——项目里已有 auto-compact 兜底
（`2026-07-26-auto-compact-design`），但那是兜底不是策略；20 个 thread 的完整转录
会撑爆任何窗口（见 §6.4）。

这就是决策 5「结构抽象」的落地：可检索，而非一份要维护的大文档。

### 7.3 lead_memory：`memory.md`（决策 5 的补充）

路径：`<project>/.yi-agent/lead_memory/memory.md`。

**它只装「不可再生」的信息**——这是它与 §7.1/§7.2 的分工边界：

| 内容 | 归属 | 理由 |
|------|------|------|
| 决策与理由（「为什么打回 thread X」） | `memory.md` | 现算不出来；结论在 delivery 记录里，但**理由**只在当时的对话里，且会被压缩吃掉 |
| 你交代过的长期偏好 / 跨 thread 约定 | `memory.md` | 同上 |
| thread 状态 / 进度 / 交付结论 | §7.1 索引（现算） | 可再生的写进 memory 就是重复，且会过期 |

**不装可再生的东西**，因此 `memory.md` 与既有数据没有重叠，不构成第二个真相源。
这是它区别于「一份要维护的项目文档」的关键。

三条硬约束：

1. **只由 lead 写**，且必须经工具 `lead_remember`（禁止 lead 直接用 `write`/`edit`
   改它）。单一写者，因此无并发写冲突；且 write / edit 工具的既有约束
   （必须落在 workspace root 内，`yi-agent-rs/crates/yi-agent-tools/src/fs/path_util.rs:11`）
   依然生效。
2. **不确认、但可审计**：写入无需用户批准（否则后台无人值守时写不了，缺口原样回来），
   但每条条目自带时间戳与来源（关联 thread id / turn id），且这次工具调用会进 lead
   自己的 `.jsonl` 转录，因此任何时候都能回答「这条是谁、什么时候、因为什么写的」。
3. **进上下文必须按需**：绝不把整份 `memory.md` 灌进历史——那会立刻吃掉上下文预算，
   正好抵消它存在的意义（见 §6.4）。检索走 `lead_recall`（§7.2 的工具族）。

**沙箱与 git**：`.yi-agent/` 在 workspace root 之下，`WorkspaceWrite` 模式下天然可写
（writable roots = workspace root + `extra_writable_roots`，
`yi-agent-rs/crates/yi-agent-tools/src/sandbox.rs:87`）；且项目已用
`.git/info/exclude` 把 `.yi-agent/` 标为忽略，因此 `memory.md` 不会弄脏 checkout、
不破坏 worktree 创建（`yi-agent-rs/crates/yi-agent-tools/src/worktree.rs:94`）。

**工作目录一致性**：lead 的 workdir 是**项目根**（不是子 agent 的
`.worktrees/<branch>/`），因此 lead 读写 `memory.md` 的路径始终一致；子 worker 看不到它，
这是刻意的（§7.5）。

### 7.4 明确不做

- 不做「lead 记得的一切」这种泛化 memory——那会退化成第二个真相源。
- 不做 `memory.md` 之外的结构化记忆表（需要时再说）。
- 不让子 worker 读写 lead_memory。

### 7.5 边界：谁能读写

| 对象 | 读 | 写 |
|------|----|----|
| lead | `memory.md`、全部 thread 索引与转录（§7.1/§7.2） | `memory.md`（仅经 `lead_remember`） |
| 普通 thread | 不读 lead_memory | 不写 |
| 子 worker（`spawn_agent`） | 不读 | 不写 |

子 worker 的 workspace 是 `.worktrees/<branch>/`，`.yi-agent/` 不在其内，因此天然看不到
lead_memory——这是刻意的：`memory.md` 是 lead 的私有账本，不是协作媒介。thread 要影响
lead，走 `ask_lead`（§5）。

### 7.6 已知限制：`memory.md` 会无限增长

本期**接受**，不做裁剪。

`memory.md` 是无界追加的：没有任何机制删除或归纳它。长期看会有两个后果：

1. **检索退化**：`lead_recall` 要扫全文，文件越大越慢、越容易召回噪声。
2. **上下文预算压力**：若检索一次性返回太多，反而挤占 lead 的上下文（与 §6.4 的原则冲突）。

**为什么不本期修**：裁剪策略（多久归纳一次、按什么丢、丢掉的如何留索）需要真实数据
才能定准，现在定就是猜。而且这个限制是**渐进劣化**、不是悬崖——文件先小后大，
你有充足时间发现并上报。

**将来修的位置**（不改架构）：在 `lead_remember` 里加「超限时触发 lead 自行归纳压缩」
——归纳仍是一份 `memory.md`，不引入新载体；也可配合 §7.1 的索引思路，给条目加可检索
的头部。触发时应当留痕（§8），因为归纳会丢信息。

### 7.7 前提：项目纪律靠注入，不靠 lead 每次强调

lead 起的 thread 必须遵守项目纪律（本项目即「开始任何动作前先加载 `using-superpowers`」）。
**这个前提曾不成立**，现已修复：

- `be027f46 feat: load project AGENTS instructions` 曾完整实现 `<workdir>/AGENTS.md`
  注入（含 3 个测试），经 `f5dc5ce4` 合入主线。
- 在 `34c63d2c merge: integrate subagent runtime` 中，提示词助手取了从未携带该功能的
  一侧，实现被**静默丢弃**；此后 `b4a7018e` 搬迁装配时 `resolve_system_prompt` 已只剩
  `(user)` 参数，功能无从搬起。项目文档只记了现象（「全仓未找到实现」），未记根因。
- **已恢复**：只读 `<workdir>/AGENTS.md`（不向父目录搜索、缺失即 `None`、其它 IO 错误记
  警告后按缺失处理），注入顺序固定为 默认 prompt → user prompt → 项目指令 → 日期；
  `build_prompt_setup` 与 `build_catalog_handle` 两个入口都传 `workdir`，因此
  **catalog 非空的常规路径也带上项目指令**。代码：
  `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`；验证：
  `cargo test -p yi-agent-runtime --lib resolve_system_prompt_`、
  `cargo test -p yi-agent-runtime --lib catalog_handle_carries_project_instructions`。

**为什么这决定了「不需要额外强调」**：纪律现在是 system prompt 的一部分，对
用户起的 thread、lead 起的 thread、lead 自己、TUI、桌面**一律生效**。因此
**不在 lead 派活时重复写强调语**——那会依赖 LLM 每次记得写，而机制保证远比
提示词自律可靠。若将来发现某个前端路径不经过 `build_prompt_setup`，正确做法是
修那条路径，而不是在 lead 的 objective 里打补丁。


## 8. 权限边界与审计

后台循环可写：取消 / 打回 / 改 objective。但每个动作必须：

1. **留痕**：写进该 thread 的转录 + lead 的 IM 对话（谁、何时、为什么）；
2. **可撤销**：有对应的撤销路径；
3. **不绕过状态机**：走既有 delivery/review 路径
   （`review_agent` 强制 feedback/reason；`accept` 只由 Git ancestry 观察完成）。

这与现有审核底线一致：打破它等于让无人值守的写动作变成黑箱。

## 9. 前端

### 9.1 桌面

- lead 在侧栏是一个独立条目（IM 式头像），点开是一个与普通 thread 同构的对话框
  （复用 `desktop/src/components/ChatView.tsx`、`ThreadSidebar.tsx`）。
- lead 名下 thread 的分组：`thread/listAll` 已有「按目录分组 + `driver_project`
  跨目录折拢」的先例（`server.rs:3388`；字段原名 `board_project`，G1 已通用化为
  `driver_project`，旧 meta 靠 serde alias 读入），lead 归属沿用同一形状。
- wire 侧 `thread_summary_json`（`server.rs:5943`）新增 `role` 与 `created_by` 字段。

### 9.2 移动端远程

lead 状态本就是 `Delivery::List` 层（`protocol.rs:434`），恒推不违背内容按需订阅
（`protocol.rs:424`），因此移动端天然能看到 lead 与会话状态。

### 9.3 TUI

本期不改 TUI 的 UI。要求仅是：能起 thread、thread 内能发言——`ask_lead` 是后端
工具，thread 在 TUI 里自动具备。

## 10. 危险点与对策

| 危险点 | 对策 |
|--------|------|
| 后台 turn 与前台 turn 争同一个 agent（turn 在同 thread 内串行） | 前台抢占 + 让位（§6.3），复用 interjection 与打断原语 |
| 阻塞式 `ask_lead` 永久挂住 thread | 有界超时（§5.3），超时返回可读文本 |
| lead 后台循环随时起 turn，烧钱且不可预期 | 事件驱动 + 低频兜底（§6.2）；且目标只来自用户（决策 13） |
| 摘要规则写死导致 lead 瞎 | 索引只放机械可判的事实，内容一律按需拉取（§7） |
| 无人值守的写动作变成黑箱 | 留痕 + 可撤销 + 不绕过状态机（§8） |
| 与 daemon 隔离不变量冲突 | 通信全部经 app-server，不新增跨 root capability（§4） |
| lead 长寿命导致上下文被压碎 | 可再生信息每轮现算、不进历史；不可再生的落 `memory.md`（§6.4、§7.3） |
| `memory.md` 变成第二个真相源 | 职责收窄为「不可再生」；只经 `lead_remember` 写；可再生的一律不进（§7.3） |
| `memory.md` 无限增长 | **本期接受为已知限制**（§7.6）：渐进劣化、非悬崖；将来在 `lead_remember` 加归纳压缩 |
| lead 起的 thread 不遵守项目纪律 | 已从根上修：项目指令注入 system prompt，全体生效（§7.7）；**不靠** lead 重复强调 |

## 11. 实现分期

spec 一次写全，实现拆三期，每期一个可验收的垂直切片。

### P1 最小闭环

lead thread 存在 + 自动归属 + 单向 `ask_lead` + 桌面能点开 lead 并对话。

验收：能自动出现一个 lead thread；普通 thread 能单向投递消息给 lead；lead 能在
IM 里看到并回应；桌面侧栏能看到 lead 条目。

**状态：已实现**（`docs/superpowers/plans/2026-10-17-project-lead-agent-p1.md`，
4 任务）。已知限制（本期接受）：`listAll` 懒创建只在空目录种组长（可达性由桌面 Step 9a
补建闭合）；warm 投递路径不占 `active_turn_id`（消息在容量 8 的 channel 排队串行执行，
不丢不坏）；冷启动投递未对齐 `thread/resume` 的 `promote_partial`（仅崩溃恢复角落可达）。

### P2 前后台 async + lead_memory

后台循环 + 事件唤醒 + 前台抢占 + 阻塞式 `ask_lead` + `lead_memory/memory.md` 与
`lead_remember` / `lead_recall`。

**注意**：前后台是全新机制——今天所有 turn 都由用户输入触发。复杂度集中于此。
`memory.md` 放本期（而非 P3）的理由：P2 的后台循环正是产生「决策与理由」的地方，
缺了它，P2 一上线就会开始丢过程性记忆（§6.4）。

### P3 lead 视野与安全

按需拉取工具（`lead_list_threads` / `lead_read_thread` 的完整形态）+ 权限留痕/撤销
+ 移动端远程。

## 12. 范围外

- 不为 lead 另建一套转录/持久化（复用 thread 那套）。
- 不新增跨 root capability，不改 daemon 的会话隔离模型。
- 不做「lead 记得的一切」式泛化 memory；`memory.md` 只装不可再生的信息（§7.3）。
- 不把 lead_memory 暴露给普通 thread 或子 worker（§7.5）。
- 不做多组长并存（一个项目一个 lead）。
- 不改 TUI 的 UI。
- 不改现有审批、delivery、review 状态机的语义。