# 拖入路径（Drag & Drop Path）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** macOS 桌面端把文件或文件夹从 Finder 拖到输入卡片上时，自动把它的全局绝对路径录入当前会话的输入框（草稿）。

**Architecture:** Tauri 2 默认拦截 OS 拖放（`dragDropEnabled: true`），webview 拿不到 HTML5 `drop`/`dataTransfer`，因此唯一可靠通路是 `@tauri-apps/api/webview` 的 `getCurrentWebview().onDragDropEvent()`（`enter`/`over`/`drop`/`leave`，`enter`/`drop` 带绝对路径 `paths`，全部带物理像素 `position`）。排版规则抽成纯函数 `insertDroppedPaths`（无 React / 无 Tauri）；Tauri 事件形状收进 hook `useDragDropPaths`（注入 `subscribe`，jsdom 可直接驱动）；`App` 负责"落到哪个会话的草稿"并复用既有的 `changeDraft` 通路；`MessageInput` 只负责渲染高亮与读写光标。

**Tech Stack:** TypeScript / React 19 / Vitest + Testing Library（jsdom），`desktop/`（Vite + Tauri 2）。无新增依赖。

**Spec:** `docs/superpowers/specs/2026-10-18-desktop-drag-drop-path-design.md`

## Global Constraints

- 所有改动在 worktree `.worktrees/drag-drop-path`（分支 `feat/desktop-drag-drop-path`）内，**不碰 `main`**。
- 前端命令在 `<worktree>/desktop/` 下跑；先 `export PATH="/opt/homebrew/bin:$PATH"`；vitest 需 `TMPDIR="$PWD/.tmpverify"`，该目录在 worktree 里**尚不存在**，首次使用前 `mkdir -p .tmpverify`。
- **不新增任何 npm / Rust 依赖**（`@tauri-apps/api` 已在依赖里）。
- 提交用 conventional commits（`feat:` / `test:` / `docs:`），首行 ≤72 字符，**不写 `Co-Authored-By` 行**；`git status` 核对后**精确 `git add` 具体文件，不要 `git add -A`**。
- 中文注释：本仓库 `desktop/` 下的注释与 `docs/` 均为中文，新代码沿用中文注释。
- 远端（iOS）分支行为必须逐字节不变：所有 Tauri 相关代码必须被 `isRemoteClient()` 挡住（`desktop/src/lib/platform.ts:20`）。
- 不改任何 RPC / 协议 / 服务端代码。
- 桌面端既有测试选择器契约：卡片是 `data-testid="composer-card"`、工具栏是 `data-composer-toolbar`，新增状态标记一律用 `data-*` 属性（沿用仓库既有做法）。

---

### Task 1: 纯函数 `insertDroppedPaths` —— 路径排版规则

**Files:**
- Create: `desktop/src/lib/droppedPaths.ts`
- Test: `desktop/src/lib/droppedPaths.test.ts`

**Interfaces:**
- Consumes: 无（第一个任务）。
- Produces:
  - `quotePathIfNeeded(path: string): string`
  - `insertDroppedPaths(draft: string, caret: number, paths: readonly string[]): { text: string; caret: number }`

**规则（来自 spec，逐条可测）：** 光标处插入；前面有非空白内容时补一个空格；多条各占一行（`\n`）；路径含空白字符时用双引号包住；返回插入后光标应落的位置（最后一条路径末尾）。

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/droppedPaths.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import { insertDroppedPaths, quotePathIfNeeded } from "./droppedPaths";

describe("quotePathIfNeeded", () => {
  it("路径不含空白时原样返回", () => {
    expect(quotePathIfNeeded("/Users/me/src")).toBe("/Users/me/src");
  });

  it("路径含空格时用双引号包住", () => {
    expect(quotePathIfNeeded("/Users/me/My Docs")).toBe('"/Users/me/My Docs"');
  });

  it("路径含制表符也算需要引号", () => {
    expect(quotePathIfNeeded("/Users/me/a\tb")).toBe('"/Users/me/a\tb"');
  });
});

