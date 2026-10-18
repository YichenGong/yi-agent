# 设计：长会话性能退化（每帧开销随会话长度增长）

日期：2026-10-18
状态：待评审
分支：`perf/long-session-scaling`
worktree：`.worktrees/long-session-perf`

## 1. 背景与问题

用户报告：**thread 内容越长，越慢**。最初的猜测是「出站消息都走 RPC，可能是带宽瓶颈」。

该猜测**已被证据否掉**：出站帧确实是 RPC 帧（所有出站帧经 `Broadcaster` 扇出，
`yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs:3`），但帧载荷是**增量**——
`item/delta` 只带 `thread_id` + `item_id` + `delta` 文本
（`yi-agent-app-server/src/protocol.rs:338`），**大小与会话长度无关**。整段转录只在
`thread/resume` 回放时传一次，且分块限在 256 KiB（`server.rs:3214`）。

真正的根因是**多处「每帧做与整个会话成正比的工作」**，分布在两个前端渲染路径与
一条持久化路径上。它们每一个单独看都"只是一次线性扫描"，但叠加在**每个流式 delta**
上，就变成了用户可感的退化。

### 1.1 量化证据（本次实测）

以下数字用仓库现有生产代码实测得出，脚本见 §7。**绝对数值不可等同于真机**
（前端数字跑在 node + jsdom 下，不是 WebKit），但**增长趋势是结构性的**，
与实现一一对应。

**(a) 前端数据层：每个 delta 一次 O(N) 扫描**

`desktop/src/lib/session.ts:475`：

```ts
const index = this.items.findIndex((i) => i.id === item_id);
```

流式目标恒为数组末尾项，`findIndex` 因此每次都扫完整表。实测
（`item/delta` 打到末尾项）：

```
items=  200  per-delta=1.08 us
items= 1000  per-delta=4.84 us
items= 4000  per-delta=9.16 us
items= 8000  per-delta=21.18 us
items=16000  per-delta=35.27 us
```

同一个 `findIndex` 模式还出现在 `:319`、`:344`、`:430` 与
`upsertItems`（`:333`）内。

**(a′) 但"目标在末尾"这个前提本身也不牢**：推理块会 `push` 到数组末尾
（`session.ts:500`），所以 CoT 在正文之后继续到达时，正在流式的 agent 消息
会被挤到 `length-2`。实测一个真实交错序列（6 段推理 / 120 个 delta），
41 次 delta 中有 **20 次目标不在末尾**（偏移恒为 -1）。这条在 §3.1 决定了
修复的形态——见那里的对照实测。

**(b) 前端渲染层：每帧重建全部子元素（主因）**

`App.tsx:1248` 每条通知都 `force((v) => v + 1)`，`ChatView.tsx:241` 随之
`items.map(...)` 重建 **N 个 React 元素**并 diff。实测每 delta 的重渲染耗时：

```
items=  100  per-delta-render=1.26 ms
items=  500  per-delta-render=2.41 ms
items= 1000  per-delta-render=4.41 ms
items= 2000  per-delta-render=9.25 ms
items= 4000  per-delta-render=19.18 ms
```

**(c) 前端渲染层：流式 markdown 每个 delta 重解析整段**

`MarkdownText`（`MarkdownText.tsx:66`）按 `text` memo，但正在增长的那条消息
每个 delta 都变，于是每次都重解析**整段已累积文本**。单次渲染随文本长度线性：

```
chars=  2115  render= 2.0 ms
chars=  8366  render= 5.5 ms
chars= 20915  render=13.1 ms
chars= 41783  render=27.5 ms
```

整轮累计即**平方级**（实测一条 40k 字符回复分 1334 个 delta 流式抵达：

```
final=  5000 chars  wall=202 ms
final= 10000 chars  wall=540 ms
final= 20000 chars  wall=1883 ms
final= 40000 chars  wall=8035 ms
```

**(d) 服务端：checkpoint 整份重写——实测后判定**不是**瓶颈**

driver 每收到一个 `ItemCompleted` 置 `checkpoint_dirty`（`server.rs:4451`），
500ms tick 触发 `checkpoint`（`server.rs:4334`、`:4557`），经
`write_partial` → `write_atomic`（`thread_store.rs:659`、`:833`）**原子重写整份
快照**，其中 `messages`（`build_partial`，`server.rs:3679`）随会话线性增长。

