# 会话页位置与选中会话恢复 — 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让会话页在 A→B→A、切后台再回来、冷启动三种情形下恢复到用户离开时的会话与阅读位置。

**Architecture:** 位置由「视口顶部那条消息的协议 id + 偏移」表达，贴底记为 `null`；锚点存在 `useSessionState` 的内存 Map 里（不触发重渲染），`ChatView` 在滚动时采集、在 `threadId` 变化时用 `useLayoutEffect` 对位。选中会话改由 `selectedThread` 小键持久化，握手时统一按「当前会话 → 记住的会话 → 列表第一条」回落。

**Tech Stack:** React 19 + TypeScript + Vitest + jsdom；Tauri 桌面端与 iOS 远端共用同一 bundle。

**Spec:** `docs/superpowers/specs/2026-10-25-session-view-restore-design.md`

## Global Constraints

- 所有工作在本 worktree `.worktrees/feat-session-view-restore`（分支 `feat/session-view-restore`）内进行，**不得**在 `main` 上改文件或提交。
- 桌面端布局与行为除本计划点明的三处（切会话不强制到底、重复选中不清面板、握手保持会话）外必须逐字节不变。
- 位置只在内存里，**不写磁盘**；冷启动只恢复「哪条会话」，不恢复像素位置。
- 锚点只认服务端 id；`local-*` 前缀的本地项不得入锚。
- 提交信息用 conventional commits，不写 `Co-Authored-By`。
- 每次提交前在 `desktop/` 下跑 `npx tsc --noEmit`；测试跑法见各任务。
- 路径常量：会话页组件 `desktop/src/components/ChatView.tsx`，宿主 `desktop/src/App.tsx`，会话关注点 `desktop/src/hooks/useSessionState.ts`。

---

### Task 1: 纯函数锚点内核

把「找锚」「算回滚位置」做成无 DOM 依赖的纯函数，先钉住语义再接触 DOM。

**Files:**
- Create: `desktop/src/lib/viewAnchor.ts`
- Test: `desktop/src/lib/viewAnchor.test.ts`

**Interfaces:**
- Produces:
  - `interface ViewAnchor { itemId: string; offset: number }`
  - `interface ItemBox { id: string; top: number }`
  - `isAnchorableId(id: string | undefined): id is string`
  - `findAnchor(boxes: ItemBox[], scrollTop: number): ViewAnchor | null`
  - `restoreScrollTop(box: ItemBox | undefined, anchor: ViewAnchor): number | null`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/viewAnchor.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import { findAnchor, restoreScrollTop, isAnchorableId, type ItemBox } from "./viewAnchor";

const boxes = (tops: Record<string, number>): ItemBox[] =>
  Object.entries(tops).map(([id, top]) => ({ id, top }));

describe("isAnchorableId", () => {
  it("accepts server ids", () => {
    expect(isAnchorableId("item-1")).toBe(true);
  });
  it("rejects local optimistic ids, which are rebuilt on reopen", () => {
    expect(isAnchorableId("local-3")).toBe(false);
  });
  it("rejects missing and empty ids", () => {
    expect(isAnchorableId(undefined)).toBe(false);
    expect(isAnchorableId("")).toBe(false);
  });
});

describe("findAnchor", () => {
  it("picks the last message whose top is at or above the viewport top", () => {
    // 视口顶在 250：item-b 从 100 开始、item-c 从 300 开始 ⇒ b 是顶部那条。
    const got = findAnchor(boxes({ "item-a": 0, "item-b": 100, "item-c": 300 }), 250);
    expect(got).toEqual({ itemId: "item-b", offset: -150 });
  });

  it("records a zero offset when a message starts exactly at the viewport top", () => {
    expect(findAnchor(boxes({ "item-a": 0, "item-b": 200 }), 200)).toEqual({
      itemId: "item-b",
      offset: 0,
    });
  });

  it("never anchors on a local id", () => {
    const got = findAnchor(boxes({ "item-a": 0, "local-1": 100 }), 150);
    expect(got).toEqual({ itemId: "item-a", offset: -150 });
  });

  it("returns null when nothing is above the viewport top", () => {
    expect(findAnchor(boxes({ "item-a": 400 }), 100)).toBeNull();
    expect(findAnchor([], 0)).toBeNull();
  });
});