describe("insertDroppedPaths", () => {
  it("空 draft 光标在 0 时直接插入，不加前导空格", () => {
    expect(insertDroppedPaths("", 0, ["/Users/me/src"])).toEqual({
      text: "/Users/me/src",
      caret: 12,
    });
  });

  it("前面已有内容时补一个空格", () => {
    expect(insertDroppedPaths("看看", 2, ["/Users/me/src"])).toEqual({
      text: "看看 /Users/me/src",
      caret: 16,
    });
  });

  it("前面已是空白时不重复补空格", () => {
    expect(insertDroppedPaths("看看 ", 3, ["/Users/me/src"])).toEqual({
      text: "看看 /Users/me/src",
      caret: 16,
    });
  });

  it("在中间插入时保留后缀文本，光标落在路径末尾", () => {
    expect(insertDroppedPaths("ab", 1, ["/p"])).toEqual({ text: "a /pb", caret: 4 });
  });

  it("多条各占一行", () => {
    expect(insertDroppedPaths("", 0, ["/a", "/b"])).toEqual({ text: "/a\n/b", caret: 5 });
  });

  it("含空格的那条被引号包住，其余不加", () => {
    expect(insertDroppedPaths("", 0, ["/a", "/x y"])).toEqual({
      text: '/a\n"/x y"',
      caret: 10,
    });
  });

  it("空路径数组时原样返回，光标被夹回合法范围", () => {
    expect(insertDroppedPaths("hi", 99, [])).toEqual({ text: "hi", caret: 2 });
  });

  it("caret 为负或非有限数时退回安全位置", () => {
    expect(insertDroppedPaths("hi", -5, ["/a"])).toEqual({ text: " /ahi", caret: 3 });
    expect(insertDroppedPaths("hi", Number.NaN, ["/a"])).toEqual({ text: "hi/a", caret: 4 });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/drag-drop-path/desktop && mkdir -p .tmpverify && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/lib/droppedPaths.test.ts
```

Expected: FAIL —— 无法解析 `./droppedPaths`（模块不存在）。

- [ ] **Step 3: 写最小实现**

创建 `desktop/src/lib/droppedPaths.ts`：

```ts
/**
 * 拖入路径的排版规则（纯函数）。
 *
 * 与 Tauri、React、DOM 都无关：这里只有字符串进出，故拖放的三种规则
 * （光标处插入、多条各占一行、含空白加引号）可以穷举单测，不需要真实拖拽。
 */

/**
 * 路径含空白字符时用双引号包住。
 *
 * 目录名带空格在 macOS 上很常见（`~/My Documents`），不包引号时人眼与下游解析
 * 都难判断路径在哪里结束。不含空白的路径保持裸形式——与用户手输 `/Users/me/src`
 * 的习惯一致，不额外加噪音。
 */
export function quotePathIfNeeded(path: string): string {
  return /\s/.test(path) ? `"${path}"` : path;
}

/** 把光标位置夹回 `[0, draft.length]`；坏值（负 / NaN / 非有限）退回安全位置。 */
function clampCaret(draft: string, caret: number): number {
  if (!Number.isFinite(caret)) return draft.length;
  return Math.max(0, Math.min(draft.length, Math.trunc(caret)));
}

/**
 * 把拖入的路径插进草稿，返回新文本与插入后的光标位置。
 *
 * 规则：光标处插入；前缀有内容且不以空白结尾时补一个空格（避免与已有文字粘连）；
 * 多条路径各占一行；含空白的路径加引号。光标落在插入块的末尾，用户拖完可以继续
 * 往下打字，不必再点一下鼠标。
 */
export function insertDroppedPaths(
  draft: string,
  caret: number,
  paths: readonly string[],
): { text: string; caret: number } {
  const at = clampCaret(draft, caret);
  if (paths.length === 0) return { text: draft, caret: at };

  const inserted = paths.map(quotePathIfNeeded).join("\n");
  const needsSpace = at > 0 && !/\s$/.test(draft.slice(0, at));
  const block = needsSpace ? ` ${inserted}` : inserted;
  return { text: draft.slice(0, at) + block + draft.slice(at), caret: at + block.length };
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/lib/droppedPaths.test.ts
```

Expected: PASS（11 个用例）。

- [ ] **Step 5: 提交**

```bash
cd .worktrees/drag-drop-path && git add desktop/src/lib/droppedPaths.ts desktop/src/lib/droppedPaths.test.ts && git commit -m "feat(desktop): 拖入路径的排版纯函数" 
```

---

### Task 2: hook `useDragDropPaths` —— 订阅 Tauri 拖放 + 落点判定

**Files:**
- Create: `desktop/src/hooks/useDragDropPaths.ts`
- Test: `desktop/src/hooks/useDragDropPaths.test.ts`

**Interfaces:**
- Consumes: 无（Task 1 的纯函数在 Task 4 才被 `App` 串起来）。
- Produces:
  - `type DragDropPosition = { x: number; y: number }`
  - `type DragDropPayload = { type: "enter"; paths: string[]; position: DragDropPosition } | { type: "over"; position: DragDropPosition } | { type: "drop"; paths: string[]; position: DragDropPosition } | { type: "leave" }`
  - `type DragDropSubscribe = (handler: (event: DragDropPayload) => void) => Promise<() => void>`
  - `dragHitsTarget(el: HTMLElement | null, position: DragDropPosition, ratio?: number): boolean`
  - `useDragDropPaths(opts: { subscribe: DragDropSubscribe; isRemote: boolean; targetRef: RefObject<HTMLElement | null>; onDrop: (paths: string[]) => void }): { active: boolean }`

**要点：** `position` 是**物理像素**，`getBoundingClientRect()` 是 CSS 像素，判定前必须 `÷ devicePixelRatio`（Retina 上不除会整体偏移约一倍）。`isRemote` 为真时**完全不订阅**（远端没有该 API）。卸载时调用 `unlisten`。

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/hooks/useDragDropPaths.test.ts`：

```ts
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act, cleanup } from "@testing-library/react";
import { useRef } from "react";
import { useDragDropPaths, dragHitsTarget, type DragDropPayload } from "./useDragDropPaths";

afterEach(cleanup);

/** 造一个带固定 rect 的元素，模拟 jsdom 里"落在卡片上"的几何。 */
function elWithRect(rect: { left: number; top: number; right: number; bottom: number }) {
  const el = document.createElement("div");
  el.getBoundingClientRect = () => ({ ...rect, width: rect.right - rect.left, height: rect.bottom - rect.top, x: rect.left, y: rect.top, toJSON: () => ({}) }) as DOMRect;
  document.body.appendChild(el);
  return el;
}

/** 假订阅：捕获 handler 供测试直接喂事件，并记录 unlisten 是否被调用。 */
function fakeSubscribe() {
  const handlerBox: { fn: ((e: DragDropPayload) => void) | null } = { fn: null };
  const unlisten = vi.fn();
  const subscribe = async (handler: (e: DragDropPayload) => void) => {
    handlerBox.fn = handler;
    return unlisten;
  };
  return { subscribe, handlerBox, unlisten };
}

function renderHookWithTarget(opts: {
  isRemote: boolean;
  onDrop: (paths: string[]) => void;
  rect?: { left: number; top: number; right: number; bottom: number };
}) {
  const el = elWithRect(opts.rect ?? { left: 0, top: 0, right: 100, bottom: 100 });
  const sub = fakeSubscribe();
  const view = renderHook(() => {
    const ref = useRef<HTMLElement | null>(el);
    return useDragDropPaths({
      subscribe: sub.subscribe,
      isRemote: opts.isRemote,
      targetRef: ref,
      onDrop: opts.onDrop,
    });
  });
  return { ...sub, el, view };
}

describe("dragHitsTarget", () => {
  it("物理像素除以 devicePixelRatio 后落在矩形内即为命中", () => {
    const el = elWithRect({ left: 10, top: 20, right: 110, bottom: 120 });
    // ratio=2：物理 (120, 100) → CSS (60, 50)，在矩形内。
    expect(dragHitsTarget(el, { x: 120, y: 100 }, 2)).toBe(true);
  });

  it("右/下边界为开区间，左上边界为闭区间", () => {
    const el = elWithRect({ left: 10, top: 20, right: 110, bottom: 120 });
    expect(dragHitsTarget(el, { x: 110, y: 20 }, 1)).toBe(false); // 右边界外
    expect(dragHitsTarget(el, { x: 10, y: 20 }, 1)).toBe(true); // 左上角计入
  });

  it("元素为 null 时不命中", () => {
    expect(dragHitsTarget(null, { x: 0, y: 0 }, 1)).toBe(false);
  });
});

describe("useDragDropPaths", () => {
  it("enter 落在卡片上时 active 为真", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    expect(view.result.current.active).toBe(false);
    act(() => handlerBox.fn!({ type: "enter", paths: ["/a"], position: { x: 50, y: 50 } }));
    expect(view.result.current.active).toBe(true);
  });

  it("over 移出卡片时 active 转假", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "over", position: { x: 50, y: 50 } }));
    expect(view.result.current.active).toBe(true);
    act(() => handlerBox.fn!({ type: "over", position: { x: 500, y: 500 } }));
    expect(view.result.current.active).toBe(false);
  });

  it("drop 落在卡片上时回传 paths 并清掉高亮", async () => {
    const onDrop = vi.fn();
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "drop", paths: ["/a", "/b"], position: { x: 50, y: 50 } }));
    expect(onDrop).toHaveBeenCalledWith(["/a", "/b"]);
    expect(view.result.current.active).toBe(false);
  });

  it("drop 落在卡片之外时什么都不做", async () => {
    const onDrop = vi.fn();
    const { handlerBox } = renderHookWithTarget({ isRemote: false, onDrop });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "drop", paths: ["/a"], position: { x: 900, y: 900 } }));
    expect(onDrop).not.toHaveBeenCalled();
  });

  it("drop 的 paths 为空时不回调", async () => {
    const onDrop = vi.fn();
    const { handlerBox } = renderHookWithTarget({ isRemote: false, onDrop });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "drop", paths: [], position: { x: 50, y: 50 } }));
    expect(onDrop).not.toHaveBeenCalled();
  });

  it("leave 清掉高亮", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "enter", paths: [], position: { x: 50, y: 50 } }));
    expect(view.result.current.active).toBe(true);
    act(() => handlerBox.fn!({ type: "leave" }));
    expect(view.result.current.active).toBe(false);
  });

  it("远端（iOS）完全不订阅", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: true, onDrop: vi.fn() });
    await act(async () => {});
    expect(handlerBox.fn).toBeNull();
    expect(view.result.current.active).toBe(false);
  });

  it("卸载时调用 unlisten", async () => {
    const { unlisten, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    view.unmount();
    expect(unlisten).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/hooks/useDragDropPaths.test.ts
```

Expected: FAIL —— 无法解析 `./useDragDropPaths`（模块不存在）。

- [ ] **Step 3: 写最小实现**

创建 `desktop/src/hooks/useDragDropPaths.ts`：

```ts
import { useEffect, useRef, useState, type RefObject } from "react";

/**
 * Tauri 拖放事件的形状（`@tauri-apps/api/webview` 的 `DragDropEvent`）。
 *
 * 这里自己声明一份而不是 import 那个类型：远端（iOS）构建不该因为类型引用而把
 * `@tauri-apps/api` 拉进静态图（本仓库一律**动态** import 它，见 `App.tsx`）。
 * 结构上与真类型一致，`App` 里那一层 subscribe 直接把它转发进来。
 */
export type DragDropPosition = { x: number; y: number };

export type DragDropPayload =
  | { type: "enter"; paths: string[]; position: DragDropPosition }
  | { type: "over"; position: DragDropPosition }
  | { type: "drop"; paths: string[]; position: DragDropPosition }
  | { type: "leave" };

/** 注入的订阅接缝：返回的 Promise 解析为 `unlisten`。 */
export type DragDropSubscribe = (handler: (event: DragDropPayload) => void) => Promise<() => void>;

/**
 * 拖拽点是否落在 `el` 的矩形内。
 *
 * `position` 来自 Tauri，是**物理像素**；`getBoundingClientRect()` 是 CSS 像素。
 * 必须除以 `devicePixelRatio`（Retina 上是 2，不除会让判定整体偏移约一倍）。
 * 边界取「左闭右开」：与 `elementFromPoint` 一类的命中语义一致，且矩形相邻时
 * 不会出现一个点同时命中两个框。
 */
export function dragHitsTarget(
  el: HTMLElement | null,
  position: DragDropPosition,
  ratio: number = globalThis.devicePixelRatio || 1,
): boolean {
  if (el === null) return false;
  const r = el.getBoundingClientRect();
  const scale = ratio > 0 ? ratio : 1;
  const x = position.x / scale;
  const y = position.y / scale;
  return x >= r.left && x < r.right && y >= r.top && y < r.bottom;
}

/**
 * 订阅窗口级拖放事件，判定落点是否在 `targetRef` 上，并给出高亮状态。
 *
 * 为什么挂在窗口级而不是把监听装在输入框上：拖放事件是窗口级的，而输入框在
 * 看板视图/无会话时不渲染——监听随组件装卸会漏掉 `leave` 一类的边界。这里订阅
 * 一次、整窗口有效，落点仍由 `targetRef` 的矩形决定。
 *
 * `isRemote` 为真（iOS/远端）时**完全不订阅**：那条路没有 Tauri webview API。
 * `subscribe` / `onDrop` / `targetRef` 都经 ref 读取，故调用方不必为每次渲染
 * 重建订阅（重建会漏事件）。
 */
export function useDragDropPaths({
  subscribe,
  isRemote,
  targetRef,
  onDrop,
}: {
  subscribe: DragDropSubscribe;
  isRemote: boolean;
  targetRef: RefObject<HTMLElement | null>;
  onDrop: (paths: string[]) => void;
}): { active: boolean } {
  const [active, setActive] = useState(false);
  const subscribeRef = useRef(subscribe);
  const onDropRef = useRef(onDrop);
  useEffect(() => {
    subscribeRef.current = subscribe;
    onDropRef.current = onDrop;
  }, [subscribe, onDrop]);

  useEffect(() => {
    if (isRemote) return;
    let alive = true;
    let unlisten: (() => void) | null = null;

    void subscribeRef
      .current((event) => {
        if (event.type === "enter" || event.type === "over") {
          setActive(dragHitsTarget(targetRef.current, event.position));
          return;
        }
        if (event.type === "leave") {
          setActive(false);
          return;
        }
        const hit = dragHitsTarget(targetRef.current, event.position);
        setActive(false);
        if (hit && event.paths.length > 0) onDropRef.current(event.paths);
      })
      .then((fn) => {
        // 订阅解析回来时组件可能已经卸载：那时直接摘掉，别把监听留在窗口上。
        if (alive) unlisten = fn;
        else fn();
      });

    return () => {
      alive = false;
      unlisten?.();
    };
  }, [isRemote, targetRef]);

  return { active };
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/hooks/useDragDropPaths.test.ts
```

Expected: PASS（11 个用例）。

- [ ] **Step 5: 提交**

```bash
cd .worktrees/drag-drop-path && git add desktop/src/hooks/useDragDropPaths.ts desktop/src/hooks/useDragDropPaths.test.ts && git commit -m "feat(desktop): 订阅窗口拖放并判定落点" 
```

---

### Task 3: `MessageInput` —— 高亮 + 光标读写

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx`（props 约 `:28`–`:84`；卡片 `div` 约 `:164`–`:167`；`textarea` 约 `:184`–`:279`）
- Test: `desktop/src/components/MessageInput.test.tsx`（文件末尾追加）

**Interfaces:**
- Consumes: 无（Task 1/2 的产物在 Task 4 接线）。
- Produces（`MessageInput` 新增 4 个 props，Task 4 依赖这些**确切名字**）：
  - `cardRef?: Ref<HTMLDivElement>` —— 挂到卡片 `div` 上，供上层做落点判定
  - `dropActive?: boolean`（默认 `false`）—— 拖拽悬停高亮
  - `caretSeek?: { seq: number; pos: number } | null`（默认 `null`）—— 请求把光标放到 `pos`
  - `onCaretChange?: (caret: number) => void`（默认无操作）—— 把用户当前光标位置报给上层

**注意：** 高亮用 `ring`（box-shadow）而不是换 `border-*` 类——卡片基础类 `border-line-strong` 必须逐字节保留（既有测试 `desktop/src/components/MessageInput.test.tsx:164` 断言它），两条 `border-*` 类同时存在时谁生效取决于生成样式的顺序，不可靠。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/MessageInput.test.tsx` **文件末尾**追加：

```tsx
describe("拖入高亮与光标", () => {
  it("dropActive 为真时卡片加高亮 ring 并带 data-drop-active 标记", () => {
    renderInput({ dropActive: true });
    const card = screen.getByTestId("composer-card");
    expect(card.getAttribute("data-drop-active")).not.toBeNull();
    expect(card.className).toContain("ring-2");
    expect(card.className).toContain("ring-blue-500");
  });

  it("默认（无拖拽）时没有高亮、没有标记，基础类逐字节不变", () => {
    renderInput();
    const card = screen.getByTestId("composer-card");
    expect(card.getAttribute("data-drop-active")).toBeNull();
    expect(card.className).not.toContain("ring-2");
    expect(card.className).toContain("rounded-lg");
    expect(card.className).toContain("border-line-strong");
  });

  it("用户移动光标时把位置报给上层", () => {
    const onCaretChange = vi.fn();
    renderInput({ value: "hello", onCaretChange });
    const textarea = screen.getByRole("textbox") as HTMLTextAreaElement;
    textarea.setSelectionRange(3, 3);
    fireEvent.select(textarea);
    expect(onCaretChange).toHaveBeenCalledWith(3);
  });

  it("caretSeek 提供时把光标放到指定位置", () => {
    renderInput({ value: "hello world", caretSeek: { seq: 1, pos: 6 } });
    const textarea = screen.getByRole("textbox") as HTMLTextAreaElement;
    expect(textarea.selectionStart).toBe(6);
    expect(textarea.selectionEnd).toBe(6);
  });

  it("cardRef 落到卡片元素上", () => {
    const ref: { current: HTMLDivElement | null } = { current: null };
    renderInput({ cardRef: ref });
    expect(ref.current).toBe(screen.getByTestId("composer-card"));
  });
});
```

注意两点：

- 本文件没有 jest-dom（仓库未安装 `@testing-library/jest-dom`，`vite.config.ts` 也没有 `test.setupFiles`），故一律用裸断言（`expect(x).toBe(...)`），**不要**用 `toHaveValue` 一类匹配器。
- `caretSeek` 直接在挂载时提供即可覆盖：effect 在挂载后立刻跑一次 `setSelectionRange`。无需 `rerender`（`renderInput` 的 harness 是独立组件，`rerender` 换元素会绕过它）。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/components/MessageInput.test.tsx
```

Expected: FAIL —— `dropActive` / `caretSeek` / `onCaretChange` / `cardRef` 不是已知 props（TS/运行时均不生效），5 个新用例失败。

- [ ] **Step 3: 实现**

改 `desktop/src/components/MessageInput.tsx`：

1）`import` 行（文件第 1 行）补 `useEffect` 与 `Ref` 类型：

```tsx
import { useEffect, useRef, useState, type ReactNode, type Ref } from "react";
```

2）props 类型里追加 4 项（放在 `modelPicker` 之后）：

```tsx
  /**
   * 追加在工具栏里的控件（与 ModeChip / 发送按钮同一区域，居发送按钮左侧）。当前由
   * App 传入会话级模型下拉；`MessageInput` 不关心它是什么，只负责给它一个位置。
   */
  modelPicker?: ReactNode;
  /**
   * 卡片元素本身的 ref：上层（`App` 的拖放落点判定）需要卡片的矩形来算"拖到这里
   * 才算数"。DOM 归属仍在本组件，故由本组件挂到卡片上，而不是让上层去查 DOM。
   */
  cardRef?: Ref<HTMLDivElement>;
  /**
   * 拖拽悬停在卡片上：整卡加一圈高亮，让"这里可以放"一眼可见。
   *
   * 用 `ring`（box-shadow）而非换 `border-*` 类：基础类里的 `border-line-strong`
   * 是本卡边框的唯一来源（既有测试逐字节断言它），再叠一条 `border-*` 谁生效取决于
   * 生成样式顺序，不可靠。
   */
  dropActive?: boolean;
  /**
   * 请求把光标放到 `pos`。带 `seq` 是为了让**连续两次落在同一位置**也能各生效一次
   * ——否则两次拖入的参数对象相等，effect 依赖判不出变化。
   */
  caretSeek?: { seq: number; pos: number } | null;
  /**
   * 用户当前的光标位置，报给上层。上层记着它，拖入时才知道该往哪里插。
   */
  onCaretChange?: (caret: number) => void;
```

3）解构参数处补默认值（在 `modelPicker,` 之后、`}: {` 之前）：

```tsx
  modelPicker,
  cardRef,
  dropActive = false,
  caretSeek = null,
  onCaretChange,
```

4）光标定位 effect（放在既有 `useEffect(() => { setSelected(0); }, [text]);` 之后）：

```tsx
  // 上层插入路径后请求把光标放回插入点：受控 textarea 的文本由父级换掉，光标
  // 会跳到末尾，不这样摆一次，用户就得手动点回来。
  useEffect(() => {
    if (caretSeek === null) return;
    const el = inputRef.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(caretSeek.pos, caretSeek.pos);
  }, [caretSeek]);
```

5）卡片 `div`（`data-testid="composer-card"` 那个）：

```tsx
      <div
        ref={cardRef}
        data-drop-active={dropActive ? "" : undefined}
        className={
          "rounded-lg border border-line-strong bg-surface transition-colors focus-within:border-fg-subtle" +
          (dropActive ? " ring-2 ring-blue-500" : "")
        }
        data-testid="composer-card"
      >
```

6）`textarea` 补光标上报（与既有 `onChange` / `onCompositionEnd` / `onBlur` 并列）：

```tsx
            onSelect={(e) => onCaretChange?.(e.currentTarget.selectionStart)}
            onClick={(e) => onCaretChange?.(e.currentTarget.selectionStart)}
            onKeyUp={(e) => onCaretChange?.(e.currentTarget.selectionStart)}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/components/MessageInput.test.tsx src/lib/droppedPaths.test.ts src/hooks/useDragDropPaths.test.ts
```

Expected: PASS —— 新增 5 例通过，且既有用例（含 `:164` 那条基础类断言）**不回归**。

- [ ] **Step 5: 提交**

```bash
cd .worktrees/drag-drop-path && git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx && git commit -m "feat(desktop): 输入卡片拖入高亮与光标定位" 
```

---

### Task 4: `App` 接线 —— 订阅 + 落到当前会话草稿

**Files:**
- Modify: `desktop/src/App.tsx`（import 区约 `:1`–`:20`；`changeDraft` 附近约 `:1410`；`<MessageInput>` 约 `:2170`）
- Create: `desktop/src/lib/dragDropSubscribe.ts`（真实 Tauri 订阅的**唯一**落点，便于测试 mock）
- Test: `desktop/src/App.test.tsx`（文件末尾追加）

**Interfaces:**
- Consumes（Task 1/2/3 的产物，名字必须一致）：
  - `insertDroppedPaths(draft, caret, paths) → { text, caret }`
  - `useDragDropPaths({ subscribe, isRemote, targetRef, onDrop }) → { active }`
  - `MessageInput` 的 `cardRef` / `dropActive` / `caretSeek` / `onCaretChange`
- Produces:
  - `subscribeDragDrop: DragDropSubscribe`（`desktop/src/lib/dragDropSubscribe.ts` 导出）

**要点：** 拖入落在**当前会话**的草稿上：走既有 `changeDraft` 同类通路（`store.setDraft(id, text)` + `force`），切走再切回路径仍在。无当前会话（`disabled`）时忽略。`isRemoteClient()` 为真时 hook 不订阅。光标位置存在一个 ref 里（不驱动渲染），插入后的定位请求存在 state 里。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/App.test.tsx` **文件末尾**追加：

```tsx
describe("拖入路径", () => {
  it("drop 落在输入卡片上时把路径写进当前会话草稿", async () => {
    render(<App />);
    // 首屏自动 resume 第一条会话后，输入框才不是 disabled。
    await waitFor(() => expect(screen.getByRole("textbox")).toBeTruthy());
    const card = screen.getByTestId("composer-card");
    stubRect(card, { left: 0, top: 0, right: 400, bottom: 120 });
    await act(async () => {
      dragDrop.fn?.({ payload: { type: "drop", paths: ["/Users/me/My Docs"], position: { x: 10, y: 10 } } });
    });
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe('"/Users/me/My Docs"');
  });

  it("drop 落在卡片之外时不改草稿", async () => {
    render(<App />);
    await waitFor(() => expect(screen.getByRole("textbox")).toBeTruthy());
    const card = screen.getByTestId("composer-card");
    stubRect(card, { left: 0, top: 0, right: 400, bottom: 120 });
    await act(async () => {
      dragDrop.fn?.({ payload: { type: "drop", paths: ["/a"], position: { x: 900, y: 900 } } });
    });
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("");
  });
});
```

在**文件顶部的 `vi.hoisted` 区**扩容（与 `clients` / `state` 同一处，`vi.mock` 会被提升到文件最前，普通 `const` 会被甩在它后面，故必须用 `vi.hoisted`）：

```tsx
const { clients, state, dragDrop } = vi.hoisted(() => ({
  // …`clients` / `state` 原样不动…
  // 捕获 `@tauri-apps/api/webview` 注册的拖放 handler，供用例驱动事件。
  dragDrop: { fn: null as null | ((event: unknown) => void) },
}));
```

并在既有 `vi.mock("./transportFactory", …)` 之前加：

```tsx
// 拖放订阅走动态 import 的 `@tauri-apps/api/webview`；在模块边界桩掉它，
// 并把注册的 handler 记在 `dragDrop.fn` 上。Tauri 交来的是事件对象
// （`{ payload }`），故这里回放时也要包一层——`App` 的 subscribe 只转发 payload。
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: async (handler: (event: unknown) => void) => {
      dragDrop.fn = handler;
      return () => {};
    },
  }),
}));

/** jsdom 的 rect 恒为 0，落点判定需要一个真矩形。 */
function stubRect(el: HTMLElement, r: { left: number; top: number; right: number; bottom: number }) {
  el.getBoundingClientRect = () =>
    ({ ...r, width: r.right - r.left, height: r.bottom - r.top, x: r.left, y: r.top, toJSON: () => ({}) }) as DOMRect;
}
```

`beforeEach` 里补一行 `dragDrop.fn = null;`（与其它 `state.*` 复位并列），避免用例间串 handler。

注意：
- 本文件没有 jest-dom，断言一律用裸 `expect(...).toBe(...)`（不用 `toHaveValue`）。
- `render(<App />)` 已在近 100 个既有用例里这样用，无需自造渲染工具；`act` / `waitFor` / `screen` 都已在文件顶部 import。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/App.test.tsx -t "拖入路径"
```

Expected: FAIL —— `dragDrop.fn` 恒为 `null`（`App` 还没订阅），草稿为空。

- [ ] **Step 3: 实现**

1）新建 `desktop/src/lib/dragDropSubscribe.ts`：

```ts
import type { DragDropPayload, DragDropSubscribe } from "../hooks/useDragDropPaths";

/**
 * 真实的 Tauri 拖放订阅（桌面端唯一落点）。
 *
 * 单独成文件是为了让 `App` 与测试都只依赖一个稳定符号：`@tauri-apps/api` 一律
 * **动态** import（本仓库既有做法，见 `App.tsx` 的 `core` / `plugin-dialog`），
 * 远端构建因此不会把它拉进静态图。测试直接在模块边界 mock 掉它。
 */
export const subscribeDragDrop: DragDropSubscribe = async (handler) => {
  const { getCurrentWebview } = await import("@tauri-apps/api/webview");
  // Tauri 交来的是事件对象（`{ payload }`），hook 只认 payload 本身。
  return getCurrentWebview().onDragDropEvent((event) =>
    handler(event.payload as DragDropPayload),
  );
};
```

2）`desktop/src/App.tsx` import 区追加：

```tsx
import { useDragDropPaths } from "./hooks/useDragDropPaths";
import { subscribeDragDrop } from "./lib/dragDropSubscribe";
import { insertDroppedPaths } from "./lib/droppedPaths";
```

3）在 `changeDraft` 定义（`desktop/src/App.tsx:1410` 附近）之后追加：

```tsx
  /**
   * 拖入的路径落点：光标位置（不驱动渲染，故用 ref）与最近一次定位请求。
   *
   * `caret` 记用户最后一次移动光标的位置，拖入时用它决定插在哪；`seek` 是给
   * `MessageInput` 的"把光标放到这里"请求，用自增 `seq` 让同位置连拖也能各生效一次。
   */
  const composerCaret = useRef(0);
  const seekSeq = useRef(0);
  const [caretSeek, setCaretSeek] = useState<{ seq: number; pos: number } | null>(null);
  const composerCardRef = useRef<HTMLDivElement | null>(null);

  /**
   * 把拖入的路径插进**当前会话**的草稿。
   *
   * 与 `changeDraft` 同一条通路（写 `view(id).draft` + `force`）：草稿属于会话，
   * 所以切走再切回，路径仍在原会话的输入框里。没有当前会话时无处可放，忽略——
   * 与输入框的 `disabled` 护栏一致，不留"拖进去却静默丢路径"的死路。
   */
  const dropPathsIntoDraft = (paths: string[]): void => {
    const id = store.currentId;
    if (!id) return;
    const { text, caret } = insertDroppedPaths(store.view(id).draft, composerCaret.current, paths);
    store.setDraft(id, text);
    composerCaret.current = caret;
    setCaretSeek({ seq: ++seekSeq.current, pos: caret });
    force((v) => v + 1);
  };

  const dragDrop = useDragDropPaths({
    subscribe: subscribeDragDrop,
    isRemote: isRemoteClient(),
    targetRef: composerCardRef,
    onDrop: dropPathsIntoDraft,
  });
```

4）`<MessageInput>`（`desktop/src/App.tsx:2170`）补 4 个 props：

```tsx
                cardRef={composerCardRef}
                dropActive={dragDrop.active}
                caretSeek={caretSeek}
                onCaretChange={(caret) => {
                  composerCaret.current = caret;
                }}
```

（`useRef` / `useState` 已在 `App.tsx` 顶部 import，无需新增。）

- [ ] **Step 4: 跑测试确认通过**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx vitest run src/App.test.tsx src/components/MessageInput.test.tsx src/lib/droppedPaths.test.ts src/hooks/useDragDropPaths.test.ts
```

Expected: PASS（含新增 2 例；`App.test.tsx` 既有用例不回归）。

- [ ] **Step 5: 提交**

```bash
cd .worktrees/drag-drop-path && git add desktop/src/App.tsx desktop/src/lib/dragDropSubscribe.ts desktop/src/App.test.tsx && git commit -m "feat(desktop): 拖入的文件/文件夹路径自动录入当前会话" 
```

---

### Task 5: 全量校验 + 文档

**Files:**
- Modify: `docs/project-management/desktop.md`（新增一条 `[x]`）
- Modify: `README.md:268`（模块索引表的 desktop 计数）

**Interfaces:**
- Consumes: Task 1–4 的全部产物（本任务不改代码，除非校验暴露真问题）。
- Produces: 无。

- [ ] **Step 1: 类型检查 + 全量测试**

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && TMPDIR="$PWD/.tmpverify" npx tsc --noEmit && TMPDIR="$PWD/.tmpverify" npx vitest run
```

Expected: `tsc` 无输出；vitest 全绿（既有用例 + 本次新增 29 例：Task 1 的 11 例 + Task 2 的 11 例 + Task 3 的 5 例 + Task 4 的 2 例）。

- [ ] **Step 2: 更新模块文档**

在 `docs/project-management/desktop.md` 的功能清单里追加一条（与同文件既有条目同格式：一句话 + 可验证判据 = 代码位置或可执行命令）：

```markdown
- [x] 拖入文件/文件夹自动录入绝对路径（拖到输入卡片上即把全局路径插入当前会话草稿；落在卡片外无反应；悬停整卡高亮；多条各占一行；含空格的路径加双引号）— 通路：Tauri 2 默认拦截 OS 拖放（`dragDropEnabled` 未关，webview 拿不到 HTML5 `drop`），故走 `@tauri-apps/api/webview` 的 `getCurrentWebview().onDragDropEvent()`（`desktop/src/lib/dragDropSubscribe.ts:12`，动态 import + 只转发 `event.payload`）；落点判定按物理像素换算 `desktop/src/hooks/useDragDropPaths.ts`（`dragHitsTarget`：`position ÷ devicePixelRatio` 与卡片 rect 做左闭右开包含）；排版纯函数 `desktop/src/lib/droppedPaths.ts`（`insertDroppedPaths` 光标处插入 / 多条 `\n` 分隔 / 含空白加双引号）；接线 `desktop/src/App.tsx`（`dropPathsIntoDraft` 写当前会话 `draft` + `caretSeek` 定位，无当前会话则忽略）、`desktop/src/components/MessageInput.tsx`（`cardRef` / `dropActive` 高亮 `ring-2 ring-blue-500` / `caretSeek` / `onCaretChange`）；远端 `isRemoteClient()` 为真时不订阅。判据：`cd desktop && npx tsc --noEmit && TMPDIR="$PWD/.tmpverify" npx vitest run src/lib/droppedPaths.test.ts src/hooks/useDragDropPaths.test.ts src/components/MessageInput.test.tsx src/App.test.tsx`。**待真机实测**：Retina 下坐标换算、Finder 拖入符号链接时 `paths` 是否为已解引用路径 — [设计](../superpowers/specs/2026-10-18-desktop-drag-drop-path-design.md)
```

（写完代码后，用 `grep -n "export const subscribeDragDrop" desktop/src/lib/dragDropSubscribe.ts` 与 `grep -n "export function insertDroppedPaths" desktop/src/lib/droppedPaths.ts` 取真实行号，替换上面那两处占位与 `src/App.tsx` 的接线行号。）

- [ ] **Step 3: 更新 README 计数**

把 `README.md:268` 的 desktop 行由 `| desktop | 59 / 71 |` 改为 `| desktop | 60 / 72 |`（新增 1 条 feature，未完成数不变）。

- [ ] **Step 4: 提交**

```bash
cd .worktrees/drag-drop-path && git add docs/project-management/desktop.md README.md && git commit -m "docs: 记录拖入路径功能与模块计数" 
```

- [ ] **Step 5: 收尾（按 finishing-a-development-branch）**

用 `superpowers:finishing-a-development-branch` 决定合回方式：本仓库约定为回 `main` 做 `git merge --no-ff feat/desktop-drag-drop-path`，合并后删分支 + `git worktree remove .worktrees/drag-drop-path`。

---

## 手工验收（不进 CI，需真机）

```bash
cd .worktrees/drag-drop-path/desktop && export PATH="/opt/homebrew/bin:$PATH" && npm run tauri dev
```

1. 从 Finder 拖一个**文件夹**到输入卡片上 → 卡片出现蓝色圆环高亮，松手后输入框出现该文件夹绝对路径。
2. 拖到侧栏或消息区 → 无高亮、无变化。
3. 一次拖**多个**项目 → 各占一行。
4. 目录名含空格（如 `My Docs`）→ 路径被双引号包住。
5. 输入框先打字，再把光标点到文字中间后拖入 → 插在光标处。
6. 切到别的会话再切回 → 路径仍在原会话草稿里。
7. Retina 屏下重复第 1 步 → 判定仍准确（若出现"边缘几十像素内判定错"，即坐标换算问题）。