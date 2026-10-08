# 手机端会话面板长按（= 右键）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 配对后的 iPhone 客户端上，左侧会话面板的**工作区分组头**（文件夹那行）与会话行都支持长按，弹出与桌面右键等价的操作菜单。

**Architecture:** 三块切分。①**手势 hook**（`desktop/src/lib/useLongPress.ts`）：纯 Pointer Events 的「长按」识别，挂在**侧栏容器**上做事件委托，由 `resolveTarget(e)` 判定按到的是会话行还是组头，因此组件里只在顶层调用一次，`renderThread` 的 `.map()` 不被 Hooks 规则牵连。②**侧栏接线**（`desktop/src/components/ThreadSidebar.tsx`）：把现有 `contextWs` 提升为 `menu` 联合以容纳新的会话菜单，接线两个长按结果，新增 `data-ws-header` / `data-thread-row` 两个标记属性，并新增 `isMobile` prop 门控（桌面端不装监听、不渲染会话菜单）。③**移动端 CSS**（`desktop/src/index.css` 的 `[data-mobile="true"]` 段）：关掉行上的 `-webkit-touch-callout` / `user-select`，否则 iOS 的文字选择/放大镜 callout 会抢走长按。

**Tech Stack:** TypeScript + React 19 + Tailwind CSS v4 + vitest（jsdom / node 双环境）。

**依据 spec:** `docs/superpowers/specs/2026-10-08-mobile-sidebar-longpress-design.md`（§2 决策、§4 hook 契约、§5 状态与渲染、§6 桌面零影响、§7 CSS、§8 测试）。

## Global Constraints

- **只在 `desktop/` 内改动**（前端）。不碰 Rust、不碰线协议。
- **桌面端行为逐字节不变**：长按由 `isMobile` 门控；`isMobile === false` 时 hook 不装任何监听，且不渲染会话菜单。
- **不依赖 `contextmenu`**：iOS 的 WKWebView 长按不派发该事件。手势一律走 Pointer Events。
- **不 `preventDefault` pointerdown**：否则会破坏抽屉滚动；滚动由 `pointercancel` + 位移阈值取消长按。
- **JSX logo 用 `??` 而不是 `||`**：`title ?? t.thread_id`，避免空串标题被当成 null。
- **菜单项文案**（会话菜单）：`重命名` / `置顶`（未置顶时）/ `取消置顶`（已置顶时）/ `删除`。
- 测试文件首行 `/** @vitest-environment jsdom */`（node 环境的静态门禁用 `/** @vitest-environment node */`，见 `desktop/src/mobileLayout.test.ts`）。
- 组件测试 `afterEach(cleanup)`；用 fake timers 的测试 `afterEach(() => vi.useRealTimers())`。
- 推进 React 更新：`await act(async () => { await vi.advanceTimersByTimeAsync(MS); });`（**同步** `advanceTimersByTime` 不 flush React 更新，会看到菜单还没渲染）。
- 命令前缀（本机 node 在 homebrew）：`export PATH="/opt/homebrew/bin:$PATH" && cd desktop && ...`
- 验证口径：`npx tsc --noEmit` + `npx vitest run`。
- commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。
- **worktree 已建好**：`.worktrees/mobile-sidebar-longpress`（分支 `fix/mobile-sidebar-longpress`），从 `main` 分出。该 worktree 的 `desktop/` 下**没有 `node_modules`**（已 gitignore），首次跑测试前先建软链：
  `ln -sfn <repo>/desktop/node_modules <worktree>/desktop/node_modules`

---

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `desktop/src/lib/useLongPress.ts` | 长按手势识别（委托式，一份监听管全部行），不认识 thread/workspace | 新建 |
| `desktop/src/lib/useLongPress.test.ts` | hook 单测（阈值/取消/吞 click/门控/忽略交互子元素） | 新建 |
| `desktop/src/components/ThreadSidebar.tsx` | `isMobile` prop、`menu` 联合状态、会话菜单渲染、两个长按挂点（容器级委托） | 改 |
| `desktop/src/components/ThreadSidebar.test.tsx` | 手机长按（行/组头）+ 桌面不长按 + 删除/重命名确认弹窗 | 改 |
| `desktop/src/App.tsx` | 给 `ThreadSidebar` 传 `isMobile={isMobile}` | 改 |
| `desktop/src/index.css` | `[data-mobile="true"]` 下抑制行长按的原生 callout/选择 | 改 |
| `desktop/src/mobileLayout.test.ts` | 静态门禁：断言上面那条 CSS 存在于 `[data-mobile="true"]` 作用域 | 改 |
| `desktop/src/lib/sidebarPress.ts` | 纯函数 `resolveSidebarPressTarget`（长按落点判定，脱 DOM 可测） | 新建 |
| `desktop/src/lib/sidebarPress.test.ts` | 上者单测（行 / 组头 / 交互子元素 / 空白） | 新建 |
| `docs/project-management/desktop.md` | 登记 feature（`[x]`，带可验证判据） | 改 |
| `README.md` | 模块索引表 desktop 计数同步 | 改 |

