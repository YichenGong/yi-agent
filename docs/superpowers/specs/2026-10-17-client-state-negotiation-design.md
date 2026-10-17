# 客户端状态协商与同步对账（A 时序 / D 骨架 / B 幽灵 / C 前提）

> 目标：手机端（iOS WebView 远程客户端）与桌面共用同一 app-server 会话时，
> 消除「拿着旧信息做操作」这一类错误。做法是引入**每会话世代标记
> `content_epoch`** 作为统一骨架：连上那一刻客户端声明自己的状态（D），
> 服务端据此裁决同步范围；由此顺带修掉同步窗口内的过早解禁（A）、
> `thread/clear` 后永不清理的幽灵内容（B），以及 `turn/start` 的旧前提校验（C）。
> 桌面端行为逐字节不变。
>
> 日期：2026-10-17
> 相关：`2026-10-02-mobile-remote-access-design.md`（父 spec）、
> `2026-10-03-shared-session-relay-design.md`、
> `2026-10-11-mobile-local-cache-design.md`（本地缓存；本文**修正**其 §5.C 的
> 幽灵来源描述）、`2026-10-17-mobile-repair-path-design.md`

## 1. 问题

手机端是 app-server 的远程客户端：经中继（`wss://`）连桌面进程，本地有一份
首屏缓存（`desktop/src/lib/sessionCache.ts` + `contentStore.ts`）。缓存只做
**首屏占位**，服务端权威内容到达后就地覆盖。这套机制在「覆盖一定会到」时是
自洽的，但下面四条让它有了缺口。

### 1.1 A：连上之后有一小段窗口，界面已解禁而内容是旧的

握手（`desktop/src/App.tsx:1453`）依次发 `initialize` → `ui/settings/read` →
`workspace/list` → onboarding → `thread/listAll` → `selectThread`。三条叠加：

1. **running 会话的补尾部是 fire-and-forget。** `selectThread` 对
   `running` / `awaiting_approval` 会话走只读的 `thread/readItems`
   （`App.tsx:873-901`，刻意不用 `resume`——`resume` 会打断在途回合），
   但那条请求是 `void (async () => …)()`（`App.tsx:882`），**不 await**；
   紧接着 `if (inWindow || running) return;`（`App.tsx:901`）让它立刻返回。
2. **握手继续走到「已连上」。** `setStatus("connected")`（`App.tsx:1520`）
   在权威内容到达之前执行。
3. **「离线内容」提示被全局清掉。** `useEffect` 在 `status === "connected"`
   时 `cachedAt.current.clear()`（`App.tsx:554`），清的是**整张表**，
   不看具体会话是否已对完账。

后果：用户看到一个已卸掉「这是缓存」告示、且发送门槛（`App.tsx:1663`
`if (offline)`）已解除的界面，而屏幕上的内容是旧的。此时发出去的消息会进入
真实会话，而服务端无从察觉客户端的上下文缺了中间几轮。

### 1.2 B：`thread/clear` 之后，手机上的旧内容**永远不会**被清掉

前置 spec `2026-10-11` §5.C 的三段式（先显示缓存 → 服务端回放就地覆盖 →
清掉幽灵项）在 `thread/clear` 场景下**两个守卫分别挡住**，收敛根本不发生：

- **idle 路径**：`selectThread` 走 `thread/resume`，回放集 `replayIds` 由
  `beginReplay` / `endReplay` 收集（`session.ts:91` / `:100`）。clear 之后
  服务端 items 为空，回放**不带任何 item 帧**，于是
  `if (replayed !== null && replayed.size > 0) dropStale(...)`
  （`App.tsx:924`）判定「无帧可对账」而**跳过**清理。
- **running 路径**：`thread/readItems` 只补尾部、按设计就不做删项
  （`2026-10-11` §5.D，理由正当）。

于是手机屏上留下服务端早已不存在的整段内容，且没有任何路径会清它。

**同时修正一处事实**：幽灵来源**只有 `thread/clear`**，`thread/compact`
**不是**来源。`thread/compact` 把 session messages 换成压缩版后调
`persist_and_finish_turn`，落盘的是 `items: Vec::new()`（`server.rs:6312`），
而 `read_log` 是**跨行拼接** items（`thread_store.rs:239`）——历史 items 在
jsonl 里分毫未动，`readItems` / `resume` 照旧返回它们。真正让 items 从服务端
消失的只有 `thread/clear` 的 `store.truncate`（删 jsonl，`thread_store.rs:617`）。
这一修正直接决定 §4 的世代递增点只放在 clear。

### 1.3 C：`turn/start` 不校验客户端的前提

`prepare_turn_core`（`server.rs:7625`）只校验两件事：thread 存在、
`active_turn_id` 空闲。**没有任何「客户端以为的历史」与「服务端实际历史」
的一致性检查。** A 与 B 的旧内容因此不只是显示问题——它是**新的错误的输入**：
客户端基于旧上下文发出的消息被照单全收。

