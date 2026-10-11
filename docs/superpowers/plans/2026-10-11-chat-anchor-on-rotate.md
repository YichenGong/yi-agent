# 会话区旋转/视口变化后保持贴底 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `ChatView` 在手机旋转（及一切视口高度变化）后保持用户的「贴底意图」——原本贴底的重新贴底，原本在翻历史的不被拽走。

**Architecture:** 新增一个小 hook `useStickToBottom`：用容器自身的 `ResizeObserver` 感知高度变化，用容器的 `scroll` 事件维护「高度变化前是否贴底」的 `atBottomRef`，在高度变化且原本贴底时于下一帧把 `scrollTop` 拉到底。`ChatView` 用它替换现有的 `endRef` + `scrollIntoView` 贴底。

**Tech Stack:** React 19、TypeScript、Vite、Vitest（jsdom）、Tailwind（已有，不改）。

## Global Constraints

- 只在 `desktop/` 前端改动，不碰 Rust / iOS 工程 / `index.css`。
- 不改任何既有文案与 `[data-mobile]` CSS；不引入新的 npm 依赖。
- 部署目标 iOS 14.0（`desktop/src-tauri/gen/apple/desktop.xcodeproj/project.pbxproj` `IPHONEOS_DEPLOYMENT_TARGET = 14.0`）≥ `ResizeObserver` 所需 13.4；仍须守卫 `typeof ResizeObserver !== "undefined"`，因为 **jsdom 不提供它**（已实测 `typeof ResizeObserver === "undefined"`）。
- 提交信息用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 提交前须 `npx tsc --noEmit` 通过；`tsconfig` 开了 `noUnusedLocals`/`noUnusedParameters`，删掉的引用必须一并从 import 里清理。
- 执行环境：worktree 的 `desktop/node_modules` 可能不存在；测试前先使其可用（`cd desktop && npm ci`，或从主仓库软链：`ln -s ../../../desktop/node_modules desktop/node_modules`）。命令一律在 `desktop/` 下执行。
- 每个 Task 结束都必须 `cd desktop && npx tsc --noEmit && npx vitest run` 通过（至少跑本 Task 相关文件），再 commit。
- 参考 spec：`docs/superpowers/specs/2026-10-11-chat-anchor-on-rotate-design.md`。

---

## File Structure

- Create: `desktop/src/lib/useStickToBottom.ts` — 单一职责：维持一个滚动容器的贴底意图。
- Create: `desktop/src/lib/useStickToBottom.test.ts` — 该 hook 的单元测试。
- Modify: `desktop/src/components/ChatView.tsx` — 用 hook 替换 `endRef`/`scrollIntoView` 贴底。
- Modify: `desktop/src/components/ChatView.test.tsx` — 移除已无用的 `scrollIntoView` 打桩。
- Modify: `docs/bug-list.md` — 登记该 bug 的根因与修复。

---

### Task 1: `useStickToBottom` hook

**Files:**
- Create: `desktop/src/lib/useStickToBottom.ts`
- Test: `desktop/src/lib/useStickToBottom.test.ts`

**Interfaces:**
- Consumes: 无（纯前端）。
- Produces:
  - `export const STICK_BOTTOM_THRESHOLD_PX = 4;`
  - `export function isAtBottom(el: Pick<HTMLElement, "scrollHeight" | "clientHeight" | "scrollTop">, thresholdPx?: number): boolean`
  - `export function useStickToBottom(): { ref: RefObject<HTMLDivElement | null>; scrollToBottom: () => void }`

  注：spec §4.1 里画的是泛型 `<T extends HTMLElement>`；本计划收窄为 `HTMLDivElement`（唯一消费者是一个 `<div>`，YAGNI）。

- [x] **Step 1: 写失败测试**

创建 `desktop/src/lib/useStickToBottom.test.ts`（`.ts` 非 `.tsx`，故用 `createElement`，与 `lib/useLongPress.test.ts` 同构）：