依赖方向：`ThreadSidebar.tsx` → `sidebarPress.ts` + `useLongPress.ts`；两个 lib 互不依赖。

---

### Task 1: 长按手势 hook + 落点判定纯函数

**Files:**
- Create: `desktop/src/lib/useLongPress.ts`
- Create: `desktop/src/lib/sidebarPress.ts`
- Test: `desktop/src/lib/useLongPress.test.ts`
- Test: `desktop/src/lib/sidebarPress.test.ts`

**Interfaces:**
- Consumes: `react`（`useCallback` / `useEffect` / `useRef`）、`react` 的 `PointerEvent` 类型。
- Produces：
  - `useLongPress(opts: LongPressOptions): { onPointerDown: (e: React.PointerEvent<HTMLElement>) => void }`
  - `type LongPressTarget = { kind: "thread"; id: string } | { kind: "group"; ws: string }`
  - `resolveSidebarPressTarget(target: Element | null): HTMLElement | null`（返回落点行/组头元素，空白或交互子元素返回 null；`sidebarPress.ts`）
  - `type SidebarPressTarget = { kind: "thread"; row: HTMLElement } | { kind: "group"; header: HTMLElement }`（由 Task 2 的 `resolveTarget` 从上面的元素再读 `dataset` 得到）
  - 常量 `LONG_PRESS_MS = 500`、`MOVE_TOLERANCE_PX = 10`、`PRESS_IGNORE_SELECTOR`

- [ ] **Step 1: 写失败测试（落点判定）**

新建 `desktop/src/lib/sidebarPress.test.ts`：

```ts
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { resolveSidebarPressTarget } from "./sidebarPress";

afterEach(() => {
  document.body.innerHTML = "";
});

/** 造一棵与侧栏同构的迷你 DOM：组头 + 一条会话行（含一个按钮）。 */
function tree(): { header: HTMLElement; row: HTMLElement; pin: HTMLElement; blank: HTMLElement } {
  document.body.innerHTML = `
    <aside>
      <div data-ws-header=""><span id="h-text">projA</span></div>
      <div data-thread-row="t1"><span id="r-text">alpha</span><button id="pin">pin</button></div>
      <div id="blank">footer</div>
    </aside>
  `;
  return {
    header: document.querySelector("[data-ws-header]")!,
    row: document.querySelector("[data-thread-row]")!,
    pin: document.querySelector("#pin")!,
    blank: document.querySelector("#blank")!,
  };
}

describe("resolveSidebarPressTarget", () => {
  it("resolves a press inside a thread row to that row", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#r-text"))).toBe(
      document.querySelector("[data-thread-row]"),
    );
  });

  it("resolves a press inside a workspace header to that header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#h-text"))).toBe(
      document.querySelector("[data-ws-header]"),
    );
  });

  it("returns null for a press on an interactive child (pin / delete button)", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#pin"))).toBeNull();
  });

  it("returns null for a press outside any row or header", () => {
    tree();
    expect(resolveSidebarPressTarget(document.querySelector("#blank"))).toBeNull();
  });

  it("returns null for a null target", () => {
    expect(resolveSidebarPressTarget(null)).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/sidebarPress.test.ts`
Expected: FAIL —`Cannot find module './sidebarPress'`（文件尚未创建）。

- [ ] **Step 3: 实现落点判定**

新建 `desktop/src/lib/sidebarPress.ts`：

```ts
/**
 * 侧栏「长按落点」判定：把一次 pointerdown 的目标解析成它所属的可长按元素。
 *
 * 抽成纯函数（只读 DOM 属性、不碰 React）是为了能脱组件单测——它锁住的契约是
 * 「按在会话行上 → 该行；按在组头（文件夹）上 → 该组头；按在行内交互控件或空白处
 * → 什么都不做」。这条契约错了会表现成两种相反的故障：漏判 = 长按没反应，
 * 多判 = 想点图钉却弹出菜单。
 */

/** 交互控件：按在这些上面必须走原生行为，不能当长按。 */
export const PRESS_IGNORE_SELECTOR = "button, input, a, textarea, select";

/** 会话行 / 工作区分组头的标记属性（由 ThreadSidebar 渲染时写上）。 */
export const THREAD_ROW_ATTR = "data-thread-row";
export const WORKSPACE_HEADER_ATTR = "data-ws-header";

/** 长按落点：会话行或工作区分组头（文件夹）。 */
export function resolveSidebarPressTarget(target: Element | null): HTMLElement | null {
  if (target === null) return null;
  if (target.closest(PRESS_IGNORE_SELECTOR)) return null;
  const row = target.closest<HTMLElement>(`[${THREAD_ROW_ATTR}]`);
  if (row) return row;
  return target.closest<HTMLElement>(`[${WORKSPACE_HEADER_ATTR}]`);
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/sidebarPress.test.ts`
Expected: PASS（5 passed）。