仓库里已有这条范式的先例：`ThreadStore::set_title_if_unchanged(expected, new)`
（`thread_store.rs:374`）——按「预期值未变」才落笔，否则返回 `false`。

### 1.4 D：缺口的结构性原因——双方都不声明自己手上的版本

缓存条目的 `savedAt`（`contentStore.ts:19`）只用于显示「更新于 3 小时前」，
**不参与任何判定**；`Session.lastServerItemId`（`session.ts:82`）是客户端的
续读游标，**从不送给服务端**。反过来，服务端返回的 `ThreadSummary`
（`thread_summary_json`，`server.rs:5943`）只有 `updated_at` 这类**活动时间**，
没有「内容世代」这样的**版本**概念。

即：客户端盲拉、服务端盲推，**没有一处地方能回答「这个客户端手上的内容和
我的一致吗」**。A/B/C 都是这一条的表现，D 是它们的公共解。

## 2. 目标与非目标

**目标**

- 手机端**任何时刻**屏幕上显示的内容，要么标注了它是旧内容，要么已与服务端对完账。
- 点进/重连一个会话时，同步**完成的信号**驱动解禁（提示条退场、发送开闸），
  而不是「连接建立」这个事件。
- `thread/clear` 之后，手机上的旧内容在**下次连上**时被清掉（不需要服务端
  回放任何帧作为判据）。
- `turn/start` 能识别「客户端前提已失效」并拒绝，而不是把错误的上下文
  静默并入会话。
- 服务端在握手时就知道客户端的状态，并把「该怎么同步」裁决权拿回服务端。
- 桌面端（`isRemoteClient()` 恒 false / stdio 单客户端）行为逐字节不变。
- **缺字段时必须退化为今日行为**，绝不因为老服务端/老客户端少一个字段而误判。

**非目标**

- 不做离线写入 / 发送队列重发（继续沿用 `2026-10-11` §6.3：离线只提示）。
- 不做 UPLOAD / 看板 / 子 agent 面板的对账（YAGNI）。
- 不做「端到端加密」「多用户」等父 spec 的非目标。
- 不改变「审批先到先得」语义（`route_client_response` 的 no-op，
  `server.rs:5968`）——已在 §7.4 评估为足够安全。
- 不为 `compact` 递增世代（§1.2 已证其不丢 items）。
- 不引入 per-client 的持久状态：服务端只按本次连接处理 hints，不落盘。

## 3. 核心概念：`content_epoch`

一个**每会话**的单调计数，语义是「该会话**可见内容集合**的世代」。

- 存放：`ThreadMeta.content_epoch: u64`，`#[serde(default)]` ⇒ 旧 meta 读到 0
  （零迁移，与 `pin_seq` / `reasoning_effort` 同款）。
- **递增点仅一处**：`thread/clear` 成功执行 `store.truncate` 之后。
  判据（写进代码注释，防后来者误加）：**只有会改变服务端 item 集合的操作才递增**。
  `compact` 不递增（§1.2）；`delete` 不需要（会话整体消失，由列表存在性判定）；
  普通 turn 追加不递增（那是同一个世代内新增内容，不是替换）。
- 递增实现：`ThreadStore` 新增 `bump_content_epoch(id) -> io::Result<bool>`，
  复用既有 `update_meta` 的 `meta_lock`（`thread_store.rs:324`），
  与 `set_permission_mode` 同款——**有意不刷新 `updated_at`**（世代变更属内容
  维护，不该改变按活动时间的列表排序）。

配套字段：`ThreadMeta.last_item_id: Option<String>`，由 `persist_and_finish_turn`
在 `append_turn` 成功后与 `touch` 同一把锁写入（该处**已经**要读改写 meta，
零额外 IO）。它的存在是为了让 `thread/listAll` 能带上「该会话最后一条 item」
而**不必逐个读 jsonl**——`ThreadStore::list` 今天只读 `*.meta.json`
（`thread_store.rs:293` 注释明示），若在列表路径上 load 每个会话的日志，
会随会话数与日志体积线性变慢。这是本设计里唯一一处为性能而做的冗余。

**第二处写入点必须一并维护（易漏）。** `ThreadStore::promote_partial`
（`thread_store.rs:588`）是**另一条** `append_turn` 调用路径——它把崩溃残留的
partial 升格进主 jsonl，**不经过** `persist_and_finish_turn`。它是实现里唯一
第二条写 items 的路径（全仓实测：生产代码只有 `server.rs:6162` 与
`thread_store.rs:588` 两处 `append_turn`）。若只改前者，一条「崩溃后 resume
恢复了一轮」的会话其 `last_item_id` 会停在恢复前的旧值，导致 `listAll` 给出的
锚点偏旧。缓解有两条，**取第二条**：

1. 在 `promote_partial` 里也维护 `last_item_id`（要新读改写 meta）；
2. **让 `last_item_id` 只作为「保守下界」使用**——§4.2 的裁决只用它做
   「客户端是否落后」的判断，而真正的**锚点有效性**校验读真实 jsonl
   （§4.2 第 3 行），故偏旧的 `last_item_id` 最坏只会让客户端多补一段内容
   （幂等 upsert），不会漏内容。