```ts
/** @vitest-environment jsdom */
import { createElement, useEffect } from "react";
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, fireEvent, cleanup, act } from "@testing-library/react";
import {
  useStickToBottom,
  isAtBottom,
  STICK_BOTTOM_THRESHOLD_PX,
} from "./useStickToBottom";

/** 可控的 ResizeObserver 替身：测试手动把「尺寸变化」喂给回调。 */
type RoCallback = (entries: Array<{ contentRect: { height: number } }>) => void;
let observers: Array<{ cb: RoCallback }> = [];
class FakeResizeObserver {
  cb: RoCallback;
  targets: Element[] = [];
  constructor(cb: RoCallback) {
    this.cb = cb;
    observers.push(this);
  }
  observe(t: Element) {
    this.targets.push(t);
  }
  unobserve() {}
  disconnect() {
    this.targets = [];
  }
}

let rafCallbacks = new Map<number, FrameRequestCallback>();
let rafId = 0;
const flushRaf = () => {
  const cbs = Array.from(rafCallbacks.values());
  rafCallbacks.clear();
  cbs.forEach((cb) => cb(0));
};

let api: { scrollToBottom: () => void } | null = null;

beforeEach(() => {
  observers = [];
  rafCallbacks = new Map();
  rafId = 0;
  api = null;
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
    const id = ++rafId;
    rafCallbacks.set(id, cb);
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => {
    rafCallbacks.delete(id);
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function Harness() {
  const stick = useStickToBottom();
  useEffect(() => {
    api = { scrollToBottom: stick.scrollToBottom };
  }, [stick.scrollToBottom]);
  return createElement("div", { ref: stick.ref, "data-testid": "scroller" }, "x");
}

/** 给 jsdom（无布局引擎）的元素装上可读写的滚动度量。 */
function installMetrics(
  el: HTMLElement,
  init: { scrollHeight: number; clientHeight: number; scrollTop?: number },
) {
  const state = {
    scrollHeight: init.scrollHeight,
    clientHeight: init.clientHeight,
    scrollTop: init.scrollTop ?? 0,
  };
  Object.defineProperty(el, "scrollHeight", { configurable: true, get: () => state.scrollHeight });
  Object.defineProperty(el, "clientHeight", { configurable: true, get: () => state.clientHeight });
  Object.defineProperty(el, "scrollTop", {
    configurable: true,
    get: () => state.scrollTop,
    set: (v: number) => {
      state.scrollTop = v;
    },
  });
  return state;
}

const fireResize = (height: number) => {
  act(() => {
    observers.forEach((o) => o.cb([{ contentRect: { height } }]));
  });
};

describe("isAtBottom", () => {
  it("is true when scrolled to the bottom", () => {
    expect(isAtBottom({ scrollHeight: 1000, clientHeight: 400, scrollTop: 600 })).toBe(true);
  });
  it("is false when not at the bottom", () => {
    expect(isAtBottom({ scrollHeight: 1000, clientHeight: 400, scrollTop: 100 })).toBe(false);
  });
  it("is true when the content fits (no overflow)", () => {
    expect(isAtBottom({ scrollHeight: 300, clientHeight: 400, scrollTop: 0 })).toBe(true);
  });
  it("tolerates sub-pixel gaps within the threshold", () => {
    const at =
      STICK_BOTTOM_THRESHOLD_PX; /* e.g. 4 */
    expect(isAtBottom({ scrollHeight: 1000, clientHeight: 400, scrollTop: 600 - at })).toBe(true);
    expect(isAtBottom({ scrollHeight: 1000, clientHeight: 400, scrollTop: 600 - at - 1 })).toBe(
      false,
    );
  });
});

describe("useStickToBottom", () => {
  it("re-pins to the bottom after a height change when it was at the bottom", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el); // user is at the bottom
    fireResize(400); // baseline height
    state.clientHeight = 300; // rotate: viewport shrank; scrollTop stays absolute
    fireResize(300);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(1000);
  });

  it("leaves the position alone when the user was reading history", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 100 });
    fireEvent.scroll(el); // user is mid-history
    fireResize(400);
    state.clientHeight = 300;
    fireResize(300);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(100);
  });

  it("exposes a command that pins to the bottom immediately", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 0 });
    act(() => api!.scrollToBottom());
    expect(state.scrollTop).toBe(1000);
  });

  it("coalesces several size changes in one frame into a single pin", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el);
    fireResize(400);
    state.clientHeight = 300;
    fireResize(300);
    state.clientHeight = 280;
    fireResize(280);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(1000);
  });

  it("stops reacting after unmount", () => {
    const { getByTestId, unmount } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el);
    fireResize(400);
    unmount();
    state.clientHeight = 300;
    fireResize(300);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(600);
  });

  it("degrades gracefully when ResizeObserver is unavailable", () => {
    vi.stubGlobal("ResizeObserver", undefined);
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 0 });
    expect(() => act(() => api!.scrollToBottom())).not.toThrow();
    expect(state.scrollTop).toBe(1000);
  });
});
```