第一版设计把它当成热点并提出降频。**实测推翻了这一点**：真实 checkpoint 最大
1224 KiB，单次写入（序列化 + `write_atomic`）约 **9.3 ms**，每 500ms 最多一次
（不足 driver 墙钟的 2%），且它在**该 thread 自己的 driver task** 里、
不在共享主循环上。它正是"崩溃后能看到已产出内容"的保证——用户明确要求
「崩溃后还是每次都能看到，不要丢东西」。故 **§3.4 已撤回优化，改为回归保护**；
这里保留证据以免读者以为漏查。

**(e) TUI：流式期间每帧全表指纹扫描 + tail 重解析**

`HistoryState` 已有 cell 级 memo（`history.rs:160` `CachedCell`、`:171`
`HistoryCache`），注释明写它就是为了消除「每帧按整个 transcript 计费」
（`history.rs:167-170`）。但流式期间 `ensure_cache`（`history.rs:313`）仍要
遍历**全部** cell 做指纹比对（`:337`），且指纹是 O(1)（只散列文本长度，
`history.rs:51`），**变化检测便宜、但遍历本身仍是 O(N) 每帧**；同时末尾那个
正在增长的 cell 会被判为"变了"，其 markdown 每帧重解析（`cell.rs:79` →
`markdown.rs:17`）。此外**没有 cell 上限/舍弃策略**，对应
`docs/bug-list.md:57` 那条未修项（「如果一个对话前后过长，terminal 会变得比较
卡顿。TUI 里面显示的上下文可以有舍弃」）。

### 1.2 为什么这值得一份 spec

三项症状同源（"每帧做与全会话成正比的工作"），落在**三个不同模块**，
其中两处（前端数据层 / 渲染层）**互相依赖**：只修数据层，渲染层仍每帧 O(N)；
只修渲染层，`item/delta` 的**就地修改**会让 memo 永远命中、界面直接冻住（见 §3.1）。
所以它们必须一起设计、一起验收。

第四项（服务端 checkpoint）**经实测判定不是瓶颈**，已从优化目标降级为回归保护
（§3.4）——它对应的是用户的另一条要求「崩溃后还是每次都能看到，不要丢东西」。

## 2. 目标与非目标

### 目标

- **G1 前端数据层**：`item/delta` / `item/completed` 的定位从 O(N) 降为**摊还 O(1)**，
  不改变任何可见行为与转录顺序。
- **G2 前端渲染层**：流式期间每帧重渲染的元素数**与历史条数无关**（只重渲染活跃尾部）。
- **G3 前端 markdown**：流式期间重解析次数**脱离 delta 频率**（按时间节流），
  整轮成本从 O(N²) 降为 O(N²/常数)。
- **G4 服务端 checkpoint：不许退化（非优化目标）**。实测 checkpoint**本来就不是瓶颈**
  （见 §3.4），且它正是"崩溃后每次都能看到已产出内容"的保证。故本项**不做降频、
  不做减载**，只加**回归保护**：用测试钉死"每个已完成 item 都会被写入 checkpoint"
  与"崩溃恢复能看到全部已完成内容"这两条现有行为，防止后续优化（包括本 spec 的部分）
  把它弄丢。
- **G5 TUI**：流式期间每帧的 cell 遍历不随历史长度增长；新增**历史舍弃**上限，
  闭合 `docs/bug-list.md:57`。
- **G6 零语义回归**：协议不变、线路上字节不变、转录内容与顺序不变、
  持久化格式不变。（帧的**数量/时序**也不变——见非目标。）
- **G7 崩溃恢复不丢东西（用户硬要求）**：任何改动都**不得**减少崩溃后可恢复的内容。
  现状是"每个已完成的 item 都落 checkpoint、turn 开始即落提问"，这一行为必须
  **原样保留**并被测试钉死。本 spec 明确**不**为性能牺牲恢复完整度（见 §3.4）。

### 非目标

- **不做远程 delta 合并**（见 §8「本 spec 明确未做」）：它改的是帧边界与时序，
  与「越用越慢」不是同一个问题。
- **不做前端列表虚拟化**（只渲染可视窗口）。§3.2 的"冻结前缀"已把每帧成本降为
  O(1) 个元素；虚拟化要额外处理变高行、`useStickToBottom`
  （`ChatView.tsx:232`）与 17 个既有 `ChatView` 断言，风险显著更大。
  **触发条件**：若 §3.2 落地后单帧仍 >4ms（真机实测），再单独开一份 spec。
- 不改 `thread_store` 的持久化格式，不改 `PartialTurn` 的恢复语义
  （`clear_partial` / `promote_partial` 的原子文件语义保持原样）。
- 不做 TUI 之外的终端形态（如 Web UI）适配。

## 2.5 收益（回答"这些改动换来什么"）