取 (2) 的理由：不增加第二处 meta 读改写（也就不会与 driver 的 `touch`
争 `meta_lock`），且正确性不依赖「每个写入点都记得维护」。§10 测试 8 覆盖
主路径，测试 17 覆盖「`last_item_id` 略旧仍不影响正确性」。

## 4. D（骨架）：状态声明与同步裁决

### 4.1 客户端在握手第一帧声明状态

`initialize` 的 params 新增可选字段（`params` 今天恒为空对象，
`server.rs:2455` 不读它）：

```jsonc
{
  "resumeHints": [
    { "threadId": "thread-…", "contentEpoch": 3, "lastItemId": "item-…", "cachedAt": 1728700000000 }
  ]
}
```

- 手机冷启/重连时带上：**当前选中会话**（必有缓存或必有选择）+ 缓存里的会话。
- 上限 20 条（超出取 `cachedAt` 最新的 20 条）——这是「非常小的数据量」的
  硬边界；字段本身只有 id、两个整数、一个可选 id。
- 桌面端不发此字段（`isRemoteClient()` 门控）；老客户端不发 → 服务端走今日路径。

### 4.2 服务端在响应里裁决

`initialize` 的 result 新增：

```jsonc
{
  "serverInfo": { … },
  "protocolVersion": 1,
  "capabilities": {},
  "syncPlan": [
    { "threadId": "thread-…", "action": "full", "epoch": 4,
      "reason": "epoch_changed" }
  ]
}
```

`action` 三种取值，服务端按以下序判定（**顺序即优先级**）：

| 条件 | `action` | 客户端动作 |
| --- | --- | --- |
| 该 thread 已不在服务端（被删） | `drop` | 丢弃本地缓存条目与该会话视图 |
| hint 的 `contentEpoch` ≠ 服务端当前 epoch | `full` | 丢弃本地内容，走 `thread/resume` 全量回放 |
| epoch 相同、`lastItemId` 在服务端日志中缺失 | `full` | 同上（游标锚点已失效） |
| epoch 相同、`lastItemId` 在日志中（**含落后于末条**） | `none` | 缓存内容**仍然有效**，无需作废 |
| hint 未提及的 thread | （不出现在 plan 里） | 由 §4.3 的 listAll 字段兜底 |

**`none` 的准确语义是「缓存内容不必作废」，不是「客户端已是最新」。**
这一点必须写死，否则实现者会为了判「是否完全一致」而去计算服务端末条 item，
既多读日志、又引入一个与 §7.2 冲突的判据。判据只有一条：**锚点是否仍存在于
服务端日志中**（「落后」是正常的并存形状，见 §7.2）。若客户端确实落后，
后续正常的 `resume` / `readItems` / 订阅流会把它补齐——那是常规路径，
与本次裁决无关。

**但有一条例外，且它是客户端侧的。** 缓存条目可能因超 1MB 而**丢过头部**
（`ContentEntry.trimmed === true`，`contentStore.ts:80`）。这种条目的
`lastItemId` 锚点是有效的、epoch 也是同一代，所以服务端会正确地回 `none`——
可客户端的缓存**本就不是完整内容**。若照 `none` 跳过 `resume`，用户就会
静默看到一个被截断的会话。规则：**客户端收到 `none` 后，若本地该条目的
`trimmed` 为 true，仍按 `full` 处理（走 `thread/resume` 把头部补回来）。**
这一判断只依赖客户端自己知道的信息（自己有没有裁过），故**不**需要放进
hint、也不影响 `syncPlan` 的形状。测试 18 锁定。

`reason` 是诊断用的稳定串（`epoch_changed` / `anchor_missing` / `deleted`），
客户端**不**据它分支，只用于日志与「离线内容」提示条的文案微调。

**`action: "drop"` 作用在当前选中会话上时**：该会话已不存在，`selectThread`
对它必然拿到 `unknown_thread`。客户端必须（a）丢弃该会话的视图与缓存条目
（`store.drop(id)` + `contentStore.delete(id)`，都已存在），（b）`currentId`
回落——复用 `App.tsx:1507-1516` 既有的「kept 不在服务端新列表里 → 回落第一条」
判定（`drop` 的会话天然不在 listAll 里，故该分支已覆盖它，无需新逻辑）；
（c）不显示错误气泡（会话被另一端删除是正常事件，不是故障）。
测试 13 的 `drop` 用例锁定 (a)(b)。

**`action: "none"` 不等于「什么都不做」。** 它只表示「缓存内容无需重建」；
`selectThread` 仍必须照常执行（订阅 `thread/subscribe`、补 running 会话的
订阅窗口、刷新 mode chip 等）。若实现者把 `none` 理解成「跳过 selectThread」，
当前会话的实时流会**静默停摆**——这正是 `2026-10-11` §6.4 的实现注记已经
踩过的坑（「否则视图留着而该会话的实时更新会静默停摆」）。这条要有测试锁定。

