# 设计：侧栏「运行中会话数」指示器

日期：2026-10-18
状态：已评审
关联：`docs/superpowers/specs/2026-10-17-thread-color-tags-design.md`（同为桌面端小改动，
复用其「纯函数 + 组件接线 + 同 commit 更新模块文档」的落地范式）；
`docs/superpowers/specs/2026-09-30-desktop-sidebar-running-animation-design.md`（同为侧栏状态呈现层
改动，本设计复用其 `useSpinPhase` 旋转机制）

## 1. 背景与问题

会话可以并发运行：侧栏每行都有状态徽标，正在跑的会话显示一个旋转的环。但当同时有
多个会话在跑时，用户必须**逐行扫视**才能知道到底有几个——侧栏一屏之外的行会滚动
出视野，分组折叠起来更是完全看不见。没有一处给出「此刻全局有几个会话在跑」的汇总。

用户要的是一个一眼可读的数字（形如「3 个会话运行中」），放在设置入口旁边。

### 现状事实（已核对代码）

1. **状态已在客户端，且是全局的。** `App.tsx:1888` 每轮渲染都从 `ThreadStore`
   重建一张 `statuses: Map<thread_id, ThreadStatus>`，覆盖 `sess.groups` 里**全部
   workspace 的全部会话**。侧栏徽标（`ThreadSidebar.tsx` 的 `StatusBadge`）就读这张
   表。故「有几个在跑」**已经可算**，不需要新的数据通路。
2. **状态的实时性是既有保证，不是本功能引入的。** `thread/status/updated`
   （`protocol.rs:250`）的投递层级是 `Delivery::List`（`protocol.rs:505` 的注释）：
   「列表层……**恒推**（列表要实时，与订阅无关）」。快照侧由 `thread/listAll` 携带
   `status` 补齐（`thread_summary_json`，`server.rs:3484`）。所以数字既能即时跟随
   推送变化，也能在冷启动 / 重连后由列表快照重新落值。
3. **`ThreadStatus` 的取值是三分。** `desktop/src/lib/generated/ThreadStatus.ts`：
   `"idle" | "running" | "awaiting_approval"`。`failed` 刻意缺席——失败是事件不是
   状态，失败后立刻回 `idle`。
4. **设置入口就在侧栏 footer。** `ThreadSidebar.tsx:1100` 是 footer 容器
   （`mt-auto flex items-center border-t border-line p-2`），`ThreadSidebar.tsx:1101`
   起是设置齿轮按钮。footer 在桌面与手机端**都渲染**（同一段 JSX，无 `isMobile` 分支；
   `isMobile` 只影响搜索栏位置等处，见 `:1056/:1058`）。
5. **旋转动画有现成的、相位锚定墙钟的实现。** `useSpinPhase`
   （`desktop/src/lib/useSpinPhase.ts`）配 `StatusBadge` 使用，解决「行重排导致 CSS
   动画从头开始（视觉回退）」与「running ↔ awaiting_approval 翻转时时间轴断裂」两个
   问题。本功能的新指示器要复用同一机制，避免自己写一个会闪的 spinner。
6. **「过期」有既有的表达。** 手机端离线时 `App.tsx:2041` 传 `statusStale={offline}`，
   徽标据此降透明度并加「（可能过期）」title。本指示器沿用这一语义。
7. **纯前端可测的既有范式。** 展示层的折叠 / 派生逻辑放在 `desktop/src/lib/*.ts`
   的纯模块里、与组件分开测（`lib/processes.ts:1-12` 顶部注释明确写了这条理由）。
   本功能的计数逻辑照此落位。

## 2. 目标与非目标

### 目标

1. 侧栏 footer 里、设置齿轮**旁边**，显示当前**全局**正在运行的会话数，形如
   `3 个会话运行中`。
2. 「运行中」严格等于 `status === "running"`；`awaiting_approval` 与 `idle` 都不计入。
3. 计数为 0 时**整个指示器不渲染**——footer 与改动前逐像素一致，不出现常驻的
   「0 个会话运行中」噪音。
4. 数字跟随 `thread/status/updated` 推送**即时**变化，无需重连或手动刷新。
5. 手机端离线（`statusStale`）时按既有约定标注「可能过期」，不假装数字是新的。
6. 计数逻辑是纯函数，可脱离 React 单测。

### 非目标

1. **不动协议、不动 Rust 后端。** 不加 RPC（如 `thread/countActive`），不加通知字段。
   前端已收全量状态推送，服务端算只会多一次网络往返换一个更旧的数字。
2. **不数后台进程、不数子 agent。** 它们**按会话隔离**（`ProcessRail` 的
   `processes` 来自 `proc.refreshProcesses(sess.currentId)`，只覆盖当前会话；`SubagentRail`
   同理），做全局计数没有现成数据源，需要新增后端聚合。本设计不做。
3. **不做点击交互。** 指示器是只读呈现，不可点击、不筛会话、不跳转。列表面板已有
   自己的入口（状态栏的「会话详情面板」按钮）。