- [ ] **Step 5: 写失败测试（hook）**

新建 `desktop/src/lib/useLongPress.test.ts`（整个文件）：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, act } from "@testing-library/react";
import { useLongPress, LONG_PRESS_MS, type LongPressTarget } from "./useLongPress";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

/**
 * 测试夹具：把 hook 挂在一个容器上，容器里有「会话行」和「组头」两个可长按元素。
 * `onLongPress` 记录落点，`resolver` 复用与 ThreadSidebar 相同的 dataset 读法。
 */
function Harness({
  enabled = true,
  onLongPress,
}: {
  enabled?: boolean;
  onLongPress: (t: LongPressTarget) => void;
}) {
  const lp = useLongPress({
    enabled,
    resolveTarget: (e) => {
      const el = (e.target as Element).closest<HTMLElement>("[data-thread-row], [data-ws-header]");
      if (!el) return null;
      if (el.dataset.threadRow !== undefined) return { kind: "thread", id: el.dataset.threadRow, el };
      return { kind: "group", ws: el.dataset.wsHeader ?? "", el };
    },
    onLongPress,
  });
  return (
    <div {...lp}>
      <div data-ws-header="/work/projA">
        <span data-testid="header-text">projA</span>
      </div>
      <div data-thread-row="t1">
        <span data-testid="row-text">alpha</span>
        <button data-testid="pin">pin</button>
      </div>
    </div>
  );
}

const pressDown = (el: Element, extra: Record<string, unknown> = {}) =>
  fireEvent.pointerDown(el, { clientX: 5, clientY: 5, button: 0, ...extra });

const advance = async (ms: number) => {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
};

