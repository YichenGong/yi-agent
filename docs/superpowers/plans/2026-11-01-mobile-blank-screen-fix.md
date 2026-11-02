# 手机端黑屏根治 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让手机端在「内容还没到」时显示骨架屏、「加载失败」时显示可重试的失败态，并让一次 resume 失败不再永久卡死一个会话；同时补上根级错误兜底，使一次渲染异常只毁掉转录区而不是整棵树。

**Architecture:** 五处改动围绕同一个状态机展开。(1) `ThreadView.syncState` 从二态扩成三态 `syncing | synced | failed`，失败时除置态外必须 `SubscriptionWindow.forget(id)` 解开 `selectThread` 的窗口早退，这是「失败后无法重试」的唯一修法；(2) 新增纯函数模块 `lib/syncWatchdog.ts`，用「连续 N 毫秒收不到该 thread 任何帧」判停滞，而不是总时长超时；(3) 新增 `components/TranscriptPlaceholder.tsx` 承载骨架 / 失败 / 崩溃三种态，由 `ChatView` 在转录为空时渲染；(4) `ui/ErrorBoundary.tsx` 增量一个可选 `resetKey`，在根级（`main.tsx`）与转录区（`App.tsx`）各包一层；(5) `CacheNotice` 补失败分支并修掉冷启动时误报「连接断开」。

**Tech Stack:** TypeScript 6 / React 19 / vitest 3 + jsdom / Tailwind 4（`@import "tailwindcss"`，无配置文件，语义色 token 定义在 `src/index.css` 的 `:root`）

**设计文档：** `docs/superpowers/specs/2026-11-01-mobile-blank-screen-root-cause-design.md`

## Global Constraints

- **开工前先跑一次基线并记下两个数**：`cd desktop && npx vitest run`。本计划起草时实测为 **146 个文件 / 1933 条全绿**，`npx tsc --noEmit` 干净（exit 0）。任务完成后的数字必须 ≥ 基线，且失败数为 0。
- **每个新分支都必须先问「桌面端会不会经到」**。`thread/resume` 的 `try/catch/finally`（`App.tsx:1093`—`:1158`）在 `isRemoteClient()` **之外**，两种客户端共享。设计文档 §6.1 有逐条影响面表，改这里必须同时补桌面端用例。
- **占位组件一律先判 `items.length === 0`**，非空时绝不渲染。用户正在读的转录不能被盖住。
- **骨架屏只在 `isRemoteClient()` 为真时渲染**（设计 §4.3 判定顺序第二条）。桌面端不加任何新视觉元素。
- **进入 `failed` 必须与 `subWindow.current.forget(id)` 成对发生**，统一走 `failSync()`，不许在别处单独写 `syncState = "failed"`。
- **`failed` 必须与 `syncing` 一样拦在 `send` 之外**（`App.tsx:2411`）。漏掉这条会把「卡住」升级成「对着不一致的上下文发消息」。
- **`watchdog.stop()` 必须在 `finally` 里**，且 `syncWatchdogs` 在会话被 drop / 重置时清空对应条目，防定时器泄漏。
- **`ErrorBoundary` 只用可选 `resetKey` 增量，不传即保持既有行为**。`PairingScreen.tsx:307` 那处调用不加新属性，语义逐字节不变。
- **不使用 React 的 `key` 来重置 boundary**：那会连带重挂载 `ChatView`，丢掉滚动位置与展开的 reasoning 块（正是本卡要修的切换体验）。
- **不新增任何 npm 依赖**。
- **测试里造「手机端」只需一行**：往 `localStorage` 写 `yi-agent.remote`（`isRemoteClient()` 的唯一判据）。**不要**顺带设 `state.platformUa = IPHONE_UA`——那会让 `isIos()` 为真、`needsPairing()` 可能把 App 换成配对界面，用例会以「找不到侧栏」这种无关症状失败。
- **提交前跑 `cd desktop && npx tsc --noEmit && npx vitest run`**，两者都干净才算过。本计划无 Rust 改动，不跑 cargo。
- **commit message 用 conventional commits，禁止写 `Co-Authored-By`。**

## 文件结构

| 文件 | 职责 | 状态 |
| --- | --- | --- |
| `desktop/src/lib/syncWatchdog.ts` | 停滞检测纯函数：`createWatchdog` / `Watchdog` / `SYNC_IDLE_TIMEOUT_MS`。不 import React、不 import RPC | 新建 |
| `desktop/src/lib/syncWatchdog.test.ts` | 上述模块单测 | 新建 |
| `desktop/src/components/TranscriptPlaceholder.tsx` | 三种占位态（骨架 / 失败 / 崩溃）的纯展示组件，无状态、无 RPC | 新建 |
| `desktop/src/components/TranscriptPlaceholder.test.tsx` | 上述组件单测 | 新建 |
| `desktop/src/ui/AppCrashedScreen.tsx` | 根级崩溃页：满屏说明 + 「重新加载」按钮，不依赖任何 App 状态 | 新建 |
| `desktop/src/ui/ErrorBoundary.tsx` | 增量可选 `resetKey` | 修改 |
| `desktop/src/ui/ErrorBoundary.test.tsx` | `resetKey` 行为与向后兼容 | 新建 |
| `desktop/src/lib/subscriptionWindow.ts` | 增量 `forget(id)` | 修改 |
| `desktop/src/lib/subscriptionWindow.test.ts` | `forget` 行为 | 修改 |
| `desktop/src/lib/threadStore.ts` | `syncState` 三态 + `syncError` 字段 | 修改 |
| `desktop/src/components/CacheNotice.tsx` | 增量 `failed` 分支 | 修改 |
| `desktop/src/components/CacheNotice.test.tsx` | 三态文案 | 新建 |
| `desktop/src/components/ChatView.tsx` | 渲染占位 + 新增 `syncError` / `onRetrySync` / `isRemote` 三个 prop | 修改 |
| `desktop/src/App.tsx` | `failSync`、watchdog 接线、发送拦截、`ChatView` prop、转录区 boundary、重置/清理点 | 修改 |
| `desktop/src/App.test.tsx` | 行为用例 | 修改 |
| `desktop/src/main.tsx` | 根级 boundary + `unhandledrejection` 日志 | 修改 |

## 任务依赖

```
Task 1 (watchdog)  ─┐
Task 2 (forget)    ─┼─→ Task 5 (App 接线: failSync + watchdog + send) ─→ Task 7 (整体验收)
Task 3 (boundary)  ─┤
Task 4 (占位组件)  ─┴─→ Task 6 (ChatView 渲染占位 + 转录区 boundary)
```

Task 1—4 互相独立，可并行。Task 5 依赖 1、2。Task 6 依赖 3、4。Task 7 依赖全部。

---

### Task 1: 停滞检测模块 `syncWatchdog`

**Files:**
- Create: `desktop/src/lib/syncWatchdog.ts`
- Test: `desktop/src/lib/syncWatchdog.test.ts`

**Interfaces:**
- Consumes: 无（零依赖纯函数）
- Produces:
  - `SYNC_IDLE_TIMEOUT_MS: number`（值 `15000`）
  - `interface Watchdog { touch(): void; stop(): void; readonly fired: boolean }`
  - `function createWatchdog(timeoutMs: number, onTimeout: () => void, timer?: Timer): Watchdog`
  - 内部类型 `interface Timer { setTimeout: typeof setTimeout; clearTimeout: typeof clearTimeout }`

**为什么用「连续无活动」而不是「总时长」**：大会话的正常回放可能超过十几秒，总时长超时会频繁误判——那正是「有时黑、有时不黑」的来源之一。

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/syncWatchdog.test.ts`：

```ts
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { createWatchdog, SYNC_IDLE_TIMEOUT_MS } from "./syncWatchdog";

const T = 1000;