**为什么裁决放服务端而不是让客户端自己比 epoch：** 因为「该会话是否已被删除」
与「该 item id 是否仍在日志里」这两件事客户端无法自行判断——前者要服务端列表，
后者要读日志。把判断集中在一处，客户端就不必猜。

### 4.3 兜底：列表自带版本

**客户端写回缓存的基线来源（三处，别搞混）：**

- `contentEpoch`：优先取 `syncPlan` 条目里的 `epoch`（服务端权威值）；
  plan 未覆盖的会话取 `ThreadSummary.content_epoch`。
- `lastItemId`：`full` 的会话由 `resume` 回放的内容自行得出（`upsertItems`
  已维护 `lastServerItemId`）；其余取 `ThreadSummary.last_item_id`。
  故 `syncPlan` **不需要**携带 `lastItemId`——它每次握手都会紧接着发
  `thread/listAll`，而 listAll 已带全量基线。

`ThreadSummary` 新增两个字段（`thread_summary_json`）：

```jsonc
{
  "thread_id": "thread-…", …,
  "content_epoch": 4,
  "last_item_id": "item-…"   // 可为 null（空会话）
}
```

用途两条：

1. **兜底裁量**：`syncPlan` 未覆盖的会话（hints 上限截断、或客户端后来才发现
   的会话），客户端用这两个字段自行判断走 `resume` 还是 `readItems`。
2. **下次 hint 的基线**：客户端把本次读到的 `content_epoch` / `last_item_id`
   写回缓存（`contentStore.ts` 的条目结构加这两个字段），下次连上时作为 hint 上报。

**缺失语义（关键不变量）**：字段缺失（老服务端）时客户端读到 `undefined`，
必须解释为「无法裁量」⇒ 走今日的保守路径（冷会话一律 `resume`），
**绝不**解释为「epoch 相等」。这一条要有专门测试钉住。

### 4.4 不做的事（YAGNI）

- **不加全局 `threads_revision`**：每会话已有 epoch，列表变化客户端从
  `listAll` 的存在性直接看得出；全局计数是冗余的第二真相源。
- **不做服务端主动 `thread/invalidated` 推送**：客户端只在握手时持有 hint，
  连接期间其缓存不会再变旧（内容层订阅流在推）；要推送就得维护 per-client
  状态，收益不足。
- **不新增 RPC**：`initialize` 是连上那一刻的**第一帧且必发**，把声明与裁决
  合进去 = 零新增往返，正合「极小数据量的交互」。

## 5. A：同步窗口（解禁由「对账完成」驱动）

### 5.1 每会话的同步态

新增 per-thread `syncState: "syncing" | "synced"`（落在 `ThreadView`，
`threadStore.ts:50` 的 `create()` 初值 `"synced"`，因为桌面/首次进入无需同步）。

置 `syncing` 的时机：`selectThread` 决定要走 `resume` **或** `readItems` 时
（即 `App.tsx:873` 起的 remote 分支与 `:911` 的 resume 分支）。

置 `synced` 的时机（**两个路径都要覆盖，这是 A 的修复核心**）：

- `resume` 完成（含 `dropStale` 之后、`warm.current.add(id)` 那一处，
  `App.tsx:929` 附近）。
- `readItems` 完成——在 `App.tsx:882` 那段 `void (async () => …)()` 的
  **`.then` 收尾处**置 `synced`。`readItems` 是只读、不打断在途回合，
  不动它的 fire-and-forget 形状（`selectThread` 的 `running` 早退
  `:901` 保留：只换视图，不阻塞握手）。
- 失败路径（`catch`）也要置 `synced`（并保留已有内容），否则会永久卡在
  「正在同步」。

**刻意不选「让握手 await 同步」。** 另一种修法是让 `handshake` 等到
`readItems` / `resume` 回来才 `setStatus("connected")`。它被否掉的理由：
侧栏、其它会话、看板都不该因为**当前这条**会话的对账而被推迟，而且
`resume` 长会话的回放本就不该卡住整个握手。用 per-thread 的显式状态
（`syncing` / `synced`）表达「这一条还没对完账」，比用「握手是否返回」
表达要精确，也不会把 A 的修复与握手时序耦合在一起。

### 5.2 提示条与发送门槛改按会话

- `CacheNotice`（`App.tsx:2202`）显示条件从
  `offline && cachedAt.has(currentId)`
  改为 `(offline && cachedAt.has(currentId)) || syncState === "syncing"`，
  文案区分两态：离线 → 「离线内容 · 更新于 X」；同步中 → 「正在同步…」。
  两种文案**互斥**（`syncing` 优先）：同步中不再是「离线」，说「离线内容」会误导。
- `send` 的门槛（`App.tsx:1663`）从 `if (offline)` 改为
  `if (offline || 当前会话的 syncState === "syncing")`——**注意是当前会话
  （`store.currentId`）的**同步态，不是任意会话的：后台会话在同步不该拦住对
  当前会话的发送。提示文案相应区分。**只拦不排**，草稿保留
  （沿用 `2026-10-11` §6.3 的语义）。
