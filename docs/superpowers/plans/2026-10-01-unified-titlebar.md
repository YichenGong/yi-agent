# 统一顶部标题栏配色（macOS Overlay 标题栏）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端窗口顶部一线与侧边栏同色（#171717 + `border-neutral-800`），借助 macOS Overlay 标题栏把原生浅色标题栏替换为自绘拖拽条。

**Architecture:** 窗口层用 Tauri 的 `titleBarStyle: "Overlay"` 把系统标题栏变成覆盖在内容上的透明层（红绿灯保留、标题隐藏），并设 `backgroundColor: "#171717"` 兜底；前端在最外层加一条通栏 `TitleBar` 组件（`data-tauri-drag-region` + 侧边栏同色）承担拖拽与配色。根容器由「左右两列」改为「上（标题条）+ 下（左右两列）」的外列布局。

**Tech Stack:** Tauri 2.11（Rust 窗口配置 + 能力/权限）、React 19 + TypeScript、Tailwind CSS v4、Vitest + @testing-library/react（jsdom）。

## Global Constraints

- 工作区：本计划在 worktree `.worktrees/feat/macos-overlay-titlebar`（分支 `feat/macos-overlay-titlebar`）内执行。**严禁在 `main` 上改动**。
- 只在 `desktop/` 下改动；**不得**触碰 `yi-agent-rs/`（TUI）。
- Node/npm 不在默认 PATH：每条命令前加 `export PATH="/opt/homebrew/bin:$PATH"`。
- 沙箱禁止写系统临时目录，Vitest 必须带 `TMPDIR="$(pwd)/.tmp"`（在 `desktop/` 下先 `mkdir -p .tmp`）。
- Commit：conventional commits、首行 ≤72 字符、**不写** `Co-Authored-By` 行。
- 基线（改动前已确认）：`cd desktop && TMPDIR="$(pwd)/.tmp" npx vitest run` → 19 文件 / **170 passed**；`npx tsc --noEmit` 通过。
- 配色常量：侧边栏面色 `bg-neutral-900`、边界色 `border-neutral-800`（与 `ThreadSidebar.tsx:266`、`StatusBar.tsx:19` 完全一致）；窗口底色 `#171717`（= `bg-neutral-900`）。

---

### Task 1: macOS Overlay 窗口配置 + 拖拽权限

**Files:**
- Modify: `desktop/src-tauri/tauri.conf.json`（`app.windows[0]` 对象内，现有 `"title"` / `"width"` / `"height"` 之后）
- Modify: `desktop/src-tauri/capabilities/default.json`（`permissions` 数组末尾）

**Interfaces:**
- Consumes: 无。
- Produces: 一个允许 `data-tauri-drag-region` 生效的能力集（`core:window:allow-start-dragging`），供 Task 2 的 `TitleBar` 组件使用；一套 macOS Overlay 窗口外观（`titleBarStyle` / `hiddenTitle` / `backgroundColor` / `theme`）。

- [ ] **Step 1: 在 `tauri.conf.json` 的窗口配置里加 4 个字段**

把 `desktop/src-tauri/tauri.conf.json` 的 `app.windows[0]` 由：

```json
      {
        "title": "Yi-Agent",
        "width": 800,
        "height": 600
      }
```

改为：

```json
      {
        "title": "Yi-Agent",
        "width": 800,
        "height": 600,
        "titleBarStyle": "Overlay",
        "hiddenTitle": true,
        "backgroundColor": "#171717",
        "theme": "Dark"
      }
```

- `titleBarStyle: "Overlay"` — 仅 macOS：标题栏变透明覆盖层，系统红绿灯保留。
- `hiddenTitle: true` — 隐藏标题栏文字（Overlay 下会落到内容上）。
- `backgroundColor: "#171717"` — 窗口/图板底色，防首帧白闪；透明标题栏区露出的即此色。
- `theme: "Dark"` — Windows/macOS 10.14+ 生效，顺带把 Windows/Linux 的原生标题栏变深色。

- [ ] **Step 2: 在 `capabilities/default.json` 的 `permissions` 里加拖拽权限**

把该文件 `permissions` 数组由：

```json
  "permissions": [
    "core:default",
    "opener:allow-open-url",
    "opener:allow-default-urls",
    "dialog:allow-open"
  ]
```

改为：

```json
  "permissions": [
    "core:default",
    "core:window:allow-start-dragging",
    "opener:allow-open-url",
    "opener:allow-default-urls",
    "dialog:allow-open"
  ]
```

证据：`core:window:default`（经 `core:default` 引入）的权限清单里**没有** `allow-start-dragging`
（见 `desktop/src-tauri/gen/schemas/acl-manifests.json` 的 `core:window.default_permission`），
而 `data-tauri-drag-region` 依赖 `start_dragging` 命令，缺它会直接拖不动窗口。

- [ ] **Step 3: 验证两个 JSON 合法且字段就位**