describe("restoreScrollTop", () => {
  it("puts the anchored message back at the offset it had", () => {
    // 锚点偏移 -150 表示该消息顶边在视口上方 150px ⇒ 回滚到 250+150。
    expect(restoreScrollTop({ id: "item-b", top: 250 }, { itemId: "item-b", offset: -150 })).toBe(400);
  });

  it("clamps to zero instead of using a negative scrollTop", () => {
    expect(restoreScrollTop({ id: "item-b", top: 10 }, { itemId: "item-b", offset: -150 })).toBe(0);
  });

  it("returns null when the anchored message is gone", () => {
    expect(restoreScrollTop(undefined, { itemId: "item-b", offset: 0 })).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/viewAnchor.test.ts`
Expected: FAIL — `Failed to resolve import "./viewAnchor"`

- [ ] **Step 3: 实现**

创建 `desktop/src/lib/viewAnchor.ts`：

```ts
/**
 * 会话页的阅读位置：视口顶部那条消息 + 它相对视口顶部的偏移。
 *
 * 为什么锚消息而不是记像素：同一条会话在后台会继续追加内容，像素位置会在
 * 每次进入后指向另一段文字；消息 id 在服务端重放时保持稳定，锚它才能"回到
 * 我原来在读的那条"。
 *
 * 贴底不在这里表达——那是 `null`（见 `ChatView` 的 `anchor` 属性）。
 */
export interface ViewAnchor {
  /** 协议里的 item id。只会是服务端 id，不会是 `local-*`。 */
  itemId: string;
  /** 该消息顶边相对视口顶部的偏移（负数 = 已滚过它的顶边）。 */
  offset: number;
}

/** 一条消息当前的纵向位置：`top` 是相对滚动内容顶部的距离。 */
export interface ItemBox {
  id: string;
  top: number;
}

/**
 * 本地乐观项（`local-*`）不入锚：它由 `Session` 在本地分配序号，冷启动与历史
 * 重放后会重建，用它做锚在第二次进入时必然找不到目标。
 */
export function isAnchorableId(id: string | undefined): id is string {
  return typeof id === "string" && id.length > 0 && !id.startsWith("local-");
}

/**
 * 视口顶部那条消息：顶边在 `scrollTop` 之上（含相等）的**最后**一条。
 *
 * 允许 1px 误差：带边框/亚像素的行高会让"正好在顶部"落成 199.5，严格比较会在
 * 这种情况下退到上一条，读起来就差一行。
 */
export function findAnchor(boxes: ItemBox[], scrollTop: number): ViewAnchor | null {
  let best: ItemBox | null = null;
  for (const box of boxes) {
    if (!isAnchorableId(box.id)) continue;
    if (box.top > scrollTop + 1) continue;
    if (best === null || box.top >= best.top) best = box;
  }
  if (best === null) return null;
  return { itemId: best.id, offset: best.top - scrollTop };
}

/**
 * 恢复时的目标 `scrollTop`。`box` 缺失（消息被删/历史被截断）或不是可锚 id 时
 * 返回 `null`，由调用方回退到底部——宁可看到最新的，也不要停在无意义的像素上。
 */
export function restoreScrollTop(box: ItemBox | undefined, anchor: ViewAnchor): number | null {
  if (!box || !isAnchorableId(box.id)) return null;
  return Math.max(0, box.top - anchor.offset);
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/viewAnchor.test.ts`
Expected: PASS（12 tests）

- [ ] **Step 5: 提交**

```bash
cd .worktrees/feat-session-view-restore
git add desktop/src/lib/viewAnchor.ts desktop/src/lib/viewAnchor.test.ts
git commit -m "feat(desktop): add view anchor pure functions"
```

---

### Task 2: 给每条消息标注稳定 id

恢复与采集都靠 `[data-item-id]`，先把这个属性铺上，且**不改动布局**。

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`（`ChatItem` 的返回、`HistoryPrefix`、`LiveTail`）
- Test: `desktop/src/components/ChatView.test.tsx`

**Interfaces:**
- Consumes: 无（本任务只加属性）
- Produces: 每条消息根节点上带 `data-item-id="<协议 id>"`。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/ChatView.test.tsx` 的 `describe("ChatView auto-scroll", ...)` **之前**插入新 describe：

```ts
describe("ChatView item ids", () => {
  it("tags every message with its protocol id so a reading position can be anchored", () => {
    const list = items();
    const { container } = render(<ChatView items={list} threadId="t1" />);
    const ids = Array.from(container.querySelectorAll("[data-item-id]")).map(
      (el) => el.getAttribute("data-item-id"),
    );
    expect(ids).toEqual(list.map((it) => it.id));
  });

  it("keeps the message's own alignment when wrapping it for the id", () => {
    // `display: contents` 让包裹层不生成盒子，子元素仍直接参与滚动容器的
    // flex 布局；否则用户气泡的内缩/右对齐会被一层普通 div 破坏。
    const { container } = render(<ChatView items={items()} threadId="t1" />);
    const first = container.querySelector("[data-item-id]") as HTMLElement;
    expect(first.className).toContain("contents");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "item ids"`
Expected: FAIL — 找不到 `[data-item-id]`

- [ ] **Step 3: 实现**

`desktop/src/components/ChatView.tsx`，把 `ChatItem` 的 switch 结果提成局部变量，统一包一层：

找到 `  switch (item.type) {` 这一行（在 `ChatItem` 内），在它前面插入：

```ts
    // 消息根节点统一包一层 `contents` 包裹，只为挂 `data-item-id`：阅读位置
    // 靠它定位。`display: contents` 不生成盒子，因此子元素仍是滚动容器的直接
    // flex item —— 用户气泡的 `self-end`、助手消息的 `self-start` 都不受影响。
    const node = (() => {
      switch (item.type) {
```

然后把它原来的 `switch (item.type) {` 删掉（已并入上面），并在 `ChatItem` 的 switch 结束处（`  },\n  // Compare the mutable primitive, never \`item.text\`.` 之前）插入：

```ts
      }
    })();
    return (
      <div className="contents" data-item-id={item.id}>
        {node}
      </div>
    );
```

即最终形状：

```ts
    const node = (() => {
      switch (item.type) {
        case "notice": /* …原样… */
        // …
        default: { /* …原样… */ }
      }
    })();
    return (
      <div className="contents" data-item-id={item.id}>
        {node}
      </div>
    );
  },
  (prev, next) => prev.item === next.item && prev.text === next.text && prev.query === next.query,
);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS（原 35 + 新增 2）

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "feat(desktop): tag transcript rows with their protocol id"
```

---

### Task 3: ChatView 采集阅读位置

滚动时把「顶部那条消息」记进宿主，贴底记 `null`；不新增渲染。

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`
- Modify: `desktop/src/lib/useStickToBottom.ts`（新增 `releaseFollow`）
- Test: `desktop/src/components/ChatView.test.tsx`、`desktop/src/lib/useStickToBottom.test.ts`

**Interfaces:**
- Consumes: `findAnchor`、`isAnchorableId`、`type ItemBox`（Task 1）
- Produces:
  - `ChatView` 新属性 `onRememberView?: (threadId: string, anchor: ViewAnchor | null) => void`
  - `useStickToBottom()` 返回值新增 `releaseFollow: () => void`（把贴底意图置为 false，不移动视口）

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/ChatView.test.tsx` 的 `describe("ChatView item ids", ...)` 之后插入：

```ts
describe("ChatView remembers the reading position", () => {
  /** jsdom 没有布局：给滚动容器与每条消息装上可读的位置度量。 */
  function installLayout(scroller: HTMLElement, tops: number[], scrollHeight = 3000) {
    const state = { scrollTop: 0 };
    Object.defineProperty(scroller, "scrollHeight", { configurable: true, get: () => scrollHeight });
    Object.defineProperty(scroller, "clientHeight", { configurable: true, get: () => 600 });
    Object.defineProperty(scroller, "scrollTop", {
      configurable: true,
      get: () => state.scrollTop,
      set: (v: number) => { state.scrollTop = v; },
    });
    scroller.getBoundingClientRect = () =>
      ({ top: 0, left: 0, bottom: 600, right: 0, width: 0, height: 600, x: 0, y: 0, toJSON() {} }) as DOMRect;
    const nodes = Array.from(scroller.querySelectorAll<HTMLElement>("[data-item-id]"));
    nodes.forEach((node, i) => {
      node.getBoundingClientRect = () =>
        ({ top: tops[i] - state.scrollTop, left: 0, bottom: tops[i], right: 0,
           width: 0, height: 0, x: 0, y: 0, toJSON() {} }) as DOMRect;
    });
    return state;
  }

  it("reports the top visible message while the user reads history", async () => {
    const seen: Array<[string, unknown]> = [];
    const { container } = render(
      <ChatView
        items={items()}
        threadId="t1"
        onRememberView={(id, a) => seen.push([id, a])}
      />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [0, 1000, 2000]);
    state.scrollTop = 1200;
    fireEvent.scroll(scroller);
    await new Promise((r) => requestAnimationFrame(() => r(null)));
    expect(seen.at(-1)?.[0]).toBe("t1");
    expect(seen.at(-1)?.[1]).toEqual({ itemId: items()[1].id, offset: -200 });
  });

  it("reports null when the user is at the bottom", async () => {
    const seen: Array<[string, unknown]> = [];
    const { container } = render(
      <ChatView items={items()} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [0, 1000, 2000]);
    state.scrollTop = 2400; // scrollHeight 3000 - clientHeight 600
    fireEvent.scroll(scroller);
    await new Promise((r) => requestAnimationFrame(() => r(null)));
    expect(seen.at(-1)).toEqual(["t1", null]);
  });

  it("attributes the last position to the thread that was on screen", async () => {
    // 切换线程时旧内容已被替换，靠"离开前最后记住的那次"归属到旧 id。
    const seen: Array<[string, unknown]> = [];
    const list = items();
    const { container, rerender } = render(
      <ChatView items={list} threadId="t1" onRememberView={(id, a) => seen.push([id, a])} />,
    );
    const scroller = container.firstElementChild as HTMLElement;
    const state = installLayout(scroller, [0, 1000, 2000]);
    state.scrollTop = 1200;
    fireEvent.scroll(scroller);
    await new Promise((r) => requestAnimationFrame(() => r(null)));
    rerender(<ChatView items={list} threadId="t2" onRememberView={(id, a) => seen.push([id, a])} />);
    expect(seen.filter(([id]) => id === "t1").at(-1)?.[1]).toEqual({
      itemId: list[1].id,
      offset: -200,
    });
  });
});
```

在 `desktop/src/lib/useStickToBottom.test.ts` 末尾（`describe("useStickToBottom", …)` 的最后一个
`});` 之前）插入：

```ts
  it("releases follow without moving the viewport", () => {
    // 恢复阅读位置后必须放弃跟随，否则下一个 delta 会把人拽回底部；
    // 但它不能自己动视口——位置由调用方刚设好。
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 500 });
    fireEvent.scroll(el);
    act(() => api!.releaseFollow());
    state.scrollHeight = 2000;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(500);
  });
```

该文件用模块级 `api` 暴露 hook 的返回（见文件顶部的 `Harness`），所以还要把
`api` 的类型与赋值补上新的命令：

```ts
let api: {
  scrollToBottom: () => void;
  followIfAtBottom: () => void;
  releaseFollow: () => void;
} | null = null;
```

```ts
    api = {
      scrollToBottom: stick.scrollToBottom,
      followIfAtBottom: stick.followIfAtBottom,
      releaseFollow: stick.releaseFollow,
    };
  }, [stick.scrollToBottom, stick.followIfAtBottom, stick.releaseFollow]);
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "remembers the reading position" && npx vitest run src/lib/useStickToBottom.test.ts -t "releases follow"`
Expected: FAIL — `onRememberView` 属性不存在 / `releaseFollow is not a function`

- [ ] **Step 3: 实现**

`desktop/src/lib/useStickToBottom.ts`：在 `StickToBottom` 接口里加入：

```ts
  /**
   * 放弃跟随，但**不动视口**。
   *
   * 用于"恢复阅读位置"：调用方刚把 `scrollTop` 设到锚点，此刻若仍保持贴底意图，
   * 下一个 delta 就会把人拽回底部。与 `scrollToBottom` 的区别正是它不移动元素。
   */
  releaseFollow: () => void;
```

在 `followIfAtBottom` 之后实现，并加入返回对象：

```ts
  const releaseFollow = useCallback(() => {
    atBottomRef.current = false;
  }, []);
```

```ts
  return { ref, scrollToBottom, followIfAtBottom, releaseFollow };
```

`desktop/src/components/ChatView.tsx`：

1. 顶部 import 加入：

```ts
import { findAnchor, type ItemBox, type ViewAnchor } from "../lib/viewAnchor";
```

2. `ChatView` 属性新增（放在 `scrollerRef` 之后）：

```ts
  /**
   * 该会话上次离开时的阅读位置；`null` = 离开时贴底，`undefined` = 从没记过。
   * 由宿主按 `threadId` 提供。
   */
  anchor,
  /**
   * 滚动时上报当前位置。签名带 `threadId` 是为了让"切换前的最后一次采集"仍归属
   * 到旧会话——切换之后 DOM 里已经是新内容，闭包再读当前 id 就错位了。
   */
  onRememberView,
```

类型块里对应加：

```ts
  anchor?: ViewAnchor | null;
  onRememberView?: (threadId: string, anchor: ViewAnchor | null) => void;
```

3. 解构处补 `releaseFollow`：

```ts
  const { ref: scrollRef, scrollToBottom, followIfAtBottom, releaseFollow } = useStickToBottom();
```

4. 在 `const effectiveCall = call ?? NO_CALL;` 之后加入记忆逻辑：

```ts
  // 采集用的元素句柄：与 `useStickToBottom` 的 ref 指向同一个 DOM 节点。
  const elRef = useRef<HTMLDivElement | null>(null);
  const rememberRef = useRef(onRememberView);
  rememberRef.current = onRememberView;

  useEffect(() => {
    const el = elRef.current;
    const remember = rememberRef.current;
    if (!el || !remember) return;
    // 采集闭包固定住本次 effect 对应的会话 id（见属性注释）。
    const owner = threadId;
    let frame: number | null = null;

    const capture = () => {
      frame = null;
      const node = elRef.current;
      const report = rememberRef.current;
      if (!node || !report) return;
      if (isAtBottom(node)) {
        report(owner, null);
        return;
      }
      // `getBoundingClientRect` 的 top 是视口坐标，换上滚动偏移即"内容坐标"。
      const base = node.getBoundingClientRect().top - node.scrollTop;
      const boxes: ItemBox[] = [];
      for (const child of node.querySelectorAll<HTMLElement>("[data-item-id]")) {
        const id = child.dataset.itemId;
        if (!id) continue;
        boxes.push({ id, top: child.getBoundingClientRect().top - base });
      }
      report(owner, findAnchor(boxes, node.scrollTop));
    };

    const onScroll = () => {
      // 一轮滚动只量一次：读 `getBoundingClientRect` 会强制布局，滚动期间每帧
      // 量 N 条消息会把长会话的滚动拖慢。
      if (frame !== null) return;
      frame = requestAnimationFrame(capture);
    };

    el.addEventListener("scroll", onScroll, { passive: true });
    capture();
    return () => {
      el.removeEventListener("scroll", onScroll);
      if (frame !== null) cancelAnimationFrame(frame);
    };
  }, [threadId]);
```

5. `setScroller` 里补 `elRef.current = el;`：

```ts
  const setScroller = useCallback(
    (el: HTMLDivElement | null) => {
      elRef.current = el;
      scrollRef.current = el;
      if (scrollerRef) scrollerRef.current = el;
    },
    [scrollRef, scrollerRef],
  );
```

6. 顶部 import 补 `isAtBottom`：

```ts
import { isAtBottom, useStickToBottom } from "../lib/useStickToBottom";
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx src/lib/useStickToBottom.test.ts`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ChatView.tsx desktop/src/lib/useStickToBottom.ts desktop/src/lib/useStickToBottom.test.ts desktop/src/components/ChatView.test.tsx
git commit -m "feat(desktop): remember the top visible message while scrolling"
```

---

### Task 4: ChatView 恢复到锚点

切到有记录的会话时对回位置；锚点丢失则回退到底部。同时反转旧断言。

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`
- Test: `desktop/src/components/ChatView.test.tsx:673-679`（改写）+ 新增

**Interfaces:**
- Consumes: `anchor` 属性、`restoreScrollTop`、`releaseFollow`（Task 1/3）
- Produces: 无新增导出

- [ ] **Step 1: 把旧断言改写成两条**

把 `desktop/src/components/ChatView.test.tsx` 里的：

```ts
  it("jumps to the newest content when the thread changes", () => {
    // 切会话是明确意图：要看到这个会话的最新内容，而不是沿用上一个会话
    // 的阅读位置（更不能因为上一个会话上翻过就停在中途）。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    rerender(<ChatView items={list} threadId="t2" />);
    expect(state.scrollTop).toBe(1000);
  });
```

替换为：

```ts
  it("jumps to the newest content when the target thread has no remembered position", () => {
    // 首次进入一条没有记录的会话：仍然落在最新内容上。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    rerender(<ChatView items={list} threadId="t2" anchor={undefined} />);
    expect(state.scrollTop).toBe(1000);
  });

  it("restores the remembered position instead of jumping to the newest content", () => {
    // A → B → A：A 有记录，回来必须停在那条消息上，而不是被拽到底部。
    const list = items();
    const { container, rerender } = render(<ChatView items={list} threadId="t2" />);
    const scroller = container.firstElementChild as HTMLElement;
    const state = installMetrics(scroller, {
      scrollHeight: 3000,
      clientHeight: 400,
      scrollTop: 0,
    });
    const rows = Array.from(scroller.querySelectorAll<HTMLElement>("[data-item-id]"));
    const tops = [0, 800, 1600];
    rows.forEach((row, i) => {
      row.getBoundingClientRect = () =>
        ({ top: tops[i] - state.scrollTop, left: 0, bottom: tops[i], right: 0,
           width: 0, height: 0, x: 0, y: 0, toJSON() {} }) as DOMRect;
    });
    scroller.getBoundingClientRect = () =>
      ({ top: 0, left: 0, bottom: 400, right: 0, width: 0, height: 400, x: 0, y: 0, toJSON() {} }) as DOMRect;

    rerender(
      <ChatView
        items={list}
        threadId="t1"
        anchor={{ itemId: list[1].id, offset: -100 }}
      />,
    );
    // 800 - (-100) = 900
    expect(state.scrollTop).toBe(900);
  });

  it("falls back to the bottom when the anchored message is gone", () => {
    const list = items();
    const { container, rerender } = render(<ChatView items={list} threadId="t2" />);
    const scroller = container.firstElementChild as HTMLElement;
    const state = installMetrics(scroller, { scrollHeight: 2000, clientHeight: 400, scrollTop: 0 });
    rerender(
      <ChatView items={list} threadId="t1" anchor={{ itemId: "item-that-was-deleted", offset: 0 }} />,
    );
    expect(state.scrollTop).toBe(2000);
  });

  it("does not follow new content once a restored position is on screen", () => {
    // 恢复后必须放弃跟随；否则下一条 delta 就把人拽回底部。
    const { list, rerender, state } = setup({ scrollTop: 100 });
    rerender(<ChatView items={list} threadId="t2" anchor={null} />);
    expect(state.scrollTop).toBe(1000);
    setText(list, "new delta");
    state.scrollHeight = 1400;
    rerender(<ChatView items={list} threadId="t2" anchor={null} />);
    expect(state.scrollTop).toBe(1000);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "restores the remembered position"`
Expected: FAIL — `scrollTop` 仍是 1000（旧行为强制到底）

- [ ] **Step 3: 实现**

`desktop/src/components/ChatView.tsx`：

1. import 补 `useLayoutEffect`：

```ts
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef } from "react";
```

2. 在 Task 3 的 `elRef` 声明之后加恢复逻辑：

```ts
  // 每条会话只恢复一次：`anchor` 之后可能因为采集而变化，重复对位会把用户
  // 正在做的滚动不断拉回去。
  const restoredFor = useRef<string | null | undefined>(undefined);
  useLayoutEffect(() => {
    const el = elRef.current;
    if (!el) return;
    if (restoredFor.current === threadId) return;
    restoredFor.current = threadId;
    // `null` = 离开时贴底，`undefined` = 从没记过：两者都走既有的"到底"。
    if (anchor == null) return;
    const target = el.querySelector<HTMLElement>(`[data-item-id="${CSS.escape(anchor.itemId)}"]`);
    const base = el.getBoundingClientRect().top - el.scrollTop;
    const top = target ? target.getBoundingClientRect().top - base : undefined;
    const next = restoreScrollTop(top === undefined ? undefined : { id: anchor.itemId, top }, anchor);
    if (next === null) {
      // 锚点消失（消息被删、历史被截断）：回退到底部，别停在无意义的像素上。
      scrollToBottom();
      return;
    }
    el.scrollTop = next;
    // 恢复完立刻放弃跟随，否则下一条 delta 会把人拽回底部。
    releaseFollow();
  }, [threadId, anchor, scrollToBottom, releaseFollow]);
```

3. import 补 `restoreScrollTop`：

```ts
import { findAnchor, restoreScrollTop, type ItemBox, type ViewAnchor } from "../lib/viewAnchor";
```

4. 改自动贴底 effect 的分支（原 `if (firstRun || threadChanged || sentByUser || newError) scrollToBottom();`）：

```ts
    // 恢复锚点的会话不在这里动视口：`useLayoutEffect` 已经对好位，再调
    // `scrollToBottom` 会把它顶掉。
    const restoring = threadChanged && anchor != null;
    if (firstRun || sentByUser || newError) scrollToBottom();
    else if (threadChanged) {
      if (!restoring) scrollToBottom();
    } else followIfAtBottom();
```

5. 依赖数组补 `anchor`：

```ts
  }, [items.length, lastText, error, threadId, anchor, scrollToBottom, followIfAtBottom]);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS（原 37 + 新增 3 - 拆分净增 3）

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "feat(desktop): restore a thread's reading position on re-entry"
```

---

### Task 5: 锚点存储 + 宿主接线

把锚点放进 `useSessionState`（ref Map，不触发渲染），并由 `App` 传给 `ChatView`。

**Files:**
- Modify: `desktop/src/hooks/useSessionState.ts`
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/hooks/useSessionState.test.tsx`（新建）

**Interfaces:**
- Consumes: `ViewAnchor`（Task 1）
- Produces:
  - `useSessionState` 返回值新增 `rememberView(threadId, anchor)`, `anchorFor(threadId): ViewAnchor | null | undefined`, `forgetView(threadId)`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/hooks/useSessionState.test.tsx`（直接测 hook：宿主层的像素断言
依赖真实布局，jsdom 下不可靠，位置的真实行为已由 Task 4 在 `ChatView` 上钉住；
本任务钉的是"按会话分开存"和"`undefined` 与 `null` 语义不同"）：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useSessionState } from "./useSessionState";
import { ThreadStore } from "../lib/threadStore";
import { SubagentRailStore } from "../lib/subagents";

const mount = () =>
  renderHook(() =>
    useSessionState({
      rpc: () => null,
      store: new ThreadStore(),
      force: vi.fn(),
      setCurrentError: vi.fn(),
      railStore: new SubagentRailStore(),
      initialListCache: null,
    }),
  );

describe("useSessionState view anchors", () => {
  it("distinguishes never-recorded from recorded-at-bottom", () => {
    const { result } = mount();
    // 从没记过 → undefined（走"首次进入"默认到底）
    expect(result.current.anchorFor("t1")).toBeUndefined();
    act(() => result.current.rememberView("t1", null));
    // 记过且当时贴底 → null（同样是到底，但语义是"我看的是最新"）
    expect(result.current.anchorFor("t1")).toBeNull();
  });

  it("keeps each thread's anchor separate", () => {
    const { result } = mount();
    act(() => {
      result.current.rememberView("t1", { itemId: "a", offset: -100 });
      result.current.rememberView("t2", { itemId: "b", offset: 0 });
    });
    expect(result.current.anchorFor("t1")).toEqual({ itemId: "a", offset: -100 });
    expect(result.current.anchorFor("t2")).toEqual({ itemId: "b", offset: 0 });
  });

  it("overwrites the anchor on every report", () => {
    const { result } = mount();
    act(() => result.current.rememberView("t1", { itemId: "a", offset: -100 }));
    act(() => result.current.rememberView("t1", { itemId: "z", offset: -20 }));
    expect(result.current.anchorFor("t1")).toEqual({ itemId: "z", offset: -20 });
  });

  it("forgets a deleted thread's anchor", () => {
    const { result } = mount();
    act(() => result.current.rememberView("t1", { itemId: "a", offset: 0 }));
    act(() => result.current.forgetView("t1"));
    expect(result.current.anchorFor("t1")).toBeUndefined();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/hooks/useSessionState.test.tsx`
Expected: FAIL — `result.current.anchorFor is not a function`

- [ ] **Step 3: 实现**

`desktop/src/hooks/useSessionState.ts`：

1. import 补类型：

```ts
import type { ViewAnchor } from "../lib/viewAnchor";
```

2. 在 `const [detailTab, setDetailTab] = useState<...>("subagents");` 之后加入：

```ts
  /**
   * 每条会话的阅读位置。`null` = 上次离开时贴底；没有键 = 从没记过。
   *
   * 放 **ref** 不放 state：`ChatView` 每轮滚动都会上报，若进 state 就是滚动期
   * 一次重渲染。它只在切会话时被读一次（`ChatView` 的 `anchor` 属性），
   * 天然不需要驱动渲染。
   */
  const viewAnchors = useRef(new Map<string, ViewAnchor | null>());
```

3. 在 `refreshSubagents` 之前加入三个 reader：

```ts
  const rememberView = (threadId: string, anchor: ViewAnchor | null) => {
    viewAnchors.current.set(threadId, anchor);
  };

  /** 没记过返回 `undefined`——它与"记过且贴底"（`null`）的语义不同。 */
  const anchorFor = (threadId: string): ViewAnchor | null | undefined =>
    viewAnchors.current.get(threadId);

  const forgetView = (threadId: string) => {
    viewAnchors.current.delete(threadId);
  };
```

4. 返回对象里加入：

```ts
    rememberView,
    anchorFor,
    forgetView,
```

`desktop/src/App.tsx`：

1. `ChatView` 处补两个属性（`scrollerRef={transcriptRef}` 之前）：

```tsx
                  anchor={sess.currentId ? sess.anchorFor(sess.currentId) : undefined}
                  // 每次采集都按**当时屏幕上那条**会话归属（见 ChatView 的属性
                  // 注释），所以直接透传，不在闭包里另读 currentId。
                  onRememberView={sess.rememberView}
```

2. `deleteThread` 里与 `warm.current.delete(id);` 同处一行：

```ts
    sess.forgetView(id);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/hooks/useSessionState.test.tsx src/App.test.tsx src/components/ChatView.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/hooks/useSessionState.ts desktop/src/hooks/useSessionState.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): keep a reading position per thread"
```

---

### Task 6: 桌面端切回前台保持当前会话

`keptId` 去掉平台分支；反转那条把桌面行为钉死的断言。

**Files:**
- Modify: `desktop/src/App.tsx:1719-1730`
- Test: `desktop/src/App.test.tsx:5503`（反转）

**Interfaces:**
- Consumes: `sess.currentIdRef`
- Produces: 无

- [ ] **Step 1: 反转旧断言**

把 `desktop/src/App.test.tsx` 的：

```ts
  it("still selects the first thread on the desktop reconnect (unchanged)", async () => {
```

整条替换为：

```ts
  it("keeps the current thread across a foreground reconnect on the desktop too", async () => {
    state.threads = threads();
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(resumedOn(clients[0])).toContain("t2"));
    clients[0].requests.length = 0;
    foregroundAfterDeadSocket();
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    await waitFor(() =>
      expect(clients[clients.length - 1].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    // 桌面端同样保持：切到别的 app 再回来，看到的还是离开时那条会话。
    const last = clients[clients.length - 1];
    await waitFor(() => expect(resumedOn(last)).toContain("t2"));
    expect(resumedOn(last)).not.toContain("t1");
  });

  it("still falls back to the first thread when the current one is gone", async () => {
    state.threads = threads();
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(resumedOn(clients[0])).toContain("t2"));
    // 服务端列表里已经没有 t2：必须回落第一条，而不是抱着一个不存在的会话。
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    clients[0].requests.length = 0;
    foregroundAfterDeadSocket();
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    await waitFor(() =>
      expect(resumedOn(clients[clients.length - 1])).toContain("t1"),
    );
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "on the desktop too"`
Expected: FAIL — 最后一条客户端 resume 的是 `t1`

- [ ] **Step 3: 实现**

`desktop/src/App.tsx`，把注释与三行改为（`keptId` 去掉平台分支）：

```ts
        // 置顶分区在最上方，服务端给的顺序就是首屏该选中的第一个。
        // 握手（含重连/切回前台）**不是**「用户换了页面」：当前会话——冷启从缓存
        // 恢复的、或用户正看着的——必须保持，把它踢回第一条会让人丢掉正在看的
        // 东西，桌面端与手机端一致。并且重连后新进程记忆为空（warm 与订阅窗口在
        // exited 时已清），必须对它再走一次 selectThread：冷则补 resume、running
        // 则补 readItems（不打断在途回合）、warm 则补 subscribe，否则视图留着而
        // 该会话的实时更新会静默停摆。仅当当前会话不在服务端新列表里（被删）或
        // 根本没有已选会话时，才回落第一条。
        const keptId = sess.currentIdRef.current;
        const kept =
          keptId !== null &&
          list.groups.some((g) => g.threads.some((t) => t.thread_id === keptId));
        if (kept) {
          await selectThread(keptId!, { leaveBoard: false });
        } else {
          const first = (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
          if (first) await selectThread(first.thread_id, { leaveBoard: false });
        }
```

同时删掉旧的「（副作用：selectThread 会 resetThreadDiff()…）」那段注释——副作用已由 Task 7 消除。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): keep the current thread across a foreground reconnect"
```

---

### Task 7: 重复选中同一会话不再清空附属视图

只在**换了会话**时清右侧面板 / diff / 进程详情。

**Files:**
- Modify: `desktop/src/App.tsx:694-702`（`selectThread` 内）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: 无
- Produces: 无

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/App.test.tsx` 里（与「会话详情面板」相关用例同区）插入：

```ts
  it("keeps the detail panel open when the same thread is selected again", async () => {
    // 重连与「点已选中的那一行」都会走 selectThread；会话没变就不该把右侧
    // 面板、diff、进程详情当成"上一个会话的残留"清掉。
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const toggle = await screen.findByRole("button", { name: "会话详情面板" });
    fireEvent.click(toggle);
    await waitFor(() => expect(toggle.getAttribute("aria-expanded")).toBe("true"));
    // 再点一次同一会话：默认夹具里它叫 "one"（见 App.test.tsx 的 listAll 桩）。
    fireEvent.click(screen.getByText("one"));
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume")).toHaveLength(1),
    );
    // 面板没有被当成"上一个会话的残留"收起。
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
  });
```

> `screen.getByText("one")` 对应 `App.test.tsx` 默认夹具里那条
> `{ thread_id: "t1", title: "one" }`；若你在该 describe 里改过夹具，换成自己的标题。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "keeps the detail panel open"`
Expected: FAIL — `aria-expanded` 变为 `"false"`

- [ ] **Step 3: 实现**

`desktop/src/App.tsx` 的 `selectThread` 里：

```ts
    const prevId = store.currentId;
    const changed = prevId !== null && prevId !== id;
```

把原来的：

```ts
    sess.setPanelOpen(false);
    diff.resetThreadDiff();
```

改为：

```ts
    // 只有**换了会话**才清附属视图：它们的清空是为了"别把 A 的数据画在 B 上"，
    // 会话没变时（重连重放、点已选中的那一行）不存在错配，清掉纯属打断用户。
    if (changed) {
      sess.setPanelOpen(false);
      diff.resetThreadDiff();
    }
```

把原来的：

```ts
    proc.setOpenProcessId(null);
    proc.setProcessOutput(null);
```

改为：

```ts
    if (changed) {
      // 同上：进程详情属于特定会话的进程。
      proc.setOpenProcessId(null);
      proc.setProcessOutput(null);
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS（含既有的 208 条）

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "fix(desktop): keep detail views when the same thread is reselected"
```

---

### Task 8: 冷启动记住最后选中的会话

新增 `selectedThread` 小键，握手时统一回落顺序。

**Files:**
- Create: `desktop/src/lib/selectedThread.ts`
- Test: `desktop/src/lib/selectedThread.test.ts`
- Modify: `desktop/src/App.tsx`（`selectThread`、`newThread`、`deleteThread`、握手的 `keptId`）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: 无
- Produces:
  - `SELECTED_THREAD_KEY = "yi-agent.selectedThread"`
  - `readSelectedThread(storage?): string | null`
  - `writeSelectedThread(storage, id: string | null): void`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/selectedThread.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import { readSelectedThread, writeSelectedThread, SELECTED_THREAD_KEY } from "./selectedThread";

const memory = () => {
  const data = new Map<string, string>();
  return {
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
};

describe("selectedThread", () => {
  it("round-trips an id", () => {
    const s = memory();
    writeSelectedThread(s, "thread-1");
    expect(readSelectedThread(s)).toBe("thread-1");
  });

  it("writes the key the cache is documented under", () => {
    const s = memory();
    writeSelectedThread(s, "thread-1");
    expect(s.getItem(SELECTED_THREAD_KEY)).toBe("thread-1");
  });

  it("treats null as absent rather than the string 'null'", () => {
    const s = memory();
    writeSelectedThread(s, "thread-1");
    writeSelectedThread(s, null);
    expect(readSelectedThread(s)).toBeNull();
  });

  it("degrades to null for garbage storage", () => {
    const s = memory();
    s.setItem(SELECTED_THREAD_KEY, "");
    expect(readSelectedThread(s)).toBeNull();
  });

  it("never throws when storage is unavailable", () => {
    expect(readSelectedThread(undefined)).toBeNull();
    expect(() => writeSelectedThread(undefined, "x")).not.toThrow();
  });
});
```

在 `desktop/src/App.test.tsx` 里插入：

```ts
describe("App cold start restores the last selected thread", () => {
  beforeEach(() => {
    localStorage.setItem("yi-agent.selectedThread", "t2");
  });
  afterEach(() => {
    localStorage.removeItem("yi-agent.selectedThread");
  });

  it("resumes the thread that was selected before the app quit", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() =>
      expect(
        clients[0].requests.filter((r) => r.method === "thread/resume")
          .map((r) => (r.params as { threadId: string }).threadId),
      ).toContain("t2"),
    );
    const resumed = clients[0].requests
      .filter((r) => r.method === "thread/resume")
      .map((r) => (r.params as { threadId: string }).threadId);
    expect(resumed).not.toContain("t1");
  });

  it("falls back to the first thread when the remembered one is gone", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    render(<App />);
    await waitFor(() =>
      expect(
        clients[0].requests.some(
          (r) =>
            r.method === "thread/resume" &&
            (r.params as { threadId: string }).threadId === "t1",
        ),
      ).toBe(true),
    );
  });

  it("records the selection so the next cold start can use it", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(screen.getByText("two")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(localStorage.getItem("yi-agent.selectedThread")).toBe("t2"));
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/selectedThread.test.ts src/App.test.tsx -t "cold start"`
Expected: FAIL — 模块不存在 / 冷启动 resume 的是 `t1`

- [ ] **Step 3: 实现**

创建 `desktop/src/lib/selectedThread.ts`：

```ts
/**
 * 最后选中的会话 id，用于冷启动复位。
 *
 * 与 `theme.ts` 同一条契约：存储只是缓存，**从不抛**，坏值一律当没有。它只有
 * 一个用途——下次启动时把手放回那条会话；会话本身是否存在由服务端列表裁决
 * （见 `App` 握手里的回落分支），这里不校验。
 *
 * 为什么不复用 `threadListCache.selectedId`：那份快照的形状被解析器与测试逐字段
 * 钉住，且它记的是"上次列表刷新那一刻"的选中项，与本键"最后看的"不是同一件事。
 */
import type { StorageLike } from "./remoteConfig";

export const SELECTED_THREAD_KEY = "yi-agent.selectedThread";

export function readSelectedThread(
  storage: StorageLike | undefined = globalThis.localStorage as StorageLike | undefined,
): string | null {
  if (!storage) return null;
  try {
    const raw = storage.getItem(SELECTED_THREAD_KEY);
    return raw !== null && raw.length > 0 ? raw : null;
  } catch {
    return null;
  }
}

export function writeSelectedThread(
  storage: StorageLike | undefined,
  id: string | null,
): void {
  if (!storage) return;
  try {
    if (id === null) storage.removeItem(SELECTED_THREAD_KEY);
    else storage.setItem(SELECTED_THREAD_KEY, id);
  } catch {
    // 配额/隐私模式：丢一条缓存不值得打断切会话。
  }
}
```

`desktop/src/App.tsx`：

1. import：

```ts
import { readSelectedThread, writeSelectedThread } from "./lib/selectedThread";
```

2. `selectThread` 里 `sess.setCurrentId(id);` 之后：

```ts
    writeSelectedThread(localStorage, id);
```

3. `newThread` 里 `sess.setCurrentId(t.thread_id);` 之后：

```ts
      writeSelectedThread(localStorage, t.thread_id);
```

4. `deleteThread` 里 `if (id === sess.currentId) { sess.setCurrentId(null);` 之后：

```ts
      writeSelectedThread(localStorage, null);
```

5. 握手的 `keptId` 改为记住值兜底：

```ts
        // 回落顺序：当前会话 → 上次选中的（冷启动没有当前会话）→ 列表第一条。
        // 记住值同样要过服务端列表这一关：会话可能已被别的端删掉。
        const remembered = readSelectedThread();
        const candidate = sess.currentIdRef.current ?? remembered;
        const keptId = candidate;
```

（`kept` 的判据与回落分支不变。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/selectedThread.test.ts src/App.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/selectedThread.ts desktop/src/lib/selectedThread.test.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): restore the last selected thread on cold start"
```

---

### Task 9: 文档登记与全量验证

按项目规范更新项目管理与 bug 列表，并跑一遍完整验证。

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/bug-list.md`

**Interfaces:**
- Consumes: 前面全部任务
- Produces: 无

- [ ] **Step 1: 跑全量验证**

Run:
```bash
cd desktop && npx vitest run && npx tsc --noEmit && npm run build
```
Expected: 全绿；构建成功。

- [ ] **Step 2: 更新 `docs/project-management/desktop.md`**

在文件末尾追加一条（与既有条目同格式，带可验证判据）：

```markdown
- [x] 会话页位置与选中会话恢复（A→B→A 回到原阅读位置、切后台回来仍是同一条会话、冷启动回到上次会话、重复选中不再清右侧面板）— 位置模型 `desktop/src/lib/viewAnchor.ts`（锚视口顶部那条消息 + 偏移，贴底记 `null`，`local-*` 不入锚）；采集/恢复 `desktop/src/components/ChatView.tsx`（滚动时 rAF 节流采集、`threadId` 变化时 `useLayoutEffect` 对位、锚点消失回退到底部、恢复后 `releaseFollow()` 放弃跟随）；按会话存放 `desktop/src/hooks/useSessionState.ts` 的 `viewAnchors`（ref Map，不驱动渲染）+ `rememberView`/`anchorFor`/`forgetView`；切回前台保持当前会话 `desktop/src/App.tsx`（`keptId` 去掉 `isRemoteClient()` 分支）；重复选中不清面板 `desktop/src/App.tsx`（`changed` 守卫）；冷启动 `desktop/src/lib/selectedThread.ts` + 握手回落顺序「当前 → 记住 → 第一条」。判据：`cd desktop && npx vitest run src/lib/viewAnchor.test.ts src/lib/useStickToBottom.test.ts src/lib/selectedThread.test.ts src/components/ChatView.test.tsx src/App.test.tsx && npx tsc --noEmit && npm run build`；**反转过两条旧断言**（`ChatView.test.tsx` 的 `jumps to the newest content when the thread changes` 拆成"无记录到底 / 有记录恢复"，`App.test.tsx` 的 `still selects the first thread on the desktop reconnect (unchanged)` 改为"桌面端也保持"），它们钉住的正是用户报的行为 — [设计](../superpowers/specs/2026-10-25-session-view-restore-design.md)
```

- [ ] **Step 3: 更新 `docs/bug-list.md`**

把第 66 行那条：

```markdown
- [ ] 桌面启动自动恢复的是「最近工作目录里最近更新的对话」，而非跨目录全局最近更新的对话（`desktop/src/App.tsx` 取 `groups.flatMap(g=>g.threads)[0]`，而 groups 按目录索引顺序、组内按 `updated_at` 降序）
```

改为：

```markdown
- [x] 桌面启动自动恢复的是「最近工作目录里最近更新的对话」，而非跨目录全局最近更新的对话（修复：新增 `yi-agent.selectedThread` 小键记录**最后选中**的会话（`desktop/src/lib/selectedThread.ts`，切换/新建时写、删除当前会话时清），握手回落顺序改为「当前会话 → 记住的会话 → 列表第一条」，两个平台一致；记住值同样要过服务端 `thread/listAll` 这一关，会话已被删除时回落第一条。判据：`cd desktop && npx vitest run src/lib/selectedThread.test.ts src/App.test.tsx`（含 `resumes the thread that was selected before the app quit`、`falls back to the first thread when the remembered one is gone`、`records the selection so the next cold start can use it`））
```

并在文件末尾未修复区追加两条新结论（原本由本次修复覆盖）：

```markdown
- [x] A→B→A 后不再停在 A 原来的阅读位置（切会话无条件 `scrollToBottom()`，且没有按会话保存位置的机制）。修复见 [设计](superpowers/specs/2026-10-25-session-view-restore-design.md)：锚「视口顶部那条消息 + 偏移」、按会话存放、恢复后放弃跟随；锚点消失回退到底部。判据：`cd desktop && npx vitest run src/lib/viewAnchor.test.ts src/components/ChatView.test.tsx`（`restores the remembered position instead of jumping to the newest content`、`falls back to the bottom when the anchored message is gone`）
- [x] 切到别的 app 再切回来，桌面端回到列表第一条而不是离开时那条会话（握手的 `keptId` 只对手机端生效）。修复：`desktop/src/App.tsx` 去掉 `isRemoteClient()` 分支，桌面端同样保持当前会话；重连本身保留（sidecar 崩溃仍能自愈）。判据：`cd desktop && npx vitest run src/App.test.tsx`（`keeps the current thread across a foreground reconnect on the desktop too`、`still falls back to the first thread when the current one is gone`）
```

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/desktop.md docs/bug-list.md
git commit -m "docs: register session view restore in project management"
```

- [ ] **Step 5: 合并回 main（按项目规范）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/session-view-restore
git worktree remove .worktrees/feat-session-view-restore
git branch -d feat/session-view-restore
```

---

## 计划自查

- **Spec 覆盖**：§3.1 位置模型 → Task 1；§3.2 采集 → Task 2 + 3；§3.3 恢复 → Task 4；§3.3.1 旧语义表 → Task 4 的断言拆分；§3.4 归属存储 → Task 5；§3.5 切回前台 → Task 6；§3.6 重复选中 → Task 7；§3.7 冷启动 → Task 8；§4 测试表 → 各任务 Step 1 + Task 9 验证；§5 风险 1/2 → Task 3 的 rAF 节流与 Task 4 的 `useLayoutEffect`；风险 3 → Task 6 保留重连；风险 4 → Task 8 的回落断言。无遗漏。
- **占位符**：无 TBD/TODO；每个代码步骤都给出可直接粘贴的实现。
- **类型一致性**：`ViewAnchor` / `ItemBox` 在 Task 1 定义，Task 3/4/5 的用法与字段名（`itemId` / `offset` / `id` / `top`）保持一致；`rememberView(threadId, anchor)` 与 `ChatView` 的 `onRememberView(threadId, anchor)` 签名一致。
