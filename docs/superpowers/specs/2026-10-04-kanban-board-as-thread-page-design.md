# Superpowers 看板：与会话平级的主区域页面

日期：2026-10-04
状态：设计已确认，待用户 review 后进入 writing-plans。
范围：桌面端（`desktop/`）纯前端。不动插件、不动宿主 RPC（`board.*` / `plugin/query`）、
不改调度与状态机。

## 1. 背景与问题

看板现在**不是**浮层，但它的导航手感像浮层。主区域在 `desktop/src/App.tsx:1266` 处按
`selectedBoard !== null && !boardCollapsed` 分支渲染：真则画看板 `<section>`，假则画
`ChatView + MessageInput`（二者互斥）。问题出在**导航模型**：

- `selectedBoard`（`App.tsx:231`）与 `currentId`（`App.tsx:231` 一带）是两套相互独立的
  状态，注释明说「看会话不动看板，看板也不动会话」。
- 侧栏点会话走 `selectThread`（`App.tsx:412`），它只改 `currentId`，**不碰 `selectedBoard`**。
- 于是：正在看某项目的看板时，点侧栏里另一条会话 → `currentId` 变了、看板仍盖在主区域，
  用户看不到刚点的会话，必须再手动点一次「收起看板」。

这与既有 spec 的既定意图不符：`2026-10-03-kanban-board-ui-redesign-design.md` §4.7 写的是
「看板与会话是主区域的两个平级视图：侧栏点『Superpowers 看板』进看板，点任意会话回对话」。
但代码只实现了后半句的**卡片链接**路径（`App.tsx:1313`，测试 `App.test.tsx:1004`），
**侧栏会话路径漏了**。

用户诉求：看板要像会话那样是一个**页面**，点另一条会话时从 UI 上直接「跳转」过去，
而不是还要先点一下收起。

## 2. 目标行为

主区域只有两个平级页面：**会话页**与**看板页**（每个登记过的项目一块看板）。侧栏是唯一
的页面导航。

1. **选中会话 → 离开看板。** 任何把某条会话切到前台的路径，都同时清掉 `selectedBoard`。
   于是点侧栏会话即「跳转」到该会话页，无需再点收起。这条由 `selectThread` 统一承载，
   覆盖：侧栏点会话、命令式开场（`App.tsx:916`）、审批横幅跳转（`App.tsx:1343`）、
   卡片会话链接（`App.tsx:1317`，已是此语义）。
2. **新建会话 → 离开看板。** `newThread`（`App.tsx:482`）不经 `selectThread`，需自行清掉
   `selectedBoard`：新建会话后焦点应在会话页，与点会话一致。
3. **进入看板 → 侧栏点该项目的「看板」条目。** `onOpenBoard`（`App.tsx:553`）已工作，
   保留。点卡片上的会话链接仍是「离开看板去会话」（已是此语义）。
4. **删除：收起/展开全套。** 既然点会话即离场，「收起看板」成为语义冗余的第二套离场方式
   （用户明确选择删除）。移除 `boardCollapsed` 状态、`SuperpowersKanbanCollapsedBar` 组件
   与测试、`SuperpowersKanbanSettings` 的 `onCollapse` prop 与「收起」按钮。

## 3. 范围

**做：**

- `selectThread` 增加 `setSelectedBoard(null)`（会话切到前台即离开看板）。
- `newThread` 增加 `setSelectedBoard(null)`。
- `onOpenBoard` 去掉 `boardCollapsed` 相关逻辑，保留「换项目重置 `boardError` / `boardCards` /
  `doneExpanded`」，并让「点已选中看板」成为纯 no-op。
- 删除 `boardCollapsed` 状态（`App.tsx:234`）。
- 删除 `SuperpowersKanbanCollapsedBar` 组件与其测试文件。
- 删除 `SuperpowersKanbanSettings` 的 `onCollapse` prop、「收起」按钮，及其测试用例。
- 更新既有测试中依赖收起/展开的用例，与涉及「点会话不动看板」的断言（见 §6）。
- 提交本 spec 与后续 plan。