§1.1 的曲线说明的是**增长**，不是**用户等的那一个回合**。所以这里补一次端到端
实验（脚本 `desktop/scripts/perf/turn.mjs`）：同一个回合（4000 条历史 + 一条
约 10k 字符、334 个 delta 的回复）在三种形态下的**主轴（main thread）占用**。

```
current   turn=  7515 ms   per-delta=22.50 ms
G2        turn=   510 ms   per-delta= 1.53 ms
G2+G3     turn=    53 ms   per-delta= 0.16 ms
```

**关键不是倍数，而是"是否随会话变长"**——同一回合、不同历史深度：

```
variant   0 items  500 items  1000 items  2000 items  4000 items
current       499       1260         2049         3786        7377
G2            503        467          502          463         515
```

这张表就是用户症状的复现：**`current` 随历史线性爬升，G2 持平**。

由此得到每一项的收益与理由：

| 项 | 收益 | 是否值得做 |
|---|---|---|
| **G2 冻结前缀** | 一次回合从 7.5s → 0.5s 主轴占用，且**不再随会话变长**（14x） | **值得，且是主项**——它直接消掉用户报的"越用越慢" |
| **G3 markdown 节流** | 在 G2 基础上 0.51s → 0.05s（再 10x） | **值得**——G2 让每 delta 只剩重解析这一项，而它仍是随文本增长的 |
| **G1 查找去 O(N)** | 数据层：16k 项时每 delta 35us → 常数（§1.1(a)） | **值得但优先级低**——相比前两项是数量级更小的成本；它是 G2 的前置基础设施，成本极低故顺带做 |
| **G5 TUI** | 与 G2/G3 同理（每帧全表扫描 + tail 重解析） | **值得**——闭合 `docs/bug-list.md:57`，且 TUI 是独立路径、无前端那套风险 |
| ~~G4 checkpoint~~ | 实测本来就不是瓶颈（§3.4） | **不做**——收益为零，且与"不丢东西"冲突 |

**诚实边界**：上表数字来自 node + jsdom，**不是 WebKit**；`G2+G3` 还假定
6ms/delta 的到达节奏（取自 §1.1(c) 的实测 8025ms/1334 delta）与 60ms 节流窗口。
可信的是**量级与"是否增长"**，不是精确毫秒。真机复测属 §6 的参考基线。

**结论**：值得做的核心理由不是"快多少倍"，而是**把成本从"随会话长度增长"
变成"恒定"**——这正是用户报告的现象。G1/G5 是同一原理的另外两处站点，
成本低、风险独立，故一并做；G4 已撤回。

## 3. 设计

### 3.1 前端数据层：末尾快路径 + 位置索引（G1）

**为什么不只是"加个 Map 索引"**：`session.ts` 有两条语义约束必须保住——

1. `item/delta` 与 `item/completed` 是**就地修改**（mutate in place），
   `Session.items` 是**稳定引用**，`App.tsx` 依赖这一点（注释见
   `ChatView.tsx:107` 与 `ChatView.tsx:224`）。
2. `insertAfterLastServerItem`（`session.ts:262`）会 `splice` 插入，**插入点之前的
   下标全部不变、之后的全部平移**。裸索引会因此失效。

**做法（三层，按代价递增）**：

**第一层：末尾快路径 + 向后的有界回扫**（覆盖绝大多数流式 delta）。

**⚠️ 这里有一个实测推翻过的直觉**：先写"末尾命中就 O(1)、否则退回
`Array.prototype.findIndex`（向前扫）"是**不够的**。因为推理块（`reasoning`）
会 `push` 到数组**末尾**（`session.ts:500`），一旦 CoT 在正文之后继续到达，
它就把正在流式的 agent 消息**挤到 `length-2`**。实测一个含 6 段推理、120 个
delta 的真实回合，在用真实 `session.ts` 代码计数的对照下：

```
策略                               history=1000   history=4000
末尾快路径 + 向前 findIndex 兜底      100420         400420     ← 等于没优化
末尾快路径 + 向后有界回扫                420            420     ← 常数
末尾快路径 + 自校验缓存下标                120            120     ← 最优
```

即**向前兜底在真实流式序列里退化为 O(N)**，因为目标恒在数组后部、却总要先扫
完整个历史。正确做法是**从末尾向前**找：

```ts
// Resolve the index of `id`, biased to the tail. During a turn the mutating
// target is always near the end — the trailing agent message, or that message
// displaced by one when a reasoning block resumes after it (session.ts:486) —
// so scanning backwards from the tail hits in a bounded number of steps and
// never touches the history ahead of it.
private indexOfItem(id: string): number {
  for (let i = this.items.length - 1; i >= 0; i -= 1) {
    if (this.items[i].id === id) return i;
  }
  return -1;
}
```

