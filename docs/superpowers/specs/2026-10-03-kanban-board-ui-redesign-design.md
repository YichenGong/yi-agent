# Superpowers 看板：四列主视图 Design

日期：2026-10-03
状态：设计已定稿，待用户 review 后进入 writing-plans。
注：§4.7 中「侧栏点任意会话回对话」当时只落地了卡片链接路径，侧栏路径遗漏；「收起看板仍走
现有 `SuperpowersKanbanCollapsedBar`」一句已作废。二者均由
`2026-10-04-kanban-board-as-thread-page-design.md` 修正/取代。
范围：桌面端（`desktop/`）纯前端。不动插件、不动宿主 RPC、不改调度/状态机。

## 1. 目标与问题

桌面端看板现在是**叠在对话上方的单列列表**（`max-h-[60%]` 的面板）：`SuperpowersKanbanView`
把卡片按 `order` 平铺成一行一行。两个问题：

1. **看不出"在途 vs 完成 vs 待我"**：所有卡片混在一个列表里，一屏看不出队列压力，也看不出
   哪些卡在等你。
2. **卡片信息几乎为空**：卡片只画出 `id` 与 `state` 两个有值的字段。

**本设计**：把主区域改成**四列看板**（queued / doing / need decision / done），覆盖对话区，
让"排队多少、在跑什么、什么在等我、最近完成了什么"一屏可读。

### 1.1 一个必须顺手修的缺陷（本设计的一部分）

第 2 点不是渲染取舍，是**数据链断了**：

- 插件 `list`（`plugins/superpowers-kanban/.../service.rs::list`）返回的卡片只有
  `id / state / spec_path / plan_path / workdir / thread_id / kind / source / base`——
  **没有** `progress`，也**没有** `detail`。
- 桌面侧 `desktop/src/lib/superpowersKanbanSwitch.ts::fetchBoard` 直接把插件返回的
  `cards` 当作 `BoardCardDto[]` 交给 App；`App.tsx` 再把它塞进 `SuperpowersKanbanView`。
  于是 `card.progress` 恒为 `undefined`、`card.detail` 恒为 `undefined`。
- 真正把插件原始卡归一化（推导 `detail`、`threadId`、`state` 小写）的
  `desktop/src/lib/superpowersKanbanState.ts::parseBoard` **没有被 App 引用**，只被它自己的
  单测引用（死代码）。

所以现在卡上只有一长串 `id` 和 `state`。本设计**把 `parseBoard` 那条归一化链接进 App**，
并扩展归一化以支撑新卡片（标题、分支等）。这是纯桌面侧改动，插件与宿主协议不变。

## 2. 范围

**做：**

- 四列看板主视图，覆盖主区域的对话区（ChatView + MessageInput）。
- 状态 → 列的映射（§3），含未知状态的安全回退。
- done 列默认折叠、可展开。
- need decision 列视觉强调。
- 卡片内容充实（标题、id、状态徽标、会话链接）。
- 接上归一化链（§1.1）：`normalizeCard` + `parseBoard` 复用。
- 提交 `docs/superpowers/specs/`、`docs/superpowers/plans/` 与代码。

**不做（明确非目标）：**

- 不做卡片写操作（取消 / 确认合并 / 归档 / 改优先级）。这些有真实后果且归档功能尚未落地，
  作为**后续独立设计**。
- 不做拖拽换列、不做列宽拖拽、不做列排序自定义。
- 不持久化 done 折叠状态（每次进入重置为折叠）。
- 不改 TUI（`/superpowers-kanban` 仍是一次性斜杠命令）。
- 不改插件（`plugins/superpowers-kanban/`）、不改宿主 RPC（`board.*` / `plugin/query`）、
  不改状态机、不改调度器。
- 不改侧栏（`ThreadSidebar` 的看板条目、项目右键菜单）与看板设置/入队组件的行为。
- 不新增跨项目的总览看板。

## 3. 列映射（状态 → 列）

插件 `CardState` 有 10 个状态，列只有 4 个。映射如下（§7 测试逐条覆盖）：

| 列 | 收录状态 | 理由 |
| --- | --- | --- |
| **queued** | `queued`、`launching`、`paused` | 都还没在跑：`launching` 是起会话的瞬间，`paused` 是停着的队列成员 |
| **doing** | `running`、`merging` | 正在动用资源（会话或合并进程） |
| **need decision** | `needs_you`、`awaiting_merge` | 在途未完成，但**只有人能推进** |
| **done** | `done`、`failed`、`cancelled` | 三个终态，不再有自动推进 |

**未知状态 → `doing`。** 判据是保守可见：一个未来新增/拼错的状态若丢进 done，用户会把它当
"完成了"而漏掉；放进 doing 最多是"还在跑"，不会造成误判。**绝不把未知状态放进 done。**

