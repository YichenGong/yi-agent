# 可拖拽侧边栏 + 应用显示名设计与实现方案

- 日期：2026-09-28
- 状态：设计定稿，待实现
- 模块：`desktop`（Tauri GUI）
- 关联：`docs/project-management/desktop.md`、`desktop/README.md`

## 1. 背景与现状

`desktop/` 是 yi-agent 的原生桌面应用（Tauri 2.x + React 19 + TS + Tailwind v4）。

两处待改进：

**（1）侧边栏宽度写死。** 历史侧栏 `desktop/src/components/ThreadSidebar.tsx:148`
根节点为 `<aside className="flex w-64 shrink-0 ...">`，宽度固定 16rem（256px），
用户无法调整。`desktop/src` 全目录**无任何拖拽/分栏实现**：搜 `onMouseDown` /
`mousemove` / `resiz` 均无（唯一 `resize` 命中是输入框上的 `resize-none` 工具类），
`package.json` 也无分栏库。窗口默认 800×600（`desktop/src-tauri/tauri.conf.json:16`），
窄屏下固定 256px 侧栏挤占聊天区。

**（2）应用显示名不统一。** 用户可见字符串里：

| 位置 | 文件:行 | 当前值 |
|---|---|---|
| HTML `<title>` | `desktop/index.html:7` | `Tauri + React + Typescript`（模板残留默认值） |
| Tauri 窗口标题 | `desktop/src-tauri/tauri.conf.json:15` | `yi-agent` |
| Tauri `productName` | `desktop/src-tauri/tauri.conf.json:3` | `yi-agent` |

运行进程在 Activity Monitor / 菜单栏显示为 `desktop`，但该名称源自 **Rust crate 名**
（`desktop/src-tauri/Cargo.toml:2` 的 `desktop`），**本次不改**（见 §4 非目标）。

## 2. 已确认的设计决策

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | 「可拖拽」含义 | **拖动侧栏与聊天区之间的竖直分隔条改变宽度**（非移动位置、非仅折叠） |
| 2 | 宽度持久化 | **持久化到 `localStorage`**，重启恢复 |
| 3 | 状态归属 | **收在 `ThreadSidebar.tsx` 内部**，`App.tsx` 不改（侧栏自持宽度，内聚） |
| 4 | 交互实现 | `onMouseDown` + **document 级 `mousemove`/`mouseup`**（经典 splitter；jsdom 下 `fireEvent` 可测，pointer capture 在 jsdom 是桩不可测） |
| 5 | 持久化时机 | **`mouseup` 时写一次**，而非每次 `mousemove` 都写 |
| 6 | 改名范围 | **仅用户可见字符串**：`index.html` 的 `<title>` 与窗口标题 |
| 7 | 显示大小写 | 显示用 `Yi-Agent`；不改任何 identifier / 包名 |
| 8 | `productName` | **保持 `yi-agent` 不改**（见 §5） |

## 3. 设计

### 3.1 可拖拽侧边栏

**新增 `desktop/src/lib/sidebarWidth.ts`（纯逻辑，便于单测）：**

- 常量：`DEFAULT_SIDEBAR_WIDTH = 256`、`MIN_SIDEBAR_WIDTH = 200`、
  `MAX_SIDEBAR_WIDTH = 480`、`STORAGE_KEY = "yi-agent.sidebarWidth"`。
- `clampSidebarWidth(px: number): number`：非有限值（`NaN`/`Infinity`）回退默认值；
  否则夹到 `[MIN, MAX]`。
- `loadSidebarWidth(): number`：从 `localStorage` 读取并 `parseFloat`，
  经 `clampSidebarWidth` 归一；读取抛错（受限环境）或缺失时返回默认值。
- `saveSidebarWidth(px: number): void`：`localStorage.setItem`，包 try/catch，
  写入前先 `clampSidebarWidth`。

**`desktop/src/components/ThreadSidebar.tsx` 改动：**

