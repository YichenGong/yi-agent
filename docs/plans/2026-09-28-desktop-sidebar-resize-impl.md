# 可拖拽侧边栏 + 应用显示名 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 `desktop` 应用的历史侧栏可拖动分隔条改变宽度并持久化；把用户可见的应用名统一为 `Yi-Agent`。

**Architecture:** 宽度状态与拖拽逻辑全部内聚在 `ThreadSidebar.tsx` 内，`App.tsx` 不改。纯计算（夹取/读写 localStorage）抽到 `src/lib/sidebarWidth.ts` 以便无 DOM 单测。拖拽用 `onMouseDown` + document 级 `mousemove`/`mouseup`（jsdom 可测），在 `mouseup` 时写一次 `localStorage`。

**Tech Stack:** React 19 + TypeScript + Tailwind CSS v4 + Vitest + @testing-library/react (jsdom) + Tauri 2。

**工作区：** 本计划在 worktree `.worktrees/feat/desktop-sidebar-resize`（分支 `feat/desktop-sidebar-resize`）内执行。所有命令先 `cd` 到该 worktree 的 `desktop/` 目录。

**设计文档：** `docs/plans/2026-09-28-desktop-sidebar-resize-design.md`

---

## 前置约定

- 每个 Task 遵循 TDD：先写失败测试 → 跑测试确认失败 → 最小实现 → 跑测试确认通过 → commit。
- 单测命令统一：`cd <worktree>/desktop && npx vitest run <路径>`。
- Commit message 用 conventional commits，**不写 `Co-Authored-By`**。
- 本改动不涉及 Rust，**不要跑 cargo**。

---

### Task 1: `clampSidebarWidth` 纯函数

**Files:**
- Create: `desktop/src/lib/sidebarWidth.ts`
- Test: `desktop/src/lib/sidebarWidth.test.ts`

**Step 1: 写失败测试**

创建 `desktop/src/lib/sidebarWidth.test.ts`：

```ts
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import {
  DEFAULT_SIDEBAR_WIDTH,
  MIN_SIDEBAR_WIDTH,
  MAX_SIDEBAR_WIDTH,
  clampSidebarWidth,
} from "./sidebarWidth";

afterEach(() => localStorage.clear());

describe("clampSidebarWidth", () => {
  it("returns an in-range value unchanged", () => {
    expect(clampSidebarWidth(300)).toBe(300);
  });

  it("clamps below MIN up to MIN", () => {
    expect(clampSidebarWidth(10)).toBe(MIN_SIDEBAR_WIDTH);
  });

  it("clamps above MAX down to MAX", () => {
    expect(clampSidebarWidth(9999)).toBe(MAX_SIDEBAR_WIDTH);
  });

  it("falls back to the default for non-finite input", () => {
    expect(clampSidebarWidth(NaN)).toBe(DEFAULT_SIDEBAR_WIDTH);
    expect(clampSidebarWidth(Infinity)).toBe(DEFAULT_SIDEBAR_WIDTH);
    expect(clampSidebarWidth(-Infinity)).toBe(DEFAULT_SIDEBAR_WIDTH);
  });
});
```

**Step 2: 跑测试确认失败**

Run: `cd <worktree>/desktop && npx vitest run src/lib/sidebarWidth.test.ts`
Expected: FAIL — 无法解析模块 `./sidebarWidth`。

**Step 3: 最小实现**

创建 `desktop/src/lib/sidebarWidth.ts`：

```ts
/** Sidebar width persisted to localStorage, in CSS pixels. */
export const DEFAULT_SIDEBAR_WIDTH = 256;
export const MIN_SIDEBAR_WIDTH = 200;
export const MAX_SIDEBAR_WIDTH = 480;
export const SIDEBAR_WIDTH_STORAGE_KEY = "yi-agent.sidebarWidth";

/**
 * Normalize a candidate width: non-finite values (NaN/±Infinity) fall back to the
 * default, everything else is clamped into [MIN, MAX].
 */
export function clampSidebarWidth(px: number): number {
  if (!Number.isFinite(px)) return DEFAULT_SIDEBAR_WIDTH;
  return Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, px));
}
```

**Step 4: 跑测试确认通过**

Run: `cd <worktree>/desktop && npx vitest run src/lib/sidebarWidth.test.ts`
Expected: PASS（4 tests）。

**Step 5: Commit**

```bash
cd <worktree>
git add desktop/src/lib/sidebarWidth.ts desktop/src/lib/sidebarWidth.test.ts
git commit -m "feat(desktop): add sidebar width clamp helper"
```

---

### Task 2: `loadSidebarWidth` / `saveSidebarWidth`

