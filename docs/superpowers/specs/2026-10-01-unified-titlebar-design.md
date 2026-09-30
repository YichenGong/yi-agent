# 统一顶部标题栏配色（macOS Overlay 标题栏）设计

日期：2026-10-01
状态：待实现
模块：[desktop（Tauri GUI）](../../project-management/desktop.md)

## 问题

桌面端窗口用的是 **macOS 原生标题栏**（`desktop/src-tauri/tauri.conf.json`
的窗口配置里没有任何 `titleBarStyle` / `decorations`，Tauri 2 默认
`titleBarStyle: "Visible"`）。原生标题栏跟随**系统外观**：系统为浅色时它就是一条
浅灰/白条，直接压在应用深色内容之上。

应用内容本身是深色的：

- 根容器 `bg-neutral-950`（#0a0a0a）— `desktop/src/App.tsx:381`
- 侧边栏 `bg-neutral-900`（#171717）+ `border-r border-neutral-800`
  — `desktop/src/components/ThreadSidebar.tsx:266`
- 顶部状态行 `bg-neutral-900` + `border-b border-neutral-800`
  — `desktop/src/components/StatusBar.tsx:19`

于是窗口最顶部那条**浅色原生标题栏**与下方的深色界面（侧边栏 #171717）割裂，
观感上"顶部一节颜色和侧边栏对不上"。注意：应用内那条状态行本来就已是 #171717，
与侧边栏同色——真正需要处理的是**原生标题栏**这一条。

## 目标

让窗口顶部一线与侧边栏**同色连成一体**（#171717、下边界 `border-neutral-800`），
保留 macOS 原生红绿灯按钮，且窗口仍可拖动。

## 非目标

- 不改侧边栏、状态行、输入区等既有配色（它们已统一）。
- 不自绘红绿灯/最小化/关闭按钮，不接管系统按钮。
- 不做 Windows/Linux 的自定义无边框标题栏（仅用深色主题纠正亮条问题，见下）。
- 不改 TUI（`yi-agent-rs/`）。

## 方案

macOS 原生 **Overlay** 标题栏：系统标题栏变成覆盖在内容之上的透明层，标题文字
隐藏，系统红绿灯保留并浮在左上角；我们在同一位置铺设一条与侧边栏同色的拖拽条。

### 1. 窗口配置 `desktop/src-tauri/tauri.conf.json`

主窗口新增：

```json
"titleBarStyle": "Overlay",
"hiddenTitle": true,
"backgroundColor": "#171717",
"theme": "Dark"
```

- `titleBarStyle: "Overlay"` — 仅 macOS；标题栏变透明覆盖层，保留系统红绿灯。
- `hiddenTitle: true` — 隐藏标题栏文字（Overlay 下若显示会落到应用内容上）。
- `backgroundColor: "#171717"` — 设置窗口/图板背景色，避免首帧白闪；
  Overlay 下系统标题栏区域透明，露出的即此色。
- `theme: "Dark"` — Windows/macOS 10.14+ 生效，将原生标题栏转为深色，
  修掉 Windows/Linux 上同样的"亮条压深色"问题（代价极小，顺带收益）。

### 2. 权限 `desktop/src-tauri/capabilities/default.json`

新增权限 `"core:window:allow-start-dragging"`。

证据：`core:window:default` 的权限列表（`src-tauri/gen/schemas/acl-manifests.json`）
**不含** `allow-start-dragging`（只有 `allow-get-all-windows`、`allow-inner-size`、
`allow-title` 等只读项）。Tauri 的 `data-tauri-drag-region` 需要调用
`start_dragging` 命令，缺权限则点击标题栏无法拖动窗口。

### 3. 新组件 `desktop/src/components/TitleBar.tsx`

一条通栏拖拽条：

- `data-tauri-drag-region`（Tauri 据此识别可拖拽区域）
- `h-8`（32px，容纳红绿灯）、`shrink-0`
- `bg-neutral-900` + `border-b border-neutral-800`（与侧边栏/状态行同色同边）
- 不渲染标题文字

### 4. 布局接线 `desktop/src/App.tsx`

把根容器由「`flex h-screen flex-row`（仅左右两列）」改为外列：

```jsx
<div className="flex h-screen flex-col bg-neutral-950 text-neutral-100">
  <TitleBar />                                   {/* 通栏拖拽条，高 32px */}
  <div className="flex min-h-0 flex-1 flex-row">
    <ThreadSidebar … />
    <div className="relative flex min-w-0 flex-1 flex-col">…</div>
  </div>
</div>
```

- 标题栏**通栏**（跨侧边栏与对话列），因此其左段就在红绿灯下方，
  红绿灯浮于 #171717 之上，观感与侧边栏一体。
- `min-h-0` 是新外列必须的：否则内层 `flex-1` 的滚动区无法收缩。
- 侧边栏/对话列内部**无需额外留白**：红绿灯落在 32px 高的标题栏行内（y 0–32），
  侧边栏内容从其下方（y=32）开始，`+ New thread` 不会被遮挡（
  `ThreadSidebar.tsx:269` 已是 `p-2`，纵向余量足够）。

## 备选方案（未采用）

- **仅 `Transparent` 标题栏**：标题栏仍独立成行，只改背景色；改动更小但
  标题栏与内容仍是两层，且 macOS 对 `backgroundColor` 作用于透明标题栏的
  着色行为不确定，需现场试。
- **自绘无边框窗口**（`decorations: false` + 自绘红绿灯）：控制最彻底，
  但要自己实现最小化/最大化/关闭与三平台差异，成本与风险显著更高。

## 验证

- 单测（新增 `desktop/src/components/TitleBar.test.tsx`）：
  渲染出带 `data-tauri-drag-region` 的元素，且 class 含 `bg-neutral-900` 与
  `border-b`（锁住"与侧边栏同色"这一回归点）。
- 回归：`cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
  （基线 170 passed）。
- 手动：`cd desktop && npm run tauri dev`，确认
  ① 顶部一线与侧边栏同色、无浅色缝隙；
  ② 红绿灯可见、位置正常；
  ③ 拖动标题栏空白处可移动窗口、双击可最大化。

## 风险

- **Tauri Overlay 已知限制**：窗口未获得焦点时无法拖动
  （上游 issue tauri-apps/tauri#4316），属于框架层限制，接受。
- **首帧**：`backgroundColor` 减轻白闪；若某些环境下仍闪，后续可加
  `"noRedirectionBitmap": true`（Windows）。
- **回滚**：移除 `TitleBar` 组件与 App 接线、撤销 3 个窗口字段与 1 条权限即可，
  不涉及数据与协议。