- 两者都包在 `isRemoteClient()` 内；桌面端 `syncState` 恒 `synced`、
  `offline` 恒 false ⇒ 条件恒假，DOM 与今日一致。

### 5.3 `cachedAt` 改为按会话清除

`App.tsx:554` 的全局 `cachedAt.current.clear()` 删除，改为在该会话的
`contentEpoch` 对账完成后 `cachedAt.current.delete(id)`（与 §5.1 的
`synced` 同一处）。理由：全局清会让「另一个仍在显示缓存的会话」失去标注。

## 6. B：幽灵内容清理

有了 `content_epoch`，收敛判据从「服务端回放了哪些帧」改为
**「世代是否变过」**——不再依赖 `replayIds` 是否非空。

### 6.1 客户端规则

在 `Session` 上新增：

```ts
/**
 * Record the authoritative epoch seen for this session, and drop the whole
 * cached transcript when it changed.
 *
 * Returns true when content was invalidated (caller must do a full resume).
 */
adoptEpoch(epoch: number, lastItemId: string | null): boolean
```

- epoch 与 `this.contentEpoch`（新字段，初值 `null`）不同 ⇒ `items = []`、
  `lastServerItemId = null`、记录新 epoch、**返回 true**（必须全量 resume）。
- epoch 相同 ⇒ 只更新 `lastServerItemId`（若给了），返回 false。
- `this.contentEpoch === null`（本次首见，无缓存）⇒ 只记录，返回 false。

### 6.2 `dropStale` 的守卫保留但不再是唯一判据

`App.tsx:924` 的 `replayed.size > 0` 守卫**保留**（它对 `resume` 路径仍是
正确的补充：能清掉同一世代内被裁剪的 item）。但 `adoptEpoch` 在它**之前**
执行：世代变了就整段作废，此时「回放集为空」不再意味着「无帧可对账」，
而是「服务端内容本来就是空的」——语义因此闭合。

### 6.3 running 路径也覆盖

`readItems` 路径同样先 `adoptEpoch`（用 `syncPlan` 或 `ThreadSummary` 给的值）。
这是 §1.2 那个「无条件不清理」缺口的修复：running 会话若跨越了 clear，
本地内容整段作废后再补尾部。

**世代失效时对 running 会话必须请求全量。** `readItems` 的 `afterItemId` 取自
`store.peek(id)?.session.lastServerItemId`（`App.tsx:881`）；`adoptEpoch` 作废
内容时已把 `lastServerItemId` 置 `null`，故这次请求**天然**是
`{ threadId: id }` 无锚点的全量形态——服务端在锚点缺失时也回退返回全部
（`server.rs:5575`）。两处语义一致，无需额外分支；但实现顺序**必须**是
先 `adoptEpoch` 再发 `readItems`，否则会带着已被作废的锚点去请求。
测试 13 锁定这个顺序。

### 6.4 必须同时修掉 `prefillFromCache` 的复活竞态（本方案引入的回归）

`adoptEpoch` 把 `items` 置空，恰好**解除了** `prefillFromCache` 的守卫，
让过期缓存有机会重新注回——这正是 B 要修的缺陷被复活。

链条（全部实测确认）：

1. `selectThread` 先调 `prefillFromCache(id)`（`App.tsx:848`），它是
   `void store0.read(id).then(...)` 的**异步 fire-and-forget**（`:230`）。
2. 该 then 回调的守卫是 `if (!v || v.session.items.length > 0) return;`
   （`:235`）——**「内容非空就跳过」**。
3. `adoptEpoch` 把 `items` 清空 ⇒ 若 `adoptEpoch` 排在 prefill 的回调
   **之前**，后续那个早已排队的 prefill 回调看到 `items.length === 0`，
   判定「服务端还没填」，于是把**已被判为过期的缓存整段灌回**。

修法两条，**两条都要做**：

- **（a）置位判据。** `prefillFromCache` 的守卫改用「本次连上是否已对过账」
  的显式标记（即 §5.1 的 `syncState`）：`if (view.syncState !== "synced" ||
  view.session.items.length > 0) return;` 无法表达「已判过期」，故改为
  **一行显式闸门**——新增 `Session.epochInvalidated: boolean`，`adoptEpoch`
  判定世代变化时置 `true`，`prefillFromCache` 的回调在 `epochInvalidated`
  为真时直接返回（不注入）。
- **（b）顺序。** `selectThread` 内在 `await` 任何同步动作**之前**先完成
  `adoptEpoch`，使「已判过期」的标记在 prefill 回调可能执行之前就已就位。
  由于 prefill 的回调是微任务、`adoptEpoch` 是同步调用，只要把 `adoptEpoch`
  放在 `selectThread` 的**同步段**（`prefillFromCache` 调用点附近、
  两者之间不插入 `await`）即可保证顺序。

**为什么不能只靠 (b)**：`adoptEpoch` 也可能发生在 `readItems` / `resume`
的响应处理里（那时 prefill 的回调早已在微任务队列中排队）。闸门 (a) 是
与调度顺序无关的兜底，两者缺一不可。测试 16 专门钉住（构造「prefill 回调
晚于 adoptEpoch 」的时序，断言过期缓存**没有**被注入）。