**Files:**
- Modify: `desktop/src/lib/sidebarWidth.ts`
- Test: `desktop/src/lib/sidebarWidth.test.ts`

**Step 1: 追加失败测试**

在 `desktop/src/lib/sidebarWidth.test.ts` 顶部 import 增补 `loadSidebarWidth, saveSidebarWidth, SIDEBAR_WIDTH_STORAGE_KEY`，并追加 describe：

```ts
describe("loadSidebarWidth / saveSidebarWidth", () => {
  it("returns the default when nothing is stored", () => {
    expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
  });

  it("returns the default for a corrupt stored value", () => {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "abc");
    expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "");
    expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
  });

  it("clamps an out-of-range stored value", () => {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "9999");
    expect(loadSidebarWidth()).toBe(MAX_SIDEBAR_WIDTH);
  });

  it("round-trips a saved value", () => {
    saveSidebarWidth(300);
    expect(loadSidebarWidth()).toBe(300);
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe("300");
  });

  it("clamps before saving", () => {
    saveSidebarWidth(1);
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(String(MIN_SIDEBAR_WIDTH));
  });
});
```

（将 `SIDEBAR_WIDTH_STORAGE_KEY` 加入文件的 import 列表。）

**Step 2: 跑测试确认失败**

Run: `cd <worktree>/desktop && npx vitest run src/lib/sidebarWidth.test.ts`
Expected: FAIL — `loadSidebarWidth` / `saveSidebarWidth` is not a function。

**Step 3: 最小实现**

在 `desktop/src/lib/sidebarWidth.ts` 末尾追加：

```ts
/** Read the persisted width, clamped. Falls back to the default if absent/corrupt/unavailable. */
export function loadSidebarWidth(): number {
  try {
    const raw = localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY);
    if (raw === null) return DEFAULT_SIDEBAR_WIDTH;
    return clampSidebarWidth(parseFloat(raw));
  } catch {
    // localStorage can throw in restricted contexts; never let that break rendering.
    return DEFAULT_SIDEBAR_WIDTH;
  }
}

/** Persist the width (clamped). Storage failures are non-fatal. */
export function saveSidebarWidth(px: number): void {
  try {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, String(clampSidebarWidth(px)));
  } catch {
    // Ignore: losing the persisted width is not worth breaking the drag.
  }
}
```

**Step 4: 跑测试确认通过**

Run: `cd <worktree>/desktop && npx vitest run src/lib/sidebarWidth.test.ts`
Expected: PASS（9 tests）。

**Step 5: Commit**

```bash
cd <worktree>
git add desktop/src/lib/sidebarWidth.ts desktop/src/lib/sidebarWidth.test.ts
git commit -m "feat(desktop): persist sidebar width to localStorage"
```

---

### Task 3: `ThreadSidebar` 应用宽度 + 渲染拖拽手柄

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Step 1: 写失败测试**

在 `desktop/src/components/ThreadSidebar.test.tsx`：

1. 把文件顶部 `afterEach(cleanup);` 改为：

```ts
afterEach(() => {
  cleanup();
  localStorage.clear();
});
```

2. 追加 describe：

```ts
import { DEFAULT_SIDEBAR_WIDTH, SIDEBAR_WIDTH_STORAGE_KEY } from "../lib/sidebarWidth";

const aside = (container: HTMLElement) => container.querySelector("aside")!;

describe("ThreadSidebar width", () => {
  it("renders the drag handle with separator semantics", () => {
    const { container } = renderSidebar();
    const handle = container.querySelector('[role="separator"]')!;
    expect(handle).not.toBeNull();
    expect(handle.getAttribute("aria-orientation")).toBe("vertical");
  });

  it("defaults to the default width", () => {
    const { container } = renderSidebar();
    expect(aside(container).style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH}px`);
  });

  it("restores the persisted width on mount", () => {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "320");
    const { container } = renderSidebar();
    expect(aside(container).style.width).toBe("320px");
  });
});
```

**Step 2: 跑测试确认失败**

Run: `cd <worktree>/desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL — 无 `[role="separator"]`；aside 无 `width` inline style。

**Step 3: 最小实现**

修改 `desktop/src/components/ThreadSidebar.tsx`：

1. import 行补充（`useRef` 与 helper）：

```tsx
import { useCallback, useEffect, useRef, useState } from "react";
```

（第 1 行由 `import { useCallback, useEffect, useState } from "react";` 改为上面。）

2. 在第 3 行 import 之后新增：

```tsx
import { clampSidebarWidth, loadSidebarWidth, saveSidebarWidth } from "../lib/sidebarWidth";
```

3. 组件内（第 51 行 `const [contextWs, setContextWs] = ...` 之后）新增状态：