describe("createWatchdog", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("fires after the timeout with no activity", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    expect(wd.fired).toBe(false);
    vi.advanceTimersByTime(T - 1);
    expect(onTimeout).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onTimeout).toHaveBeenCalledTimes(1);
    expect(wd.fired).toBe(true);
  });

  it("touch resets the deadline so a long replay never trips it", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    // 每半个超时续一次命，共续 6 次（跨度 3T）：总时长远超 T，但不该触发。
    for (let i = 0; i < 6; i += 1) {
      vi.advanceTimersByTime(T / 2);
      wd.touch();
    }
    expect(onTimeout).not.toHaveBeenCalled();
  });

  it("stop prevents a late fire after the resume settles", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    vi.advanceTimersByTime(T / 2);
    wd.stop();
    vi.advanceTimersByTime(T * 10);
    expect(onTimeout).not.toHaveBeenCalled();
    expect(wd.fired).toBe(false);
  });

  it("touch after fire is a no-op (no double report)", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    vi.advanceTimersByTime(T);
    expect(onTimeout).toHaveBeenCalledTimes(1);
    wd.touch();
    vi.advanceTimersByTime(T * 5);
    expect(onTimeout).toHaveBeenCalledTimes(1);
  });

  it("touch after stop does not resurrect the timer", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    wd.stop();
    wd.touch();
    vi.advanceTimersByTime(T * 5);
    expect(onTimeout).not.toHaveBeenCalled();
  });

  it("stop after fire does not throw", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    vi.advanceTimersByTime(T);
    expect(() => wd.stop()).not.toThrow();
  });

  it("defaults to a 15s idle threshold", () => {
    expect(SYNC_IDLE_TIMEOUT_MS).toBe(15_000);
  });
});
```

- [ ] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/lib/syncWatchdog.test.ts`
Expected: FAIL，`Failed to resolve import "./syncWatchdog"`（模块还不存在）。

- [ ] **Step 3: 写最小实现**

创建 `desktop/src/lib/syncWatchdog.ts`：

```ts
/**
 * resume 的「停滞检测」：连续这么久收不到该 thread 的任何帧，就判定这次
 * 全量回放已经死了。
 *
 * 为什么不是「总时长超时」：大会话的正常回放可以超过十几秒（服务端按
 * 200 条 / 256 KiB 分块下发），一刀切会在长会话上频繁误判——那正是「有时
 * 卡、有时不卡」的来源。用「连续无活动」判据，健康的长回放每收一帧就续命
 * 一次，永远不会误伤。
 *
 * 为什么不低于 10s：高丢包的移动网络下，单块重传就可能占掉几秒。
 */
export const SYNC_IDLE_TIMEOUT_MS = 15_000;

export interface Watchdog {
  /** 收到该 thread 的活动（任意帧 / 响应），重置计时。 */
  touch(): void;
  /** 停止计时。resume 落定（成功或失败）后必须调用，否则会延迟误触发。 */
  stop(): void;
  /** 是否已经判定超时。 */
  readonly fired: boolean;
}

interface Timer {
  setTimeout: typeof setTimeout;
  clearTimeout: typeof clearTimeout;
}

const defaultTimer: Timer = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (h) => clearTimeout(h),
};

/**
 * 建一个只会计时、不会自己重试的看门狗。`onTimeout` 保证最多调用一次。
 *
 * `timer` 可注入是为了单测能用 fake timers 之外的实现；生产一律用默认值。
 */
export function createWatchdog(
  timeoutMs: number,
  onTimeout: () => void,
  timer: Timer = defaultTimer,
): Watchdog {
  let handle: ReturnType<typeof setTimeout> | null = null;
  let fired = false;
  let stopped = false;

  const arm = (): void => {
    if (handle !== null) timer.clearTimeout(handle);
    handle = timer.setTimeout(() => {
      handle = null;
      // `stop()` 之后不该再有回调；`fired` 保证不重复上报。
      if (stopped || fired) return;
      fired = true;
      onTimeout();
    }, timeoutMs);
  };

  arm();

  return {
    touch(): void {
      // 已判死或已停机时续命没有意义：前者会掩盖已上报的失败，后者会泄漏定时器。
      if (stopped || fired) return;
      arm();
    },
    stop(): void {
      stopped = true;
      if (handle !== null) {
        timer.clearTimeout(handle);
        handle = null;
      }
    },
    get fired(): boolean {
      return fired;
    },
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/syncWatchdog.test.ts`
Expected: PASS，7 passed。

- [ ] **Step 5: 跑一次类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: exit 0，无输出。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/lib/syncWatchdog.ts desktop/src/lib/syncWatchdog.test.ts
git commit -m "feat(mobile): 新增 resume 停滞检测纯函数模块

用「连续无活动」而非总时长判停滞：健康的长回放每收一帧就续命，
不会在大会话上误判。onTimeout 保证最多触发一次。"
```

---

### Task 2: `SubscriptionWindow.forget` 与 `syncState` 三态

**Files:**
- Modify: `desktop/src/lib/subscriptionWindow.ts`
- Modify: `desktop/src/lib/threadStore.ts:40-48`（`syncState` 类型与 `syncError` 字段）
- Modify: `desktop/src/lib/threadStore.ts:81`（`create()` 默认值）
- Test: `desktop/src/lib/subscriptionWindow.test.ts`
- Test: `desktop/src/lib/threadStore.test.ts`

**Interfaces:**
- Consumes: 无
- Produces:
  - `SubscriptionWindow.forget(id: string): void`
  - `ThreadView.syncState: "syncing" | "synced" | "failed"`
  - `ThreadView.syncError?: string | null`

**背景（改动原因）**：`selectThread` 进入时无条件 `subWindow.touch(id)`（`App.tsx:978`），而早退判据是 `inWindow || running`（`App.tsx:1040`）。resume 失败后该 id 已在窗口里，用户再点多少次都被早退挡掉——实测连点三次，`thread/resume` 请求总数固定为 2。`forget` 就是解开这个死锁的唯一手段。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/subscriptionWindow.test.ts` 末尾追加（先看一遍该文件已有的 `describe` 结构，把新用例放进新的 `describe` 里）：

```ts
describe("SubscriptionWindow.forget", () => {
  it("removes the id so the early-return guard no longer matches", () => {
    const w = new SubscriptionWindow(3);
    w.touch("t1");
    expect(w.has("t1")).toBe(true);
    w.forget("t1");
    expect(w.has("t1")).toBe(false);
  });

  it("keeps the other ids in most-recent-first order", () => {
    const w = new SubscriptionWindow(3);
    w.touch("a");
    w.touch("b");
    w.touch("c");
    w.forget("b");
    expect(w.current()).toEqual(["c", "a"]);
  });

  it("is a no-op for an unknown id", () => {
    const w = new SubscriptionWindow(3);
    w.touch("a");
    expect(() => w.forget("zzz")).not.toThrow();
    expect(w.current()).toEqual(["a"]);
  });

  it("does not break the LRU cap afterwards", () => {
    const w = new SubscriptionWindow(2);
    w.touch("a");
    w.touch("b");
    w.forget("a");
    w.touch("c");
    w.touch("d");
    expect(w.current()).toEqual(["d", "c"]);
  });
});
```

在 `desktop/src/lib/threadStore.test.ts` 末尾追加：

```ts
describe("ThreadView syncState", () => {
  it("defaults syncState to synced so the desktop path is unchanged", () => {
    const s = new ThreadStore();
    expect(s.view("t1").syncState).toBe("synced");
  });

  it("carries a failure reason alongside the failed state", () => {
    const s = new ThreadStore();
    const v = s.view("t1");
    v.syncState = "failed";
    v.syncError = "unknown thread";
    expect(s.peek("t1")?.syncState).toBe("failed");
    expect(s.peek("t1")?.syncError).toBe("unknown thread");
  });

  it("leaves syncError absent on a healthy view", () => {
    const s = new ThreadStore();
    expect(s.view("t1").syncError).toBeUndefined();
  });
});
```

> 若 `threadStore.test.ts` 里已有 `defaults syncState to synced so the desktop path is unchanged` 这个同名用例（本计划起草时它已存在于 `:387`），**不要重复添加**；只补后两条，并把它们放进同一个 `describe` 块。

- [ ] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/lib/subscriptionWindow.test.ts src/lib/threadStore.test.ts`
Expected: FAIL。`forget` 报 `w.forget is not a function`；`syncError` 报类型错误或断言失败。

- [ ] **Step 3: 实现 `forget`**

在 `desktop/src/lib/subscriptionWindow.ts` 的 `current()` 之前插入：

```ts
  /**
   * 忘掉某条 thread。
   *
   * 唯一的调用点是「对账失败」：`App.selectThread` 进入时无条件 `touch(id)`
   * 把它记进窗口，而 `:1040` 的早退判据是 `inWindow || running`。不把它摘掉，
   * 用户再点多少次都会被那个早退挡回去，永远没有重试机会（实测连点三次，
   * `thread/resume` 总数固定为 2）。
   */
  forget(id: string): void {
    this.ids = this.ids.filter((x) => x !== id);
  }