- [x] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/lib/useStickToBottom.test.ts`
Expected: FAIL —— 模块 `./useStickToBottom` 不存在（`Failed to resolve import` / `Cannot find module`）。

- [x] **Step 3: 写最小实现**

创建 `desktop/src/lib/useStickToBottom.ts`：

```ts
import { useCallback, useEffect, useRef } from "react";
import type { RefObject } from "react";

/**
 * 「是否贴底」的像素容差。
 *
 * 滚动位置与 scrollHeight/clientHeight 都可能落在亚像素上（分数像素、设备像素比），
 * 严格相等会把「就差 1–2px」判成非贴底，于是旋转后不再贴底。取 4px 容忍这点误差。
 */
export const STICK_BOTTOM_THRESHOLD_PX = 4;

/** 判断一个滚动容器当前是否贴在底部。 */
export function isAtBottom(
  el: Pick<HTMLElement, "scrollHeight" | "clientHeight" | "scrollTop">,
  thresholdPx = STICK_BOTTOM_THRESHOLD_PX,
): boolean {
  return el.scrollHeight - el.clientHeight - el.scrollTop <= thresholdPx;
}

export interface StickToBottom {
  ref: RefObject<HTMLDivElement | null>;
  /** 立即贴底（供流式追加时调用）。 */
  scrollToBottom: () => void;
}

/**
 * 让一个滚动容器在**视口高度变化**（手机旋转、`dvh` 变化、键盘顶起、桌面拖窗口）
 * 之后保持用户的「贴底意图」：原本贴底的重新贴底；原本在上翻历史的则不动。
 *
 * 为什么需要它：这类高度变化**不派发 `scroll`**，浏览器保留的是**绝对像素** `scrollTop`，
 * 而 Safari/WKWebView **不实现 scroll anchoring**（Chrome/Firefox 会替你粘住位置），
 * 于是手机「竖→横→竖」后内容整体被推离底部（实测差 4346px）。
 *
 * 为什么用容器自身的 `ResizeObserver`：旋转/键盘/dvh 变化的共同效果就是容器盒高变了，
 * 以 `clientHeight` 为唯一事实来源最准；`window.resize` 时序偏抖且不覆盖父容器变化。
 *
 * 注意 `clientHeight`（含 padding）与 `entry.contentRect.height`（内容盒）相差一个常量；
 * 后者只用作**变化信号**，贴底判定一律直接读元素自身的三个度量，避免两套坐标混淆。
 *
 * `ResizeObserver` 缺失时（如 jsdom）退化为「只提供 `scrollToBottom`」，不建立观察，
 * 也不抛错。
 */
export function useStickToBottom(): StickToBottom {
  const ref = useRef<HTMLDivElement>(null);
  // 旋转本身不派发 `scroll`，所以这个 ref 恰好保留了「高度变化之前」是否贴底。
  const atBottomRef = useRef(true);
  const frameRef = useRef<number | null>(null);

  const scrollToBottom = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    const onScroll = () => {
      atBottomRef.current = isAtBottom(el);
    };
    // 初次同步一次真实状态（挂载时元素可能已经有滚动位置）。
    onScroll();
    el.addEventListener("scroll", onScroll, { passive: true });

    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      let previousHeight: number | null = null;
      observer = new ResizeObserver((entries) => {
        const entry = entries[0];
        if (!entry) return;
        const height = entry.contentRect.height;
        // 首次回调只用来建立基准高度，不算「高度变化」。
        if (previousHeight === null) {
          previousHeight = height;
          return;
        }
        if (height === previousHeight) return;
        previousHeight = height;
        if (!atBottomRef.current) return;
        // 延后一帧：iOS 上 `dvh` 的高度变化可能晚于这次回调，且要等布局落定再赋值。
        // 同一帧内多次尺寸抖动只保留一次贴底。
        if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
        frameRef.current = requestAnimationFrame(() => {
          frameRef.current = null;
          const node = ref.current;
          if (node) node.scrollTop = node.scrollHeight;
        });
      });
      observer.observe(el);
    }

    return () => {
      el.removeEventListener("scroll", onScroll);
      observer?.disconnect();
      if (frameRef.current !== null) {
        cancelAnimationFrame(frameRef.current);
        frameRef.current = null;
      }
    };
  }, []);

  return { ref, scrollToBottom };
}
```

- [x] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/useStickToBottom.test.ts`
Expected: PASS（10 个用例：`isAtBottom` 4 + `useStickToBottom` 6）。