4. **不改侧栏徽标的语义或样式。** 徽标仍对 `running` 与 `awaiting_approval` 都显示；
   本指示器只数前者。两者含义不同是**有意**的（见 §3.1）。
5. **不引入 `ThreadStore` 新方法。** 计数由已有的 `statuses` 表派生，不新增第二条
   数据路径去 store 里再读一遍（两条路径会漂移）。
6. **不做历史统计 / 峰值 / 图表。** 只有「此刻」这一个数。

## 3. 设计

### 3.1 语义：什么算「运行中」

```
countRunningSessions(statuses) = |{ s ∈ statuses.values() : s === "running" }|
```

三个取值与指示器的关系：

| status | 侧栏徽标 | 计入本指示器 | 理由 |
|---|---|---|---|
| `running` | 旋转的环 | 是 | 模型正在推进，正是用户要数的东西 |
| `awaiting_approval` | 琥珀色静默点 | 否 | 会话在**等人**，没在跑 |
| `idle` | 不渲染 | 否 | 无回合 |

因此本指示器的数字**不等于**侧栏转圈徽标数——审批中的会话有徽标但不计数。这是选定的
语义（用户明确选择「只算 running，不算停在审批窗口的」），不是实现缺陷。

**已知副作用（用户已接受）。** 权限提示会让状态在 `running → awaiting_approval → running`
之间来回切（`ThreadSidebar.tsx:28-30` 的注释即描述这条翻转）。默认权限模式下每次需要
审批时，计数会**先掉 1、批准后再涨回**。存在可见抖动。消除抖动的唯一办法是把
`awaiting_approval` 也算进来（即回到「活跃」定义），但那与选定的语义相悖，故不采纳。

范围是**全部 workspace 的会话**，跨项目全局。数字的落点是「同时有几个在跑」，与当前
选中哪个项目无关。

### 3.2 数据流

```
thread/status/updated (Delivery::List, 恒推) ─┐
thread/listAll 快照的 status 字段 ───────────┼─→ ThreadStore.views[*].status
                                              │        │
                                              │        ▼
                                              │   App.tsx:1888 每轮渲染重建
                                              │   statuses: Map<thread_id, ThreadStatus>
                                              │        │
                                              │        ├─→ ThreadSidebar 的 StatusBadge（既有）
                                              │        └─→ countRunningSessions() → 新指示器
```

没有任何新数据源：指示器读的就是徽标读的那张表。因此有一条**可断言的不变量**：
指示器的数字恒等于 `statuses` 里值为 `"running"` 的条目数，即侧栏里正在转圈的徽标数。

### 3.3 组件与落点

**新增 `desktop/src/lib/runningSessions.ts`**

```ts
import type { ThreadStatus } from "./protocol";

/**
 * 「此刻有几个会话真的在跑」——严格 `running`。
 *
 * `awaiting_approval` 不计：那种会话在等人，没在干活（与侧栏徽标的语义刻意不同，
 * 徽标对两者都显示）。idle 自然不计。
 *
 * 入参是侧栏徽标用的同一张表（App 每轮渲染由 ThreadStore 重建），故无第二条数据路径。
 */
export function countRunningSessions(statuses: ReadonlyMap<string, ThreadStatus>): number {
  let n = 0;
  for (const s of statuses.values()) if (s === "running") n += 1;
  return n;
}
```

纯函数、无 React 依赖、无副作用。另导出一个文案构造函数便于组件与测试共用：

```ts
/** 文案：`3 个会话运行中`。与 §2 目标 1 的措辞一致。 */
export function runningSessionsLabel(n: number): string {
  return `${n} 个会话运行中`;
}
```

**改 `desktop/src/components/ThreadSidebar.tsx`**

新增一个内部组件 `ActiveSessionCount`，渲染在 footer 里、齿轮的**左侧**（齿轮位置不动）。
props 只要两个：`count: number`（由 `countRunningSessions(statuses)` 在 `ThreadSidebar`
函数体内算出）与 `stale?: boolean`（沿用既有 `statusStale`）。

- `count === 0` → 返回 `null`（目标 3）。
- `count > 0` → 渲染一个小胶囊：一个复用 `useSpinPhase` 的旋转小环（与 `StatusBadge`
  的 running 变体同源：`size-3` / `border-2` / `border-line-strong` / `border-t-fg`，
  写死为 running 形态——它只在有会话运行时出现，不存在 `awaiting_approval` 形态）
  + `text-xs text-fg-subtle` 的文案。
- `stale` → 与 `StatusBadge` 同一约定：`opacity-40` + title 后缀「（可能过期）」。

无障碍：容器 `role="status"` + `aria-live="polite"`，数字变化时读屏播报；`aria-label`
用同一句文案，保证测试能用可访问名定位（与既有「设置」按钮的写法一致）。

**为什么在 `ThreadSidebar` 内而不是 `App` 里算**