describe("useLongPress", () => {
  it("fires once after the threshold, on a thread row", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("row-text"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress).toHaveBeenCalledTimes(1);
    expect(onLongPress.mock.calls[0][0]).toMatchObject({ kind: "thread", id: "t1" });
  });

  it("fires on a workspace header", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("header-text"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress.mock.calls[0][0]).toMatchObject({ kind: "group", ws: "/work/projA" });
  });

  it("does not fire on a short press", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("row-text"));
    fireEvent.pointerUp(window, { pointerId: 0 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("cancels when the finger moves past the tolerance", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("row-text"));
    fireEvent.pointerMove(window, { pointerId: 0, clientX: 100, clientY: 100 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("cancels on pointercancel (the system's scroll takeover)", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("row-text"));
    fireEvent.pointerCancel(window, { pointerId: 0 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("ignores a second finger", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("row-text"));
    fireEvent.pointerDown(window, { pointerId: 7, clientX: 5, clientY: 5 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("swallows the click that follows a long press", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const onSelect = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    // 用行自己的 onClick 验证「吞 click」：长按后紧跟的 click 不能到 React。
    getByTestId("row-text").closest("[data-thread-row]")!.addEventListener("click", onSelect);
    pressDown(getByTestId("row-text"));
    await advance(LONG_PRESS_MS);
    fireEvent.click(getByTestId("row-text"));
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("stays inert when disabled", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness enabled={false} onLongPress={onLongPress} />);
    pressDown(getByTestId("row-text"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("does not start from an interactive child", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(<Harness onLongPress={onLongPress} />);
    pressDown(getByTestId("pin"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 6: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/useLongPress.test.ts`
Expected: FAIL —`Cannot find module './useLongPress'`。

- [ ] **Step 7: 实现 hook**

新建 `desktop/src/lib/useLongPress.ts`：

```ts
import { useCallback, useEffect, useRef } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { PRESS_IGNORE_SELECTOR } from "./sidebarPress";

/** 按住多久算长按。 */
export const LONG_PRESS_MS = 500;
/** 手指允许漂移多少像素；超过即认定为滚动/拖动，取消长按。 */
export const MOVE_TOLERANCE_PX = 10;

/** 长按落点：会话行或工作区分组头（文件夹）。 */
export type LongPressTarget =
  | { kind: "thread"; id: string; el: HTMLElement }
  | { kind: "group"; ws: string; el: HTMLElement };

/** 由调用方把 pointerdown 的目标解析成落点；返回 null 表示这次按下不算长按。 */
export type ResolveTarget = (e: ReactPointerEvent<HTMLElement>) => LongPressTarget | null;

export interface LongPressOptions {
  /** false 时完全不装监听（桌面端走这条）。 */
  enabled: boolean;
  resolveTarget: ResolveTarget;
  onLongPress: (target: LongPressTarget) => void;
  thresholdMs?: number;
  moveTolerancePx?: number;
}

/** 原生 click 吞掉后多久自动摘掉监听：久到够覆盖那一次合成 click，又不至于吃掉下一次真点击。 */
const SWALLOW_WINDOW_MS = 400;

/**
 * 长按识别，委托式：挂在侧栏容器上，一份监听管住所有会话行与组头。
 *
 * 为什么是委托而不是每行一个 hook：行由 `renderThread` 在 `.map()` 里渲染，
 * 组数随数据变化，行内调 hook 会违反 Hooks 规则（渲染间的 hook 数量必须稳定）。
 * 委托把唯一的 hook 钉在组件顶层，落点交给 `resolveTarget` 判定。
 *
 * 为什么不用 `contextmenu`：iOS 的 WKWebView 长按**不派发**它（弹的是文字选择
 * callout），Android 则派发——同一份代码两端行为不一致。Pointer Events 两端一致。
 *
 * 为什么触发后要吞 click：长按会话行的同时，手指抬起会补一次 click，那会走
 * `onSelect` → `thread/resume`。吞掉它，长按才是「打开菜单」而不是「选中会话」。
 *
 * 为什么不 `preventDefault` pointerdown：那会连抽屉的滚动一起废掉。滚动改由
 * `pointercancel`（系统接管手势时派发）与位移阈值取消。
 */
export function useLongPress({
  enabled,
  resolveTarget,
  onLongPress,
  thresholdMs = LONG_PRESS_MS,
  moveTolerancePx = MOVE_TOLERANCE_PX,
}: LongPressOptions): { onPointerDown: (e: ReactPointerEvent<HTMLElement>) => void } {
  // 回调放 ref：事件监听在 pointerdown 时一次性注册，闭包不应随每次渲染重建。
  const cb = useRef(onLongPress);
  const resolve = useRef(resolveTarget);
  useEffect(() => {
    cb.current = onLongPress;
    resolve.current = resolveTarget;
  }, [onLongPress, resolveTarget]);

  const timer = useRef<number | null>(null);
  const origin = useRef<{ x: number; y: number } | null>(null);
  const activeId = useRef<number | null>(null);
  const disposers = useRef<(() => void)[]>([]);
  const swallowTimer = useRef<number | null>(null);

  /** 收尾：清计时器、摘监听、复位状态。幂等，可重复调用。 */
  const end = useCallback(() => {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
    origin.current = null;
    activeId.current = null;
    for (const dispose of disposers.current) dispose();
    disposers.current = [];
  }, []);

  // 卸载时务必清干净：否则组件没了，window 上还挂着监听和一个待触发的计时器。
  useEffect(() => end, [end]);

  // 冒泡阶段 stopPropagation，把这次 click 挡在 React 根委托之外（React 19 在根上
  // 监听，元素自身的冒泡监听先跑，能拦住它）。捕获阶段拦不住委托。
  const swallow = (ev: Event) => {
    ev.stopPropagation();
    ev.preventDefault();
  };

  const onPointerDown = (e: ReactPointerEvent<HTMLElement>) => {
    if (!enabled) return;
    // 只认主按钮（鼠标左键 / 单指）。
    if (e.button !== 0) return;
    // 已有一根手指在计时：交给「第二指针」处理，不重复起手势。
    if (activeId.current !== null) return;
    const target = resolve.current(e);
    if (target === null) return;
    const el = target.el;

    const id = e.pointerId;
    activeId.current = id;
    origin.current = { x: e.clientX, y: e.clientY };

    const onMove = (ev: PointerEvent) => {
      if (ev.pointerId !== activeId.current) return;
      const o = origin.current;
      if (!o) return;
      if (Math.hypot(ev.clientX - o.x, ev.clientY - o.y) > moveTolerancePx) end();
    };
    const onRelease = (ev: PointerEvent) => {
      if (ev.pointerId === activeId.current) end();
    };
    const onSecondPointer = (ev: PointerEvent) => {
      if (ev.pointerId !== id) end();
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onRelease);
    window.addEventListener("pointercancel", onRelease);
    window.addEventListener("pointerdown", onSecondPointer);
    disposers.current.push(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onRelease);
      window.removeEventListener("pointercancel", onRelease);
      window.removeEventListener("pointerdown", onSecondPointer);
    });

    timer.current = window.setTimeout(() => {
      timer.current = null;
      const stillActive = activeId.current === id;
      end();
      if (!stillActive) return;
      el.addEventListener("click", swallow);
      if (swallowTimer.current !== null) window.clearTimeout(swallowTimer.current);
      swallowTimer.current = window.setTimeout(() => {
        el.removeEventListener("click", swallow);
        swallowTimer.current = null;
      }, SWALLOW_WINDOW_MS);
      cb.current(target);
    }, thresholdMs);
  };

  return { onPointerDown };
}

export { PRESS_IGNORE_SELECTOR };
```

- [ ] **Step 8: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/useLongPress.test.ts src/lib/sidebarPress.test.ts`
Expected: PASS（9 + 5 passed）。

- [ ] **Step 9: 类型检查 + 提交**

```bash
export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit
git add desktop/src/lib/useLongPress.ts desktop/src/lib/useLongPress.test.ts desktop/src/lib/sidebarPress.ts desktop/src/lib/sidebarPress.test.ts
git commit -m "feat(desktop): 长按手势 hook 与侧栏落点判定"
```

---

### Task 2: 侧栏接线（菜单状态、长按、isMobile 门控）

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Modify: `desktop/src/App.tsx:1512-1538`（`<ThreadSidebar ... />` 传 `isMobile`）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: Task 1 的 `useLongPress` / `LONG_PRESS_MS` / `LongPressTarget` / `PRESS_IGNORE_SELECTOR`；`sidebarPress.ts` 的 `THREAD_ROW_ATTR` / `WORKSPACE_HEADER_ATTR`。
- Produces（`ThreadSidebar` 新 prop）：`isMobile?: boolean`（默认 `false`）。未传 = 桌面 = 现状。
- 新增 DOM 标记：会话行 `data-thread-row=""`、组头 `data-ws-header=""`、侧栏容器 `data-sidebar-longpress=""`（当 `isMobile` 为真时）。
- 新增 DOM 标记：会话菜单面板 `data-thread-menu=""`。

- [ ] **Step 1: 写失败测试**

把下面两个 describe 追加到 `desktop/src/components/ThreadSidebar.test.tsx` 末尾（文件首行已是 `/** @vitest-environment jsdom */`；顶部 import 需补 `act`、`afterEach`、`LONG_PRESS_MS`）：

```tsx
// 顶部 import 行改成：
// import { describe, it, expect, vi, afterEach } from "vitest";
// import { render, fireEvent, cleanup, screen, act } from "@testing-library/react";
// import { LONG_PRESS_MS } from "../lib/useLongPress";

/**
 * 手机端长按（= 右键）。落点由 hook 委托判定，故这里直接对行/组头做 pointerdown，
 * 再用 fake timers 推过阈值。
 *
 * 推进必须用 `act(async () => await vi.advanceTimersByTimeAsync(...))`：同步的
 * `advanceTimersByTime` 不 flush React 更新，会看到菜单还没渲染。
 */
async function longPress(el: Element) {
  await act(async () => {
    fireEvent.pointerDown(el, { clientX: 5, clientY: 5, button: 0 });
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);
  });
}

describe("ThreadSidebar 手机长按", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("长按会话行打开会话菜单且不选中会话", async () => {
    vi.useFakeTimers();
    const onSelect = vi.fn();
    const { container } = renderSidebar({ isMobile: true, onSelect });

    await longPress(screen.getByText("alpha-thread"));

    expect(container.querySelector('[data-thread-menu=""]')).not.toBeNull();
    expect(screen.getByText("重命名")).toBeTruthy();
    expect(screen.getByText("删除")).toBeTruthy();
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("长按会话行的「重命名」进入编辑态", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar({ isMobile: true });

    await longPress(screen.getByText("alpha-thread"));
    fireEvent.click(screen.getByText("重命名"));

    expect(container.querySelector("input")).not.toBeNull();
    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
  });

  it("长按会话行的「置顶」切换置顶，且不选中会话", async () => {
    vi.useFakeTimers();
    const onTogglePin = vi.fn();
    const onSelect = vi.fn();
    renderSidebar({ isMobile: true, onTogglePin, onSelect });

    await longPress(screen.getByText("alpha-thread"));
    fireEvent.click(screen.getByText("置顶"));

    expect(onTogglePin).toHaveBeenCalledWith("1", true);
    // 菜单渲染在行 div 内部，而行 div 自己 onClick=onSelect；点菜单项不能连带选中。
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("已置顶的行菜单显示「取消置顶」", async () => {
    vi.useFakeTimers();
    const onTogglePin = vi.fn();
    renderSidebar({ isMobile: true, pinned: [pinnedThread("9", "pin-a")], onTogglePin });

    await longPress(screen.getByText("pin-a"));
    fireEvent.click(screen.getByText("取消置顶"));

    expect(onTogglePin).toHaveBeenCalledWith("9", false);
  });

  it("长按会话行后确认「删除」才调 onDelete", async () => {
    vi.useFakeTimers();
    const onDelete = vi.fn();
    const confirmSpy = vi.spyOn(window, "confirm").mockReturnValue(true);
    renderSidebar({ isMobile: true, onDelete });

    await longPress(screen.getByText("alpha-thread"));
    fireEvent.click(screen.getByText("删除"));

    expect(confirmSpy).toHaveBeenCalled();
    expect(onDelete).toHaveBeenCalledWith("1");
    confirmSpy.mockRestore();
  });

  it("长按组头打开工作区菜单", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar({ isMobile: true });

    const header = container.querySelector<HTMLElement>("[data-ws-header]")!;
    await longPress(header);

    const menu = container.querySelector('[role="menu"]');
    expect(menu).not.toBeNull();
    expect(menu!.textContent).toContain("New thread here");
    expect(menu!.textContent).toContain("Remove from list");
  });

  it("Escape 关闭会话菜单", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar({ isMobile: true });

    await longPress(screen.getByText("alpha-thread"));
    expect(container.querySelector('[data-thread-menu=""]')).not.toBeNull();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
  });

  it("桌面端（未传 isMobile）长按不开任何菜单", async () => {
    vi.useFakeTimers();
    const { container } = renderSidebar();

    await longPress(screen.getByText("alpha-thread"));

    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
    expect(container.querySelector('[role="menu"]')).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL —新用例报 `data-thread-menu` 找不到（`null`），组头用例也可能因 `[data-ws-header]` 为 `null` 而抛错。

- [ ] **Step 3: 接线**

在 `ThreadSidebar.tsx` 里做以下改动。

**(a) import：**

```ts
import { useLongPress, type LongPressTarget } from "../lib/useLongPress";
import { resolveSidebarPressTarget } from "../lib/sidebarPress";
```

**(b) props 增加 `isMobile`**（在解构参数里，`onOpenSettings` 之后）：

```ts
  /** 手机端才挂长按（= 右键）；桌面端保持现状，不装监听、不渲染会话菜单。 */
  isMobile?: boolean;
```

并在参数里给默认值：`isMobile = false,`。

**(c) 菜单状态：把 `contextWs` 提升为 `menu`。** 把

```ts
  const [contextWs, setContextWs] = useState<string | null>(null);
```

改为

```ts
  // 侧栏自绘菜单：新建下拉（newMenuOpen）与长按/右键菜单（menu）三者互斥。
  // menu 用联合类型：`group` 是桌面右键那份工作区菜单，`thread` 是手机长按新增的
  // 会话菜单——同一个槽位保证两种菜单不会同时开着。
  const [menu, setMenu] = useState<
    { kind: "group"; ws: string } | { kind: "thread"; id: string } | null
  >(null);
```

**(d) `closeMenus`：**

```ts
  const closeMenus = useCallback(() => {
    setNewMenuOpen(false);
    setMenu(null);
  }, []);
```

**(e) Escape effect 的条件：** 把 `if (!newMenuOpen && contextWs === null) return;` 改为
`if (!newMenuOpen && menu === null) return;`，依赖数组里的 `contextWs` 改为 `menu`。

**(f) 长按接线（放在 `closeMenus` 定义之后、`commit` 之前）：**

```ts
  // 长按 → 打开对应菜单。落点判定复用 sidebarPress 的标记属性；桌面端 enabled=false
  // 时 hook 不装任何监听。
  const longPress = useLongPress({
    enabled: isMobile,
    resolveTarget: (e): LongPressTarget | null => {
      const el = resolveSidebarPressTarget(e.target as Element);
      if (!el) return null;
      // 编辑态的行不弹菜单（否则长按会打断正在输入的标题）。
      if (el.dataset.threadRow !== undefined) {
        if (editingId === el.dataset.threadRow) return null;
        return { kind: "thread", id: el.dataset.threadRow, el };
      }
      return { kind: "group", ws: el.dataset.wsHeader ?? "", el };
    },
    onLongPress: (t) => {
      setNewMenuOpen(false);
      setMenu(
        t.kind === "thread" ? { kind: "thread", id: t.id } : { kind: "group", ws: t.ws },
      );
    },
  });
```

**(g) 组头：** 把 `onContextMenu` 里的 `setContextWs(g.workspace)` 改为
`setMenu({ kind: "group", ws: g.workspace })`；`aria-expanded={contextWs === g.workspace}`
改为 `aria-expanded={menu?.kind === "group" && menu.ws === g.workspace}`；键盘分支里的
`setContextWs((cur) => (cur === g.workspace ? null : g.workspace))` 改为：

```ts
                  setMenu((cur) =>
                    cur?.kind === "group" && cur.ws === g.workspace
                      ? null
                      : { kind: "group", ws: g.workspace },
                  );
```

组头 div 上再加 `data-ws-header=""`，并把菜单的展开条件
`{contextWs === g.workspace && (` 改为
`{menu?.kind === "group" && menu.ws === g.workspace && (`。

**(h) 会话行（`renderThread`）：** 在行的 `<div key={t.thread_id} {...dataAttr}` 上再加
`data-thread-row={t.thread_id}`；在 `renderThread` 的 `return` 之后（行 `<div>` 内部、
`</div>` 收尾之前）加会话菜单渲染（见 (i)）。

**(i) 会话菜单 JSX（放在 `renderThread` 里行 div 的最后、`edit` 分支之外）：**

注意 backdrop 与菜单面板都必须 `stopPropagation`：它们渲染在**行 div 内部**，而行
div 自身有 `onClick={onSelect}`（`ThreadSidebar.tsx:366`），不拦住的话点任何菜单项
都会冒泡上去顺手选中一次会话（一次多余的 `thread/resume`）。

```tsx
        {menu?.kind === "thread" && menu.id === t.thread_id && (
          <>
            <div
              className="fixed inset-0 z-10"
              onClick={(e) => {
                e.stopPropagation();
                closeMenus();
              }}
            />
            <div
              role="menu"
              data-thread-menu=""
              onClick={(e) => e.stopPropagation()}
              className="absolute top-full left-3 z-20 mt-0.5 min-w-32 rounded-md border border-line-strong bg-raised py-1 shadow-xl"
            >
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  closeMenus();
                  setEditingId(t.thread_id);
                  setDraft(t.title ?? "");
                }}
                className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
              >
                重命名
              </button>
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  closeMenus();
                  onTogglePin(t.thread_id, !isPinned);
                }}
                className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
              >
                {isPinned ? "取消置顶" : "置顶"}
              </button>
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  closeMenus();
                  onDelete(t.thread_id);
                }}
                className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
              >
                删除
              </button>
            </div>
          </>
        )}
```

注意：会话菜单要能被 `absolute` 定位到行上，行的 className 已经是
`relative flex items-center ...`（`ThreadSidebar.tsx:373`），无需再改。

**(j) 侧栏容器挂长按：** `<aside className="relative flex shrink-0 ...">` 上展开
`{...(isMobile ? longPress : {})}`。

> 说明：`data-thread-row` 的值是 thread_id，故行上直接写
> `data-thread-row={t.thread_id}`；组头的标记是空值属性 `data-ws-header=""`。
> 两者与 `sidebarPress.ts` 的 `THREAD_ROW_ATTR` / `WORKSPACE_HEADER_ATTR` 必须同名，
> 否则长按静默失效。

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（原 52 例 + 新增 8 例）。

- [ ] **Step 5: App.tsx 传 isMobile**

在 `desktop/src/App.tsx` 的 `<ThreadSidebar ... />` 上，`onOpenSettings={...}` 之前加一行：

```tsx
          isMobile={isMobile}
```

- [ ] **Step 6: 全量测试 + 类型检查**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit && npx vitest run`
Expected: tsc 退出 0；vitest 全绿（含 `App.test.tsx`）。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): 侧栏长按（= 右键）：组头工作区菜单 + 会话行菜单"
```

---

### Task 3: 手机端抑制原生长按手势的 CSS

**Files:**
- Modify: `desktop/src/index.css`（`[data-mobile="true"]` 段内）
- Test: `desktop/src/mobileLayout.test.ts`
- Modify: `docs/project-management/desktop.md`
- Modify: `README.md`

**Interfaces:**
- Consumes: Task 2 写入的 `data-thread-row` 标记（CSS 选择器依赖它）。
- Produces: 无对外接口，纯样式与文档。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/mobileLayout.test.ts` 的 `describe("phone layout CSS", ...)` 里追加：

```ts
  it("suppresses the native long-press callout on sidebar thread rows", () => {
    // 按住会话行时 iOS 会先弹文字选择/放大镜 callout，把长按抢走——长按=右键就
    // 永远触发不了。这三条都必须限定在 [data-mobile="true"] 下，桌面端逐字节不变。
    const rule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+\[data-thread-row\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/-webkit-touch-callout:\s*none/);
    expect(rule).toMatch(/-webkit-user-select:\s*none/);
    expect(rule).toMatch(/user-select:\s*none/);
  });

  it("keeps the rename field's text selectable on the phone", () => {
    // 关掉 user-select 后，重命名输入框里选不中字——那是同一段 CSS 的必须豁免。
    const rule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+\[data-thread-row\]\s+input\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/user-select:\s*text/);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/mobileLayout.test.ts`
Expected: FAIL —— 两条新用例的 `rule` 为空串（`expected '' to match ...`）。

- [ ] **Step 3: 加 CSS**

在 `desktop/src/index.css` 里，紧跟 `[data-mobile="true"] .app-sidebar aside { ... }` 规则之后追加：

```css
/*
 * 手机端长按（= 右键）的前提：按住会话行时，iOS 会先弹文字选择 / 放大镜 callout，
 * 把长按手势整个抢走——`useLongPress` 便永远等不到那 500ms。这三条把原生手势关掉，
 * 长按才是「打开菜单」而不是「选中文字」。
 *
 * 只作用于手机（选择器带 `[data-mobile="true"]`，由 `useIsMobile` 设在 `<html>` 上），
 * 桌面端不匹配，布局与行为逐字节不变。
 */
[data-mobile="true"] .app-sidebar [data-thread-row] {
  -webkit-touch-callout: none;
  -webkit-user-select: none;
  user-select: none;
}
/* 行内唯一需要选字的地方是重命名输入框，单独豁免，否则改名时选不中字。 */
[data-mobile="true"] .app-sidebar [data-thread-row] input {
  -webkit-user-select: text;
  user-select: text;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/mobileLayout.test.ts`
Expected: PASS（原 7 例 + 新增 2 例 = 9）。

- [ ] **Step 5: 登记文档**

在 `docs/project-management/desktop.md` 的 Features 列表里，紧跟「输入框草稿按会话隔离」那一条之后，加一行：

```markdown
- [x] 手机端侧栏长按（= 右键）— 工作区分组头长按弹桌面右键那份工作区菜单（New thread here / 创建·移除看板 / Remove from list），会话行长按弹会话菜单（重命名 / 置顶·取消置顶 / 删除），长按不误触发选中（吞掉跟随 click）；桌面端不装监听、不渲染会话菜单。代码 `desktop/src/lib/useLongPress.ts`（委托式长按识别，含吞 click 与 pointercancel/位移取消）、`desktop/src/lib/sidebarPress.ts`（落点判定）、`desktop/src/components/ThreadSidebar.tsx`（`isMobile` prop + `menu` 联合状态 + 会话菜单）、`desktop/src/App.tsx`（传 `isMobile`）、`desktop/src/index.css`（`[data-mobile="true"]` 下关闭 `-webkit-touch-callout`/`user-select`）；判据：`cd desktop && npx vitest run src/lib/useLongPress.test.ts src/lib/sidebarPress.test.ts src/components/ThreadSidebar.test.tsx src/mobileLayout.test.ts && npx tsc --noEmit`（回退 hook 的取消/吞 click、或漏掉 CSS 那三条即失败）
```

然后把同文件末尾的 **验证命令** 一行按实际跑出的数字更新（新增 2 个测试文件
`useLongPress.test.ts` 9、`sidebarPress.test.ts` 5；`ThreadSidebar.test.tsx` 52→60；
`mobileLayout.test.ts` 7→9；总文件数 67→69），并同步 `README.md` 模块索引表的
`desktop` 行计数（`34 / 47` → `36 / 49`）。

- [ ] **Step 6: 全量测试 + 提交**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全绿。

```bash
git add desktop/src/index.css desktop/src/mobileLayout.test.ts docs/project-management/desktop.md README.md
git commit -m "feat(desktop): 手机端长按抑制原生 callout；登记 desktop 模块进度"
```

---

## Self-Review

**1. Spec coverage：**
- §2 决策 1（组头 → 工作区菜单；行 → 会话菜单）→ Task 2 Step 3(g)(i)。
- §2 决策 2（不做整组区域）→ 落点只认 `[data-thread-row]` / `[data-ws-header]`（Task 1 Step 3）。
- §2 决策 3（Pointer Events）→ Task 1 Step 7。
- §2 决策 4（500ms/10px）→ `LONG_PRESS_MS` / `MOVE_TOLERANCE_PX`（Task 1 Step 7）。
- §2 决策 5（吞 click）→ Task 1 Step 7 `swallow` + Task 1 Step 5 用例。
- §2 决策 6（桌面门控）→ `enabled: isMobile` + Task 2 Step 1 桌面用例。
- §4 hook 契约（取消条件、第二指针、卸载清 timer、可测性）→ Task 1 Step 5 全部用例。
- §5 状态提升（`menu` 联合、三者互斥、Escape/backdrop）→ Task 2 Step 3(c)(d)(e)(i)。
- §7 CSS → Task 3 Step 3 + 静态门禁 Step 1。
- §8 测试与验证 → 三个 Task 的测试步骤 + 全量命令。
- §3/§5 的「置顶区与会话区一并覆盖」→ 会话菜单写在 `renderThread` 内，置顶区复用同一函数。
- §8 的「项目进度维护」（CLAUDE.md 强制）→ Task 3 Step 5。

**2. Placeholder scan：** 无 TBD/TODO；每个代码步骤都给了可直接粘贴的完整内容。

**3. Type consistency：** `LongPressTarget`（Task 1 定义）在 Task 2 `resolveTarget` 与 `onLongPress` 中签名一致；`THREAD_ROW_ATTR`/`WORKSPACE_HEADER_ATTR` 在 Task 1 定义，Task 2 的 CSS 与选择器写法与之对齐（`[data-thread-row]` / `[data-ws-header]`）；`menu` 联合的 `{kind,ws}` / `{kind,id}` 在 (c)(f)(g)(i) 处处一致。

**4. 已知取舍：**
- 组头长按与桌面右键共用 `menu.kind === "group"` 槽位，故桌面右键打开的菜单与手机长按打开的是同一份渲染——这正是「长按 = 右键」。
- 行菜单用 `absolute top-full left-3`，在被 `overflow-y-auto` 裁剪的边界场景可能被裁（与既有组菜单同款局限，spec §10 已记录为已知限制，不在本次范围）。