Run:
```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/macos-overlay-titlebar/desktop && \
python3 -c "
import json
c=json.load(open('src-tauri/tauri.conf.json'))['app']['windows'][0]
assert c['titleBarStyle']=='Overlay', c
assert c['hiddenTitle'] is True, c
assert c['backgroundColor']=='#171717', c
assert c['theme']=='Dark', c
cap=json.load(open('src-tauri/capabilities/default.json'))['permissions']
assert 'core:window:allow-start-dragging' in cap, cap
print('config OK:', {k:c[k] for k in ('titleBarStyle','hiddenTitle','backgroundColor','theme')})
print('permissions OK:', cap)
"
```

Expected: 打印 `config OK: {...}` 与 `permissions OK: [...]`，无 `AssertionError`。

- [ ] **Step 4:（可选，需 Rust 工具链）确认配置能被 Tauri 解析**

Run: `cd desktop/src-tauri && cargo check`
Expected: 编译通过（本步不读取窗口配置，仅保证改动未破坏工程；配置正确性由 Step 3 的断言保证）。

- [ ] **Step 5: Commit**

```bash
git add desktop/src-tauri/tauri.conf.json desktop/src-tauri/capabilities/default.json
git commit -m "feat(desktop): overlay the macOS title bar and allow window dragging"
```

---

### Task 2: `TitleBar` 通栏拖拽条 + App 接线 + 文档同步

**Files:**
- Create: `desktop/src/components/TitleBar.tsx`
- Create: `desktop/src/components/TitleBar.test.tsx`
- Modify: `desktop/src/App.tsx:379-441`（`return (...)` 的根容器与两列包裹）
- Modify: `docs/project-management/desktop.md`（Features 增一条 + 末尾「验证命令」计数）
- Modify: `docs/project-management/README.md:28`（desktop 计数）

**Interfaces:**
- Consumes: Task 1 的 `core:window:allow-start-dragging` 权限（无它则 `data-tauri-drag-region` 无效）。
- Produces: 无导出 API 供后续任务消费；`<TitleBar />` 为无 props 组件，`export function TitleBar(): JSX.Element`。

- [ ] **Step 1: 先写失败的测试 `desktop/src/components/TitleBar.test.tsx`**

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { TitleBar } from "./TitleBar";

afterEach(cleanup);

describe("TitleBar", () => {
  it("exposes a Tauri drag region so the window can be moved", () => {
    const { container } = render(<TitleBar />);
    expect(container.querySelector("[data-tauri-drag-region]")).not.toBeNull();
  });

  it("uses the sidebar surface color so the top strip blends in", () => {
    const { container } = render(<TitleBar />);
    const bar = container.querySelector<HTMLElement>("[data-tauri-drag-region]")!;
    expect(bar).not.toBeNull();
    expect(bar.className).toContain("bg-neutral-900");
    expect(bar.className).toContain("border-b");
  });
});
```

- [ ] **Step 2: 跑测试确认失败（模块不存在）**

Run:
```bash
export PATH="/opt/homebrew/bin:$PATH" && cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/macos-overlay-titlebar/desktop && mkdir -p .tmp && TMPDIR="$(pwd)/.tmp" npx vitest run src/components/TitleBar.test.tsx
```
Expected: FAIL —— 报错形如 `Failed to resolve import "./TitleBar"`（模块尚不存在）。

- [ ] **Step 3: 创建 `desktop/src/components/TitleBar.tsx`**

```tsx
/**
 * Full-width title-bar strip for the macOS Overlay window.
 *
 * With `titleBarStyle: "Overlay"` the native title bar becomes a transparent
 * layer over the content, so this element supplies both the surface behind the
 * traffic lights and the drag region that moves the window. It deliberately
 * reuses the sidebar's surface color (`bg-neutral-900` + `border-b
 * border-neutral-800`) so the top strip and the sidebar read as one surface.
 */