### 6.5 修正前置 spec

`2026-10-11-mobile-local-cache-design.md` §5.C 的幽灵来源清单
「`thread/clear` / `compact` / 或被裁剪」应改为「`thread/clear` / 或被裁剪」，
并在该节加一行指向本文 §1.2 的说明。这是文档同步，非代码改动。

## 7. C：`turn/start` 的前提校验

### 7.1 客户端带锚点

`turn/start` 的 params 新增**可选** `contextAnchor`：

```jsonc
{ "threadId": "thread-…", "input": [ … ],
  "contextAnchor": { "contentEpoch": 4, "lastItemId": "item-…" } }
```

手机端发；桌面端（`isRemoteClient()` false）**不发**；老客户端不发。
字段取自该会话当前 `Session.contentEpoch` / `lastServerItemId`。

### 7.2 服务端判定

在 `prepare_turn_core` 之前（`server.rs:4798` 的 `turn/start` 分支内）插入
一次校验，**只在 params 携带 `contextAnchor` 时生效**：

| 情形 | 判定 |
| --- | --- |
| 未携带 `contextAnchor` | 不校验（桌面/老客户端零回归） |
| `contentEpoch` < 服务端当前 epoch | **拒绝** |
| `lastItemId` 非 null 且不在服务端日志中 | **拒绝** |
| `lastItemId` 在日志中，但**落后于**最后一条 | **放行**（见下） |
| `lastItemId` 与最后一条一致 | 放行 |

**为什么「落后」要放行**：落后是**正常并存**的形状（桌面刚跑完一轮、手机
还在收订阅流），而不是不一致。拒绝它会误伤「两端同时用」这一父 spec 的
核心场景。真正的危害是**锚点已失效**（跨越了 `clear`）——那时客户端持有的
是服务端**不存在**的内容，它看到的上下文与服务端实际上下文结构性不一致。
这与 `set_title_if_unchanged` 的判据同源：只认「预期值已变」。

拒绝时返回新错误码：

```rust
/// 客户端的上下文锚点已失效（该会话在其不知情时被 clear）。
///
/// 数值码与 `board_query` / `model_not_found` 同族：`data.code` 才是
/// UI 分支的稳定词汇（`stale_context`）。
pub fn stale_context(thread_id: &str) -> Self {
    Self { code: -32026, .., data: Some(json!({ "code": "stale_context" })) }
}
```

（码空间实况：`-32001`、`-32010`~`-32014`、`-32020`~`-32025` 已占用
——`-32025` 是 `model_not_found`，`protocol.rs:161`；中间有空洞，但为便于
维护取**下一个空位** `-32026`，不填洞。）

### 7.3 客户端处理

收到 `data.code === "stale_context"` 时：**不自动重发**。原因与 §6.1 一致——
锚点失效意味着服务端内容已整段变化，静默重发会让「用户的意图」被应用到
一个他没看过的上下文上。行为是：把该会话标记为需全量同步、置 `syncing`、
提示「会话内容已在别处被清空，正在重新加载」、走 `resume`；草稿保留，
由用户确认后再发。（与 `2026-10-11` §6.3「不排队、不自动重发」同款。）

**重同步前必须清掉两道守卫，否则 `resume` 不会真的发生**（实测确认）：

- `warm.current.has(id)` 与 `inFlightResume.current.has(id)`
  （`App.tsx:903`）会让 `selectThread` 直接 `return`；
- 一个已经被 `resume` 过（在 `warm` 里）的会话，其内容仍躺在 `Session` 里，
  不 `warm.current.delete(id)` 就永远走不到 `resume` 分支。

故处理顺序固定为：`adoptEpoch`（使内容作废、置 `epochInvalidated`）→
`warm.current.delete(id)` → `inFlightResume.current.delete(id)` →
置 `syncing` → 调 `selectThread(id, { leaveBoard: false })`。
`leaveBoard: false` 沿用 `App.tsx:1512` 的同款理由：这不是用户换页，
不该把正在看看板的人踢回会话。**不**做「改 lastItemId 后让下一次
`readItems` 兜住」——那会绕过全量校验，正是 C 要防的形态。测试 13 锁定
「`stale_context` 后确实发出了 `thread/resume`」。

### 7.4 已评估、不需要校验的操作

| 操作 | 结论 | 依据 |
| --- | --- | --- |
| `item/toolCall/requestApproval` 的应答 | 不需要 | 已被处理过的审批是**幂等 no-op 成功**（`server.rs:5968` `route_client_response`），且状态通知会清掉残留审批框（`threadStore.ts:136`）；手机缓存**不存** `approval`（`2026-10-11` §4.1） |
| `thread/setModel` / `setPermissionMode` | 不需要 | 幂等元数据写入，重复设置无害 |
| `thread/delete` | 不需要 | 已有存在性校验（`unknown_thread`） |
| `thread/rename` | 不需要 | 已有 `set_title_if_unchanged` 的乐观检查 |

