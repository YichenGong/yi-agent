# Superpowers 看板：可折叠面板

日期：2026-10-01
状态：已实施（Spec 5，桌面端 30 文件 / 263 测试通过）；**已被
`2026-10-04-kanban-board-as-thread-page-design.md` 取代**——看板改为会话的平级页面后，
「收起/展开」不再是离开看板的方式，本设计的可折叠列模型作废。


## 1. 背景

桌面端左侧有一个固定宽度（`w-72`）的看板列，里面同时放着两件事：

- `SuperpowersKanbanSettings`：Superpowers 看板的功能开关（持久写到项目层 `preferences.json`）。
- `SuperpowersKanbanView`：卡片列表（含「未启用」与「空」两种提示态）。

这个列**常驻**，占宽且不可收起。用户希望在不需要盯看板时把它缩小让出空间，需要时再打开。

现状参照：侧栏的「子 agent」面板（`SubagentRail`）已有一个「收起」按钮，但**收起后没有任何再打开的入口**——这是个既有缺陷。本设计不重蹈覆辙：收起与展开必须成对存在。

## 2. 范围

**做：**

- 桌面端看板列可折叠：展开态显示「功能开关 + 卡片列表」，收起态让出全部宽度，只在原位置留一条极窄竖条，竖条上仅有一个「展开」按钮。
- 折叠默认展开，**不持久化**。
- 功能关闭时**同样允许**折叠。

**不做（明确非目标）：**

- 不改 TUI（`/kanban` 是一次性斜杠命令，无常驻面板可收）。
- 不改 daemon、不改插件、不新增任何 RPC。
- 不持久化折叠状态（不写 `preferences.json`）。
- 不做拖拽调宽。
- 不修复「子 agent」面板「只能收不能开」的既有缺陷（独立议题）。
- 不新增卡片的创建/操作入口（见 §6 遗留）。

## 3. 行为

| 状态 | 界面 |
| --- | --- |
| 展开（默认） | 左侧 `w-72` 列：`SuperpowersKanbanSettings`（标题行右侧多一个「收起」按钮）+ `SuperpowersKanbanView` 卡片列表 |
| 收起 | 该列被一条窄竖条取代（约 `w-8`），条上只有一个「展开」按钮；中间聊天区获得全部剩余宽度 |

- 折叠是**纯界面动作**：不写盘、不通知 daemon、不影响 2 秒轮询。卡片数据照常拉取，展开后即为当前值。
- 功能开关（`switchOn`）与折叠状态**正交**：开关关着时，展开态照常显示「未启用」提示，且仍可收起；展开后开关的持久状态原样恢复。
- 每次启动 app 都默认展开（不记忆）。

## 4. 组件与数据流

按仓库既有惯例组织，每个单元单一职责。

**`SuperpowersKanbanSettings`（改）** — 新增可选 `onCollapse?: () => void`。传入时在标题行右侧渲染一个「收起」按钮并触发回调；不传时不渲染。与 `SubagentRail` 的 `onCollapse` 约定一致，不重复引入标题。

**`SuperpowersKanbanCollapsedStrip`（新增，展示组件）** — 收起态的窄竖条。渲染 `<aside aria-label="Superpowers 看板">`，内部一个 `type="button"`、`aria-label="展开看板"` 的按钮，点击调用 `onExpand`。

**`App.tsx`（改）** — 新增 `const [kanbanCollapsed, setKanbanCollapsed] = useState(false)`。把现有那段内联列改为条件渲染：

- `kanbanCollapsed === false` → `<SuperpowersKanbanSettings ... onCollapse={() => setKanbanCollapsed(true)} />` + `<SuperpowersKanbanView ... />`
- `kanbanCollapsed === true` → `<SuperpowersKanbanCollapsedStrip onExpand={() => setKanbanCollapsed(false)} />`

**`SuperpowersKanbanView`（不动）** — 仍是卡片列表（含 disabled / empty 两种文案），不掺入面板外壳逻辑。

数据流为纯本地 state，无副作用。

## 5. 边界情况与测试

**边界：**

- 空看板 / 看板未启用：`SuperpowersKanbanView` 既有文案照常显示，两种情形都允许折叠。
- 宽度：展开保持现有 `w-72`；收起竖条定窄（`w-8`），竖排文字，不影响中间聊天区布局。

**测试（vitest + jsdom，沿用现有写法）：**

1. `SuperpowersKanbanSettings.test.tsx`：传 `onCollapse` 时出现「收起」按钮并回调一次；不传时不出现。
2. `SuperpowersKanbanCollapsedStrip.test.tsx`（新）：渲染「展开看板」按钮，点击回调一次。
3. `App` 级：点击收起后列消失、竖条出现；点展开后恢复。

## 6. 遗留（本次不做，另行决定）

- 桌面端**没有创建卡片的入口**。`desktop/src/lib/superpowersKanbanSwitch.ts` 里的 `enqueueBoardCard()` 目前**未接到任何组件**（死代码）；卡片目前只能经 TUI `/superpowers-kanban add <spec> <plan>` 入队。是否补桌面端入口，见既有 spec 的 §7 / §9.1「卡片交互操作」。
- 「子 agent」面板收起后无法再打开（既有缺陷，与本设计同构，可复用 `SuperpowersKanbanCollapsedStrip` 同样的竖条方案）。

## 7. 命名对齐（2026-10-01 更新）

本 spec 早于命名统一（Spec 1）。组件名已对齐为 `SuperpowersKanbanView` / `SuperpowersKanbanSettings` / `SuperpowersKanbanCollapsedStrip`，state 为 `kanbanCollapsed`。实施顺序上本项排在 Spec 1 之后。