- [x] **Step 5: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无错误。

- [x] **Step 6: 提交**

```bash
git add desktop/src/lib/useStickToBottom.ts desktop/src/lib/useStickToBottom.test.ts
git commit -m "feat(desktop): add useStickToBottom for viewport-change re-pinning"
```

---

### Task 2: 在 `ChatView` 接上 hook，移除旧的贴底锚点

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`（import 第 1 行、`endRef` 第 157 行、effect 第 168-170 行、容器第 173 行、末尾锚点第 197 行）
- Modify: `desktop/src/components/ChatView.test.tsx`（第 31 行的 `scrollIntoView` 打桩）
- Modify: `docs/bug-list.md`

**Interfaces:**
- Consumes: `useStickToBottom(): { ref: RefObject<HTMLDivElement | null>; scrollToBottom: () => void }`（Task 1 产出）。
- Produces: 无对外新接口；纯内部接线。

- [x] **Step 1: 改 import（第 1 行）**

把

```tsx
import { memo, useEffect, useRef } from "react";
```

改为

```tsx
import { memo, useEffect } from "react";
```

（`noUnusedLocals` 生效：`endRef` 移除后 `useRef` 不再使用，留着会让 `tsc` 报错。）

- [x] **Step 2: 新增 hook 调用，替换 `endRef`（第 157 行附近）**

把

```tsx
  const endRef = useRef<HTMLDivElement>(null);
```

改为

```tsx
  const { ref: scrollRef, scrollToBottom } = useStickToBottom();
```

并在文件顶部的 import 区（与其它 `../lib/...` 同组）加一行：

```tsx
import { useStickToBottom } from "../lib/useStickToBottom";
```

- [x] **Step 3: 改贴底 effect（第 168-170 行）**

把

```tsx
  useEffect(() => {
    endRef.current?.scrollIntoView({ block: "end" });
  }, [items.length, lastText, error]);
```

改为

```tsx
  // 新内容 / 流式文本变化时贴底。`scrollToBottom` 是稳定引用（useCallback []），
  // 放进依赖数组不会引入额外触发。
  useEffect(() => {
    scrollToBottom();
  }, [items.length, lastText, error, scrollToBottom]);
```

保留上方那段关于「依赖 `items.length`/`lastText` 而非 `items`」的注释不动。

- [x] **Step 4: 把 ref 挂到滚动容器（第 173 行）**

把

```tsx
    <div className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-3">
```

改为

```tsx
    <div
      ref={scrollRef}
      className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-3"
    >
```

**类名逐字符不变**（既有测试断言 `overflow-y-auto`/`overflow-x-hidden`/`min-w-0`，且界面外观不变）。

- [x] **Step 5: 删除末尾贴底锚点（第 197 行）**

删除

```tsx
      <div ref={endRef} />