```

- [ ] **Step 4: 实现 `syncState` 三态与 `syncError`**

在 `desktop/src/lib/threadStore.ts` 中把该字段（`:40-48` 的文档注释 + 类型）改为：

```ts
  /**
   * Whether this thread's transcript has been reconciled with the server on
   * this connection. `syncing` means the catch-up (resume / readItems) is in
   * flight: the content on screen may be the cached one, so the UI must say so
   * and must not let the user send against it (spec §5).
   *
   * `failed` 是本次新增：对账**失败**（服务端拒绝、或者连续一段时间收不到
   * 任何帧被判停滞）。它与 `syncing` 的区别是「已经不用等了」，必须给用户
   * 一个显式重试入口，且同样不许发送——屏上的内容可能与服务端不一致。
   *
   * 注意：`failed` 不只在手机端出现。`thread/resume` 的收尾是两种客户端
   * 共享的代码，桌面端失败时也会置成 `failed`（见设计文档 §6.1）。
   */
  syncState: "syncing" | "synced" | "failed";
  /**
   * 上次对账失败的原因（已格式化，可直接上屏）。仅 `syncState === "failed"`
   * 时有意义；其余状态下应为 `undefined`。
   */
  syncError?: string | null;
```

`create()` 的返回值（`:81` 附近）保持 `syncState: "synced"`，**不要**加 `syncError` 的默认值——保持 `undefined` 才能让「没有失败」与「失败但原因为空」区分开，且不改变既有对象的形状。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/subscriptionWindow.test.ts src/lib/threadStore.test.ts`
Expected: PASS。

- [ ] **Step 6: 跑全量，确认加宽枚举没有打破任何既有断言**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: tsc exit 0；测试 ≥ 1933 条全绿。**若某处 `switch` 或 `if/else` 因新枚举值报类型错，就地补齐分支并说明理由**——那正是「枚举比布尔量好」的价值所在。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/lib/subscriptionWindow.ts desktop/src/lib/subscriptionWindow.test.ts \
        desktop/src/lib/threadStore.ts desktop/src/lib/threadStore.test.ts
git commit -m "feat(mobile): syncState 增加 failed 态，并为订阅窗口加 forget

selectThread 进入时无条件 touch(id)，而早退判据是 inWindow，
于是 resume 失败后该会话再也点不动。forget 是唯一的解锁手段。"
```

---

### Task 3: `ErrorBoundary` 增 `resetKey` + 根级崩溃页

**Files:**
- Modify: `desktop/src/ui/ErrorBoundary.tsx`
- Create: `desktop/src/ui/AppCrashedScreen.tsx`
- Test: `desktop/src/ui/ErrorBoundary.test.tsx`

**Interfaces:**
- Consumes: 无
- Produces:
  - `ErrorBoundaryProps.resetKey?: unknown`
  - `AppCrashedScreen(): JSX.Element`（无 props）

**为什么不用 `key`**：`key` 变化会让 React **卸载并重挂**整棵子树。转录区 boundary 若用 `key={currentId}`，每次切会话都会重挂 `ChatView`，丢掉滚动位置与展开的 reasoning 块——恰好砸在本卡要修的切换体验上。`resetKey` 只清 `hasError`，子树原地保留。

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/ui/ErrorBoundary.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup, act } from "@testing-library/react";
import { useState } from "react";
import { ErrorBoundary } from "./ErrorBoundary";

afterEach(cleanup);

function Boom(): JSX.Element {
  throw new Error("boom");
}

describe("ErrorBoundary", () => {
  it("keeps the pre-resetKey behaviour when resetKey is omitted", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(
      <ErrorBoundary fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    expect(screen.getByTestId("fb")).toBeTruthy();
    spy.mockRestore();
  });

  it("calls onError exactly once", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const onError = vi.fn();
    render(
      <ErrorBoundary onError={onError} fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    expect(onError).toHaveBeenCalledTimes(1);
    spy.mockRestore();
  });

  it("renders children untouched when nothing throws", () => {
    render(
      <ErrorBoundary resetKey="a">
        <div data-testid="ok">ok</div>
      </ErrorBoundary>,
    );
    expect(screen.getByTestId("ok")).toBeTruthy();
  });

  function Recovering() {
    const [resetKey, setResetKey] = useState("a");
    const [broken, setBroken] = useState(true);
    return (
      <>
        <button
          onClick={() => {
            setBroken(false);
            setResetKey("b");
          }}
        >
          fix
        </button>
        <ErrorBoundary resetKey={resetKey} fallback={<div data-testid="fb">failed</div>}>
          {broken ? <Boom /> : <div data-testid="recovered">recovered</div>}
        </ErrorBoundary>
      </>
    );
  }

  it("clears hasError when resetKey changes", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    render(<Recovering />);
    expect(screen.getByTestId("fb")).toBeTruthy();
    // React 19 下必须包 act，否则状态更新不会同步刷新断言。
    act(() => {
      screen.getByText("fix").click();
    });
    expect(screen.getByTestId("recovered")).toBeTruthy();
    spy.mockRestore();
  });

  it("does NOT clear hasError while resetKey stays the same", () => {
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    const { rerender } = render(
      <ErrorBoundary resetKey="same" fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    rerender(
      <ErrorBoundary resetKey="same" fallback={<div data-testid="fb">failed</div>}>
        <Boom />
      </ErrorBoundary>,
    );
    expect(screen.getByTestId("fb")).toBeTruthy();
    spy.mockRestore();
  });
});
```

同时创建 `desktop/src/ui/AppCrashedScreen.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { AppCrashedScreen } from "./AppCrashedScreen";

afterEach(cleanup);

describe("AppCrashedScreen", () => {
  it("explains the state and offers a reload button", () => {
    render(<AppCrashedScreen />);
    expect(screen.getByText(/界面出了点问题/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "重新加载" })).toBeTruthy();
  });

  it("reloads the page when the button is pressed", () => {
    const reload = ({ reload: () => {} } as unknown) as () => void;
    void reload;
    // jsdom 的 location.reload 不可直接赋值；用 spy 观察调用即可。
    const spy = vi
      .spyOn(window.location, "reload")
      .mockImplementation(() => {});
    render(<AppCrashedScreen />);
    screen.getByRole("button", { name: "重新加载" }).click();
    expect(spy).toHaveBeenCalledTimes(1);
    spy.mockRestore();
  });
});
```

> 若 `vi.spyOn(window.location, "reload")` 在 jsdom 下不可写，改成在组件里调用
> 一个可注入的 `onReload?: () => void`，默认 `() => location.reload()`，测试传
> 自己的 spy。两侧任选其一，但**行为必须被断言到**。

- [ ] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/ui/ErrorBoundary.test.tsx src/ui/AppCrashedScreen.test.tsx`
Expected: FAIL，`Cannot find module "./AppCrashedScreen"`；`resetKey` 的两个用例失败。

- [ ] **Step 3: 给 `ErrorBoundary` 增 `resetKey`**

把 `desktop/src/ui/ErrorBoundary.tsx` 改为（**保留原有全部文档注释，只加新属性与新静态方法**）：

```tsx
import { Component, type ErrorInfo, type ReactNode } from "react";

export interface ErrorBoundaryProps {
  children: ReactNode;
  /** 捕获到异常时回调一次，供宿主收尾（收起覆盖层、显示文案等）。 */
  onError?: (error: unknown) => void;
  /** 出错时渲染的内容；缺省什么都不渲染。 */
  fallback?: ReactNode;
  /**
   * 该值变化时清掉 `hasError`，让子树在原位置恢复渲染。
   *
   * 为什么不用 React 的 `key`：`key` 变化会**卸载并重挂**整棵子树。转录区
   * 的 boundary 若用 `key={currentId}`，每次切会话都会重挂 `ChatView`，把
   * 滚动位置与展开的 reasoning 块全部丢掉——那正是本卡要修的场景。
   * `resetKey` 只重置错误位，子树原地保留。
   *
   * 不传时行为与历史版本逐字节一致（配对页那处调用就不传）。
   */
  resetKey?: unknown;
}

interface ErrorBoundaryState {
  hasError: boolean;
  /** 上一次生效的 `resetKey`，用来判断它是否真的变了。 */
  seenKey: unknown;
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { hasError: false, seenKey: this.props.resetKey };

  static getDerivedStateFromError(): Partial<ErrorBoundaryState> {
    return { hasError: true };
  }

  static getDerivedStateFromProps(
    props: ErrorBoundaryProps,
    state: ErrorBoundaryState,
  ): Partial<ErrorBoundaryState> | null {
    if (props.resetKey !== state.seenKey) {
      return { hasError: false, seenKey: props.resetKey };
    }
    // 必须返回 null（而不是 `{}`）：返回对象会触发一次多余的重渲染。
    return null;
  }