**不做（明确非目标）：**

- 不改插件、不改宿主 RPC、不改调度/状态机/并发日历。
- 不改看板四列视图本身（列映射、卡片内容、done 折叠、需你处理列的强调）。
- 不改 TUI。
- 不新增第三种「离开看板」的方式（如快捷键、Esc）。
- 不持久化任何页面选择（`selectedBoard` 仍不落盘，启动默认无选中）。
- 不改侧栏看板条目/右键菜单的行为（创建、移除看板），只依赖其 `onOpenBoard` 入口。

## 4. 组件与数据流

### 4.1 `App.tsx`（改）

- **`selectThread`（`App.tsx:412`）**：在 `store.select(id)` / `setCurrentId(id)` 之前或之后
  加 `setSelectedBoard(null)`。语义是「会话页成为当前页」。该 set 幂等：`selectedBoard` 本就
  为 `null` 时是无害的 no-op，故对现有「不在看板时切会话」路径零影响。
- **`newThread`（`App.tsx:482`）**：同样加 `setSelectedBoard(null)`。
- **`onOpenBoard`（`App.tsx:553`）**：简化为：
  - `path === selectedBoard` → 直接返回（已经在该看板页，无事可做）。
  - 否则 `setBoardError(null); setBoardCards([]); setDoneExpanded(false); setSelectedBoard(path);`
    删除其中的 `setBoardCollapsed(...)` 两处。
- **主区域渲染（`App.tsx:1257-1266`）**：
  - 删除 `SuperpowersKanbanCollapsedBar` 的 import（`App.tsx:33`）与那段 `boardCollapsed &&` 分支。
  - 条件从 `selectedBoard !== null && !boardCollapsed ?` 改为 `selectedBoard !== null ?`。
  - 看板 `<section>` 内部传给 `SuperpowersKanbanSettings` 的 `onCollapse` 删除。
- **删除状态**：`const [boardCollapsed, setBoardCollapsed] = useState(false);`（`App.tsx:234`）
  及其注释（`App.tsx:232-233`）。
- **保留不动**：`selectedBoard` / `doneExpanded` / 2 秒轮询（`App.tsx:966`）/ `boardError` /
  `boardCards` / `boardPluginMissing` / 看板条目高亮。看板数据的轮询与「是否在画」无关：
  离开看板后 `selectedBoard === null`，`refreshSelectedBoard`（`App.tsx:277`）早期返回并清空
  `boardCards`；侧栏摘要仍由 `refreshBoards`（`App.tsx:248`）维持。

### 4.2 `SuperpowersKanbanSettings.tsx`（改）

- 删除 `onCollapse?: () => void` prop 及其 `<button aria-label="收起看板">收起</button>`。
- 其余（项目开关、`switchOn` / `source`、后台值守开关、`watchmanWarning`）不变。

### 4.3 `SuperpowersKanbanCollapsedBar.tsx` 与 `.test.tsx`（删除）

- 组件只服务收起态，随收起一起移除。删除两个文件。
- 注意：`boardCollapsed` 这个名字在 `ThreadSidebar.tsx:126/488` 也出现，但那是**侧栏「看板会话」
  小节的折叠状态**（`Set<string>`，键为 workspace），**与本设计无关，必须保留**，不要误删。

### 4.4 数据流

纯本地 UI 状态，无新增副作用、无新增 RPC、无写盘。

## 5. 边界情况

- **看板未选中时切会话**：`setSelectedBoard(null)` 是 no-op；会话页照常切换。
- **点卡片会话链接**：仍 `setSelectedBoard(null)` + `selectThread(id)`，行为不变（`selectThread`
  内再加一次幂等的清空无副作用）。
- **点已选中的看板条目**：no-op，停留在看板页（不再有「从收起态展开」这层）。
- **新建会话**：无论此前是否在看板，新建后落在会话页。
- **看板被移除**（`onRemoveBoard`，`App.tsx:548`）：`selectedBoard === path` 时仍 `setSelectedBoard(null)`，
  逻辑不变。