`statuses` 已经作为 prop 传进 `ThreadSidebar`（`App.tsx:2022`），在组件体内调用纯函数
是零接线改动。若在 `App` 里另算一遍再往下传，等于把同一个数算两次、多一个 prop 要
维护，且两处可能不一致（`useMemo`/依赖写错就漂移）。计数是侧栏展示层的一部分，
归属就在侧栏。

### 3.4 桌面与手机

footer 在两端都渲染，故指示器两端都出现。**不特殊对待手机端**：并发会话信息在手机上
同样真实，抽屉底部的这颗胶囊与桌面端语义一致。手机端离线时经 `statusStale` 降透明度，
与徽标一致。

（若后续只想在桌面显示，加一个 `isMobile` 判断即可；本设计不做这个分支，避免为没有
需求依据的差异写代码。）

### 3.5 错误处理与边界

- **未连接 / 握手前**：`statuses` 为空表，计数 0，指示器不渲染。不显示「连接中」之类
  的过渡态——那属于状态栏（`StatusBar` 的连接点）的职责，不是本指示器的。
- **`ThreadStore` 里没有该 thread 的 view**：`App.tsx:1889` 的既有循环 `if (!v) continue`
  跳过，故不会进表、不计入。行为与徽标一致。
- **侧车崩溃重连**：`ThreadStore.forgetPushStatuses()`（`App.tsx:1295`）清掉「来自推送」
  的运行状态标记，随后 `thread/listAll` 快照重新落值。数字会随之复位到快照的真相，
  不残留崩溃前的假 `running`。
- **并发删除会话**：`store.drop(id)` 移除 view，下一轮渲染表里就没有它，计数自动回落。
- **布局**：`count === 0` 时不渲染，故不存在「0 与 3 之间宽度跳变」；从 0 变 1 时胶囊
  出现会占据宽度，但 footer 是 `flex`、齿轮在右侧独立定位，齿轮不会被推走。

### 3.6 测试

**`desktop/src/lib/runningSessions.test.ts`（新）**

- 空表 → 0。
- 全是 `idle` → 0。
- 混 `idle` / `awaiting_approval` / `running` → 只数 running（例如
  `[idle, running, awaiting_approval, running, idle]` → 2）。
- 全是 `awaiting_approval` → 0（**这条是本设计语义的核心断言**：审批中的会话不计）。
- 全是 `running` → 等于表大小。
- `runningSessionsLabel(3) === "3 个会话运行中"`。

**`desktop/src/components/ThreadSidebar.test.tsx`（加）**

- 传入含 2 个 `running` 的 `statuses` → `screen.getByRole("status")` 存在，文案含
  `2 个会话运行中`。
- 传入全 `idle` 的 `statuses` → 查询不到该节点（`queryByRole("status")` 为 null，
  或按可访问名查询为 null），且 footer 仍渲染「设置」按钮（确认 0 时只是少了胶囊，
  没有连带影响）。
- 传入 `statusStale` → 胶囊带「可能过期」的 title / 降透明度类。

**既有测试不被破坏**：`ThreadSidebar.test.tsx:950` 的「offers a settings button in the
footer」按可访问名 `设置` 定位齿轮；新胶囊用不同的 `aria-label`（`N 个会话运行中`），
不抢占该名字。`ThreadSidebar.test.tsx:42` 的 `sidebarProps` 已把 `statuses` 默认成
`new Map()`，新增用例只需 `renderSidebar({ statuses: ... })` 覆写，其余用例零改动
（默认空表 → 计数 0 → 不渲染，与改动前逐像素一致）。`App.test.tsx:2078` 断言 iOS 首启配对页**没有**「设置」按钮——那条路径
根本不渲染侧栏，故不受影响。

### 3.7 验证命令

```
cd desktop && npx vitest run src/lib/runningSessions.test.ts src/components/ThreadSidebar.test.tsx && npx tsc --noEmit
```

## 4. 影响面

- **前端**：新增 `desktop/src/lib/runningSessions.ts`；改
  `desktop/src/components/ThreadSidebar.tsx`（footer 加一个内部组件 + 一行调用）。
  无新 prop、无 `App.tsx` 改动。
- **后端**：无。协议、RPC、通知、存储一律不变。
- **手机端**：同一份 React 代码，随之出现同一颗胶囊。离线时降透明度。
- **TUI**：不读也不画侧栏，不受影响。
- **文档**：`docs/project-management/desktop.md` 加一条 `[x]` 记录 + 判据命令；同步
  `README.md` 模块索引表的计数（若该表按条数计）。

## 5. 未决 / 留待后续

1. **抖动本身。** §3.1 的 `running → awaiting_approval → running` 抖动是选定语义的
   固有结果。若日后用户改主意想消除抖动，办法是改判定为「`running` + `awaiting_approval`」
   （即「活跃」定义），一行改动；本设计不预留开关。
2. **点击筛会话。** 目前只读。若日后要做「点一下只看正在跑的会话」，那是侧栏筛选
   功能（与 `lib/threadSearch.ts` 的既有筛选管线合并），另立设计。
3. **阈值告警。** 例如「超过 N 个在跑就变色」。当前无需求依据，不做。