  componentDidCatch(error: unknown, _info: ErrorInfo): void {
    this.props.onError?.(error);
  }

  render(): ReactNode {
    if (this.state.hasError) return this.props.fallback ?? null;
    return this.props.children;
  }
}
```

> **若原文件的文档注释更长，全部保留**，只在其后追加 `resetKey` 的注释块。

- [ ] **Step 4: 写根级崩溃页**

创建 `desktop/src/ui/AppCrashedScreen.tsx`：

```tsx
/**
 * 根级崩溃页：整个 App 的渲染期异常被 `main.tsx` 的 `ErrorBoundary` 接住后
 * 显示它。
 *
 * 硬约束：**不依赖任何 App 状态、不 import 任何业务组件**。它可能在 App
 * 初始化到一半时被挂上（那时 store / prefs / RPC 都不可信），任何外部依赖
 * 都可能让兜底本身再抛一次。样式只用 `index.css` 的语义色 token 类。
 */
export function AppCrashedScreen({ onReload }: { onReload?: () => void }) {
  return (
    <div className="flex h-screen w-screen flex-col items-center justify-center gap-4 bg-surface px-6 text-center text-fg">
      <div className="text-base font-medium">界面出了点问题</div>
      <p className="max-w-sm text-sm text-fg-muted">
        刚才的某个界面错误让整个页面停了下来。会话内容都在服务端，重新加载不会
        丢东西。
      </p>
      <button
        type="button"
        onClick={() => (onReload ?? (() => window.location.reload()))()}
        className="rounded-md border border-line-strong px-4 py-2 text-sm hover:bg-raised"
      >
        重新加载
      </button>
    </div>
  );
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/ui/ErrorBoundary.test.tsx src/ui/AppCrashedScreen.test.tsx`
Expected: PASS。

- [ ] **Step 6: 确认配对页那条既有路径没被影响**

Run: `cd desktop && npx vitest run src/components/PairingScreen.test.tsx`
Expected: PASS（`resetKey` 不传 = 旧行为）。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/ui/ErrorBoundary.tsx desktop/src/ui/ErrorBoundary.test.tsx \
        desktop/src/ui/AppCrashedScreen.tsx desktop/src/ui/AppCrashedScreen.test.tsx
git commit -m "feat(ui): ErrorBoundary 支持 resetKey，并加根级崩溃页

resetKey 只清 hasError 而不重挂子树：用 key 会连 ChatView 一起重挂，
每次切会话丢滚动位置。不传 resetKey 时行为与历史版本一致。"
```

---

### Task 4: 占位组件 `TranscriptPlaceholder`

**Files:**
- Create: `desktop/src/components/TranscriptPlaceholder.tsx`
- Test: `desktop/src/components/TranscriptPlaceholder.test.tsx`

**Interfaces:**
- Consumes: 无
- Produces:
  - `type PlaceholderKind = "loading" | "failed" | "crashed"`
  - `function TranscriptPlaceholder(props: { kind: PlaceholderKind; reason?: string | null; onRetry?: () => void }): JSX.Element`
  - `data-testid`：`sync-skeleton`（loading）、`sync-failed`（failed）、`sync-crashed`（crashed）

**为什么 `loading` 要有 `data-testid`**：设计 §7.2 的行为用例靠它断言「有骨架、不是空容器」，这是 R1 的可测出口。

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/components/TranscriptPlaceholder.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { TranscriptPlaceholder } from "./TranscriptPlaceholder";

afterEach(cleanup);

describe("TranscriptPlaceholder", () => {
  it("renders a skeleton with an accessible label while loading", () => {
    render(<TranscriptPlaceholder kind="loading" />);
    const el = screen.getByTestId("sync-skeleton");
    expect(el).toBeTruthy();
    // 骨架是纯装饰，必须对读屏用户隐藏，避免念一串无意义的空块。
    expect(el.getAttribute("aria-hidden")).toBe("true");
  });

  it("hides the decorative bars from assistive tech but keeps a live region", () => {
    render(<TranscriptPlaceholder kind="loading" />);
    // 状态本身要能被读屏念出来。
    expect(screen.getByRole("status").textContent).toContain("正在加载");
  });

  it("does not render a retry button while loading", () => {
    render(<TranscriptPlaceholder kind="loading" onRetry={() => {}} />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("renders the failure reason and a retry button", () => {
    const onRetry = vi.fn();
    render(
      <TranscriptPlaceholder kind="failed" reason="内容加载超时，请重试" onRetry={onRetry} />,
    );
    expect(screen.getByTestId("sync-failed")).toBeTruthy();
    expect(screen.getByText("内容加载超时，请重试")).toBeTruthy();
    screen.getByRole("button", { name: "重试" }).click();
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("survives a missing reason", () => {
    render(<TranscriptPlaceholder kind="failed" onRetry={() => {}} />);
    expect(screen.getByTestId("sync-failed")).toBeTruthy();
    expect(screen.getByRole("button", { name: "重试" })).toBeTruthy();
  });

  it("does not render a retry button when no handler is given", () => {
    render(<TranscriptPlaceholder kind="failed" reason="x" />);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("renders the crash variant with its own wording", () => {
    render(<TranscriptPlaceholder kind="crashed" reason="render boom" />);
    expect(screen.getByTestId("sync-crashed")).toBeTruthy();
    expect(screen.getByText(/这段内容渲染失败/)).toBeTruthy();
    expect(screen.getByText("render boom")).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/components/TranscriptPlaceholder.test.tsx`
Expected: FAIL，`Failed to resolve import "./TranscriptPlaceholder"`。

- [ ] **Step 3: 写实现**

创建 `desktop/src/components/TranscriptPlaceholder.tsx`：

```tsx
/**
 * 转录区在**没有内容**时显示的三种态。
 *
 * 存在的理由：手机 `body` 底色是深色（`index.css` 的 `--panel`），转录区为空
 * 与「一片黑」在视觉上无法区分。用户看到黑屏时既不知道在不在加载、也不知道
 * 能不能重试。这个组件把那三种「本该有内容但没有」的情形说清楚。
 *
 * 只做展示：不读 store、不发 RPC、不持有状态。重试由宿主注入。
 */
export type PlaceholderKind = "loading" | "failed" | "crashed";

const SKELETON_WIDTHS = ["88%", "64%", "78%", "42%"];

export function TranscriptPlaceholder({
  kind,
  reason,
  onRetry,
}: {
  kind: PlaceholderKind;
  /** 失败原因（已格式化）。仅 failed / crashed 使用。 */
  reason?: string | null;
  /** 重试回调。不传则不渲染按钮（`crashed` 的复位由宿主负责）。 */
  onRetry?: () => void;
}) {
  if (kind === "loading") {
    return (
      <div
        role="status"
        className="flex flex-col gap-3 px-1 py-3"
        aria-live="polite"
      >
        <span className="sr-only">正在加载会话内容</span>
        <div data-testid="sync-skeleton" aria-hidden="true" className="flex flex-col gap-3">
          {SKELETON_WIDTHS.map((width, i) => (
            <div
              key={i}
              className="h-4 rounded-md bg-raised motion-safe:animate-pulse"
              style={{ width }}
            />
          ))}
        </div>
      </div>
    );
  }

  const crashed = kind === "crashed";
  return (
    <div
      data-testid={crashed ? "sync-crashed" : "sync-failed"}
      className="flex flex-col items-center gap-3 px-4 py-10 text-center"
    >
      <div className="text-sm font-medium text-fg">
        {crashed ? "这段内容渲染失败" : "内容加载失败"}
      </div>
      {reason ? (
        <p className="max-w-sm break-words text-xs text-fg-muted">{reason}</p>
      ) : null}
      {onRetry ? (
        <button
          type="button"
          onClick={onRetry}
          className="rounded-md border border-line-strong px-4 py-2 text-sm hover:bg-raised"
        >
          重试
        </button>
      ) : null}
      {crashed ? (
        <p className="max-w-sm text-xs text-fg-subtle">
          侧栏和输入框还可以用：可以切到其它会话，或重新连接。
        </p>
      ) : null}
    </div>
  );
}
```

> `sr-only` 与 `motion-safe:` 是 Tailwind 4 内建变体。本项目 `index.css` 用
> `@import "tailwindcss"`、无配置文件，两者都可用。若 `sr-only` 在某处被判为
> 未定义（本仓库没出现过，但先确认），改用 `className="absolute h-px w-px overflow-hidden"`
> 的等价写法并保留同一断言。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/TranscriptPlaceholder.test.tsx`
Expected: PASS，7 passed。

- [ ] **Step 5: 跑类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: exit 0。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/TranscriptPlaceholder.tsx \
        desktop/src/components/TranscriptPlaceholder.test.tsx
git commit -m "feat(mobile): 新增转录区占位组件（骨架/失败/崩溃）

手机深色底上，空转录与黑屏无法区分。三种态各自说明在加载、
加载失败可重试、渲染异常可恢复。纯展示，不读 store 不发 RPC。"
```

---

### Task 5: App 接线——`failSync`、watchdog、发送拦截

**Files:**
- Modify: `desktop/src/App.tsx:244-252`（新增 ref）
- Modify: `desktop/src/App.tsx:1027`（readItems 的 `finally`）
- Modify: `desktop/src/App.tsx:1086-1158`（resume 分支：watchdog + `failSync`）
- Modify: `desktop/src/App.tsx:1734`（通知入口 touch）
- Modify: `desktop/src/App.tsx:2411`（发送拦截）
- Modify: `desktop/src/App.tsx:931`、`:1525`、`:2906`（清理点）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes（Task 1、2 的产物）：
  - `createWatchdog(timeoutMs, onTimeout)`, `Watchdog`, `SYNC_IDLE_TIMEOUT_MS`
  - `SubscriptionWindow.forget(id)`
  - `ThreadView.syncState` 含 `"failed"`，`ThreadView.syncError`
- Produces:
  - `App.tsx` 内部函数 `failSync(id: string, error: string): void`
  - ref `syncWatchdogs: MutableRefObject<Map<string, Watchdog>>`
  - ref `notifySyncActivity: MutableRefObject<(threadId: string) => void>`

**为什么用 ref 桥接**：`selectThread` 在组件体（缩进 2），通知回调在 `useEffect` 内的 `wireClient`（缩进 8），两者不在同一作用域。用一个挂在组件体的 ref 让 effect 侧调用组件体的函数，是仓库既有的做法（`captureTranscriptRef`、`storeRef` 都是这个模式）。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/App.test.tsx` 末尾追加（沿用该文件已有的 `clients` / `state` / `asRemote` 模式；`asRemote` 在 `App remote subscription wiring (S2)` 那个 `describe` 里定义，新块里要自己重定义一次）：

```ts
describe("App 同步状态机（手机端黑屏）", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  const resumeCount = () =>
    clients[0].requests.filter((r) => r.method === "thread/resume").length;

  it("a failing resume is not reported as synced", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () => {
      throw { code: -32602, message: "unknown thread" };
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() => expect(resumeCount()).toBeGreaterThan(0));
    // 失败必须落在 failed，而不是历史行为里的「谎报 synced」。
    await waitFor(() =>
      expect(document.querySelector('[data-testid="sync-failed"]')).toBeTruthy(),
    );
  });

  it("tapping a failed thread again retries instead of bailing", async () => {
    asRemote();
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    let calls = 0;
    state.dataSources["thread/resume"] = (params) => {
      calls += 1;
      const id = (params as { threadId: string }).threadId;
      // 前两次（握手里的 t1、首次点 t2）拒绝，之后放行——模拟用户重试成功。
      if (calls <= 2) throw { code: -32602, message: "unknown thread" };
      return { thread_id: id, cwd: "/w", model: "m", permission_mode: "normal" };
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() => expect(resumeCount()).toBeGreaterThan(0));

    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(resumeCount()).toBe(2));
    // 再点一次：修复前被 inWindow 早退挡掉，计数停在 2。
    fireEvent.click(screen.getByText("two"));
    await waitFor(() => expect(resumeCount()).toBe(3));
  });

  it("retrying from the failure pane re-issues thread/resume", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () => {
      throw { code: -32602, message: "unknown thread" };
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() =>
      expect(document.querySelector('[data-testid="sync-failed"]')).toBeTruthy(),
    );
    const before = resumeCount();
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() => expect(resumeCount()).toBe(before + 1));
  });

  it("a failed thread refuses to send and explains why", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () => {
      throw { code: -32602, message: "unknown thread" };
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() =>
      expect(document.querySelector('[data-testid="sync-failed"]')).toBeTruthy(),
    );
    const box = document.querySelector("textarea") as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "hello" } });
    // 仓库的发送键是 Enter（见既有 "sends on Enter" 类用例）；用它而不是找按钮。
    fireEvent.keyDown(box, { key: "Enter", code: "Enter" });
    await waitFor(() =>
      expect(document.body.textContent).toContain("内容尚未加载完成"),
    );
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("keeps the transcript and shows no placeholder when content already arrived", async () => {
    asRemote();
    state.dataSources["thread/readItems"] = () => ({
      items: [
        {
          id: "i1",
          type: "agentMessage",
          text: "已经到达的正文",
        },
      ],
    });
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() =>
      expect(document.body.textContent).toContain("已经到达的正文"),
    );
    expect(document.querySelector('[data-testid="sync-skeleton"]')).toBeNull();
    expect(document.querySelector('[data-testid="sync-failed"]')).toBeNull();
  });

  it("never shows a skeleton to a desktop client", async () => {
    // 不写 yi-agent.remote → 桌面端
    state.dataSources["thread/resume"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 50));
    expect(document.querySelector('[data-testid="sync-skeleton"]')).toBeNull();
    expect(document.querySelector('[data-testid="sync-failed"]')).toBeNull();
  });

  it("desktop: a failed resume marks the thread failed and still blocks sending", async () => {
    // 设计 §6.1：resume 的收尾是两种客户端共享的，桌面端也会置 failed。
    state.dataSources["thread/resume"] = () => {
      throw { code: -32602, message: "unknown thread" };
    };
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() => expect(resumeCount()).toBeGreaterThan(0));
    const box = document.querySelector("textarea") as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "hello" } });
    fireEvent.keyDown(box, { key: "Enter", code: "Enter" });
    await waitFor(() =>
      expect(document.body.textContent).toContain("内容尚未加载完成"),
    );
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });
});
```

> **Enter 发送键需先确认**：动手前在 `App.test.tsx` 里搜 `key: "Enter"`，照抄
> 既有用例的触发方式（可能要求先 `fireEvent.change` 再 `keyDown`，也可能走
> Send 按钮）。**不要凭空猜**，否则用例失败原因是「没触发发送」而不是「没拦住」。

- [ ] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App 同步状态机"`
Expected: FAIL。`sync-failed` 找不到（占位还没接进 `ChatView` 之前的几条会失败）。

- [ ] **Step 3: 加 ref 与 `failSync`**

在 `desktop/src/App.tsx` 的 ref 区（`inFlightResume` 之后、`subWindow` 之前）插入：

```ts
  // 每个正在全量回放的会话一个看门狗：连续收不到该 thread 的任何帧就判停滞。
  // key 是 thread id；resume 落定（成功/失败）时删条目并 stop。
  const syncWatchdogs = useRef(new Map<string, Watchdog>());
  // 通知回调（effect 内）→ 组件体的续命入口。两者不在同一作用域，用 ref 桥接
  // （与 captureTranscriptRef / storeRef 同一模式）。
  const notifySyncActivity = useRef<(threadId: string) => void>(() => {});
```

在组件体里、`selectThread` 定义（`:841`）**之前**插入：

```ts
  /**
   * 对账失败的唯一收尾路径。
   *
   * 三件事必须一起做，缺一条就会留下一个「点不动」的会话：
   * 1. 置 `failed` + 记原因 —— 界面据此显示失败态与重试按钮；
   * 2. `subWindow.forget(id)` —— `selectThread:1040` 的早退判据是
   *    `inWindow || running`，而进入时无条件 `touch(id)` 已经把它记进窗口。
   *    不摘掉，用户再点多少次都不会重走 resume（实测连点三次，计数固定为 2）；
   * 3. `warm.delete(id)` —— 这条会话并没有成功对账过，不该被当成热会话。
   *
   * 保留 `session.lastError`：连接类错误仍要靠它触发既有的
   * `clearConnectionErrorFrom` 自愈逻辑。
   */
  const failSync = (id: string, error: string): void => {
    const v = store.peek(id);
    if (v) {
      v.syncState = "failed";
      v.syncError = error;
      v.session.lastError = error;
    }
    subWindow.current.forget(id);
    warm.current.delete(id);
    force((n) => n + 1);
  };

  notifySyncActivity.current = (threadId: string) => {
    syncWatchdogs.current.get(threadId)?.touch();
  };
```

在 `App.tsx` 顶部 import 区加：

```ts
import { createWatchdog, SYNC_IDLE_TIMEOUT_MS, type Watchdog } from "./lib/syncWatchdog";
```

- [ ] **Step 4: readItems 的 `finally` 区分成功/失败**

把 `App.tsx:1020-1033` 的 `finally` 块改为（**注意 `try` 里原本就在成功分支写 `syncState`，现在要在 `catch` 里记失败**）：

```ts
            let failed: unknown = null;
            try {
              const r = await c0.request<{
                items: Item[];
                usage?: WireUsage | null;
                totals?: WireTotals | null;
              }>(
                "thread/readItems",
                after ? { threadId: id, afterItemId: after } : { threadId: id },
              );
              const v = store.peek(id);
              if (v) {
                v.session.upsertItems(r.items);
                v.session.adoptPersistedUsage(wireUsageToUsage(r.usage));
                v.session.adoptPersistedTotals(wireTotalsToTotals(r.totals));
                force((n) => n + 1);
              }
            } catch (e) {
              // 补齐是附加能力：读失败就保留已有内容。但要记下来——否则下面那个
              // `finally` 会把它当成「已对账」，用户看到的是「既不加载也不报错」。
              failed = e;
            } finally {
              const v = store.peek(id);
              if (v) {
                if (!silentTopUp) {
                  if (failed === null) {
                    v.syncState = "synced";
                    // 该会话已与服务端对过账：去掉"这是缓存"的标注（spec §5.3）。
                    cachedAt.current.delete(id);
                  } else if (v.session.items.length === 0) {
                    // 没补到任何内容又失败了：这才是真的没东西可看。
                    // 有内容时不打扰——用户手上至少不是一片空白。
                    v.syncState = "failed";
                    v.syncError = formatError(failed);
                  } else {
                    // 有内容但补齐失败：不谎报 synced，也不判死；停在 syncing 外的
                    // 中立态（synced）但保留缓存标注是错的两个方向，故取 synced。
                    v.syncState = "synced";
                  }
                }
                force((n) => n + 1);
              }
            }
```

> 上面第三个分支就是「有内容、补齐失败」：既不显示失败占位（有内容不打扰），
> 也不能说「已对账」。取 `synced` 是为了不让 `send` 被永久拦住——内容已经能看，
> 用户发送是合理的。这一条要在 Step 2 之后**实际跑一遍**确认没有既有用例反对。

- [ ] **Step 5: resume 分支接 watchdog 与 `failSync`**

把 `App.tsx:1086` 起的 resume 分支改为（保留原有注释）：

```ts
    const coldView = store.peek(id);
    if (coldView) coldView.syncState = "syncing";
    inFlightResume.current.add(id);
    // 停滞检测：健康的回放每收一帧就续命，只有真的断了才会触发。
    const watchdog = createWatchdog(SYNC_IDLE_TIMEOUT_MS, () =>
      failSync(id, "内容加载超时，请重试"),
    );
    syncWatchdogs.current.set(id, watchdog);
    const replaySession = store.view(id).session;
    replaySession.beginReplay();
```

把 `:1141-1157` 的 `catch` / `finally` 改为：

```ts
    } catch (e) {
      replaySession.endReplay();
      // 失败路径只在视图仍存在时构造文案，避免 re-create 一个已被 drop 的
      // ThreadView（无界泄漏 + 残留 warm 条目）。可见性判断收在 failSync 里。
      //
      // 原来这里还有一句 `force((v) => v + 1)` —— 删掉它：failSync 内部
      // 已经 force 过一次，留着只是多一次无谓的重渲染。
      if (store.peek(id)) failSync(id, formatError(e));
    } finally {
      // 停机是硬要求：不停的话，一次成功的加载会被这个定时器在 15s 后
      // 反向判成失败。
      watchdog.stop();
      syncWatchdogs.current.delete(id);
      inFlightResume.current.delete(id);
    }
```

- [ ] **Step 6: 通知入口续命**

在 `App.tsx:1734` 的 `ownsThreadView` 早退**之后**、该回调的其余逻辑**之前**插入：

```ts
        // 回放帧就是「活动」：续一次命。放在 ownsThreadView 之后是刻意的——
        // 非活动槽位的帧不会写本视图，用它们续命会掩盖「当前会话真的卡住」。
        {
          const p = n.params as { thread_id?: unknown };
          if (typeof p?.thread_id === "string") {
            notifySyncActivity.current(p.thread_id);
          }
        }
```

- [ ] **Step 7: 发送拦截加 `failed`**

把 `App.tsx:2411` 起的块改为：

```ts
    // 离线时不发：缓存内容可以看，但不能对着它发消息（服务端根本收不到）。
    // 只提示、保留草稿，交给用户连上后再发——不排队、不自动重发。
    // 同样地，会话尚在对账中（syncing）也不能发：屏上可能还是旧缓存，此刻发出去
    // 的上下文与服务端实际上下文可能不一致（spec §5.1）。
    // **失败态（failed）同样要拦**：它比 syncing 更危险——用户以为自己已经看到了
    // 完整上下文，其实一条都没到。这是加 failed 时最容易漏的一处。
    const syncState = store.peek(id)?.syncState;
    if (offline || syncState === "syncing" || syncState === "failed") {
      store.view(id).session.lastError = offline
        ? "正在连接，请稍候"
        : syncState === "failed"
          ? "内容尚未加载完成，请先重试"
          : "正在同步，请稍候";
      force((v) => v + 1);
      return false;
    }
```

- [ ] **Step 8: 清理点**

在 `App.tsx:931`（`action === "drop"` 分支）与 `:1525`（删除会话）的 `store.drop(id);` 之后各加一行：

```ts
          syncWatchdogs.current.get(id)?.stop();
          syncWatchdogs.current.delete(id);
```

在 `App.tsx:2906` 一带的重置块（`subWindow.current = new SubscriptionWindow();` 附近）加：

```ts
          for (const wd of syncWatchdogs.current.values()) wd.stop();
          syncWatchdogs.current.clear();
```

- [ ] **Step 9: 跑这一组测试**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App 同步状态机"`
Expected: 除「skeleton」相关那两条（依赖 Task 6 的 `ChatView` 接线）外全部 PASS。
若 `sync-failed` 相关用例仍失败，说明 Task 6 还没做——**这是预期的**，继续到 Task 6。

- [ ] **Step 10: 跑全量确认没有回归**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: tsc exit 0；测试全绿（≥ 1933 + 新增）。**特别注意**既有的
`does not re-resume a warm thread when switching back to it`（`App.test.tsx:1018`
附近）必须仍然通过——它是 `warm` 早退的护栏，`failSync` 删的是 `warm` 不是这个行为。

- [ ] **Step 11: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(mobile): resume 失败置 failed 并解锁重试，接停滞检测

失败时置 failed + subWindow.forget + warm.delete，三件事成对做，
否则会话会在 inWindow 早退里永久卡死。resume 期间挂看门狗，
连续 15s 收不到该 thread 任何帧即判停滞。failed 与 syncing 一样拦发送。"
```

---

### Task 6: `ChatView` 渲染占位 + 转录区 boundary

**Files:**
- Modify: `desktop/src/components/ChatView.tsx:385-443`（props）
- Modify: `desktop/src/components/ChatView.tsx:742-789`（render）
- Modify: `desktop/src/App.tsx:3115-3126`（`CacheNotice` 调用）
- Modify: `desktop/src/components/CacheNotice.tsx`
- Modify: `desktop/src/App.tsx:3134-3168`（转录区包 boundary + 新增 prop）
- Modify: `desktop/src/main.tsx`
- Test: `desktop/src/App.test.tsx`、`desktop/src/components/CacheNotice.test.tsx`

**Interfaces:**
- Consumes（Task 3、4 的产物）：`TranscriptPlaceholder`、`PlaceholderKind`、`AppCrashedScreen`、`ErrorBoundary.resetKey`
- Produces:
  - `ChatView` 新 props：`syncError?: string | null`、`onRetrySync?: () => void`、`isRemote?: boolean`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/components/CacheNotice.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { CacheNotice } from "./CacheNotice";

afterEach(cleanup);

describe("CacheNotice", () => {
  it("says it is syncing while the catch-up is in flight", () => {
    render(<CacheNotice syncing />);
    expect(screen.getByRole("status").textContent).toBe("正在同步…");
  });

  it("names the failure instead of blaming the connection", () => {
    render(<CacheNotice failed />);
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).toContain("内容加载失败");
    // 反向断言：失败态不许再说「断开」——连接可能明明是通的。
    expect(text).not.toContain("断开");
  });

  it("prefers the failure wording over the syncing wording", () => {
    render(<CacheNotice syncing failed />);
    expect(screen.getByRole("status").textContent).toContain("内容加载失败");
  });

  it("keeps the offline wording when nothing is cached", () => {
    render(<CacheNotice savedAt={null} />);
    expect(screen.getByRole("status").textContent).toBe("内容未缓存 · 连接断开");
  });
});
```

在 `desktop/src/App.test.tsx` 的 `App 同步状态机` 那个 `describe` 里追加：

```ts
  it("shows a skeleton, not a blank pane, while a cold thread syncs", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() =>
      expect(document.querySelector('[data-testid="sync-skeleton"]')).toBeTruthy(),
    );
  });

  it("shows a retry affordance when resume never settles", async () => {
    // fake timers：停滞判定靠真实的 15s，必须把时间推进过去。
    vi.useFakeTimers();
    try {
      asRemote();
      state.dataSources["thread/resume"] = () => new Promise(() => {});
      render(<App />);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(15_000);
      });
      expect(document.querySelector('[data-testid="sync-failed"]')).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not show a skeleton for a genuinely empty thread", async () => {
    asRemote();
    // resume 正常返回、readItems 也返回空 → 真·空会话
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await new Promise((r) => setTimeout(r, 50));
    expect(document.querySelector('[data-testid="sync-skeleton"]')).toBeNull();
    expect(document.querySelector('[data-testid="sync-failed"]')).toBeNull();
  });

  it("a render throw inside the transcript leaves the sidebar and composer mounted", async () => {
    // **不要**在 App 层制造异常：实测 ChatItem / itemText 对坏数据有完整防御
    // （text:null / 未知 type / null item / 缺字段 五种形态都不抛），而
    // `vi.doMock` + 动态 import 会让异常越过 React 直接炸到测试层（已实测失败）。
    //
    // 这一条改为**组件层**验证：boundary 的收敛边界是它自己的契约，
    // 与"哪种坏数据"无关。App 层的真实接线由 Task 7 Step 4 人工核对。
    const spy = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      function Boom(): JSX.Element {
        throw new Error("bad item");
      }
      render(
        <div>
          <aside data-testid="sidebar">sidebar</aside>
          <div className="relative flex min-h-0 flex-1 flex-col">
            <ErrorBoundary
              fallback={<TranscriptPlaceholder kind="crashed" reason="bad item" />}
            >
              <Boom />
            </ErrorBoundary>
          </div>
          <textarea data-testid="composer" />
          <div className="app-statusbar">stat</div>
        </div>,
      );
      // 转录区收敛成崩溃页……
      expect(document.querySelector('[data-testid="sync-crashed"]')).toBeTruthy();
      // ……而它之外的一切仍然活着：用户能切走、能重连、能报错。
      expect(screen.getByTestId("sidebar")).toBeTruthy();
      expect(screen.getByTestId("composer")).toBeTruthy();
      expect(document.querySelector(".app-statusbar")).toBeTruthy();
    } finally {
      spy.mockRestore();
    }
  });

  it("the cache notice names the real reason while syncing on a cold start", async () => {
    asRemote();
    state.dataSources["thread/resume"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(clients.length).toBeGreaterThan(0));
    await waitFor(() => {
      const status = document.querySelector('[role="status"]');
      expect(status).toBeTruthy();
      // 冷启动时 offline 为真（握手还没走完），但真正的原因是「正在同步」。
      expect(status?.textContent).not.toContain("断开");
    });
  });
```

> **已实测的两条弯路，不要重走**：
> 1. `ChatItem` / `itemText` 对坏数据有完整防御——`text: null`、未知 `type`、
>    `null` item、缺字段，五种形态全都不抛（`itemText` 的 `switch` 有
>    `default: return ""`，`ChatView.tsx:127`）。**靠畸形 item 触发不了 boundary。**
> 2. `vi.doMock("./components/ChatView", ...)` + 动态 `import("./App")` 会让异常
>    **越过 React 直接炸到测试层**（实测该用例以 `Error: bad item` 失败，而不是
>    渲染出 fallback）。
>
> 所以边界用例分两层写：**组件层**证明"抛了之后兄弟节点还活着"（这是 boundary
> 的契约），**App 层**只断言接线（fallback 是崩溃占位），不制造真实异常。

- [ ] **Step 2: 跑测试确认它失败**

Run: `cd desktop && npx vitest run src/components/CacheNotice.test.tsx src/App.test.tsx -t "App 同步状态机"`
Expected: FAIL。`CacheNotice` 的 `failed` prop 还不存在；`sync-skeleton` / `sync-crashed` 找不到。

- [ ] **Step 3: `CacheNotice` 加失败态**

把 `desktop/src/components/CacheNotice.tsx` 改为（保留原有文档注释，补第三态）：

```tsx
export function CacheNotice({
  savedAt,
  syncing,
  failed,
}: {
  savedAt?: number | null;
  syncing?: boolean;
  /** 对账失败：此时说「连接断开」是错的（连接可能好着），要说出真正的原因。 */
  failed?: boolean;
}) {
  return (
    <div
      role="status"
      className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
    >
      {/* 顺序是刻意的：`failed` 压过 `syncing`。两者同为真是状态机自身的问题
          （置 failed 时必然已离开 syncing），真撞上时优先说出"失败了、可以重试"
          ——那是用户唯一能行动的结论。 */}
      {failed ? "内容加载失败 · 可重试" : syncing ? "正在同步…" : offlineLabel(savedAt)}
    </div>
  );
}
```

`offlineLabel` 一字不改。既有的 `savedAt === null → "内容未缓存 · 连接断开"`
文案保留：那是**离线**路径的正确说法（真的没连上、真的没缓存），与 `failed`
是两件不同的事。

- [ ] **Step 4: `ChatView` 加三个 prop 并渲染占位**

在 `desktop/src/components/ChatView.tsx` 的 props 解构（`:385-398`）里加：

```tsx
  syncing = false,
  syncError = null,
  onRetrySync,
  isRemote = false,
```

在类型声明（`:442` 的 `syncing?: boolean;` 之后）加：

```tsx
  /**
   * 对账失败的原因（`ThreadView.syncError`）。仅 `syncState === "failed"` 时非空。
   */
  syncError?: string | null;
  /**
   * 失败态的「重试」回调。宿主应传 `() => selectThread(id, { forceResume: true })`：
   * `forceResume` 会跳过 `selectThread` 的全部早退，是唯一可靠的强制重同步路径。
   */
  onRetrySync?: () => void;
  /**
   * 是否远端（手机）客户端。**只有远端才渲染骨架屏**——桌面端没有本地缓存、
   * 内容恒来自服务端，加骨架会改变既有观感（设计 §4.3 判定顺序第二条）。
   */
  isRemote?: boolean;
```

在 render（`:742`）的 `<HistoryPrefix ... />` **之前**插入占位判定：

```tsx
      {/* 转录为空时才谈得上「占位」。有内容一律原样，绝不遮挡用户正在读的东西。 */}
      {items.length === 0 && isRemote && (syncing || syncError !== null) ? (
        syncError !== null ? (
          <TranscriptPlaceholder kind="failed" reason={syncError} onRetry={onRetrySync} />
        ) : (
          <TranscriptPlaceholder kind="loading" />
        )
      ) : null}
```

在文件顶部 import：

```tsx
import { TranscriptPlaceholder } from "./TranscriptPlaceholder";
```

- [ ] **Step 5: App 传入新 prop 并包 boundary**

先在组件体的 state 区（`const [, force] = useState(0);` 附近）加一行：

```tsx
  // 转录区 boundary 的重试计数：boundary 的 hasError 一旦置位就粘住，
  // 重试必须换 resetKey 才清得掉。用独立的计数器而不是复用一个现有值，
  // 是为了让「切换会话」与「点重试」两条复位路径互不干扰。
  const [transcriptEpoch, setTranscriptEpoch] = useState(0);
```

把 `App.tsx:3135` 起的 `<ChatView ... />` 块替换为（**用 `ErrorBoundary` 包起来**）：

```tsx
                  <ErrorBoundary
                    // 切会话必须清掉错误位，否则切到好会话仍是错误页。
                    // 用 resetKey 而不是 key：key 会连 ChatView 一起重挂，
                    // 每次切会话丢滚动位置与展开的 reasoning 块。
                    resetKey={`${sess.currentId ?? "none"}:${transcriptEpoch}`}
                    onError={(e) => console.error("transcript render failed", e)}
                    fallback={
                      <TranscriptPlaceholder
                        kind="crashed"
                        reason="这条会话里有一段内容无法渲染"
                        // 换 resetKey 才能真的清掉 hasError；只 force 重渲染
                        // 的话 boundary 会一直停在 fallback 上。
                        onRetry={() => setTranscriptEpoch((n) => n + 1)}
                      />
                    }
                  >
                    <ChatView
                      items={current?.session.items ?? []}
                      error={current?.session.lastError ?? null}
                      retrying={current?.session.retrying ?? null}
                      threadId={sess.currentId}
                      call={imageCall}
                      structuralVersion={current?.session.structuralVersion ?? 0}
                      query={searchQuery}
                      anchor={sess.currentId ? sess.anchorFor(sess.currentId) : undefined}
                      syncing={
                        sess.currentId
                          ? store.peek(sess.currentId)?.syncState === "syncing"
                          : false
                      }
                      syncError={
                        sess.currentId
                          ? (store.peek(sess.currentId)?.syncState === "failed"
                              ? (store.peek(sess.currentId)?.syncError ?? "加载失败")
                              : null)
                          : null
                      }
                      onRetrySync={() => {
                        const id = sess.currentId;
                        if (id) void selectThread(id, { forceResume: true });
                      }}
                      isRemote={isRemoteClient()}
                      onRememberView={sess.rememberView}
                      scrollerRef={transcriptRef}
                      captureViewRef={captureTranscriptRef}
                    />
                  </ErrorBoundary>
```

把 `App.tsx:3115-3126` 的 `CacheNotice` 调用改为：

```tsx
                {sess.currentId !== null &&
                  ((offline && cachedAt.current.has(sess.currentId)) ||
                    store.peek(sess.currentId)?.syncState === "syncing" ||
                    store.peek(sess.currentId)?.syncState === "failed") && (
                    <CacheNotice
                      savedAt={cachedAt.current.get(sess.currentId) ?? null}
                      // 不再要求 !offline：冷启动时 offline 为真只是因为握手还没
                      // 走完（status 尚未置 connected），而 syncState 是更强的证据。
                      syncing={store.peek(sess.currentId)?.syncState === "syncing"}
                      failed={store.peek(sess.currentId)?.syncState === "failed"}
                    />
                  )}
```

- [ ] **Step 6: 根级 boundary 与日志**

把 `desktop/src/main.tsx` 的 render 改为：

```tsx
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary
      onError={(e) => {
        // 只留证据，不改渲染：这类异常常常是预期内的失败（一次失败的 RPC）。
        console.error("app render failed", e);
      }}
      fallback={<AppCrashedScreen />}
    >
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);

// 同一类问题在异步路径上不会经过 React 的 boundary：留一条日志便于下次排查，
// 但**不**改渲染——接住它们也不会让界面变好，反而会制造误报。
window.addEventListener("unhandledrejection", (e) => {
  console.error("unhandled rejection", e.reason);
});
window.addEventListener("error", (e) => {
  console.error("window error", e.error ?? e.message);
});
```

import 区加：

```tsx
import { ErrorBoundary } from "./ui/ErrorBoundary";
import { AppCrashedScreen } from "./ui/AppCrashedScreen";
```

- [ ] **Step 7: 跑目标测试**

Run: `cd desktop && npx vitest run src/components/CacheNotice.test.tsx src/App.test.tsx -t "App 同步状态机"`
Expected: PASS 全部。

- [ ] **Step 8: 跑全量**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: tsc exit 0；测试全绿。**重点看这几条既有用例是否仍过**：
- `keeps the transcript when content already arrived`（有内容不打扰）
- `does not show a skeleton for a genuinely empty thread`（空会话不加骨架）
- `never subscribes on the desktop build`（桌面端不加新元素）

- [ ] **Step 9: 提交**

```bash
git add desktop/src/components/ChatView.tsx desktop/src/components/CacheNotice.tsx \
        desktop/src/components/CacheNotice.test.tsx desktop/src/App.tsx \
        desktop/src/App.test.tsx desktop/src/main.tsx
git commit -m "feat(mobile): 转录区三种显式态 + 两层 ErrorBoundary

空转录且 syncing 显示骨架、failed 显示可重试的失败态；渲染异常由
转录区 boundary 就地收敛，侧栏与输入框保持可用；根级 boundary 兜住
更外层的异常并给出重新加载入口。CacheNotice 增加失败文案，不再把
冷启动的在途同步误报成连接断开。"
```

---

### Task 7: 整体验收与文档

**Files:**
- Modify: `docs/project-management/desktop.md`（记录本轮改动与测试数）
- Test: 全量

- [ ] **Step 1: 跑完整验证**

Run:
```bash
cd desktop && npx tsc --noEmit && npx vitest run && npm run build
```
Expected: tsc exit 0；测试全绿且总数 ≥ 1933 + 本轮新增；build 成功。

- [ ] **Step 2: 逐条对着设计文档的测试清单核对**

打开 `docs/superpowers/specs/2026-11-01-mobile-blank-screen-root-cause-design.md` 的 §7，
逐行确认每个用例名都能在本轮新增的测试里找到。**把对照结果写下来**（哪些已覆盖、
哪些刻意没做及原因）。已知的偏差：§7.2 的 `a stalled resume still reaches connected`
要求在停滞时底栏仍能走到「已连接」——若实现后该用例需要额外改 `handshake` 才能过，
**不要为了它改握手逻辑**，标注为「未达成，另立卡片」并说明原因。

- [ ] **Step 3: 手动验收清单（无法自动化，逐条记录结果）**

在真机上（或明确标注「未执行」）：
1. 切到长会话 → 立刻出现骨架，内容到达后消失。
2. 切后台 30s 回前台 → 骨架 → 内容。
3. 断网后切会话 → 15s 内出现「内容加载失败 + 重试」，恢复后点重试能加载。

- [ ] **Step 4: 更新项目文档**

在 `docs/project-management/desktop.md` 里追加一节，记录：
- `syncState` 三态与 `failSync` 的位置；
- `syncWatchdog` 的 15s 阈值与其理由（连续无活动，非总时长）；
- 两层 boundary 的位置；
- 本轮新增的测试条数。

同时人工核对两处**没有自动化覆盖**的接线（读代码确认，把结论写进提交说明）：
1. `App.tsx` 转录区那层 `<ErrorBoundary fallback={<TranscriptPlaceholder kind="crashed" .../>}>`
   确实包住了 `<ChatView>`，且 `resetKey` 同时含 `sess.currentId` 与 `transcriptEpoch`。
2. `main.tsx` 的根级 `<ErrorBoundary fallback={<AppCrashedScreen />}>` 确实包住了 `<App />`，
   且在 `<React.StrictMode>` 之内。

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/desktop.md
git commit -m "docs(project-management): 记录手机端黑屏修复的落地位置与测试数"
```

---

## 自检记录（写完后对 spec 的复核）

| spec 章节 | 对应任务 | 状态 |
|---|---|---|
| §4.1 `syncState` + `forget` | Task 2（模块）、Task 5（接线） | 已覆盖 |
| §4.2 watchdog | Task 1（模块）、Task 5（接线） | 已覆盖 |
| §4.3 三种显式态 | Task 4（组件）、Task 6（渲染） | 已覆盖 |
| §4.4 两层 boundary | Task 3（组件）、Task 6（接线） | 已覆盖 |
| §4.5 `CacheNotice` | Task 6 Step 3 | 已覆盖 |
| §6 不变量 1—8 | Task 5/6 各步的注释与用例 | 已覆盖 |
| §7.1 watchdog 单测 | Task 1 Step 1（4 条 + 3 条强化 = 7 条） | 已覆盖 |
| §7.2 App 行为用例 | Task 5 Step 1、Task 6 Step 1 | 已覆盖，`stalled resume still reaches connected` 见 Task 7 Step 2 的偏差说明 |
| §7.3 boundary 用例 | Task 3 Step 1、Task 6 Step 1 | 已覆盖 |
| §8 验证步骤 | Task 7 Step 1、Step 3 | 已覆盖 |

**与 spec 的已知偏差（刻意）**：
- spec §7.2 期望「停滞时底栏仍走到已连接」。实现上 `handshake` 里 `await
  selectThread` 仍会卡到 watchdog 触发为止（15s），之后的 `setStatus("connected")`
  取决于 `failSync` 是否抛错——它不抛，所以会继续走下去。Task 7 Step 2 要求实测确认。
- spec §9「不改桌面端」与 §6.1 冲突：实际会改（`failed` 状态、发送拦截、
  `CacheNotice` 文案、两处 boundary）。本计划以 §6.1 为准，并补了桌面端用例。