## 8. 协议变更汇总

| 位置 | 变更 | 兼容性 |
| --- | --- | --- |
| `thread_store::ThreadMeta` | 新增 `content_epoch: u64`（default 0）、`last_item_id: Option<String>`（default None） | 旧 meta 零迁移 |
| `thread_store::ThreadStore` | 新增 `bump_content_epoch(id)`、`contains_item_id(id, item_id) -> bool` | 新增 |
| `protocol::ThreadSummary`（`thread_summary_json`） | 新增 `content_epoch`、`last_item_id` | 新增字段 |
| `initialize` params | 新增可选 `resumeHints` | 老客户端不发 ⇒ 不变 |
| `initialize` result | 新增 `syncPlan` | 新客户端读；老客户端忽略 |
| `turn/start` params | 新增可选 `contextAnchor` | 桌面/老客户端不发 ⇒ 不变 |
| `RpcError` | 新增 `stale_context()`（`-32026`，`data.code="stale_context"`） | 新增码 |
| `PROTOCOL_VERSION` | **保持 1** | 全部为可选新增，无破坏性变更（见 §11 风险 1） |
| `Session`（前端） | 新增 `contentEpoch`、`epochInvalidated`、`adoptEpoch(epoch, lastItemId)` | 新增 |
| `ThreadView`（前端） | 新增 `syncState` | 新增 |
| `contentStore` 条目 | 新增 `contentEpoch`、可选 | 老条目缺字段 ⇒ `adoptEpoch` 收 `null` 语义 |

## 9. 模块边界

| 单元 | 做什么 | 依赖 |
| --- | --- | --- |
| `ThreadStore::bump_content_epoch` | 单一递增点；复用 `meta_lock` | 无 |
| `initialize` 的 hint 解析 + plan 裁决（`server.rs` 新函数 `plan_sync`） | 纯函数：给 hints + 服务端每会话 `(epoch, lastItemId, exists)` → `syncPlan` | 无（可脱 DOM/IO 测） |
| `ThreadStore::contains_item_id` | 校验锚点是否仍在日志中（读 jsonl）| 无 |
| `Session.adoptEpoch` | 世代比较与整段作废；纯函数 | 无 |
| `ThreadView.syncState` | 同步态；由 `App.tsx` 的 select/sync 路径驱动 | 无 |
| `App.tsx` | 接线：hints 组装、plan 消费、syncState 驱动 UI 与发送门槛 | 上述 |

**性能注记**：`plan_sync` 只在 `initialize` 时按 hint 逐条校验锚点，
hint 上限 20 条 ⇒ 最多 20 次 jsonl 读。`thread/listAll` 路径**不读 jsonl**
（靠 `last_item_id` 冗余），保持今日的「只读 meta」成本特征。

## 10. 测试策略（先写失败测试）

**Rust（`cargo test -p yi-agent-app-server --lib`）**

1. `clear_bumps_the_content_epoch` —— clear 后 meta 的 epoch = 1；且
   `updated_at` **未变**（列表排序不受影响）。
2. `compact_does_not_bump_the_content_epoch` —— 锁住 §1.2 的结论。
3. `a_hint_with_a_stale_epoch_plans_a_full_resync`。
4. `a_hint_whose_anchor_is_gone_plans_a_full_resync`。
5. `a_hint_that_matches_plans_nothing`。
6. `a_hint_for_a_deleted_thread_plans_a_drop`。
7. `an_empty_resume_hints_plans_nothing`（零回归：老客户端）。
8. `list_all_carries_content_epoch_and_last_item_id`。
9. `turn_start_without_a_context_anchor_is_not_validated`（桌面零回归）。
10. `turn_start_with_a_stale_epoch_is_rejected_with_stale_context`（`-32026`
    且 `data.code == "stale_context"`）。
11. `turn_start_with_an_anchor_present_but_behind_the_tail_is_accepted`
    （§7.2 的关键正向用例，防误伤并存场景）。

**前端（`cd desktop && npx vitest run`）**

12. `session.test.ts`：`adoptEpoch` 的 epoch 变 ⇒ items 清空 + 返回 true；
    epoch 同 ⇒ 只更新游标 + 返回 false；初值 null ⇒ 只记录 + 返回 false。
13. `App.test.tsx`：
    - 手机端 + `syncPlan: full` ⇒ 走 `resume` 且**不是** `readItems`；
    - **`syncPlan: none` 时仍走了一次 `selectThread`**（断言发出了
      `thread/subscribe`，或 running 会话发了 `readItems`）——钉住 §4.2 的
      「`none` ≠ 什么都不做」，防实时流静默停摆；
    - **`syncPlan: drop` 且该 thread 是当前选中** ⇒ `setCurrentId(null)` 或回落到
      列表第一条，且屏上不残留已删会话的内容；
    - running 会话 `readItems` 完成**之前** `syncState === "syncing"`
      （提示条在、发送被拦），完成之后转为 `synced`（提示条退、发送放行）；
    - `readItems` 抛错 ⇒ `syncState` 仍转为 `synced`（不永久卡死）；
    - 手机端 `initialize` 携带 `resumeHints`（含缓存 epoch/lastItemId）；
    - 收到 `stale_context` ⇒ 置 `syncing`、走 `resume`、**不**发第二次
      `turn/start`、草稿保留；
    - 提示条条件：`offline` 与 `syncing` 两种文案分别出现。