**注意**：这条向后扫描**在最坏情况下仍是 O(N)**（目标真的在历史深处时）。
它之所以够用，是因为流式期间变动目标恒在尾部附近（§1.1(a) 的实测曲线就是
这个前提下的结果）。但它**不是**渐进最优——若要严格 O(1)，需要第二层。

**第二层：自校验的缓存下标**（推荐与第一层一起落地，因为代价极小）。
一个 `{ id, index }` 缓存：命中校验（`items[index]?.id === id` 才认）后直接返回，
失效则退回第一层的向后扫描：

```ts
private cachedItemId: string | null = null;
private cachedItemIndex = -1;

private indexOfItem(id: string): number {
  // Self-validating: `splice` shifts the tail, so the cached index is only
  // trusted when the slot still holds this very id.
  if (this.cachedItemId === id && this.items[this.cachedItemIndex]?.id === id) {
    return this.cachedItemIndex;
  }
  for (let i = this.items.length - 1; i >= 0; i -= 1) {
    if (this.items[i].id === id) {
      this.cachedItemId = id;
      this.cachedItemIndex = i;
      return i;
    }
  }
  return -1;
}
```

实测这把常数从 420 降到 120（120 个 delta 各一次比较），且**校验本身就是
O(1)**、失效自动回退，不需要在 `splice` 处维护任何下标（避免"忘记修正"这类 bug）。

**第三层（不做）**：`Map<string, number>` 全量位置索引 + 在 `splice` 处维护。
只有在前两层落地后实测仍不够时才考虑；本 spec 不预先做。

**适用范围**：`session.ts:475`（`item/delta`）是主战场。同一手法可用于
`:430`（`item/completed`）与 `:344`（`upsertItems`）——但**注意
`upsertItems` 是批量场景**（每次处理一批，目标各不相同），缓存下标收益有限，
故只在 `:475` 与 `:430` 上启用，`:344` 保持现状（它的 `findIndex` 在批量补齐
时才会走到，不在流式热路径上）。

**行为不变量**：`items` 数组顺序、`lastServerItemId` 的推进、局部回声
（`local-` id）的插入与去重语义**一律不变**。`session.test.ts`（64 例）是主回归网。

### 3.2 前端渲染层：冻结前缀 + 活跃尾部（G2）

**核心问题**：`App.tsx:1248` 的 `force()` 让 `ChatView` 每帧重建全部子元素。
React 的 `memo` 挡不住它，因为：

- `ChatItem`（`ChatView.tsx:116`）**不是** memo 边界；
- 即便把它包成 `memo`，默认是**浅比较 props**，而流式项是**就地修改**
  （同一对象引用、`text` 变了）——比较永远为"相等"，界面会**冻住**。

**实测三种写法的差距**（同为 4000 项历史、200 个 delta，§7 脚本可复现）：

```
基线（ChatItem 无 memo）      per-delta = 20.37 ms
只加 memo(ChatItem)（比对 text） per-delta =  4.92 ms
冻结前缀 + 1 条活跃尾部          per-delta =  1.11 ms
```

"只加 memo"只降到 4.9ms，因为 `items.map` **仍每帧重建 4000 个元素对象**
（memo 只是让 React 不去 render 它们，元素创建与 diff 的开销还在）。
**结论：必须做前缀冻结**，memo 是必要但不充分条件。

**做法**：

1. **`ChatItem` 提为 memo 边界**，比较器**显式比对会变的 primitive**
   （`item` 引用 + `item.text`；对 `toolCall` 类比对 `state`/`expanded`），
   而不是对象身份——这正是叶子组件 `MarkdownText` 现在的手法
   （`MarkdownText.tsx:66`），把同一手法上提一层。
2. **前缀冻结**：把"历史项"与"活跃尾部"拆成两个子树。历史前缀由一个
   **props 在流式期间稳定**的 memo 组件承载（项数组的**长度**不变、
   元素引用不变 ⇒ 整棵子树跳过重建），活跃尾部（通常是最后 1 条）单独渲染。
   冻结边界 = `items.length - 1` 且"尾部项正在流式"；非流式时整表照旧渲染
   （此时本来就不需要优化）。