```

（`scrollTop = scrollHeight` 已不需要 DOM 锚点；此处被 `error` 区块之后紧邻的收起标签包围，只删这一行。）

- [x] **Step 6: 移除 `ChatView.test.tsx` 里已无用的打桩**

删除 `desktop/src/components/ChatView.test.tsx` 第 31 行及其上方注释：

```tsx
// jsdom has no layout, so the auto-scroll effect needs a stub.
(Element.prototype as unknown as { scrollIntoView: () => void }).scrollIntoView = () => {};
```

理由：实现已不再调用 `scrollIntoView`；保留一个无人使用的原型打桩是死代码。删掉它同时是**验收信号**——若还有任何代码路径调用 `scrollIntoView`，本文件的测试会立刻抛错。

- [x] **Step 7: 跑 ChatView 测试确认全绿**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS（含 `keeps the streamed text visible` 与横向溢出裁剪那条；无 `scrollIntoView` 报错）。

- [x] **Step 8: 全量类型检查 + 全量测试**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 无类型错误；全部测试通过。

- [x] **Step 9: 登记 bug-list**

在 `docs/bug-list.md` 的已修复区（`[x]` 条目区，紧随其它 `[x]` 行）新增一条，格式对齐现有条目：

```markdown
- [x] 手机 APP 从竖向转到横向、再转回竖向后，会话内容大幅偏移（根因：**贴底 effect 依赖与旋转无关，且前端无任何视口变化监听**。`ChatView` 是会话唯一的滚动容器，其自动贴底只挂在 `[items.length, lastText, error]` 上（`desktop/src/components/ChatView.tsx:168`），旋转三者皆不变，effect 不重跑；全前端亦无 `orientationchange`/`resize`/`visualViewport` 监听。旋转时容器 `clientHeight` 795→341→795，而浏览器保留的是**绝对像素** `scrollTop`，于是转回竖向后被推离底部。Chrome/Firefox 有 scroll anchoring 会自动粘住位置，**Safari/WKWebView 不实现**，故仅 iOS 客户端暴露。修复：新增 `desktop/src/lib/useStickToBottom.ts`——用容器自身 `ResizeObserver` 感知高度变化、用 `scroll` 事件维护「变化前是否贴底」，原本贴底则于下一帧重新贴底，原本在翻历史则不动；`ChatView` 改用它贴底并移除 `endRef`/`scrollIntoView`。验证：`cd desktop && npx vitest run src/lib/useStickToBottom.test.ts src/components/ChatView.test.tsx`、`npx tsc --noEmit && npx vitest run`；真实引擎（无头 Chrome + 真实 `dist` CSS + 同构 DOM + 关 scroll anchoring）实测：竖（贴底 scrollTop 12989）→横→竖由「停在 8643、偏离 4346px」变为「回到 12989、重新贴底」。见 [设计](superpowers/specs/2026-10-11-chat-anchor-on-rotate-design.md)）
```

- [x] **Step 10: 提交**

```bash
git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx docs/bug-list.md
git commit -m "fix(desktop): keep the chat pinned to bottom across rotation"
```

---

## 验证（人工 / 真实引擎）

自动化测试（jsdom 无布局）只覆盖 hook 逻辑；旋转偏移本身须真实引擎验证。落实现后请人工确认：

- [ ] 在 iOS 客户端（或可旋转的 WKWebView 宿主）：会话停在底部时，竖→横→竖后仍贴在底部。
- [ ] 上翻若干条历史后旋转，位置大致不变、**不**被拽到底部。
- [ ] 桌面端：把窗口高度由大拖小（原本贴底），会话仍贴在底部；原本在看历史则不动。
- [ ] 参考复现台（本轮排查所用）：`/tmp/rotate-probe/`（真实 `dist` CSS + 同构 DOM + 关闭 `overflow-anchor` + CDP 旋转），不作为最终判据。

## Self-Review

- **Spec 覆盖**：§1 根因 → Task 2 的 bug-list 条目与修复；§2 目标/非目标 → 只改 `ChatView`、无第三方依赖、不改 CSS；§3 决策（全客户端、容器 `ResizeObserver`、`scroll` 维护 atBottom、阈值容差）→ Task 1 实现；§4.1 hook 接口 → Task 1 Interfaces；§4.2 ChatView 改造 → Task 2 Step 1-5；§4.3 数据流 → Task 1 Step 1 用例（贴底/历史/命令式/去重/卸载）；§5 测试与命令 → Task 1/2 的 Run 行 + 「验证（人工）」；§6 回归 → Step 6 移除打桩 + Step 7/8 全量跑；§7/§8 YAGNI 与限制 → 未新增计划内条目，人工验证段保留真实引擎项。无遗漏。
- **占位符扫描**：无 TBD/TODO；每个代码步骤均给出完整代码与确切命令。
- **类型一致性**：`useStickToBottom` 返回 `{ ref, scrollToBottom }`、`isAtBottom` 签名、`STICK_BOTTOM_THRESHOLD_PX` 在 Task 1 定义、Task 2 只消费 `ref`/`scrollToBottom`，名称一致。