`need decision` 单列的理由（用户明确要求）：`needs_you` / `awaiting_merge` **不占并发、也没跑完，
但没有人的动作就永远不走**。按归档设计，这两个状态**永不自动归档**——若混进 done，它们会永远
赖在已完成列里，而用户恰恰是"不能时刻盯屏"的管理者，这是最不能漏的一类。

## 4. 布局与交互

### 4.1 覆盖范围

选中看板且未收起时，主区域渲染看板，**盖住 ChatView 与 MessageInput**；顶部 `TitleBar` 与
底部 `StatusBar` 保留（状态栏的 cwd / 模型 / 用量仍有参考价值）。这与现状的差异只是
"覆盖整块主区域"而非"占上方 60% 后对话仍在下面"。

### 4.2 看板顶栏

从 `App.tsx` 现有那段看板 `<section>` 拆出一个 `SuperpowersKanbanBoardShell`，顺序为：

1. 项目路径（当前是哪个项目的看板；`title` 属性给全路径，显示做中间省略）。
2. `SuperpowersKanbanSettings`（开关 / 收起 / 值守），行为不变。
3. `SuperpowersKanbanEnqueue`（加入看板），行为不变。
4. `boardError` 提示行（含"创建看板"按钮），行为不变。

下面接四列区。

### 4.3 四列

- 四列**等宽**（`grid grid-cols-4`），各自纵向滚动（列内有独立滚动区）。
- 列头：列名 + 计数，如 `DOING · 3`；计入**该列全部卡片**（done 列计数含被折叠掉的卡）。
- 卡片数量为 0 时，列**不塌陷**，显示空占位（保持四列骨架稳定）。
- **窄屏（`isMobile`）**：四列宽度收窄，整条四列区**横向滚动**，不改成单列堆叠——
  堆叠会让"四列"的语义消失。

### 4.4 done 列折叠

- 默认只显示**最近 5 张**（按完成时刻倒序；`done/failed/cancelled` 混合排）。
- 列头一个「展开全部 / 收起」开关，展开后列出全部，再点收回到 5 张。
- **不持久化**：每次进入看板重置为折叠。
- 完成时刻来源：见 §5 的 `normalizeCard`——用插件已有的 `terminal_at`（归档设计引入）优先，
  没有则回退 `enqueued_at`；两者都缺则排到该列末尾。

> 注：`terminal_at` 是 `2026-10-03-board-card-archive-design.md` 引入的字段。若该设计尚未落地，
> 本设计**不阻塞**：回退到 `enqueued_at` 只是排序精度下降，不影响功能。实现时按"字段存在则用"
> 处理（可选字段，缺失不报错）。

### 4.5 need decision 列

整列视觉强调：列头与卡片用警示色（红/琥珀，沿用既有 `text-red-400` 一类的 token），
卡片额外带一个感叹标记。目的是一眼扫到"有东西在等我"。

### 4.6 卡片内容

一张卡从上到下：

- **标题**：由 `spec_path` 的文件名派生（去扩展名，如 `2026-10-03-board-smoke.spec.md` →
  `2026-10-03-board-smoke`）；无 `spec_path` 时回退到 `id`。
- **id**：`font-mono`，中间省略（`truncate` 到中段，头尾可见），`title` 给全量。
- **状态徽标**：`state` 原文（小写）；`running` 时若将来有进度则并排显示，本轮无进度来源，
  故只显示状态。
- **会话链接**：有 `threadId` 时渲染按钮，点击回调 `onOpenThread(threadId)`；无则不渲染。
- **分支**（可选）：合并卡可显示 `source → base`；实现卡本轮不显示 worktree，避免噪音。

### 4.7 退出

看板与会话是主区域的**两个平级视图**：侧栏点「Superpowers 看板」进看板，点任意会话回对话。
**点卡片上的会话链接 = 切到那个会话、离开看板**（回到会话后再点侧栏回看板）。不引入"在看板里
内嵌开会话 + 返回"的多层导航状态。收起看板仍走现有 `SuperpowersKanbanCollapsedBar`。

## 5. 数据流与组件边界

每个单元单一职责，可独立测试。

**`superpowersKanbanState.ts`（改，纯函数）**

- 新增 `normalizeCard(raw: unknown): BoardCard | null`：把插件原始卡对象归一化成
  `BoardCard`，含：
  - `id` / `state`（小写）校验，缺任一则返回 `null`（沿用现有丢弃规则）。
  - 新增可选字段：`specPath` / `planPath` / `workdir` / `kind` / `source` / `base` /
    `terminalAt`，以及派生的 `title`。
  - `detail` 回退链沿用现状（`workdir` → 合并卡 `source → base` → `planPath`）。