3. **冻结边界的正确性约束**：凡会**改动历史项**的路径都必须让边界失效，
   否则历史会显示陈旧内容。已清点出三类：
   - `item/completed` / `items/completed` 就地替换历史项（`session.ts:430`、`:333`）；
   - 推理块**就地接管**（`session.ts:408-424` 把本地 `reasoning` 块换成服务端
     `thinking` 项，**位置不变但对象变**）；
   - `dropLocalUserMessage` / `dropDanglingLocalEchoes` 会 `splice` 删除项
     （`session.ts:319`、`:262`）。
   设计上**不做"猜哪些不变"的增量判断**，而是给 `Session` 加一个
   **`structuralVersion`**（只在上述三类结构性变更时自增；
   纯文本 append **不**自增）。`ChatView` 把 `structuralVersion` 作为
   前缀子树的 prop ⇒ 结构一变整棵前缀重建一次，流式 append 则完全跳过。
   这比"按项 fingerprint 猜"更简单、更难写错。

**行为不变量**：DOM 结构（每个 `ChatItem` 的层级、`data-testid`）、
滚动贴底（`useStickToBottom` 的 `items.length` / `lastText` 依赖数组
`ChatView.tsx:234` 保持有效）、推理块与正文的先后顺序**一律不变**。
`ChatView.test.tsx`（17 例）是主回归网。

### 3.3 前端 markdown：流式节流（G3）

**问题**：`MarkdownText` 每 delta 重解析整段，整轮 O(N²)（§1.1(c)）。

**做法**：对**正在流式的那一条**做**尾沿节流**（trailing-edge throttle，
建议 60ms，约为 1 帧）：窗口内的多次变更只重解析一次，窗口结束时补最后一次
（尾沿必须补，否则最后一段文字会停留不更新）。

- 实现落在 `MarkdownText` 的消费侧（一个 `useThrottledText(text, ms)` 钩子）
  或 `ChatView` 的活跃尾部分支，**两者择一，不要都做**。
- **节流只作用于流式中的那条消息**；历史消息（`text` 不再变）本就命中 memo，
  不受影响。
- 节流窗口结束时**必须**渲染最终文本，且流式结束后**必须**取消防抖、
  立即渲染终值，否则 `item/completed` 的权威文本可能被窗口吞掉。

**收益**：把"每个 token 一次重解析"变成"每 60ms 一次"，整轮从 O(N²) 变为
O(N²/常数)（实测 40k 字符整轮 8035ms 量级，按 60ms 窗口估算可降一个数量级）。
**注意**：这不是把平方变线性——若要做到线性需引入增量 markdown 解析，
YAGNI，本 spec 不做；判断依据写在 §6。

### 3.4 服务端：checkpoint 不做优化，改为回归保护（G4）

**本节是对第一版设计的撤回。** 第一版提出"把随会话增长的 `messages` 降频"，
以此降低写入成本。用户的否决是：「**我希望崩溃后还是每次都能看到。不要丢东西**」。
实测后确认该否决完全成立，且原方案在技术上也站不住。

**实测（用仓库里的真实 checkpoint 文件，非构造数据；脚本
`desktop/scripts/perf/checkpoint.mjs`）**：

```
6 个真实 partial：231 KiB ~ 1224 KiB
  字节占比差异很大：有的 items 74~77%、messages 21~22%；
                    有的 items 仅 9~10%、messages 74~83%
最大那个（1224 KiB）：序列化 3.31 ms + write_atomic(tmp+fsync+rename) 5.96 ms
                     ≈ 9.3 ms / 次，每 500 ms 最多一次 ≈ driver 墙钟的 1.9%
```

三条结论：

1. **checkpoint 不是瓶颈。** 最坏约 9.3ms / 每 500ms 一次（不足 driver 墙钟 2%），
   且它在**该 thread 自己的 driver task** 里（`store: Arc<ThreadStore>`，
   `server.rs:4170`），不在共享主循环上。与其他四项（每帧 O(N)、整轮 O(N²)）不同量级。
2. **"降频 messages 省不到多少"这个说法不成立——但结论反而是"更不能动它"。**
   占比因会话而异：有的快照 messages 占 74~83%，在那种会话里降频 messages
   *确实*能省下大头。**可见性上省得少、有的会话省得多，正是它危险的地方**：
   收益不确定，而代价确定——**崩溃后模型少记得一段上下文**。
   用户明确要求「崩溃后还是每次都能看到，不要丢东西」，故不做。
3. **现有行为正是用户要的。** 真实 partial 里已有上百条 `agentMessage` +
   `toolCall`，说明流式产出确实逐条落盘、崩溃后确实能恢复。
   用户的要求不是"改"，是"**别弄坏**"。

**因此本节的做法是：不改行为，只加保护。** 具体：

- **不动** `checkpoint_tick` 的 500ms 间隔与 `checkpoint_dirty` 门控——它是
  "长 turn 里每个 item 都同步写一次"与"崩溃丢太多"之间的现有折中，实际成本最坏
  约 9.3ms/500ms（不足 driver 墙钟 2%），不构成瓶颈。