export function TitleBar() {
  return (
    <div
      data-tauri-drag-region
      className="h-8 shrink-0 border-b border-neutral-800 bg-neutral-900"
    />
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run:
```bash
export PATH="/opt/homebrew/bin:$PATH" && cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/macos-overlay-titlebar/desktop && TMPDIR="$(pwd)/.tmp" npx vitest run src/components/TitleBar.test.tsx
```
Expected: PASS —— `2 passed`。

- [ ] **Step 5: 接线到 `desktop/src/App.tsx`**

在文件顶部 import 区（`StatusBar` 一行附近）加入：

```tsx
import { TitleBar } from "./components/TitleBar";
```

把 `return (...)` 中的根容器由「仅左右两列」改为「上标题条 + 下左右两列」。
原代码（`desktop/src/App.tsx:381` 起）：

```tsx
      <div className="flex h-screen flex-row bg-neutral-950 text-neutral-100">
        <ThreadSidebar
```

新代码（注意：在 `ThreadSidebar` 前插入 `<TitleBar />` 与内层两列容器，并在文件末尾为新增的
内层 `div` 补一个闭合标签）：

```tsx
      <div className="flex h-screen flex-col bg-neutral-950 text-neutral-100">
        <TitleBar />
        <div className="flex min-h-0 flex-1 flex-row">
        <ThreadSidebar
```

同时把该 `return` 的收尾由：

```tsx
        </div>
      </div>
    </>
  );
```

改为（多一层内层两列的闭合）：

```tsx
        </div>
        </div>
      </div>
    </>
  );
```

要点：
- 标题条**通栏**（跨侧边栏与对话列），其左段正好在红绿灯下方，于是红绿灯浮于 #171717 之上，
  与侧边栏连成一片。
- `min-h-0` 是新外列的必需项：否则内层 `flex-1` 的滚动区无法在列方向收缩。
- 侧边栏与对话列内部**无需加内边距**：红绿灯落在 32px 高的标题条行内（y 0–32），
  侧边栏内容从 y=32 开始，`+ New thread`（`ThreadSidebar.tsx:269` 已是 `p-2`）不会被遮挡。

- [ ] **Step 6: 跑全量前端测试 + 类型检查 + 构建**

Run:
```bash
export PATH="/opt/homebrew/bin:$PATH" && cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/macos-overlay-titlebar/desktop && TMPDIR="$(pwd)/.tmp" npx vitest run && npx tsc --noEmit && npm run build
```
Expected: `Test Files 20 passed (20)` / `Tests 172 passed (172)`；`tsc` 无输出；`npm run build` 成功产出 `dist/`。

- [ ] **Step 7: 同步模块文档 `docs/project-management/desktop.md`**

在 Features 列表最后一个 `[x]` 条目之后、`**打包交付缺口：**` 之前，插入：

```markdown
- [x] macOS Overlay 标题栏 + 通栏拖拽条（窗口顶部一线与侧边栏同色 #171717，保留系统红绿灯，窗口可拖拽）— 窗口配置 `desktop/src-tauri/tauri.conf.json:14`（窗口对象；含 `titleBarStyle: "Overlay"` / `hiddenTitle: true` / `backgroundColor: "#171717"` / `theme: "Dark"`）；拖拽权限 `desktop/src-tauri/capabilities/default.json`（`core:window:allow-start-dragging`；`core:window:default` 不含该项，见 `desktop/src-tauri/gen/schemas/acl-manifests.json`）；组件 `desktop/src/components/TitleBar.tsx:12`（`data-tauri-drag-region` + `bg-neutral-900` + `border-b border-neutral-800`）；接线 `desktop/src/App.tsx:381`（根容器改 `flex-col`，`TitleBar` 通栏置于左右两列之上）；验证 `cd desktop && npx vitest run src/components/TitleBar.test.tsx`（回退 `data-tauri-drag-region` 或 `bg-neutral-900` 即失败）；见 [设计](../superpowers/specs/2026-10-01-unified-titlebar-design.md)
```

并更新文件末尾「验证命令」里的计数与清单：
- `（170 个前端单测：` → `（172 个前端单测：`
- 在清单中 `.../ModeChip.test.tsx` 17 之后加一段：` + `desktop/src/components/TitleBar.test.tsx` 2`

- [ ] **Step 8: 同步模块索引 `docs/project-management/README.md:28`**

把：

```markdown
| desktop | 23 / 37 | [详情](./desktop.md) |
```

改为：

```markdown
| desktop | 24 / 38 | [详情](./desktop.md) |
```

- [ ] **Step 9: Commit**

```bash
git add desktop/src/components/TitleBar.tsx desktop/src/components/TitleBar.test.tsx desktop/src/App.tsx docs/project-management/desktop.md docs/project-management/README.md
git commit -m "feat(desktop): add a sidebar-colored title strip for the overlay bar"
```

---

### Task 3: 整分支验收（手动）

**Files:** 无新增/修改。

**Interfaces:**
- Consumes: Task 1 + Task 2 的全部产物。
- Produces: 通过/不通过的验收结论，供 `finishing-a-development-branch` 使用。

- [ ] **Step 1: 朝真实窗口跑一次 dev**

Run:
```bash
export PATH="/opt/homebrew/bin:$PATH" && cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/macos-overlay-titlebar/desktop && npm run tauri dev
```

逐项确认：
1. 窗口顶部一线与左侧边栏**同色无缝**（不存在浅色横条）；
2. 左上角红绿灯**可见、位置正常**，标题文字已隐藏；
3. 拖动标题条空白处能移动窗口；双击标题条能最大化/还原；
4. 顶部状态行、侧边栏、输入区既有配色未受影响。

- [ ] **Step 2: 记录结论**

把观察结果写入本计划的勾选项下方（或 PR 描述）：若第 1–4 项全部通过则本分支可合并；
若 `backgroundColor` 在透明标题栏下未生效，按设计文档「风险」节追加
`"transparent": true` + `app.macOSPrivateApi: true` 后重跑 Step 1（并同步更新
`docs/project-management/desktop.md` 的对应条目）。