14. **桌面端回归**：非 remote 时 `resumeHints` / `contextAnchor` **一个都不发**、
    `syncState` 恒 `synced`、提示条恒不出现（与今日逐字节一致）。
15. **缺字段回归**：`ThreadSummary` 无 `content_epoch` ⇒ 客户端按保守路径
    全量 `resume`，**不**判定「一致」（钉住 §4.3 的缺失语义）。
16. **复活竞态（§6.4）**：构造「`prefillFromCache` 的 IndexedDB 读回调晚于
    `adoptEpoch`」的时序（注入的假 `AsyncContentStore` 把 read 的 resolve
    推迟到 adoptEpoch 之后）⇒ 断言**过期缓存没有被注入**（`items` 保持清空，
    随后由 `resume` 填入权威内容）。回退 §6.4 的闸门 (a) 或顺序 (b) 即红。
17. **`last_item_id` 偏旧仍正确（§3 缓解 2）**：令 meta 的 `last_item_id` 故意
    停在倒数第三条（模拟 `promote_partial` 未维护），客户端连上后断言：
    plan 给出 `none`（锚点仍在日志中）、随后正常路径把落后的一段补齐、
    **内容最终与权威一致**（不丢内容）。
18. **`trimmed` 强制全量（§4.2 例外）**：预置一条 `trimmed: true` 的缓存条目
    + 服务端同 epoch 同锚点（plan 会给 `none`）⇒ 断言客户端**仍走了**
    `thread/resume`，最终内容为完整权威历史（而非被截断的缓存）。

**验收命令**

```
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
cd desktop && npx tsc --noEmit && npx vitest run
```

**真机手动验证（iOS WebView，经中继）**：桌面 `thread/clear` 一个会话后
手机重连 ⇒ 手机内容整段刷新（不再残留旧内容）；桌面跑 turn 时手机重连 ⇒
先见「正在同步…」、同步完自动转为正常、期间发送被拦；两端并存正常使用
不出现任何 `stale_context`。

## 11. 风险

1. **`PROTOCOL_VERSION` 不递增的取舍**：全部变更为「响应多字段 + 请求可缺省」，
   老客户端忽略新字段、新客户端对缺字段退化为保守路径，故无破坏性。若实现中
   发现任何一处**必须**让老客户端改变行为（例如 `syncPlan` 缺省导致误判一致），
   则该处必须重做，而不是靠递增版本号绕过——递增版本号会让所有老客户端
   立刻连不上，代价过大。测试 15 就是这条的守卫。
2. **`last_item_id` 冗余写漏**：它由 `persist_and_finish_turn` 维护。若某条落盘
   路径绕过它（例如未来新增的写入点），`listAll` 会给出过期的 `last_item_id`，
   导致客户端少补一段内容。缓解：`plan_sync` 的锚点校验读**真实 jsonl**
   （不信任 meta 冗余），故冗余只影响「是否需要同步」的保守判断，不影响正确性；
   §10 测试 8 锁住主路径。
3. **`syncState` 卡在 `syncing`**：`readItems` / `resume` 失败路径必须置
   `synced`（§5.1），否则用户永久无法发送。测试 13 覆盖失败分支。
4. **`stale_context` 的误报**：若把「落后」也判为 stale，会破坏并存场景。
   §7.2 的表格与测试 11 是硬约束。
5. **桌面回归**：所有新分支 gate 在 `isRemoteClient()` 或「params 是否存在」；
   测试 9 / 14 专门锁定。
6. **`initialize` 变重**：hints 上限 20、只校验锚点、只读最多 20 个小文件；
   桌面不发 ⇒ 桌面握手成本零变化。

## 12. 分期

1. **D 骨架（协议 + 存储）**：`content_epoch` / `last_item_id` / `resumeHints` /
   `syncPlan` / `listAll` 字段 + Rust 测试 1-8。零 UI 改动即可合并
   （客户端尚未消费）。
2. **B 收敛**：`Session.adoptEpoch` + 接入 `resume`/`readItems` 两条路径 +
   Rust 无改动 + 前端测试 12/15。此期结束 A 的一半已修（内容会清对了）。
3. **A 解禁**：`syncState` + 提示条 + 发送门槛 + `cachedAt` 按会话清除 +
   前端测试 13（`syncing` 部分）/ 14。
4. **C 前提**：`turn/start` 的 `contextAnchor` + `stale_context` + Rust 测试
   9-11 + 前端测试 13（`stale_context` 部分）。
5. **文档同步**：修正 `2026-10-11` §5.C（§6.5），更新
   `docs/project-management/yi-agent-app-server.md` 的协议面。