- `parseBoard(json)` 改为复用 `normalizeCard`（逐个卡调用），**对外签名与既有测试不变**。

**`superpowersKanbanBoard.ts`（新，纯函数）**

- `boardColumns(cards: BoardCard[]): { queued: Card[]; doing: Card[]; needDecision: Card[];
  done: Card[] }`：按 §3 映射分列，并做列内排序：
  - queued：`order` 升序（FIFO；无 `order` 则保持输入序）。
  - doing：按 `enqueued_at` 升序（先开始的在上）。该列只有 `running`/`merging`，
    没有"需人推进"的状态（那些在 need decision 列）。
  - need decision：`needs_you` 置顶（要人回话比要人合并更急），其余按 `enqueued_at`。
  - done：完成时刻倒序（`terminalAt` → `enqueuedAt` 回退）。
- `DONE_COLLAPSED_LIMIT = 5`；`collapseDone(cards, expanded): Card[]` 取前 N 或全部。

**`SuperpowersKanbanView.tsx`（重写）**

- Props 保持兼容（`switchOn` / `source` / `cards` / `pluginMissing` / `onOpenThread`）。
- done 展开状态由 **`App.tsx` 持有**（`doneExpanded` + `onToggleDone`），不放在 View 内部
  state：这样"切换看板时重置"只需 App 一处逻辑，View 保持无状态、易测。
- 保持四种提示态的输出（`pluginMissing` / `!switchOn` / 空 / 正常），文案沿用或等价，
  使既有测试的意图继续成立（见 §7 测试迁移）。

**`SuperpowersKanbanColumn.tsx`（新，展示组件）**

- 单列：列头（名 + 计数 + done 列的折叠开关）+ 卡片列表 + 空占位。
- 不碰 RPC、不碰映射，只画它拿到的卡。

**`SuperpowersKanbanCard.tsx`（新，展示组件）**

- 单卡渲染（§4.6）。只回调 `onOpenThread`，无写操作。

**`App.tsx`（改）**

- 主区域：看板 `<section>` 改为覆盖 ChatView + MessageInput 的分支（条件渲染，二者互斥）。
- 新增 `doneExpanded` state（本地，不持久化），进入/切换看板时重置。
- 保留：`selectedBoard` / `boardCollapsed` / 2 秒轮询 / CollapsedBar 逻辑不变。

## 6. 边界情况

- **插件缺失 / 开关关 / 空看板 / daemon 不可达**：整块提示（沿用现有四类文案），
  **不画空四列**。
- **旧 `board.json`**：卡片缺 `kind`/`source`/`base` 时按现有默认（实现卡、空串）处理；
  缺 `spec_path` 时标题回退到 `id`。
- **未知状态**（§3）：归 doing。
- **同一状态大量卡片**：列内滚动，不撑破主区域。
- **done 列超长**：默认折叠为 5 张，展开后列内滚动。

## 7. 测试

**单元（vitest，纯函数）：**

- `boardColumns`：每个已知状态归到预期列（10 个状态逐条）；未知状态归 doing；
  四条列内排序规则；done 折叠取前 5 / 展开取全部。
- `normalizeCard` / `parseBoard`：插件原始 JSON → 卡片字段与 `title` 派生；
  `detail` 回退（workdir → 合并卡 refs → plan_path）；缺 `spec_path` 标题回退 id；
  既有 `superpowersKanbanState.test.ts` 全部保持通过（签名不变）。

**组件（jsdom + @testing-library）：**

- 四列渲染、列头计数、空列占位。
- done 默认 5 张与展开开关。
- need decision 列的强调标记。
- 点会话链接触发 `onOpenThread`；无 thread 的卡不渲染链接。
- 四种提示态（插件缺失 / 关闭 / 空 / 正常）。

**既有测试迁移：** `SuperpowersKanbanView.test.tsx` 现有 6 条断言按新 DOM 调整选择器，
但**意图逐条保留**（能画出卡片字段、disabled 有说明、空有说明、pluginMissing 有说明、
点会话打开会话、无 thread 不渲染链接）。此为测试适配，不是能力回退。

**回归：** 桌面端既有 30 个测试文件全部通过；`npx tsc --noEmit` 通过。

## 8. 交付物

1. 本 spec（`docs/superpowers/specs/2026-10-03-kanban-board-ui-redesign-design.md`）。
2. 后续 `writing-plans` 产出的实施计划。
3. 桌面端代码改动（§5 的单元）与其测试。

## 9. 遗留（后续独立设计，本轮不做）

- 卡片写操作：取消 / 确认合并 / 归档 / 改优先级（依赖归档设计落地 + 需要确认流）。
- done 折叠状态持久化、列宽/列排序自定义。
- 进度来源：若将来插件提供任务进度（如 `3/7 tasks`），卡片增加进度条。