- `const [width, setWidth] = useState(loadSidebarWidth);`——**同步初始化**，
  避免首帧用默认值渲染后再跳到持久值（闪一下）。
- `<aside>` 由固定 `w-64` 改为 `style={{ width }}`，并加 `relative` 定位。
- 右边缘锚定拖拽手柄：

  ```tsx
  <div
    role="separator"
    aria-orientation="vertical"
    aria-label="Resize sidebar"
    onMouseDown={onHandleDown}
    className="absolute inset-y-0 right-0 z-30 w-1.5 cursor-col-resize hover:bg-neutral-700/50"
  />
  ```

- 拖拽接线：`onMouseDown` 记录 `startX`/`startWidth`，向 `document` 挂
  `mousemove`/`mouseup`；`mousemove` 中 `setWidth(clampSidebarWidth(startWidth + (e.clientX - startX)))`；
  `mouseup` 时移除监听、恢复 `body` 样式并 `saveSidebarWidth`。拖动期间设
  `document.body.style.cursor = "col-resize"` 与 `userSelect = "none"`，松开还原。
- 手柄用 `useRef` 持有「解绑函数」以便组件卸载时清理残留监听。

### 3.2 应用显示名

- `desktop/index.html:7`：`<title>Tauri + React + Typescript</title>` → `<title>Yi-Agent</title>`。
- `desktop/src-tauri/tauri.conf.json:15`：窗口 `"title": "yi-agent"` → `"title": "Yi-Agent"`。

## 4. 非目标（YAGNI）

- 键盘方向键调宽侧栏。
- 窗口缩小时自动回夹宽度（不加 `window.resize` 监听）。
- 双击分隔条复位到默认宽度。
- 改名 Rust crate / npm 包名 / bundle identifier（`desktop`、`com.gongyichen.desktop` 保持）。
- 不改 `productName`（见 §5）。

## 5. 关于 `productName` 的取舍

`productName` 当前为 `yi-agent`，本就不是 `desktop`，故不属于用户抱怨的对象。
若改成 `Yi-Agent`，产物 bundle 会从 `yi-agent.app` 变为 `Yi-Agent.app`，牵动
`desktop/README.md` 与 `docs/project-management/desktop.md` 中所有构建验证命令与
路径（`desktop.md:47` 等）。收益（菜单栏与窗口标题完全一致）小于改动面，故保持不动。
最终：菜单栏显示 `yi-agent`，窗口标题显示 `Yi-Agent`。

## 6. 测试

**新增 `desktop/src/lib/sidebarWidth.test.ts`（jsdom）：**
- `clampSidebarWidth`：区间内原样返回；小于 `MIN` → `MIN`；大于 `MAX` → `MAX`；
  `NaN`/`Infinity` → 默认值。
- `loadSidebarWidth`：无存储 → 默认值；存脏值（`"abc"`/`""`）→ 默认值；
  存越界值 → 夹取后的值；存合法值 → 原样。
- `saveSidebarWidth` 后 `loadSidebarWidth` 往返一致。

**扩 `desktop/src/components/ThreadSidebar.test.tsx`：**
- 手柄存在且带 `role="separator"` / `aria-orientation="vertical"`。
- `mouseDown` 手柄 → `document` 上 `mousemove` 增大 `clientX` → `<aside>` 宽度变大。
- 拖到极左/极右 → 宽度被夹到 `MIN`/`MAX`。
- `mouseup` 后 `localStorage` 存了夹取后的值；`mouseup` 后 `mousemove` 不再改宽度。
- `localStorage` 预置值 → 挂载时 `<aside>` 即为该宽度。
- `afterEach` 中 `localStorage.clear()`。

**验证命令：** `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`。

## 7. 文档同步（CLAUDE.md 要求）

- `docs/project-management/desktop.md`：新增一条 `[x]` Feature（可拖拽侧栏，带
  `file:line` 判据）；更新「验证命令」段与前端单测计数（80 → 新数）。
- `desktop/README.md`：测试计数与「Verification status」段同步。
- `README.md`：模块索引计数若变化则同步。