- **不动** `messages` 的写入频率。`agent.session().messages().to_vec()`
  （`server.rs:4283`、`:4565`）是对 `agent.rs:629`
  `self.session.lock().clone()` 的全量克隆——它**看起来**浪费，且在某些会话里
  确实占大头（实测最高 93% 字节），但**省它就要牺牲恢复完整度**，
  而用户的要求是"不要丢东西"。若将来要动，应作为**独立的**"减少每 tick 克隆"
  议题评估（例如只在内容真变时克隆），而不是夹在这份 spec 里顺手改。
- **不动** `clear_partial` / `promote_partial` 的恢复判据
  （`thread_store.rs:691`、`partial_is_committed` `:273`）——它们是正确性核心。

**新增回归保护**（这是本节唯一的交付）：

1. **"逐条落盘"不变量**：测试断言一个含 N 个 `ItemCompleted` 的 turn，在任意
   时序下崩溃，`load` + `promote_partial` 恢复出的 items **不少于**
   崩溃前已完成的 items（即"不丢东西"）。
2. **"提问必落"不变量**：turn 开始即写一次 checkpoint（`server.rs:4275`），
   断言"这一轮随后立刻崩溃也不丢提问"——现有注释如此声明，但缺测试钉住。
3. **频率上限保护**：断言 checkpoint 写入次数**不随 item 数线性增长**
   （即 500ms 防抖继续生效）。这条是防止有人日后改成"每个 item 写一次"。
   注意：它**不是**要求"写得更少"，而是要求"别变多"。

### 3.5 TUI：脏 cell 集合 + 历史舍弃上限（G5）

**问题**（§1.1(e)）：流式期间每帧遍历全部 cell；末尾 cell 每帧重解析；
无上限。

**做法**：

1. **脏集合替代全表扫描**。`invalidate_mutated_cells`（`history.rs:655`）
   **已经知道**哪些 cell 变了（它按事件类型直接算出下标，如
   `AssistantText` → `last`、`ReasoningDelta` → `last`、`ToolResult` → 按 id 查找）。
   但 `note_content_change`（`:633`）只自增 generation、把"变了哪些"丢掉了，
   于是 `ensure_cache`（`:313`）不得不**全表重扫指纹**（`:337`）来重新发现它。
   改为：让 `invalidate_mutated_cells` 把变更下标记入一个
   **`dirty: HashSet<usize>`**，`ensure_cache` 只处理脏集合 + 新增的尾部 cell
   （`cells.len()` 与 `entries.len()` 的差），**根本不再遍历全表**。
   `content_generation` 保留，用于"整表失效"（宽度变化、`from_cells`）的快速路径。
2. **流式 tail 的 markdown 节流**（同 §3.3 的思路，落在 `markdown.rs` 的
   调用侧：`cell.rs:79` 的 `lines()` 只在节流窗口边界被 `ensure_cache` 调用）。
   注意 TUI 是**拉模型**（每帧 `terminal.draw`，`app.rs:575`），且主循环有
   `event::poll(timeout)`（`app.rs:202`）——需确认帧节奏，节流参数应取
   **帧间隔的整数倍**而非固定毫秒，具体值待实现时按实测确定。
3. **历史舍弃上限**。给 `HistoryState.cells` 加上限（建议
   `MAX_HISTORY_CELLS = 2000`，可配），超出时从**头部**整批丢弃，
   并在丢弃处插入一条 `HistoryCell::Separator{ label: "… 已略去更早的历史" }`
   作为可见提示。要求：
   - 丢弃后 `scroll_offset` / viewport anchor 必须重新收敛
     （复用既有 `reconcile_scroll_offset`、`capture_viewport_anchor`，
     见 `app.rs:439`、`app.rs:447`）；
   - 丢弃是**不可逆**的：TUI 的历史只影响显示，服务端 `thread/resume`
     仍能取回完整转录，故不构成数据丢失。这一点必须写进注释。
   - 上限**不影响**磁盘上的 `.jsonl`（服务端权威日志不动）。

**回归网**：`history.rs` 已有专用的性能计数器测试
（`lines_call_count`，`cell.rs:175`；`rebuild_count`，`history.rs:1285`），
且 `app.rs:7854` 已有一条同型门禁（断言流式 4 token 只重渲染 ≤8 个 cell，
历史 30 个 cell）。本 spec 的门禁沿用这一范式。

## 4. 交付顺序（每步独立提交、可单独回滚）