```tsx
  const [width, setWidth] = useState(loadSidebarWidth);
  const widthRef = useRef(width);
  const cleanupDrag = useRef<(() => void) | null>(null);
```

4. 把 `<aside ...>` 开标签（第 148 行）改为：

```tsx
    <aside
      className="relative flex shrink-0 flex-col border-r border-neutral-800 bg-neutral-900"
      style={{ width }}
    >
```

5. 在 `</aside>`（第 300 行）之前、列表 `</div>`（第 299 行）之后插入手柄：

```tsx
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
        className="absolute inset-y-0 right-0 z-30 w-1.5 cursor-col-resize hover:bg-neutral-700/50"
      />
```

（本 Task 不接 `onMouseDown`，拖拽在 Task 4 实现。）

**Step 4: 跑测试确认通过**

Run: `cd <worktree>/desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（原 7 + 新 3 = 10 tests）。

**Step 5: Commit**

```bash
cd <worktree>
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): render sidebar width from persisted state"
```

---

### Task 4: 拖动改变宽度（含夹取）

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 追加：

```ts
import { fireEvent } from "@testing-library/react";
import { MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH } from "../lib/sidebarWidth";

const handle = (container: HTMLElement) =>
  container.querySelector<HTMLElement>('[role="separator"]')!;

function drag(container: HTMLElement, toClientX: number, fromClientX = 0) {
  fireEvent.mouseDown(handle(container), { clientX: fromClientX });
  fireEvent.mouseMove(document, { clientX: toClientX });
  fireEvent.mouseUp(document);
}

describe("ThreadSidebar drag-resize", () => {
  afterEach(() => {
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
  });

  it("grows the sidebar when dragging right", () => {
    const { container } = renderSidebar();
    drag(container, 100);
    expect(aside(container).style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH + 100}px`);
  });

  it("clamps to MIN when dragging far left", () => {
    const { container } = renderSidebar();
    drag(container, -10000);
    expect(aside(container).style.width).toBe(`${MIN_SIDEBAR_WIDTH}px`);
  });

  it("clamps to MAX when dragging far right", () => {
    const { container } = renderSidebar();
    drag(container, 10000);
    expect(aside(container).style.width).toBe(`${MAX_SIDEBAR_WIDTH}px`);
  });

  it("stops resizing after mouseup", () => {
    const { container } = renderSidebar();
    drag(container, 50);
    fireEvent.mouseMove(document, { clientX: 400 });
    expect(aside(container).style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH + 50}px`);
  });
});
```

**Step 2: 跑测试确认失败**

Run: `cd <worktree>/desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL — 宽度不随拖动变化（照旧 `256px`）。

**Step 3: 最小实现**

在 `ThreadSidebar.tsx` 中：

1. 在 `cleanupDrag` ref 声明之后新增 `onHandleDown` 与卸载清理 effect：

```tsx
  const onHandleDown = (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = widthRef.current;
    const prevCursor = document.body.style.cursor;
    const prevUserSelect = document.body.style.userSelect;

    const onMove = (ev: MouseEvent) => {
      const next = clampSidebarWidth(startWidth + (ev.clientX - startX));
      widthRef.current = next;
      setWidth(next);
    };

    const cleanup = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = prevCursor;
      document.body.style.userSelect = prevUserSelect;
      cleanupDrag.current = null;
    };

    const onUp = () => {
      cleanup();
      saveSidebarWidth(widthRef.current);
    };

    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    cleanupDrag.current = cleanup;
  };

  // Remove any lingering document listeners if we unmount mid-drag.
  useEffect(() => () => cleanupDrag.current?.(), []);
```

2. 给手柄补上 `onMouseDown`：

```tsx
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
        onMouseDown={onHandleDown}
        className="absolute inset-y-0 right-0 z-30 w-1.5 cursor-col-resize hover:bg-neutral-700/50"
      />
```

**Step 4: 跑测试确认通过**