- **手机端**：侧栏是抽屉，点会话本就收起抽屉（`selectThread` 内 `setSidebarOpen(false)`）；
  增加离开看板后，抽屉关闭 + 落到会话页，语义一致。
- **删除会话把当前会话删掉**（`deleteThread`）：与看板无关，不改。

## 6. 测试

**新增（`App.test.tsx`）：**

1. **看板打开时点侧栏另一条会话 → 回到会话页**：开 `/proj` 看板，点侧栏里的一条会话行，
   断言 `screen.queryByLabelText("Superpowers 看板")` 变 `null`（看板消失），且该会话被切过去
   （`thread/resume` 已发出），**无需任何「收起」点击**。
2. **看板打开时新建会话 → 落到会话页**：开看板，触发新建会话（侧栏 New），断言看板消失。
3. **从会话回看板**：切到会话后再点侧栏「看板」条目，看板重新出现（回归「侧栏是唯一导航」）。

**改写（`App.test.tsx`）：**

- `App.test.tsx:840`「看板可收起、并可从收起横条再展开」：删除。收起已不存在；其「离开看板」
  的意图由新增的会话跳转用例覆盖。
- `App.test.tsx:827` 等断言 `screen.getByLabelText("收起看板")` 的用例：改为不再断言该按钮存在
  （或断言 `queryByLabelText("收起看板")` 为 `null`，固化「不存在收起」的新语义）。
- `App.test.tsx:811` / `:1006` 的 `queryByLabelText("收起看板")` 为 `null` 断言：保留（仍成立）。

**删除（`SuperpowersKanbanCollapsedBar.test.tsx`）：** 整文件移除。

**改写（`SuperpowersKanbanSettings.test.tsx`）：** 删除针对 `onCollapse` 的用例（传 `onCollapse`
出现「收起」按钮、不传不出现），其余开关/值守用例保留；若某用例断言的 DOM 因按钮删除而位移，
按新 DOM 调整选择器。

**回归：**

- `cd desktop && npx vitest run` 全绿（删除 `SuperpowersKanbanCollapsedBar.test.tsx` 后文件数减 1）。
- `cd desktop && npx tsc --noEmit` 通过（确认无残留 import / prop 引用）。
- `cd desktop && npm run build` 通过。

## 7. 交付物

1. 本 spec（`docs/superpowers/specs/2026-10-04-kanban-board-as-thread-page-design.md`）。
2. 后续 `writing-plans` 产出的实施计划。
3. 桌面端改动（§4）与其测试（§6）。

## 8. 对既有文档的取代关系

- **取代** `docs/superpowers/specs/2026-10-01-board-collapsible-design.md` 的「可折叠面板」模型：
  那是「看板列常驻、可收起让宽」时代的产物；看板早已从左侧列改成主区域平级视图
  （`2026-10-03-kanban-board-ui-redesign-design.md` §4.1），收起/展开随之失去意义。本设计移除它。
- **补齐** `2026-10-03-kanban-board-ui-redesign-design.md` §4.7 的「侧栏点任意会话回对话」：
  该句当时只落地了卡片链接路径，侧栏路径遗漏，本设计补上。§4.7 中「收起看板仍走现有
  `SuperpowersKanbanCollapsedBar`」一句随本设计作废。
- 上述两份文件的正文不改写（历史留档），取代关系以本条为准；实施时可在这两份文件顶部按
  仓库惯例补一行「已被 `2026-10-04-kanban-board-as-thread-page-design.md` 取代」的注记。

## 9. 遗留（本轮不做）

- 页面选择（当前是会话页还是看板页、哪个项目的看板）重启后不保留；是否需要记忆，另行决定。
- 看板页与卡片会话之间的「返回看板」快捷入口（例如会话页顶部一个「回看板」按钮）：本轮不做，
  回看板统一走侧栏条目。