| # | 内容 | 依赖 | 主要回归网 |
|---|---|---|---|
| 1 | §3.1 第一层：`indexOfItem` 末尾快路径 | — | `session.test.ts` |
| 2 | §3.2：`ChatItem` memo + 冻结前缀 + `structuralVersion` | 1 | `ChatView.test.tsx`、`App.test.tsx` |
| 3 | §3.3：流式 markdown 节流 | 2 | `ChatView.test.tsx`、`MarkdownText.test.tsx` |
| 4 | §3.4：checkpoint 回归保护（**无行为变更**） | — | 新增"不丢东西"用例 + `yi-agent-app-server` 既有 checkpoint 测试 |
| 5 | §3.5：TUI 脏集合 + 舍弃上限 | — | `history.rs` 计数器测试、`app.rs:7854` 同型门禁 |

第 1、4、5 步彼此独立，可并行；第 2、3 步有先后依赖。

## 5. 风险与缓解

| # | 风险 | 缓解 |
|---|---|---|
| 1 | 冻结前缀导致历史项**不更新**（就地修改被 memo 吞掉） | `structuralVersion` 只对三类结构性变更自增（§3.2）；**必须**有测试覆盖推理块就地接管、`item/completed` 替换、`dropLocalUserMessage` 三条路径下历史仍更新 |
| 2 | markdown 节流吞掉**终值** | 节流窗口尾沿必须补发；流式结束（`turn/completed` / `item/completed`）必须立即渲染终值（§3.3） |
| 3 | ~~checkpoint messages 降频 → 崩溃恢复上下文变少~~ | **已消除**：该优化已撤回（§3.4）。用户要求「崩溃后还是每次都能看到」，故 checkpoint 行为**完全不动**，只加回归保护 |
| 4 | TUI 舍弃历史后滚动位置错乱 | 复用既有 anchor/reconcile 路径；新增测试断言舍弃后 `scroll_offset` 收敛 |
| 5 | 前端状态字段（`structuralVersion`）与 React 渲染时序不一致（异步 batch） | 只把它当**重置键**用（结构变即重建），不参与任何内容计算 |
| 6 | `App.tsx` 是新架构热点文件，与其它在途卡片冲突 | 本 spec 对 `App.tsx` 的改动**限于** §3.2 的接线与 `structuralVersion` 透传；已确认 `perf/ipc-responsiveness` 改的是 `server.rs` 主循环与 crate 树，**文件不重叠** |
| 7 | 无并发 cargo（OOM / exit 137） | 按 crate 跑测试；跑前 `ps aux \| grep cargo` 确认为空（项目规范） |

## 6. 验收

按项目规范，**结构性不变量做 CI 门禁，绝对耗时数字只作参考基线**。

| 项 | 门禁（可执行） | 判据 |
|---|---|---|
| G1 | `cd desktop && npx vitest run src/lib/session.test.ts` | 新增用例：以 O(1) 计数器（探针包住 `findIndex`）断言"主键打到末尾项时扫描次数为常数，与 `items.length` 无关" |
| G2 | `cd desktop && npx vitest run src/components/ChatView.test.tsx` | 新增用例：4000 项历史 + 1 个流式 delta，断言**历史前缀子树的 render 计数为 0**（React Profiler 或 spy）；并断言"就地修改历史项后 UI 立即更新"（覆盖 §5 风险 1 的三条路径） |
| G3 | `cd desktop && npx vitest run src/components/ChatView.test.tsx` | 新增用例：同一文本连续 100 个 delta，断言 markdown 渲染（`ReactMarkdown` 调用次数）**远小于** 100，且**流式结束后终值已渲染** |
| G4 | `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib` | 新增"不丢东西"用例：(a) 含大量 `ItemCompleted` 的 turn 在任意时刻"崩溃"后，`load` + `promote_partial` 恢复出的 items 不少于崩溃前已完成的 items；(b) turn 开始即崩溃也不丢提问；(c) 写入次数不随 item 数线性增长（防抖仍生效）。既有恢复/崩溃用例全绿 |
| G5 | `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent` | 沿用 `lines_call_count` / `rebuild_count` 范式：断言流式 N 个 token 的 cell 重渲染数与 `cells.len()` 无关；断言超出上限后 cells 收敛且滚动位置正确 |
| 全局 | `cd desktop && npx vitest run && npx tsc --noEmit` + 各 crate 测试 | 无回归 |

**参考基线**（不进 CI，仅记录）：§1.1 的实测曲线与复现脚本，作为"改前"对照；
"改后"需在真机（真实 WebKit / 真实终端）复测，数字写入本 spec 的后续修订
与 `docs/project-management/desktop.md`。