Run: `cd <worktree>/desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（14 tests）。

**Step 5: Commit**

```bash
cd <worktree>
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): drag the sidebar divider to resize"
```

---

### Task 5: `mouseup` 持久化

**Files:**
- Test: `desktop/src/components/ThreadSidebar.test.tsx`
- （实现已在 Task 4 的 `onUp` 中完成，本 Task 用测试锁定该行为）

**Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 追加：

```ts
describe("ThreadSidebar persistence", () => {
  it("saves the clamped width on mouseup", () => {
    const { container } = renderSidebar();
    drag(container, 10000); // clamps to MAX
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(String(MAX_SIDEBAR_WIDTH));
  });

  it("does not write during the drag (only on release)", () => {
    const { container } = renderSidebar();
    fireEvent.mouseDown(handle(container), { clientX: 0 });
    fireEvent.mouseMove(document, { clientX: 80 });
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBeNull();
    fireEvent.mouseUp(document);
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(
      String(DEFAULT_SIDEBAR_WIDTH + 80),
    );
  });

  it("restores a width dragged to MIN on the next mount", () => {
    const first = renderSidebar();
    drag(first.container, -10000);
    first.unmount();
    const second = renderSidebar();
    expect(aside(second.container).style.width).toBe(`${MIN_SIDEBAR_WIDTH}px`);
  });
});
```

**Step 2: 跑测试确认通过（行为已在 Task 4 实现）**

Run: `cd <worktree>/desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（17 tests）。

> 若「does not write during the drag」失败，说明实现里在 `mousemove` 中误调了
> `saveSidebarWidth`；把它挪回 `onUp` 即可。

**Step 3: Commit**

```bash
cd <worktree>
git add desktop/src/components/ThreadSidebar.test.tsx
git commit -m "test(desktop): lock sidebar width persistence on mouseup"
```

---

### Task 6: 应用显示名改为 `Yi-Agent`

**Files:**
- Modify: `desktop/index.html:7`
- Modify: `desktop/src-tauri/tauri.conf.json:15`

**Step 1: 改 HTML 标题**

`desktop/index.html` 第 7 行：

```html
    <title>Yi-Agent</title>
```

**Step 2: 改窗口标题**

`desktop/src-tauri/tauri.conf.json` 第 15 行：

```json
        "title": "Yi-Agent",
```

**Step 3: 验证**

Run:
```bash
cd <worktree>/desktop
grep -n "Yi-Agent" index.html src-tauri/tauri.conf.json
npx tsc --noEmit
npx vitest run
```
Expected: 两处命中 `Yi-Agent`；`tsc` 无输出；全部单测通过。
（`productName` 保持 `yi-agent`，identifier 保持 `com.gongyichen.desktop`，不动。）

**Step 4: Commit**

```bash
cd <worktree>
git add desktop/index.html desktop/src-tauri/tauri.conf.json
git commit -m "chore(desktop): show Yi-Agent as the app display name"
```

---

### Task 7: 文档同步 + 最终验证

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `desktop/README.md`
- Modify: `README.md`（仅当模块索引计数变化）

**Step 1: 更新 `docs/project-management/desktop.md`**

- 在 `## Features` 列表**末尾**新增一条 `[x]`（判据用实际行号，落地后回填）：

```markdown
- [x] 可拖拽侧边栏宽度（拖动右侧分隔条调宽、夹到 [200, 480]、`localStorage` 持久化、重启恢复）— `desktop/src/lib/sidebarWidth.ts:8`（`clampSidebarWidth`）/ `desktop/src/lib/sidebarWidth.ts:23`（`loadSidebarWidth` / `saveSidebarWidth`）/ `desktop/src/components/ThreadSidebar.tsx`（`onHandleDown` + `style={{ width }}` + `role="separator"` 手柄）；显示名统一为 `Yi-Agent` — `desktop/index.html:7` / `desktop/src-tauri/tauri.conf.json:15`；验证 `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
```

- 更新文件末尾「**验证命令：**」段的计数：前端单测数从 80 改为实际值（新增 `sidebarWidth.test.ts` 9 + `ThreadSidebar.test.tsx` +10），并把 `ThreadSidebar.test.tsx` 后的数字同步。

**Step 2: 更新 `desktop/README.md`**

- 「Verification status」段：把「Frontend unit tests (`npm test`, 34 tests)」的实际计数更新为当前值（先跑 `npm test` 看输出，再回填）。
- 若该段提到 UI 能力列表，可在合适处补一句侧栏可拖拽调宽。

**Step 3: 更新 `README.md` 模块索引**

- 若 `README.md` 模块索引表里 `desktop` 行的「完成 / 总计」计数受影响（新增 1 条 feature），同步数字。

**Step 4: 最终验证**

Run:
```bash
cd <worktree>/desktop
npx vitest run
npx tsc --noEmit
npm run build
```
Expected: 全部单测通过（预计 99 个）；`tsc` 无输出；`npm run build` 成功产出 `dist/`。

**Step 5: Commit**

```bash
cd <worktree>
git add docs/project-management/desktop.md desktop/README.md README.md
git commit -m "docs(desktop): record resizable sidebar + display-name change"
```

---

## 完成后

- 回 `main` 分支执行 `git merge --no-ff feat/desktop-sidebar-resize`，合并后删除分支与 worktree。
- 合并前确认 `git status` 干净、测试全绿。