**判断依据（何时需要后续 spec）**：
- 若 §3.2 落地后真机单帧仍 >4ms → 虚拟化（另开 spec）。
- 若 §3.3 落地后长回复仍呈明显平方感 → 增量 markdown 解析（另开 spec）。
- 若另有数据表明 checkpoint 写入成为真实瓶颈（当前实测最坏 9.3ms/500ms，
  不足 driver 墙钟 2%）→ 那时再单独开 spec，且**必须同时保证"崩溃后不丢东西"**。

## 7. 复现脚本（本次实测所用，已入库）

脚本位于 **`desktop/scripts/perf/`**，README 见该目录：

| 脚本 | 覆盖 |
|---|---|
| `session.mjs` | §1.1(a)：目标在末尾时的 per-delta 查找开销 |
| `index-lookup.mjs` | §3.1：三种查找策略的对照（**推翻第一版设计的证据**） |
| `render.mjs` | §1.1(b) 与 §3.2：每 delta 重渲染 + 三种渲染形态对比 |
| `markdown.mjs` | §1.1(c)：流式 markdown 的单次解析与整轮平方级 |
| `wire.mjs` | §8.1：每帧字节放大（第五项的背景数据） |
| `checkpoint.mjs` | §3.4：真实 `.partial.json` 的字节构成与写入成本（**"不丢东西"的依据**） |
| `turn.mjs` | §2.5：端到端"一个回合值多少"，含历史深度扫描（**复现用户症状**） |
| `run.mjs` | 依次跑全部上面七项 |

运行：

```bash
cd desktop && npm install && node scripts/perf/run.mjs
```

**设计要点**（决定了它为何可信）：

- 脚本**直接打包仓库里的真实 `src/lib/session.ts`**（esbuild），不复制副本——
  源码一变，脚本测的就是新代码，不存在"脚本里躺着一份陈旧拷贝"的陷阱。
- `index-lookup.mjs` 通过**改写那一处热查找**生成三个变体，其余 `apply` 逻辑
  逐字相同，所以比较计数是可比的。它还会在热查找**位置变动时主动报错**
  （找不到锚点即失败），避免脚本静默测错东西。
- 前端脚本跑在 **node + jsdom** 下，**绝对数值不可等同于真机 WebKit**；
  可信的是增长趋势。这一点同时写在脚本输出末尾与 README 里。
- 脚本**只读仓库**：esbuild 产物写到系统临时目录（`os.tmpdir()/yi-agent-perf`）。
- TUI（§1.1(e)、§3.5）**不在**这些脚本里复现——它由 Rust 侧
  `lines_call_count` / `rebuild_count` 计数器测试覆盖（见 §6），那是更合适的载体。

## 8. 本 spec 明确未做（后续项）

### 8.1 远程 delta 合并（用户指定为「可选第二阶段」，**尚未做**）

**现状**：服务端只在"存在订阅客户端"时合并逐字流
（`server.rs:4482` 的 `hub.has_subscribed_clients()`），而**桌面端恒不订阅**
（订阅是 `isRemoteClient()` 门控的，`desktop/src/App.tsx:566`）。因此桌面端
每个 provider 文本块发一帧。

**实测放大**（§1.1 脚本同一份）：

```
chunk=  1B  frame=162B  amplification=162.0x
chunk=  4B  frame=165B  amplification= 41.3x
chunk= 16B  frame=177B  amplification= 11.1x
chunk= 64B  frame=225B  amplification=  3.5x
```

**为什么不放在本 spec 做**：它改变**线路上的帧边界与时序**，而本 spec 的
G6 恰恰承诺"帧的数量/时序不变"。且仓库里有明确的字节级承诺
（`broadcast.rs` 文档写明"无订阅者时等价于全广播"），把它做成必做会让
本 spec 的验收被"是否改变了桌面线路字节"绑住——那不是用户要解决的"越用越慢"。

**⚠️ 提醒（用户明确要求）**：**本 spec 完成后，第五项（远程 delta 合并）
仍未做，需另行推进。** 触发条件：需要压缩桌面/手机的帧数与流量时，
把合并条件从"存在订阅者"改为"存在任何客户端"，并逐一核对
`broadcast.rs` / `server.rs` 的字节不变承诺与相关断言。
本条目已同时登记在 `docs/bug-list.md`。

### 8.2 其它

- 前端列表虚拟化（见 §2 非目标与 §6 判断依据）。
- 增量 markdown 解析（同上）。
- 增量 checkpoint 格式（同上）。
- `session.ts` 第三层全量位置索引（见 §3.1）。