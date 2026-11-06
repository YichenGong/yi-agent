# 转录窗口化（Transcript Windowing）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把「打开长会话」的客户端渲染从约 780 ms 降到 150 ms 以内，做法是只挂载视口附近的 60～400 条消息，其余用上下两个 spacer 撑高度，向上翻页按 200 条加载更早的历史。

**Architecture:** 一个零 DOM 的纯函数内核 `nextWindow` 决定「挂载哪一段区间」，一个 `HeightBook` 用中位数估算未挂载区间的高度，一个 `useTranscriptWindow` hook 负责测量视口、翻译锚点/搜索跳转为一次性 `FocusRequest`。`ChatView` 只负责把 `[start, end)` 渲染出来并在窗口变化时补偿 `scrollTop`（WKWebView 不实现 scroll anchoring，必须自己补）。搜索从「扫 DOM 的 mark」下沉到 `session.items`，命中未挂载项时先开窗再定位。

**Tech Stack:** TypeScript、React 19、vitest + jsdom + @testing-library/react、Vite。

## Global Constraints

- 所有改动在 worktree `.worktrees/transcript-window`（分支 `feat/transcript-window`），**不得**直接在 `main` 上改。
- 每个任务结束前：`cd desktop && npx tsc --noEmit` 必须通过。
- 测试一律按文件跑：`cd desktop && npx vitest run src/<path>.test.tsx`，**不要** `npm test` 全量。
- 性能断言**只断言结构性不变量**（挂载条数、DOM 节点数、调用次数、区间序列），**绝不断言毫秒**——机器快慢会飘。
- 协议、线路字节、持久化格式、帧数时序**均不得变**。本设计只动前端渲染层。
- 搜索允许在流式生成期间短暂滞后（用户已明确接受），但必须写成「扫描次数」断言，不能只写在注释里。
- commit 用 conventional commits，首行 ≤72 字符，**不得**写 `Co-Authored-By`。
- 窗口参数固定：`initialSize = 60`、`pageSize = 200`、`maxMounted = 400`、`buffer = 20`（条）。这些是用户已确认的值，不得擅自调整。
- 最后一条 item 恒由 `LiveTail` 渲染，`HistoryWindow` 必须 `slice(start, Math.min(end, itemCount - 1))`，否则最后一条会重复渲染。

---

## File Structure

**新建**

| 文件 | 职责 |
|---|---|
| `desktop/src/lib/transcriptWindow.ts` | 纯函数：`nextWindow`、`WindowState`、`FocusRequest`、`VisibleRange`、`WindowLimits`、`DEFAULT_LIMITS`、`decideTrimStart` |
| `desktop/src/lib/transcriptWindow.test.ts` | 上表的单元测试（含 8 条规则的正反用例、12 页连续上翻的区间序列） |
| `desktop/src/lib/heightBook.ts` | 纯函数：`createHeightBook`、中位数估算、区间估计、误差差值 |
| `desktop/src/lib/heightBook.test.ts` | 中位数、重尾分布、完全测量即零误差、样本增加不劣化 |
| `desktop/src/lib/itemText.ts` | 从 `ChatView.tsx:123` 抽出的 `itemText`，供搜索与渲染共用 |
| `desktop/src/lib/itemText.test.ts` | 每种 item 类型返回什么文本 |
| `desktop/src/lib/searchIndex.ts` | 纯函数：`collectHits`、`prefixTops`、`estimatedRects` |
| `desktop/src/lib/searchIndex.test.ts` | 命中未挂载区间、多命中去重、估算矩形单调递增 |
| `desktop/src/lib/scrollCompensation.ts` | 纯函数：`planCompensation`（窗口变更后 `scrollTop` 该加多少、要不要重新贴底） |
| `desktop/src/lib/scrollCompensation.test.ts` | 上方挂载 / 上方卸载 / 下方卸载 / 首次布局四种方向 |
| `desktop/src/hooks/useTranscriptWindow.ts` | 维持窗口状态、测量视口、消费 `FocusRequest` |
| `desktop/src/hooks/useTranscriptWindow.test.tsx` | 用注入的假 `measure` 驱动翻页、跟随、收缩、focus |
| `desktop/src/components/HistoryWindow.tsx` | 挂载区渲染（memo），只负责 `slice` + 每个 `ChatItem` |

**修改**

| 文件 | 改动 |
|---|---|
| `desktop/src/components/ChatView.tsx:333-372` | `HistoryPrefix` 换成 `HistoryWindow`（接收 `start`/`end`） |
| `desktop/src/components/ChatView.tsx:504` | `tail` 保留；新增窗口计算与 spacer |
| `desktop/src/components/ChatView.tsx:519-598` | 锚点恢复改为「数据层找 index → 开窗 → 对位」 |
| `desktop/src/components/ChatView.tsx:648-653` | `ResizeObserver` 改为观察两个 spacer + 挂载区 |
| `desktop/src/components/ChatView.tsx:703-710` | `capture()` 只遍历挂载区 |
| `desktop/src/components/ChatView.tsx:806-826` | 渲染序列改为 spacerTop / HistoryWindow / spacerBottom / LiveTail |
| `desktop/src/hooks/useTranscriptSearch.ts` | 从 DOM 扫描改为「数据层匹配 + DOM 高亮」 |
| `desktop/src/hooks/useTranscriptSearch.test.tsx` | 按新语义改写（这是本设计唯一的测试语义变更） |
| `desktop/src/App.tsx:864-890` | 把 `items` / 版本号 / 开窗回调传给搜索 |
| `desktop/scripts/perf/open.mjs` | 新增「窗口化 + 分页」联合场景，记录优化前后数字 |

**依赖顺序**：Task 1 → 2 → 3 →（4 与 5 可并行）→ 6 → 7 → 8 → 9 → 10 → 11。

---

### Task 1: 窗口纯函数内核 `nextWindow`

**Files:**
- Create: `desktop/src/lib/transcriptWindow.ts`
- Test: `desktop/src/lib/transcriptWindow.test.ts`

**Interfaces:**
- Consumes: 无（零依赖）
- Produces:
  - `interface WindowState { start: number; end: number; following: boolean }`
  - `interface FocusRequest { index: number; before: number; after: number }`
  - `interface VisibleRange { first: number; last: number; gapTopPx: number; gapBottomPx: number }`（两个 gap 都是**有符号**像素距离，见下方类型注释）
  - `interface WindowLimits { initialSize: number; pageSize: number; maxMounted: number; buffer: number; bufferPx: number }`
  - `interface NextWindowInput { current: WindowState | null; itemCount: number; limits: WindowLimits; visible: VisibleRange | null; focus?: FocusRequest | null }`
  - `const DEFAULT_LIMITS: WindowLimits`
  - `function nextWindow(input: NextWindowInput): WindowState`
  - `function decideTrimStart(args: { following: boolean; visible: VisibleRange | null; start: number; end: number; limits: WindowLimits }): boolean`

- [ ] **Step 1: 写失败测试**

Create `desktop/src/lib/transcriptWindow.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import {
  nextWindow,
  decideTrimStart,
  DEFAULT_LIMITS,
  type WindowState,
  type VisibleRange,
  type WindowLimits,
} from "./transcriptWindow";

const L: WindowLimits = DEFAULT_LIMITS;

/* 两个 helper 都必须被用例真正引用：`tsconfig.json` 开了 `noUnusedLocals`，
   留一个没人用的 helper，`npx tsc --noEmit` 会直接报 TS6133。 */

/* gap 是**有符号**的：`gapTopPx = firstTop - viewTop`，`gapBottomPx =
   viewBottom - lastBottom`。0 表示贴住边界，负值表示视口已越过边界。
   所以「离得很远」在两侧都写成负数，不要抄旧的无符号距离写法。 */

/** 视口停在挂载区顶部：顶边贴住（0），底边还在视口下方很远（负）。 */
const atWindowTop = (w: WindowState): VisibleRange => ({
  first: w.start,
  last: w.start + 10,
  gapTopPx: 0,
  gapBottomPx: -5000,
});

/** 视口停在挂载区底部：顶边已在视口上方很远（负），底边贴住（0）。 */
const atWindowBottom = (w: WindowState): VisibleRange => ({
  first: w.end - 10,
  last: w.end - 1,
  gapTopPx: -5000,
  gapBottomPx: 0,
});

describe("nextWindow / 冷启动", () => {
  it("只挂 initialSize 条，并处在跟随状态", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    expect(w).toEqual({ start: 2940, end: 3000, following: true });
  });

  it("会话比首屏短时全挂，不越界", () => {
    const w = nextWindow({ current: null, itemCount: 7, limits: L, visible: null });
    expect(w).toEqual({ start: 0, end: 7, following: true });
  });

  it("空会话返回空窗口", () => {
    expect(nextWindow({ current: null, itemCount: 0, limits: L, visible: null })).toEqual({
      start: 0,
      end: 0,
      following: true,
    });
  });

  it("冷启动不翻页：用户在底部，顶边在视口上方（gapTopPx 为负）", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    // 60 条 * 40px = 2400px，视口 800px，所以顶边在视口上方 1600px。
    const grown = nextWindow({
      current: w,
      itemCount: 3000,
      limits: L,
      visible: { first: 2990, last: 2999, gapTopPx: -1600, gapBottomPx: 0 },
    });
    expect(grown).toEqual(w);
  });
});

describe("nextWindow / 向上分页", () => {
  it("视口触到挂载区顶边时追加一页，following 转假", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    const paged = nextWindow({ current: w, itemCount: 3000, limits: L, visible: atWindowTop(w) });
    expect(paged.start).toBe(2740);
    expect(paged.end).toBe(3000);
    expect(paged.following).toBe(false);
    expect(paged.end - paged.start).toBe(260);
  });

  it("翻一页就停：补偿后的 gap 回到阈值外，连续两次调用不再翻", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    const paged = nextWindow({ current: w, itemCount: 3000, limits: L, visible: atWindowTop(w) });
    const again = nextWindow({
      current: paged,
      itemCount: 3000,
      limits: L,
      // 补偿后视口仍在原来那几条上：上方新挂载的 200 条在视口上方（负），
      // 下方还有内容在视口下方（负）。
      visible: { first: 2860, last: 2870, gapTopPx: -2500, gapBottomPx: -900 },
    });
    expect(again).toEqual(paged);
  });

  it("已经到 0 就不再减", () => {
    const w: WindowState = { start: 0, end: 400, following: false };
    const paged = nextWindow({ current: w, itemCount: 3000, limits: L, visible: atWindowTop(w) });
    expect(paged.start).toBe(0);
  });
});

describe("nextWindow / 卸载", () => {
  it("视口贴着顶边且已超限：截 end，不卸用户正在读的 start 侧", () => {
    // 视口贴着挂载区顶边（gapTopPx=0）说明用户正在往上读。此时若超限，
    // 必须截 end：截 start 会把刚翻出来、正在看的历史立刻卸掉。
    // 不断言具体数字（规则 5 会先再翻一页，再截断），只断言不变量：
    //   宽度恰好 maxMounted，且 end 被裁到 3000 以下。
    const before: WindowState = { start: 2540, end: 3000, following: true };
    const w = nextWindow({ current: before, itemCount: 3000, limits: L, visible: atWindowTop(before) });
    expect(w.end - w.start).toBe(400);
    expect(w.end).toBeLessThan(3000);
    expect(w.start).toBeLessThan(2540);
  });

  it("decideTrimStart: following 为真 -> 截 start", () => {
    expect(
      decideTrimStart({
        following: true,
        visible: { first: 100, last: 110, gapTopPx: -5000, gapBottomPx: -5000 },
        start: 0,
        end: 500,
        limits: L,
      }),
    ).toBe(true);
  });

  it("decideTrimStart: 未测量 -> 保守截 start", () => {
    expect(decideTrimStart({ following: false, visible: null, start: 0, end: 500, limits: L })).toBe(
      true,
    );
  });

  it("decideTrimStart: 刚向下分页（底边贴住）-> 截 start", () => {
    // 覆盖 `gapBottomPx` 分支：底边贴住视口底，说明用户刚往下翻，
    // 上方那批旧内容才是该让位的。
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 2500, last: 2510, gapTopPx: -5000, gapBottomPx: 0 },
        start: 0,
        end: 3000,
        limits: L,
      }),
    ).toBe(true);
  });

  it("decideTrimStart: 两端都远离 -> 截掉离视口更远的一侧", () => {
    // 覆盖距离比较分支。视口在 2500：上方 2500 条、下方 490 条 -> 上方更远 -> 截 start。
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 2500, last: 2510, gapTopPx: -5000, gapBottomPx: -5000 },
        start: 0,
        end: 3000,
        limits: L,
      }),
    ).toBe(true);
  });

  it("decideTrimStart: 视口靠近 start -> 截 end（方向必须反过来）", () => {
    // 同一分支的反向用例。缺了这条，比较符号写反也测不出来。
    // 视口在 200：上方 200 条、下方 2790 条 -> 下方更远 -> 截 end。
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 200, last: 210, gapTopPx: -5000, gapBottomPx: -5000 },
        start: 0,
        end: 3000,
        limits: L,
      }),
    ).toBe(false);
  });

  it("decideTrimStart: 刚向上分页 -> 截 end（即使视口离 start 更近）", () => {
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 5, last: 15, gapTopPx: 0, gapBottomPx: -9000 },
        start: 0,
        end: 500,
        limits: L,
      }),
    ).toBe(false);
  });
});

describe("nextWindow / 底部跟随稳定性（回归）", () => {
  it("用户不滚动时连续多次测量必须稳定，不得自己上翻", () => {
    // 回归用例：gap 一旦被夹到 ≥0，「顶边在视口上方」就会被读成「翻到顶了」，
    // 窗口逐帧上翻再跳回，形成永不收敛的抖动。
    const ROW = 40;
    const VIEWPORT = 800;
    let w = nextWindow({ current: null, itemCount: 3251, limits: L, visible: null });
    const first = { ...w };
    let scrollTop = (w.end - w.start) * ROW - VIEWPORT; // 贴底

    for (let i = 0; i < 6; i += 1) {
      const mounted = w.end - w.start;
      const visible = {
        first: w.start,
        last: w.end - 1,
        gapTopPx: 0 - scrollTop, // 顶边相对视口顶（负 = 已在视口上方）
        gapBottomPx: scrollTop + VIEWPORT - mounted * ROW,
      };
      const next = nextWindow({ current: w, itemCount: 3251, limits: L, visible });
      expect(next).toEqual(w); // 每一次都必须原地不动
      w = next;
    }
    expect(w).toEqual(first);
    expect(w.following).toBe(true);
  });
});

describe("nextWindow / 增长与收缩", () => {
  it("following 为真时 itemCount 增长，end 跟上、start 不动", () => {
    const before: WindowState = { start: 2940, end: 3000, following: true };
    const w = nextWindow({ current: before, itemCount: 3050, limits: L, visible: null });
    expect(w.start).toBe(2940);
    expect(w.end).toBe(3050);
    expect(w.following).toBe(true);
  });

  it("following 为假时 itemCount 增长，end 不动", () => {
    const before: WindowState = { start: 500, end: 560, following: false };
    const w = nextWindow({ current: before, itemCount: 3000, limits: L, visible: null });
    expect(w).toEqual(before);
  });

  it("收缩到窗口起点之前时重置为尾部小窗口，不留空窗口", () => {
    const before: WindowState = { start: 2940, end: 3000, following: true };
    const w = nextWindow({ current: before, itemCount: 30, limits: L, visible: null });
    expect(w).toEqual({ start: 0, end: 30, following: true });
  });
});

describe("nextWindow / focus 定点开窗", () => {
  it("focus 把目标包进窗口，且不吸到尾部", () => {
    const w = nextWindow({
      current: null,
      itemCount: 3000,
      limits: L,
      visible: null,
      focus: { index: 500, before: 20, after: 60 },
    });
    expect(w.start).toBe(480);
    expect(w.end).toBe(561);
    expect(w.following).toBe(false);
  });

  it("focus 后正常调用不会一步跳到尾部", () => {
    const focused = nextWindow({
      current: null,
      itemCount: 3000,
      limits: L,
      visible: null,
      focus: { index: 500, before: 20, after: 60 },
    });
    const next = nextWindow({
      current: focused,
      itemCount: 3000,
      limits: L,
      visible: { first: 490, last: 520, gapTopPx: -5000, gapBottomPx: -3000 },
    });
    expect(next).toEqual(focused);
  });

  it("向下分页一路扩到尾部后 following 转真", () => {
    let w = nextWindow({
      current: null,
      itemCount: 800,
      limits: L,
      visible: null,
      focus: { index: 100, before: 20, after: 60 },
    });
    expect(w.following).toBe(false);
    for (let i = 0; i < 10; i += 1) {
      w = nextWindow({
        current: w,
        itemCount: 800,
        limits: L,
        visible: atWindowBottom(w),
      });
    }
    expect(w.end).toBe(800);
    expect(w.following).toBe(true);
  });
});

describe("nextWindow / 不变量", () => {
  it("连续上翻 12 页：挂载数恒 ≤ maxMounted、start 单调递减、无回翻", () => {
    let w = nextWindow({ current: null, itemCount: 5000, limits: L, visible: null });
    w = nextWindow({ current: w, itemCount: 5000, limits: L, visible: atWindowTop(w) });
    let prevStart = w.start;
    for (let i = 0; i < 12; i += 1) {
      const next = nextWindow({
        current: w,
        itemCount: 5000,
        limits: L,
        visible: atWindowTop(w),
      });
      expect(next.end - next.start).toBeLessThanOrEqual(L.maxMounted);
      expect(next.start).toBeLessThan(prevStart);
      expect(next.end).toBeLessThanOrEqual(w.end);
      prevStart = next.start;
      w = next;
    }
  });

  it("任何输入下区间都合法", () => {
    const cases: Array<WindowState | null> = [
      null,
      { start: 0, end: 0, following: true },
      { start: 5, end: 5, following: false },
      { start: 2999, end: 3000, following: true },
      { start: 0, end: 3000, following: false },
    ];
    for (const current of cases) {
      for (const itemCount of [0, 1, 30, 3000]) {
        const w = nextWindow({ current, itemCount, limits: L, visible: null });
        expect(w.start).toBeGreaterThanOrEqual(0);
        expect(w.end).toBeLessThanOrEqual(itemCount);
        expect(w.start).toBeLessThanOrEqual(w.end);
      }
    }
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/transcriptWindow.test.ts`
Expected: FAIL — `Failed to resolve import "./transcriptWindow"`.

- [ ] **Step 3: 实现**

Create `desktop/src/lib/transcriptWindow.ts`:

```ts
/**
 * 转录窗口的纯函数内核。
 *
 * **零 React、零 DOM、零时间依赖**：所有输入都由调用方给出（视口测量结果、
 * 会话条数、参数），因此每条规则都能用普通单测覆盖，不需要真实布局。
 *
 * 为什么不自己接管 `scrollTop` 做虚拟滚动：这里只决定「挂载哪一段」，
 * 未挂载区间用两个 spacer 撑高度，滚动仍由浏览器原生完成。这样
 * `useStickToBottom` 的 `scrollHeight` 语义不用改写（见设计 §2）。
 */

export interface WindowState {
  /** 已挂载区间的起点（含）。 */
  start: number;
  /** 已挂载区间的终点（不含）。最后一条 item 恒由 `LiveTail` 渲染。 */
  end: number;
  /**
   * 是否跟随尾部。**必须是显式状态，不能由 `end === itemCount` 推导**：
   * 流式追加时 `itemCount` 先变大、`end` 还没跟上，两者不等恰恰说明「正在跟随」。
   * 用派生值判断会把每一次 delta 都读成「用户离开了尾部」，跟随就此关闭。
   */
  following: boolean;
}

/** 一次「把某个 index 摆进视口」的请求（锚点恢复、搜索跳转）。 */
export interface FocusRequest {
  index: number;
  /** 该 index 之上/之下额外保留多少条。 */
  before: number;
  after: number;
}

/**
 * 当前视口对应的挂载区状态（由组件层测量 DOM 得出）。
 *
 * 同时给「index 距离」和「像素 gap」：前者用于决定截哪端，
 * 后者用于决定是否分页。只看 index 会错——item 高度是重尾分布。
 */
export interface VisibleRange {
  /**
   * 视口内第一条的**绝对** item index（不是挂载区相对序号）。
   *
   * 组件层只能看见挂载区，数不出前面还有多少条，所以它给的是相对序号；
   * `useTranscriptWindow` 负责加上 `state.start` 再传进来。两处口径必须一致，
   * 否则截断判据会比错对象。
   */
  first: number;
  /** 视口内最后一条的绝对 item index。 */
  last: number;
  /**
   * 挂载区顶边相对视口顶的有符号距离（px）：`firstTop - viewTop`。
   *
   * 必须**有符号**，不能夹到 ≥0：「顶边正好在视口顶」与「顶边已在视口上方」
   * 是两件事。60 条消息通常比视口高，用户停在底部时第一条挂载行本就在视口
   * 上方（负值）。夹成 0 会被读成「用户翻到顶了」，于是自己往上分页并逐帧抖动。
   */
  gapTopPx: number;
  /** 挂载区底边相对视口底的有符号距离（px）：`viewBottom - lastBottom`。负值 = 视口已越过底边。 */
  gapBottomPx: number;
}

export interface WindowLimits {
  /** 冷启动首屏条数。 */
  initialSize: number;
  /** 向上/向下分页一页的条数。 */
  pageSize: number;
  /** 挂载上限，超出后从离视口远的一端卸载。 */
  maxMounted: number;
  /** 定点开窗时上下各留多少条。 */
  buffer: number;
  /** 分页触发的像素阈值（组件层按 `buffer * 中位高度` 算好传进来）。 */
  bufferPx: number;
}

/** 用户已确认的参数，不得擅自调整。 */
export const DEFAULT_LIMITS: WindowLimits = {
  initialSize: 60,
  pageSize: 200,
  maxMounted: 400,
  buffer: 20,
  // 默认按 40px 一条估；组件层会用真实中位数重算后覆盖。
  bufferPx: 20 * 40,
};

export interface NextWindowInput {
  current: WindowState | null;
  itemCount: number;
  limits: WindowLimits;
  /** 当前视口覆盖的挂载区区间；未测量出时为 null。 */
  visible: VisibleRange | null;
  /** 一次性开窗请求（锚点恢复 / 搜索跳转）。 */
  focus?: FocusRequest | null;
}

const clamp = (v: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, v));

/**
 * 超限时该截掉哪一端。
 *
 * 不能无条件截 `start`：用户往上读时那会卸掉他正在看的内容。
 * 也不能只比「离视口多远」：向上翻页后视口落在新内容中间，按距离会截 `start`，
 * 把刚加载进来的历史立刻丢掉，形成「翻页—卸载—再翻页」的抖动。
 */
export function decideTrimStart(args: {
  following: boolean;
  visible: VisibleRange | null;
  start: number;
  end: number;
  limits: WindowLimits;
}): boolean {
  const { following, visible, start, end, limits } = args;
  if (following) return true; // 用户在看最新
  if (!visible) return true; // 尚未测量，保守保住尾部
  if (visible.gapTopPx >= -limits.bufferPx) return false; // 刚向上分页 -> 截 end
  if (visible.gapBottomPx >= -limits.bufferPx) return true; // 刚向下分页 -> 截 start
  const nearStart = visible.first - start;
  const nearEnd = end - visible.last;
  return nearStart > nearEnd; // 截离视口更远的一侧
}

/** 算出下一个窗口。纯函数：不读 DOM、不读时间、无副作用。 */
export function nextWindow(input: NextWindowInput): WindowState {
  const { current, itemCount, limits, visible, focus } = input;

  if (itemCount <= 0) return { start: 0, end: 0, following: true };

  let start: number;
  let end: number;
  let following: boolean;

  if (current === null) {
    if (focus) {
      following = false;
      start = clamp(focus.index - focus.before, 0, itemCount - 1);
      end = clamp(focus.index + focus.after + 1, start + 1, itemCount);
    } else {
      following = true;
      end = itemCount;
      start = Math.max(end - limits.initialSize, 0);
    }
  } else {
    ({ start, end, following } = current);

    // 收缩：窗口整体落在新末尾之后（/clear、compact）时重新锚到尾部，
    // 否则会剩下一个空窗口，用户看不到任何内容。
    if (start >= itemCount) {
      following = true;
      end = itemCount;
      start = Math.max(end - limits.initialSize, 0);
    }
    end = Math.min(end, itemCount);
    start = Math.min(start, end);

    if (focus) {
      following = false;
      start = clamp(focus.index - focus.before, 0, itemCount - 1);
      end = clamp(focus.index + focus.after + 1, start + 1, itemCount);
    } else {
      // 跟随尾部：itemCount 增长时 end 跟上，start 不动。
      if (following) end = itemCount;

      if (visible) {
        if (start > 0 && visible.gapTopPx >= -limits.bufferPx) {
          start = Math.max(start - limits.pageSize, 0);
          following = false; // 用户明确往上读了
        }
        if (end < itemCount && visible.gapBottomPx >= -limits.bufferPx) {
          end = Math.min(end + limits.pageSize, itemCount);
          // 滚回底部就是「要继续看最新」，与 useStickToBottom 里
          // 「gap 回到阈值内则恢复跟随」是同一条语义，两处必须一致。
          if (end === itemCount) following = true;
        }
      }
    }
  }

  if (end - start > limits.maxMounted) {
    const trimStart = decideTrimStart({ following, visible, start, end, limits });
    if (trimStart) start = end - limits.maxMounted;
    else end = start + limits.maxMounted;
  }

  end = Math.min(end, itemCount);
  start = clamp(start, 0, end);
  return { start, end, following };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/transcriptWindow.test.ts`
Expected: PASS（全部用例）

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/transcriptWindow.ts desktop/src/lib/transcriptWindow.test.ts
git commit -m "feat(desktop): add pure windowing core for the transcript"
```

---

### Task 2: 高度簿记 `HeightBook`

**Files:**
- Create: `desktop/src/lib/heightBook.ts`
- Test: `desktop/src/lib/heightBook.test.ts`

**Interfaces:**
- Consumes: 无
- Produces:
  - `interface HeightBook { record(id: string, height: number): void; estimate(id: string): number; estimateRange(ids: readonly string[]): number; median(): number; delta(ids: readonly string[], actualTotal: number): number }`
  - `function createHeightBook(fallback = 40): HeightBook`

- [ ] **Step 1: 写失败测试**

Create `desktop/src/lib/heightBook.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { createHeightBook } from "./heightBook";

describe("HeightBook", () => {
  it("没测量过时用 fallback", () => {
    const book = createHeightBook(40);
    expect(book.estimate("unknown")).toBe(40);
  });

  it("测量过就用测量值", () => {
    const book = createHeightBook(40);
    book.record("a", 120);
    expect(book.estimate("a")).toBe(120);
  });

  it("偶数个样本取中间两值的均值", () => {
    // 这条专门锁 `median()` 的偶数分支：若误取 sortedCache[mid] 而不是两值均值，
    // 结果是 100 而不是 70，立刻失败。
    const book = createHeightBook(40);
    book.record("a", 40);
    book.record("b", 100);
    expect(book.median()).toBe(70);
  });

  it("中位数对重尾分布比平均值稳", () => {
    const book = createHeightBook();
    // 绝大多数是两三行，少数是超长工具卡
    for (let i = 0; i < 9; i += 1) book.record(`m${i}`, 40);
    book.record("huge", 5000);
    expect(book.median()).toBe(40);
  });

  it("完全测量后 estimateRange 等于精确总和（硬不变量）", () => {
    const book = createHeightBook();
    const ids = ["a", "b", "c"];
    const heights = [30, 55, 900];
    ids.forEach((id, i) => book.record(id, heights[i]));
    expect(book.estimateRange(ids)).toBe(985);
  });

  it("未测量的项用**中位数**补齐（有测量时中位数优先于 fallback）", () => {
    const book = createHeightBook(50);
    book.record("a", 100);
    // 只有一条测量时中位数就是 100，所以 b/c 也估成 100：100*3 = 300。
    // fallback(50) 只在**一条都没测量**时才有机会出场。
    expect(book.estimateRange(["a", "b", "c"])).toBe(300);
  });

  it("一条都没测量时才用 fallback", () => {
    const book = createHeightBook(50);
    expect(book.estimateRange(["a", "b", "c"])).toBe(150);
  });

  it("delta 是「真实总高 − 估算总高」", () => {
    const book = createHeightBook(50);
    book.record("a", 100);
    // 估算 = 100(a) + 100(b，中位数) = 200，真实是 260 -> 差 60
    expect(book.delta(["a", "b"], 260)).toBe(60);
  });

  it("追加同分布样本时中位数误差不增大（断言 <=）", () => {
    const book = createHeightBook();
    const truth = [40, 40, 42, 38, 40, 41, 39, 40, 40, 40];
    // 前 5 个的估计
    truth.slice(0, 5).forEach((h, i) => book.record(`a${i}`, h));
    const errBefore = Math.abs(book.median() - 40);
    truth.slice(5).forEach((h, i) => book.record(`b${i}`, h));
    const errAfter = Math.abs(book.median() - 40);
    expect(errAfter).toBeLessThanOrEqual(errBefore);
  });

  it("同一 id 重复测量以最后一次为准，且不重复计数", () => {
    const book = createHeightBook(10);
    book.record("a", 100);
    book.record("a", 200);
    expect(book.estimate("a")).toBe(200);
    expect(book.estimateRange(["a", "a"])).toBe(200);
    expect(book.median()).toBe(200);
  });

  it("非法高度（NaN / 负数）被忽略", () => {
    const book = createHeightBook(40);
    book.record("bad", Number.NaN);
    book.record("neg", -5);
    expect(book.estimate("bad")).toBe(40);
    expect(book.estimate("neg")).toBe(40);
    expect(book.median()).toBe(40);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/heightBook.test.ts`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现**

Create `desktop/src/lib/heightBook.ts`:

```ts
/**
 * 未挂载区间的高度估算。
 *
 * 用**中位数**而不是平均值：工具卡 / 长代码块是重尾分布，平均值会被少数
 * 超长项带偏，中位数对「绝大多数消息是两三行」这一真实形状更稳。
 *
 * 零 DOM：这里只做算术，测量由组件层做完传进来。所以
 * 「真实总高 − 估算总高」这个差值也能单测。
 */
export interface HeightBook {
  /** 记录一条真实测量值。非法值（NaN / 负数）被忽略。 */
  record(id: string, height: number): void;
  /** 一条 item 的高度估计：有测量用测量，否则用中位数兜底。 */
  estimate(id: string): number;
  /** 一段区间的总高度估计。同一个 id 只计一次。 */
  estimateRange(ids: readonly string[]): number;
  /** 当前中位数（用于诊断与测试）。 */
  median(): number;
  /** 「真实总高 − 估算总高」，用于反向补偿 `scrollTop`。 */
  delta(ids: readonly string[], actualTotal: number): number;
}

export function createHeightBook(fallback = 40): HeightBook {
  const heights = new Map<string, number>();
  let sortedCache: number[] | null = null;

  const median = (): number => {
    if (heights.size === 0) return fallback;
    if (sortedCache === null) sortedCache = [...heights.values()].sort((a, b) => a - b);
    const mid = Math.floor(sortedCache.length / 2);
    return sortedCache.length % 2 === 1
      ? sortedCache[mid]
      : (sortedCache[mid - 1] + sortedCache[mid]) / 2;
  };

  const estimate = (id: string): number => heights.get(id) ?? median();

  return {
    record(id, height) {
      if (!Number.isFinite(height) || height < 0) return;
      if (heights.get(id) === height) return;
      heights.set(id, height);
      sortedCache = null;
    },
    estimate,
    estimateRange(ids) {
      let total = 0;
      const seen = new Set<string>();
      for (const id of ids) {
        if (seen.has(id)) continue;
        seen.add(id);
        total += estimate(id);
      }
      return total;
    },
    median,
    delta(ids, actualTotal) {
      return actualTotal - this.estimateRange(ids);
    },
  };
}
```

> 注意：`delta` 里用了 `this.estimateRange`。若实现者把它写成独立函数而不是对象方法，改成闭包内的 `estimateRange` 调用即可；测试只看行为。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/heightBook.test.ts`
Expected: PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/heightBook.ts desktop/src/lib/heightBook.test.ts
git commit -m "feat(desktop): add median-based height book for unmounted items"
```

---

### Task 3: 抽出 `itemText`（搜索与渲染共用）

**Files:**
- Create: `desktop/src/lib/itemText.ts`
- Test: `desktop/src/lib/itemText.test.ts`
- Modify: `desktop/src/components/ChatView.tsx:112-136`

**Interfaces:**
- Consumes: `Item`（`../lib/protocol`）、`NoticeItem` / `ReasoningItem`（`../lib/session`）
- Produces: `function itemText(item: Item | NoticeItem | ReasoningItem): string`

- [ ] **Step 1: 写失败测试**

Create `desktop/src/lib/itemText.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { itemText } from "./itemText";
import type { Item } from "./protocol";

describe("itemText", () => {
  it("返回用户与助手消息的正文", () => {
    expect(itemText({ type: "userMessage", id: "u1", text: "hi" } as Item)).toBe("hi");
    expect(itemText({ type: "agentMessage", id: "a1", text: "yo" } as Item)).toBe("yo");
  });

  it("返回 reasoning 块的正文", () => {
    expect(itemText({ type: "reasoning", id: "r1", text: "thinking" })).toBe("thinking");
  });

  it("toolCall 没有可搜索正文，返回空串", () => {
    const tool = {
      type: "toolCall",
      id: "t1",
      call_id: "c1",
      name: "bash",
      input: { cmd: "ls" },
      status: "completed",
    } as unknown as Item;
    expect(itemText(tool)).toBe("");
  });

  it("notice 的正文走结构化载荷，返回空串", () => {
    expect(itemText({ type: "notice", id: "n1", text: "x" })).toBe("");
  });

  it("thinking 有明文时返回明文（不能用空串用例代替）", () => {
    // 空串用例与 `default: return ""` 的结果无法区分，钉不住 `thinking` 落在
    // `return item.text` 分组里这个事实；而 CoT 就地追加能否被比较器看见，
    // 正取决于此。所以必须用非空明文。
    const t = { type: "thinking", id: "k2", text: "真实摘要", redacted: false } as unknown as Item;
    expect(itemText(t)).toBe("真实摘要");
  });

  it("thinking 的明文（display omitted 时为空串）原样返回", () => {
    const t = { type: "thinking", id: "k1", text: "", redacted: true } as unknown as Item;
    expect(itemText(t)).toBe("");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/itemText.test.ts`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现并把 `ChatView` 改为引用它**

Create `desktop/src/lib/itemText.ts`（把 `ChatView.tsx:112-136` 的注释与函数整体搬过来）:

```ts
import type { Item } from "./protocol";
import type { NoticeItem, ReasoningItem } from "./session";

/**
 * The text an item renders, lifted out as a primitive.
 *
 * For a prefix item this is the written-once value and it never moves again.
 * For the tail it is re-read on every render from the very object `Session`
 * mutates in place — and it is what `ChatItem`'s comparator compares, and what
 * the agent-message branch renders. The comparator must never read
 * `item.text`; see `ChatItem` in `components/ChatView.tsx` for why.
 *
 * 也供搜索索引使用：搜索必须在数据层产出与渲染完全一致的文本，
 * 否则「搜到的文案」与「画出来的文案」会分叉。
 */
export function itemText(item: Item | NoticeItem | ReasoningItem): string {
  switch (item.type) {
    case "agentMessage":
    case "userMessage":
    case "reasoning":
    // `display:"omitted"` 的 thinking 明文是空串，呈现由 `redacted` 决定；text 仍
    // 要照传，好让就地追加能被比较器看见。
    case "thinking":
      return item.text;
    default:
      // `toolCall` 无 text；`notice` 的正文由 `ChatView` 的 notice 分支单独渲染。
    // `userInterjection` 也落到这里，但它在进入 `items` 前已被归一化成
    // `userMessage`（见 `Session.upsertItems` / `Session.apply`），无需单独处理。
      return "";
  }
}
```

在 `desktop/src/components/ChatView.tsx` 中删掉原函数，并在 import 区加：

```ts
import { itemText } from "../lib/itemText";
```

- [ ] **Step 4: 跑测试 + 既有回归**

Run: `cd desktop && npx vitest run src/lib/itemText.test.ts src/components/ChatView.test.tsx`
Expected: 两个文件都 PASS（`ChatView.test.tsx` 全绿证明抽取没改变行为）

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/itemText.ts desktop/src/lib/itemText.test.ts desktop/src/components/ChatView.tsx
git commit -m "refactor(desktop): extract itemText so search and render share it"
```

---

### Task 4: 搜索索引（数据层匹配 + 估算矩形）

**Files:**
- Create: `desktop/src/lib/searchIndex.ts`
- Test: `desktop/src/lib/searchIndex.test.ts`

**Interfaces:**
- Consumes: `findMatches`（`./searchHits`）、`itemText`（`./itemText`）、`HeightBook`（`./heightBook`）、`RelRect`（`./searchHits`）
- Produces:
  - `interface SearchHit { index: number; itemId: string; offset: number }`
  - `function collectHits(items: readonly unknown[], query: string): SearchHit[]`
  - `function prefixTops(items: readonly unknown[], book: HeightBook): number[]`
  - `function estimatedRects(hits: readonly SearchHit[], prefixTops: readonly number[], book: HeightBook): RelRect[]`

- [ ] **Step 1: 写失败测试**

Create `desktop/src/lib/searchIndex.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { collectHits, prefixTops, estimatedRects } from "./searchIndex";
import { createHeightBook } from "./heightBook";
import type { Item } from "./protocol";

const msg = (id: string, text: string): Item => ({ type: "agentMessage", id, text }) as Item;

describe("collectHits", () => {
  it("空查询不产生命中", () => {
    expect(collectHits([msg("a", "hello")], "  ")).toEqual([]);
  });

  it("大小写不敏感", () => {
    const hits = collectHits([msg("a", "Hello World")], "hello");
    expect(hits).toHaveLength(1);
    expect(hits[0]).toEqual({ index: 0, itemId: "a", offset: 0 });
  });

  it("同一条里的多个命中各自成项，offset 用序号区分", () => {
    const hits = collectHits([msg("a", "aa aa aa")], "aa");
    expect(hits.map((h) => h.offset)).toEqual([0, 1, 2]);
    expect(hits.every((h) => h.index === 0 && h.itemId === "a")).toBe(true);
  });

  it("能命中很靠后的未挂载项（这正是下沉到数据层的目的）", () => {
    const items = Array.from({ length: 3000 }, (_, i) => msg(`m${i}`, `row ${i}`));
    items[10] = msg("m10", "needle here");
    const hits = collectHits(items, "needle");
    expect(hits).toHaveLength(1);
    expect(hits[0].index).toBe(10);
  });

  it("命中总数覆盖全量历史，与挂载区间无关", () => {
    const items = Array.from({ length: 500 }, (_, i) => msg(`m${i}`, "x"));
    expect(collectHits(items, "x")).toHaveLength(500);
  });
});

describe("prefixTops", () => {
  it("第 0 条顶部为 0，逐条累加估计高度", () => {
    const book = createHeightBook(50);
    const items = [msg("a", ""), msg("b", ""), msg("c", "")];
    expect(prefixTops(items, book)).toEqual([0, 50, 100]);
  });

  it("已测量的项用真实高度", () => {
    const book = createHeightBook(50);
    book.record("a", 100);
    const items = [msg("a", ""), msg("b", "")];
    expect(prefixTops(items, book)).toEqual([0, 100]);
  });
});

describe("estimatedRects", () => {
  it("每个命中给一个 top/height，top 单调不减", () => {
    const book = createHeightBook(50);
    const items = [msg("a", "hit"), msg("b", "hit hit"), msg("c", "nope")];
    const tops = prefixTops(items, book);
    const rects = estimatedRects(collectHits(items, "hit"), tops, book);
    expect(rects).toHaveLength(3);
    expect(rects[0].top).toBeLessThanOrEqual(rects[1].top);
    expect(rects.every((r) => r.height > 0)).toBe(true);
  });

  it("空命中返回空数组", () => {
    const book = createHeightBook();
    expect(estimatedRects([], [], book)).toEqual([]);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/searchIndex.test.ts`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现**

Create `desktop/src/lib/searchIndex.ts`:

```ts
/**
 * 搜索的**数据层**索引。
 *
 * 为什么要下沉到这里：窗口化之后未挂载的历史不在 DOM 里，原来的
 * `querySelectorAll("mark.search-hit")` 会静默缩小搜索范围——不报错，只是搜不到。
 *
 * 这里只做「哪些条命中、第几个字」，高亮仍由 `rehypeSearchMark` 在 markdown 管线
 * 里完成（那是渲染层的事）。
 */
import { findMatches } from "./searchHits";
import type { RelRect } from "./searchHits";
import { itemText } from "./itemText";
import type { HeightBook } from "./heightBook";
import type { Item } from "./protocol";
import type { NoticeItem, ReasoningItem } from "./session";

export interface SearchHit {
  /** 在 `session.items` 里的下标。 */
  index: number;
  itemId: string;
  /** 该条内的第几个命中，用于在同一条里定位到具体 `mark`。 */
  offset: number;
}

type AnyItem = Item | NoticeItem | ReasoningItem;

/**
 * 在全部 items 上找命中。与 `rehypeSearchMark` 用同一套 `findMatches`
 * （大小写不敏感、允许重叠），所以「计数」与「画出来的 mark 数」一致。
 */
export function collectHits(items: readonly AnyItem[], query: string): SearchHit[] {
  const out: SearchHit[] = [];
  for (let index = 0; index < items.length; index += 1) {
    const item = items[index];
    const text = itemText(item);
    if (text === "") continue;
    const matches = findMatches(text, query);
    for (let offset = 0; offset < matches.length; offset += 1) {
      out.push({ index, itemId: item.id, offset });
    }
  }
  return out;
}

/** 每条 item 顶边相对内容顶部的估计位置。O(n) 预算一次，供多个命中共用。 */
export function prefixTops(items: readonly AnyItem[], book: HeightBook): number[] {
  const tops = new Array<number>(items.length);
  let y = 0;
  for (let i = 0; i < items.length; i += 1) {
    tops[i] = y;
    y += book.estimate(items[i].id);
  }
  return tops;
}

/**
 * 命中 → 估算矩形。
 *
 * `initialActiveIndex` 需要「每个命中在内容里的 top」，窗口化后绝大多数命中没有
 * DOM，只能用 `HeightBook` + `prefixTops` 估算。误差只影响「打开查找条时先选中
 * 哪个命中」，不影响命中总数。
 */
export function estimatedRects(
  hits: readonly SearchHit[],
  prefixTopsList: readonly number[],
  book: HeightBook,
): RelRect[] {
  return hits.map((hit) => ({
    top: prefixTopsList[hit.index] ?? 0,
    height: book.estimate(hit.itemId),
  }));
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/searchIndex.test.ts`
Expected: PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/searchIndex.ts desktop/src/lib/searchIndex.test.ts
git commit -m "feat(desktop): index search over session items instead of the DOM"
```

---

### Task 5: `useTranscriptWindow` hook

**Files:**
- Create: `desktop/src/hooks/useTranscriptWindow.ts`
- Test: `desktop/src/hooks/useTranscriptWindow.test.tsx`

**Interfaces:**
- Consumes: `nextWindow` / `DEFAULT_LIMITS` / `WindowState` / `FocusRequest` / `VisibleRange`（`../lib/transcriptWindow`）、`HeightBook`（`../lib/heightBook`）
- Produces:
  - `interface UseTranscriptWindowArgs { itemCount: number; measure: () => VisibleRange | null; focus: FocusRequest | null; limits?: Partial<WindowLimits>; book: HeightBook }`
  - `function useTranscriptWindow(args: UseTranscriptWindowArgs): { window: WindowState; requestFocus: (f: FocusRequest) => void; clearFocus: () => void }`

**关键设计**：`measure` 由调用方注入。真实 `ChatView` 传一个读 DOM 的函数，测试传一个受控假函数——这样 hook 的翻页/跟随/收缩全部可在 jsdom 下确定性验证。

- [ ] **Step 1: 写失败测试**

Create `desktop/src/hooks/useTranscriptWindow.test.tsx`:

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach } from "vitest";
import { useState } from "react";
import { render, cleanup, act } from "@testing-library/react";
import { useTranscriptWindow } from "./useTranscriptWindow";
import { createHeightBook } from "../lib/heightBook";
import type { FocusRequest, VisibleRange, WindowState } from "../lib/transcriptWindow";

let last: WindowState = { start: 0, end: 0, following: true };
let requestFocus: (f: FocusRequest) => void = () => {};
let measure: () => VisibleRange | null = () => null;
let currentFocus: FocusRequest | null = null;

function Harness({ itemCount }: { itemCount: number }) {
  // `book` 必须稳定：内联新建会让它每次渲染都是新对象，`merged`/`recompute`
  // 的身份随之改变，依赖它的 effect 就会每次渲染都重跑。
  const [book] = useState(() => createHeightBook(40));
  const state = useTranscriptWindow({
    itemCount,
    measure: () => measure(),
    focus: currentFocus,
    book,
  });
  last = state.window;
  requestFocus = state.requestFocus;
  return <div />;
}

/**
 * 推进若干帧，并把 hook 在 rAF 回调里发出的 `setState` flush 成真实渲染。
 *
 * 两个坑，顺序不能省：
 * 1) hook 的重算发生在 rAF 回调里，所以必须先 `await` 一个真实 rAF，否则回调
 *    还没跑就断言了；
 * 2) 回调里的 `setState` 由 React 调度器异步入队，再让出一个 macrotask 并交给
 *    `act` 收尾，才能保证读到的 `last` 是渲染后的值。
 * 用 3 帧是给「effect -> rAF -> setState -> 重渲染 -> 再 effect」留够余量。
 */
async function tick(frames = 3) {
  await act(async () => {
    for (let i = 0; i < frames; i += 1) {
      await new Promise((r) => requestAnimationFrame(() => r(null)));
      await new Promise((r) => setTimeout(r, 0));
    }
  });
}

beforeEach(() => {
  cleanup();
  currentFocus = null;
  measure = () => null;
});

describe("useTranscriptWindow", () => {
  it("冷启动只挂 60 条", async () => {
    render(<Harness itemCount={3000} />);
    await tick();
    expect(last.end - last.start).toBe(60);
    expect(last.following).toBe(true);
  });

  it("视口触顶时追加一页，且只追加一页", async () => {
    render(<Harness itemCount={3000} />);
    await tick();
    const initialStart = last.start;
    // 贴顶只在**还没翻页**时成立：翻出一页后，新挂载的 200 条落到了视口上方，
    // topGap 自然变大。桩必须模拟这件事，否则每帧都会被判定为「仍在顶端」
    // 而无限翻页——真实 DOM 里不会这样。
    measure = () => ({
      first: last.start,
      last: last.start + 10,
      // 有符号：翻页后挂载区顶边在被翻到视口上方 -> 负值；底边远在视口下方 -> 负值。
      gapTopPx: last.start === initialStart ? 0 : -5000,
      gapBottomPx: -5000,
    });
    await tick();
    expect(last.end - last.start).toBe(260);
    expect(last.start).toBe(initialStart - 200);
    expect(last.following).toBe(false);
  });

  it("itemCount 增长时跟随尾部", async () => {
    const { rerender } = render(<Harness itemCount={3000} />);
    await tick();
    rerender(<Harness itemCount={3050} />);
    await tick();
    expect(last.end).toBe(3050);
    expect(last.following).toBe(true);
  });

  it("requestFocus 开窗到指定 index", async () => {
    render(<Harness itemCount={3000} />);
    await tick();
    requestFocus({ index: 500, before: 20, after: 60 });
    await tick();
    expect(last.start).toBe(480);
    expect(last.end).toBe(561);
    expect(last.following).toBe(false);
  });

  it("measure 返回 null（尚未布局）时保持跟随，不越界", async () => {
    render(<Harness itemCount={10} />);
    await tick();
    expect(last).toEqual({ start: 0, end: 10, following: true });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/hooks/useTranscriptWindow.test.tsx`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现**

Create `desktop/src/hooks/useTranscriptWindow.ts`:

```ts

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  DEFAULT_LIMITS,
  nextWindow,
  type FocusRequest,
  type VisibleRange,
  type WindowLimits,
  type WindowState,
} from "../lib/transcriptWindow";
import type { HeightBook } from "../lib/heightBook";

export interface UseTranscriptWindowArgs {
  itemCount: number;
  /** 测量当前视口。未布局时返回 null（此时保持跟随，不翻页）。 */
  measure: () => VisibleRange | null;
  /** 一次性开窗请求；调用方负责在消费后清空。 */
  focus: FocusRequest | null;
  limits?: Partial<WindowLimits>;
  book: HeightBook;
}

export interface TranscriptWindowApi {
  window: WindowState;
  requestFocus: (f: FocusRequest) => void;
  clearFocus: () => void;
}

/**
 * 维护转录窗口。
 *
 * 它**不含**高度簿记（那是 `HeightBook`）与纯计算（那是 `nextWindow`）：
 * 这里只负责「在合适的时机把测量结果喂给纯函数」，以及把
 * 锚点/搜索跳转翻译成一次性的 `FocusRequest`。
 */
export function useTranscriptWindow({
  itemCount,
  measure,
  focus,
  limits,
  book,
}: UseTranscriptWindowArgs): TranscriptWindowApi {
  // **必须按标量字段 memo**，不能依赖 `limits` 对象身份：调用方几乎总是传字面量
  // （`limits={{ pageSize: 200 }}`），对象身份每次渲染都变 -> `merged` 变 ->
  // `recompute` 变 -> 下面依赖 `recompute` 的 effect 每次渲染都跑 -> 每帧一次
  // `setState`，哪怕窗口没变化也是在空转。按值比较才能真正稳定。
  const initialSize = limits?.initialSize ?? DEFAULT_LIMITS.initialSize;
  const pageSize = limits?.pageSize ?? DEFAULT_LIMITS.pageSize;
  const buffer = limits?.buffer ?? DEFAULT_LIMITS.buffer;
  const maxMounted = limits?.maxMounted ?? DEFAULT_LIMITS.maxMounted;
  const merged: WindowLimits = useMemo(
    () => ({
      initialSize,
      pageSize,
      buffer,
      maxMounted,
      // 像素阈值随真实中位数走：重尾分布下按条数判断会误触发。
      bufferPx: buffer * book.median(),
    }),
    [initialSize, pageSize, buffer, maxMounted, book],
  );

  const [state, setState] = useState<WindowState | null>(null);
  const stateRef = useRef<WindowState | null>(null);
  stateRef.current = state;
  // focus 用 ref 而不是 state：它只影响**一次**窗口计算，不需要触发渲染。
  // `appliedRef` 按对象身份去重——若调用方没及时把 `focus` 清空（清空时机
  // 在调用方，难免滞后），每次重算都会重新跳到同一位置，用户就再也滚不动了。
  const pendingRef = useRef<FocusRequest | null>(null);
  const appliedRef = useRef<FocusRequest | null>(null);

  // 每帧最多重算一次：滚动期间 `measure` 会读 DOM，逐帧多次没有意义。
  const frameRef = useRef<number | null>(null);
  const itemCountRef = useRef(itemCount);
  itemCountRef.current = itemCount;
  const recompute = useCallback(() => {
    if (frameRef.current !== null) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      // 把「挂载区相对序号」翻译成绝对 index：`measure` 只看得见挂载区，
      // 它数不出前面还有多少条。`nextWindow` 的截断判据用的是绝对 index。
      const raw = measure();
      const base = stateRef.current?.start ?? 0;
      const visible = raw ? { ...raw, first: base + raw.first, last: base + raw.last } : null;
      const next = nextWindow({
        current: stateRef.current,
        itemCount: itemCountRef.current,
        limits: merged,
        visible,
        focus: pendingRef.current,
      });
      // 消费掉这次 focus：它只影响一次窗口计算。
      pendingRef.current = null;
      const prev = stateRef.current;
      if (
        prev !== null &&
        prev.start === next.start &&
        prev.end === next.end &&
        prev.following === next.following
      ) {
        return; // 无变化不触发渲染
      }
      stateRef.current = next;
      setState(next);
    });
  }, [measure, merged]);

  // 条数变化（回放、流式增长、clear）与 focus 变化都要重算。
  useEffect(() => {
    recompute();
  }, [recompute, itemCount, focus]);

  // 外部传入的 focus 也生效，但同一个对象只应用一次（见 appliedRef 注释）。
  useEffect(() => {
    if (focus === null || focus === appliedRef.current) return;
    appliedRef.current = focus;
    pendingRef.current = focus;
    recompute();
  }, [focus, recompute]);

  useEffect(
    () => () => {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
    },
    [],
  );

  // 同步写入 ref 再调度：不经 React state 往返，跳转少一帧，不会先闪一下旧窗口。
  const requestFocus = useCallback(
    (f: FocusRequest) => {
      appliedRef.current = f;
      pendingRef.current = f;
      recompute();
    },
    [recompute],
  );
  const clearFocus = useCallback(() => {
    appliedRef.current = null;
    pendingRef.current = null;
  }, []);

  return {
    // 首帧同步给一个合法窗口，避免渲染出空列表再跳。
    window: state ?? { start: Math.max(itemCount - DEFAULT_LIMITS.initialSize, 0), end: itemCount, following: true },
    requestFocus,
    clearFocus,
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/hooks/useTranscriptWindow.test.tsx`
Expected: PASS

> 若「视口触顶时追加一页」因 rAF 次数不足而不通过：把 `tick()` 里的 `requestAnimationFrame` 改成等待两帧的 `await new Promise(r => setTimeout(r, 32))`。这是测试时序问题，不是实现问题；不要为了让它通过而改 `nextWindow`。

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/hooks/useTranscriptWindow.ts desktop/src/hooks/useTranscriptWindow.test.tsx
git commit -m "feat(desktop): add transcript window hook with injected measurement"
```

---

### Task 6: 滚动补偿纯函数

**Files:**
- Create: `desktop/src/lib/scrollCompensation.ts`
- Test: `desktop/src/lib/scrollCompensation.test.ts`

**Interfaces:**
- Consumes: 无
- Produces:
  - `interface CompensationInput { prevScrollHeight: number; nextScrollHeight: number; prevViewportHeight: number; scrollTop: number; wasFollowing: boolean }`
  - `interface CompensationPlan { scrollTop: number; follow: "keep" | "to-bottom" | "release" }`
  - `function planCompensation(input: CompensationInput): CompensationPlan`

**为什么需要它**：WKWebView / Safari **不实现 scroll anchoring**（`useStickToBottom.ts:66-68` 记录了本项目实测的 4346px 漂移）。所以在视口上方挂载/卸载内容时，必须自己补 `scrollTop`，否则用户看到的内容会整体位移；而「从下方卸载」会让 `scrollHeight` 变小、`scrollTop` 被浏览器往下夹，`useStickToBottom.onScroll` 会把这读成「用户上翻」并永久关闭跟随（`useStickToBottom.ts:171`）。这段算术必须是纯函数，才测得动。

- [ ] **Step 1: 写失败测试**

Create `desktop/src/lib/scrollCompensation.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { planCompensation } from "./scrollCompensation";

const base = {
  prevScrollHeight: 10_000,
  nextScrollHeight: 10_000,
  prevViewportHeight: 800,
  scrollTop: 5_000,
  wasFollowing: false,
};

describe("planCompensation", () => {
  it("上方挂载更多内容：scrollTop 增加同样的高度差", () => {
    const plan = planCompensation({ ...base, nextScrollHeight: 14_000 });
    expect(plan.scrollTop).toBe(9_000);
    expect(plan.follow).toBe("release");
  });

  it("上方卸载内容（高度变小）：scrollTop 相应减少，但不为负", () => {
    const plan = planCompensation({ ...base, scrollTop: 1_000, nextScrollHeight: 8_000 });
    expect(plan.scrollTop).toBe(0);
  });

  it("原本贴底：补偿后重新贴底并保持跟随", () => {
    const plan = planCompensation({
      ...base,
      wasFollowing: true,
      scrollTop: 9_200,
      nextScrollHeight: 11_000,
    });
    expect(plan.follow).toBe("to-bottom");
  });

  it("高度没变：scrollTop 不动，跟随意图不变", () => {
    const plan = planCompensation(base);
    expect(plan.scrollTop).toBe(5_000);
    expect(plan.follow).toBe("release");
  });

  it("新内容比视口还短：scrollTop 夹到 0", () => {
    const plan = planCompensation({
      ...base,
      prevScrollHeight: 400,
      nextScrollHeight: 300,
      scrollTop: 10,
    });
    expect(plan.scrollTop).toBe(0);
  });

  it("结果不超过新的可滚动上限", () => {
    const plan = planCompensation({
      ...base,
      scrollTop: 9_950,
      prevScrollHeight: 10_000,
      nextScrollHeight: 12_000,
    });
    expect(plan.scrollTop).toBeLessThanOrEqual(12_000 - 800);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/scrollCompensation.test.ts`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现**

Create `desktop/src/lib/scrollCompensation.ts`:

```ts
/**
 * 窗口变化后的 `scrollTop` 补偿。
 *
 * 背景（本项目实测过）：WKWebView / Safari **不实现 scroll anchoring**，
 * 所以「在视口上方挂载/卸载内容」不会自动粘住用户的阅读位置，必须自己补。
 * 详见 `useStickToBottom.ts` 顶部注释里那 4346px 的漂移。
 *
 * 纯函数：只做算术，不碰 DOM，因此四种方向（上方挂载 / 上方卸载 /
 * 下方卸载 / 首次布局）都能直接单测。
 */
export interface CompensationInput {
  prevScrollHeight: number;
  nextScrollHeight: number;
  prevViewportHeight: number;
  scrollTop: number;
  /** 变更前用户是否处于「贴底」状态。 */
  wasFollowing: boolean;
}

export interface CompensationPlan {
  scrollTop: number;
  /**
   * - `to-bottom`：原本贴底，补偿后重新贴底（同时重设基线）
   * - `release`：放弃跟随但不移动视口（把基线对齐到新布局）
   * - `keep`：无需变更
   */
  follow: "keep" | "to-bottom" | "release";
}

export function planCompensation(input: CompensationInput): CompensationPlan {
  const { prevScrollHeight, nextScrollHeight, prevViewportHeight, scrollTop, wasFollowing } = input;

  if (wasFollowing) {
    // 贴底时不按差值补：直接把视口放回新的底部更准（内容还在长高的情况也是）。
    return { scrollTop: Math.max(0, nextScrollHeight - prevViewportHeight), follow: "to-bottom" };
  }

  const delta = nextScrollHeight - prevScrollHeight;
  const maxScrollTop = Math.max(0, nextScrollHeight - prevViewportHeight);
  const target = Math.min(Math.max(0, scrollTop + delta), maxScrollTop);

  // 即使 delta 为 0 也要 release：它把 `lastTopRef`/`lastMaxRef` 对齐到新布局，
  // 下一次 onScroll 的基线才是干净的。
  return { scrollTop: target, follow: "release" };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/scrollCompensation.test.ts`
Expected: PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/scrollCompensation.ts desktop/src/lib/scrollCompensation.test.ts
git commit -m "feat(desktop): add pure scroll compensation for window changes"
```

---

### Task 7: `HistoryWindow` 组件 + `ChatView` 渲染接线

**Files:**
- Create: `desktop/src/components/HistoryWindow.tsx`
- Modify: `desktop/src/components/ChatView.tsx:333-372`（`HistoryPrefix`）、`:806-826`（渲染序列）、`:504` 附近（窗口计算）
- Test: `desktop/src/components/ChatView.test.tsx`（追加用例）

**Interfaces:**
- Consumes: `ChatItem`（同文件内部，需从 `ChatView.tsx` 导出或在 `HistoryWindow.tsx` 里 import）、`itemText`
- Produces: `function HistoryWindow(props: { items: (Item|NoticeItem|ReasoningItem)[]; start: number; end: number; threadId: string|null; call: ImageReadCall; query: string; structuralVersion: number }): JSX.Element`

> **实现方式选择**：`ChatItem` 目前是 `ChatView.tsx` 的模块内常量（`ChatView.tsx:240` 附近）。为了不制造循环依赖，**把 `HistoryWindow` 放在 `ChatView.tsx` 内部**，只把 `HistoryPrefix` 改名并改签名。这样 `ChatItem` 的 memo 比较器不用动，也不会多一个文件的导出面。本任务因此**不新建** `HistoryWindow.tsx`，只改 `ChatView.tsx`。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/components/ChatView.test.tsx` 末尾追加：

```tsx
describe("transcript windowing", () => {
  /** 建 n 条 agentMessage，id 稳定。 */
  function manyItems(n: number): Item[] {
    return Array.from({ length: n }, (_, i) => ({
      type: "agentMessage",
      id: `m${i}`,
      text: `row ${i}`,
    })) as Item[];
  }

  function renderWith(items: Item[]) {
    return render(
      <ChatView
        items={items}
        threadId="t1"
        structuralVersion={1}
        query=""
        call={() => Promise.reject(new Error("no images"))}
      />,
    );
  }

  it("3000 条会话冷启动只挂载 60 个 item 节点", () => {
    const { container } = renderWith(manyItems(3000));
    expect(container.querySelectorAll("[data-item-id]")).toHaveLength(60);
  });

  it("渲染上下的 spacer，未挂载区间不是真空", () => {
    const { container } = renderWith(manyItems(3000));
    expect(container.querySelector("[data-spacer='top']")).not.toBeNull();
    expect(container.querySelector("[data-spacer='bottom']")).not.toBeNull();
  });

  it("尾部那条只渲染一次（HistoryWindow 与 LiveTail 不重复）", () => {
    const { container } = renderWith(manyItems(3000));
    const ids = Array.from(container.querySelectorAll("[data-item-id]")).map((n) =>
      n.getAttribute("data-item-id"),
    );
    expect(ids[ids.length - 1]).toBe("m2999");
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("短会话（少于首屏）全挂且没有重复", () => {
    const { container } = renderWith(manyItems(7));
    const ids = Array.from(container.querySelectorAll("[data-item-id]")).map((n) =>
      n.getAttribute("data-item-id"),
    );
    expect(ids).toEqual(["m0", "m1", "m2", "m3", "m4", "m5", "m6"]);
  });

  it("空会话不崩", () => {
    const { container } = renderWith([]);
    expect(container.querySelectorAll("[data-item-id]")).toHaveLength(0);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "transcript windowing"`
Expected: FAIL — 挂载 3000 条，第一条断言得到 3000 而不是 60。

- [ ] **Step 3: 实现**

在 `desktop/src/components/ChatView.tsx` 里：

**(a)** 把 `HistoryPrefix` 改名并改签名（`ChatView.tsx:333-372`）：

```tsx
/**
 * 挂载区：只渲染 `[start, end)` 这一段。
 *
 * `end` 是逻辑终点，**含**尾部那条；尾部恒由 `LiveTail` 单独渲染，
 * 所以这里要 `Math.min(end, total - 1)`。漏掉这个 clamp，`end === total` 时
 * 最后一条会被渲染两次（React key 相同，控制台直接报错）。
 */
const HistoryWindow = memo(
  function HistoryWindow({
    items,
    total,
    start,
    end,
    threadId,
    call,
    query,
  }: {
    items: (Item | NoticeItem | ReasoningItem)[];
    total: number;
    start: number;
    end: number;
    threadId: string | null;
    call: ImageReadCall;
    structuralVersion: number;
    query: string;
  }) {
    const stop = Math.min(end, total - 1);
    const from = Math.min(start, stop);
    const out = [];
    for (let i = from; i < stop; i += 1) {
      const it = items[i];
      if (!it) continue;
      out.push(
        <ChatItem
          key={it.id ?? i}
          item={it}
          text={itemText(it)}
          threadId={threadId}
          call={call}
          query={query}
        />,
      );
    }
    return <>{out}</>;
  },
  // `items`/区间不变 + 无结构性变更 ⇒ 上方内容没动过。
  (prev, next) =>
    prev.items === next.items &&
    prev.total === next.total &&
    prev.start === next.start &&
    prev.end === next.end &&
    prev.structuralVersion === next.structuralVersion &&
    prev.threadId === next.threadId &&
    prev.call === next.call &&
    prev.query === next.query,
);
```

**(b)** 在 `tail` 定义之后（`ChatView.tsx:504` 附近）加窗口计算：

```tsx
  // 窗口：只在挂载区渲染 `[start, end)`，其余用 spacer 撑高度。
  // `measure` 在未布局（scrollHeight 为 0，例如 jsdom 或首帧）时返回 null，
  // 此时 nextWindow 保持跟随、不翻页——这条守卫同时让测试可确定地断言首屏条数。
  const windowBook = useMemo(() => createHeightBook(), []);
  const measureVisible = useCallback((): VisibleRange | null => {
    const el = elRef.current;
    if (!el || el.scrollHeight === 0 || el.clientHeight === 0) return null;
    const topSpacer = el.querySelector<HTMLElement>("[data-spacer='top']");
    const bottomSpacer = el.querySelector<HTMLElement>("[data-spacer='bottom']");
    const rows = el.querySelectorAll<HTMLElement>("[data-item-id]");
    if (rows.length === 0) return null;
    const viewTop = el.scrollTop;
    const viewBottom = viewTop + el.clientHeight;
    const firstNode = rows[0];
    const lastNode = rows[rows.length - 1];
    const base = el.getBoundingClientRect().top - el.scrollTop;
    const firstTop = firstNode.getBoundingClientRect().top - base;
    const lastBottom = lastNode.getBoundingClientRect().bottom - base;
    // first/last 这里是**挂载区相对序号**：组件层数不出前面还有多少条。
    // `useTranscriptWindow` 会加上 `window.start` 变成绝对 index。
    return {
      first: 0,
      last: rows.length - 1,
      // **不要夹到 ≥0**：`gapTopPx` 为负表示顶边已在视口上方，这正是
      // 「用户在窗口内部」的正常状态，必须与「顶边贴住视口顶（0）」区分开。
      // 夹掉符号会让底部跟随被误判成翻到顶，窗口逐帧抖动。
      gapTopPx: firstTop - viewTop,
      gapBottomPx: viewBottom - lastBottom,
    };
  }, []);
  // 这里**不传** `focus` prop：锚点恢复（Task 10）与搜索跳转（Task 11）都走
  // `requestFocus()`，是命令式的一次性请求。传 `focus={ref.current}` 读 ref 属于
  // 渲染期读可变值，而且值永远是 null，等于给 hook 加了一条走不通的分支。
  const { window: win, requestFocus } = useTranscriptWindow({
    itemCount: items.length,
    measure: measureVisible,
    book: windowBook,
  });
```

> `first`/`last` 在这一层是**挂载区相对序号**（0 到 mounted-1），因为组件数不出
> 前面还有多少条；`useTranscriptWindow` 负责加上 `window.start` 变成绝对 index
> 再交给 `nextWindow`。两处口径见 `VisibleRange` 的字段注释。

**(c)** 渲染序列（`ChatView.tsx:806-826`）：

```tsx
      <HistoryWindow
        items={items}
        total={items.length}
        start={win.start}
        end={win.end}
        structuralVersion={structuralVersion}
        threadId={threadId}
        call={effectiveCall}
        query={query}
      />
```

并在 `HistoryWindow` 前后插入 spacer：

```tsx
      {/* 未挂载历史的占位高度。滚动条比例因此是真实的，滚动仍由浏览器原生完成。 */}
      <div
        data-spacer="top"
        aria-hidden="true"
        style={{ height: spacerTop }}
      />
```

其中 `spacerTop` / `spacerBottom` 用 `HeightBook` 估算（**在 Task 8 里接真实测量**，本任务先用估值保证能渲染）：

```tsx
  // 未挂载区间的高度估算。阈值以上的近似即可：spacer 的意义是让滚动条比例正确，
  // 精确高度由 Task 8 的 ResizeObserver + 实测校正负责。
  //
  // 索引空间：`win.end` 是**排他**终点且含尾部那条，而 `HistoryWindow` 只渲染到
  // `stop = min(end, total - 1)`（尾部恒由 `LiveTail` 渲染）。所以尾部下方未挂载
  // 的只有 `[stop, total - 1)` 这一段：
  //   - `end === total`（跟随中）-> `stop === total - 1` -> 下方为 0，别多撑一条的高度
  //   - `end < total`          -> `stop === end`         -> 下方为 `total - 1 - end` 条
  const medianH = windowBook.median();
  const spacerTop = win.start * medianH;
  const stop = Math.min(win.end, Math.max(items.length - 1, 0));
  const spacerBottom = Math.max(0, items.length - 1 - stop) * medianH;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS（含既有全部用例与新加的 5 个）

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "feat(desktop): window the transcript with top and bottom spacers"
```

---

### Task 8: `capture()` 与 `ResizeObserver` 限定到挂载区

**Files:**
- Modify: `desktop/src/components/ChatView.tsx:648-653`（`ResizeObserver`）、`:703-710`（`capture`）
- Test: `desktop/src/components/ChatView.test.tsx`（追加）

**Interfaces:**
- Consumes: Task 7 的 `[data-spacer]` 节点
- Produces: 无新导出（行为变更）

- [ ] **Step 1: 写失败测试**

在 `ChatView.test.tsx` 追加：

```tsx
describe("capture() only measures the mounted window", () => {
  it("3000 条会话单次采集的 getBoundingClientRect 次数不超过挂载区两倍", () => {
    const spy = vi.spyOn(Element.prototype, "getBoundingClientRect");
    const items = Array.from({ length: 3000 }, (_, i) => ({
      type: "agentMessage",
      id: `m${i}`,
      text: `row ${i}`,
    })) as Item[];
    render(
      <ChatView items={items} threadId="t1" structuralVersion={1} query="" />,
    );
    spy.mockClear();
    const scroller = document.querySelector("[data-thread-id]") as HTMLElement | null;
    if (scroller) scroller.dispatchEvent(new Event("scroll"));
    // 挂载 60 条 + 两次容器/首个节点测量，远小于 3000。
    expect(spy.mock.calls.length).toBeLessThan(200);
    spy.mockRestore();
  });
});
```

> 若 `[data-thread-id]` 在 jsdom 下取不到，改用 `container.querySelector("div")` 找到滚动容器，或在渲染时传入 `scrollerRef` 并用它派发事件。断言的是**调用次数上界**，不是精确值。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "only measures the mounted window"`
Expected: FAIL — 收集到 3000 次调用。

- [ ] **Step 3: 实现**

`capture()`（`ChatView.tsx:703`）改为只遍历挂载区，并补一条注释说明唯一的语义差异：

```tsx
      // 只量挂载区：未挂载区间没有节点，且视口不可能落在它们中间。
      //
      // 唯一的语义差异：视口落在 spacerTop 内（用户把已挂载的历史全部滚到视口下方）
      // 时，没有任何已挂载消息的顶边在视口顶之上，`findAnchor` 会退回「最靠上的
      // 已挂载那条」。挂载区顶边一进入视口就向上分页（buffer 20 条），使这个窗口
      // 只持续一帧；分页到顶后最靠上的就是第一条，退路语义正确。
      const boxes: ItemBox[] = [];
      for (const child of node.querySelectorAll<HTMLElement>("[data-item-id]")) {
        const id = child.dataset.itemId;
        if (!id) continue;
        boxes.push({ id, top: child.getBoundingClientRect().top - base });
      }
```

`ResizeObserver`（`ChatView.tsx:648-653`）改为观察两个 spacer + 挂载区：

```tsx
    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      observer = new ResizeObserver(schedule);
      // 观察 spacer：它们的高度代表「未挂载区间」的布局变化。
      // 未挂载区没有节点可观察，spacer 是唯一可用的信号。
      for (const s of el.querySelectorAll("[data-spacer]")) observer.observe(s);
      // 观察挂载区：图片异步加载、工具卡展开会让已挂载条目变高。
      for (const child of el.querySelectorAll("[data-item-id]")) observer.observe(child);
    }
```

同时在渲染后把实测高度写回 `HeightBook`（新建一个 effect，依赖 `win.start`/`win.end`）：

```tsx
  // 把挂载区的真实高度写回 HeightBook：中位数越准，spacer 越准，
  // 分页后的视觉跳动越小（设计 §2.4 的螺旋收敛）。
  useEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const nodes = el.querySelectorAll<HTMLElement>("[data-item-id]");
    for (const node of nodes) {
      const id = node.dataset.itemId;
      if (!id) continue;
      const h = node.getBoundingClientRect().height;
      // **必须过滤 0**：未布局（首帧、jsdom、刚挂载还没排版）时高度是 0，
      // `HeightBook.record` 认为 0 合法（折叠条目确实可能为 0），会照收。
      // 大量 0 进来会把中位数拉到 0，spacer 高度归零，滚动条比例整个失真。
      if (h > 0) windowBook.record(id, h);
    }
  }, [win.start, win.end, windowBook]);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "perf(desktop): measure and observe only the mounted window"
```

---

### Task 9: 窗口变更时应用滚动补偿

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`（新增 effect，紧接 Task 7 的窗口计算）
- Test: `desktop/src/components/ChatView.test.tsx`（追加）

**Interfaces:**
- Consumes: `planCompensation`（`../lib/scrollCompensation`）、`useStickToBottom` 的 `scrollToBottom` / `releaseFollow` / `isFollowing`
- Produces: 无新导出

- [ ] **Step 1: 写失败测试**

在 `ChatView.test.tsx` 追加（jsdom 里高度恒为 0，所以这里验证的是**事务被调用且顺序正确**，用 spy）：

```tsx
describe("window change compensates scrollTop", () => {
  it("窗口区间变化时写入 scrollTop 并调用 releaseFollow", () => {
    const items = Array.from({ length: 3000 }, (_, i) => ({
      type: "agentMessage",
      id: `m${i}`,
      text: `row ${i}`,
    })) as Item[];
    const { container, rerender } = render(
      <ChatView items={items} threadId="t1" structuralVersion={1} query="" />,
    );
    const scroller = container.querySelector("div");
    const before = scroller?.scrollTop ?? 0;
    // 触发一次结构性变更，让窗口重算
    rerender(
      <ChatView items={items} threadId="t1" structuralVersion={2} query="" />,
    );
    // 不崩、且 scrollTop 仍是合法数字
    expect(Number.isFinite(scroller?.scrollTop ?? 0)).toBe(true);
    expect(scroller?.scrollTop ?? 0).toBeGreaterThanOrEqual(0);
    expect(before).toBeGreaterThanOrEqual(0);
  });
});
```

> 这条测试只能守住「不崩 + 合法值」。真正的补偿算术由 Task 6 的
> `planCompensation.test.ts` 覆盖——这也正是把它抽成纯函数的原因。
> **不要在 jsdom 里假装测出真实像素位移**，那只会得到一个假绿的测试。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "compensates scrollTop"`
Expected: FAIL 或报「区间变化后 DOM 顺序错乱」；若已通过，说明还没接线——继续下一步并把断言保留为回归。

- [ ] **Step 3: 实现**

在 `ChatView.tsx` 加：

```tsx
  // 窗口变化会改变内容高度，而 WKWebView **不实现 scroll anchoring**：
  // 视口上方多挂 200 条，用户看到的内容就整体位移；从下方卸载则让
  // scrollHeight 变小、scrollTop 被浏览器往下夹，`useStickToBottom` 会把这次
  // 位移读成「用户上翻」并永久关闭跟随。所以每次窗口变更都要补偿一次。
  const prevWindowRef = useRef<{ start: number; end: number } | null>(null);
  const prevHeightRef = useRef<number>(0);
  useLayoutEffect(() => {
    const el = elRef.current;
    if (!el) return;
    const prev = prevWindowRef.current;
    const prevHeight = prevHeightRef.current;
    prevWindowRef.current = { start: win.start, end: win.end };
    prevHeightRef.current = el.scrollHeight;

    // 首帧没有「上一次」，不需要补偿。
    if (prev === null || prevHeight === 0) return;
    if (prev.start === win.start && prev.end === win.end) return;

    const plan = planCompensation({
      prevScrollHeight: prevHeight,
      nextScrollHeight: el.scrollHeight,
      prevViewportHeight: el.clientHeight,
      scrollTop: el.scrollTop,
      wasFollowing: isFollowing(),
    });

    // 顺序不能变：先写 scrollTop，再 releaseFollow/releaseFollow 对齐基线。
    // 反过来会把「补偿前的旧基线」留在 ref 里，下一次 onScroll 仍会误判。
    if (Math.abs(el.scrollTop - plan.scrollTop) >= 1) {
      el.scrollTop = plan.scrollTop;
      selfScrolling.current = el.scrollTop;
    }
    if (plan.follow === "to-bottom") scrollToBottom();
    else releaseFollow();
  }, [win.start, win.end, isFollowing, scrollToBottom, releaseFollow]);
```

- [ ] **Step 4: 跑测试确认通过 + 既有贴底测试回归**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx src/lib/useStickToBottom.test.ts`
Expected: 两个文件都 PASS。**特别检查既有的「流式跟随」「上翻历史后不被拽回底部」用例仍然绿**——它们是这次改动最可能打坏的地方。

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "fix(desktop): compensate scrollTop when the window changes"
```

---

### Task 10: 锚点恢复改为「数据层找 index → 开窗 → 对位」

**Files:**
- Modify: `desktop/src/components/ChatView.tsx:519-598`
- Test: `desktop/src/components/ChatView.test.tsx`（追加）

**Interfaces:**
- Consumes: `requestFocus`（Task 7 的 hook 返回值）、`items`
- Produces: 无新导出

**要消除的行为退化**：现在 `restoreScrollTop` 在找不到节点时返回 `null`，调用方**回退到底部**（`viewAnchor.ts:59-62`）。窗口化后「锚点没挂载」会静默变成「跳到底部」。必须把两种语义分开：**没挂载**（先开窗）与**真的没了**（`index === -1`）。

- [ ] **Step 1: 写失败测试**

在 `ChatView.test.tsx` 追加：

```tsx
describe("anchor restore with windowing", () => {
  it("锚点在未挂载区间时开窗到它，而不是跳到底部", () => {
    const items = Array.from({ length: 3000 }, (_, i) => ({
      type: "agentMessage",
      id: `m${i}`,
      text: `row ${i}`,
    })) as Item[];
    const { container } = render(
      <ChatView
        items={items}
        threadId="t1"
        structuralVersion={1}
        query=""
        anchor={{ itemId: "m500", offset: 0 }}
      />,
    );
    const ids = Array.from(container.querySelectorAll("[data-item-id]")).map((n) =>
      n.getAttribute("data-item-id"),
    );
    expect(ids).toContain("m500");
  });

  it("锚点确实不存在时回退到底部（渲染最后一条）", () => {
    const items = Array.from({ length: 3000 }, (_, i) => ({
      type: "agentMessage",
      id: `m${i}`,
      text: `row ${i}`,
    })) as Item[];
    const { container } = render(
      <ChatView
        items={items}
        threadId="t1"
        structuralVersion={1}
        query=""
        anchor={{ itemId: "gone", offset: 0 }}
      />,
    );
    const ids = Array.from(container.querySelectorAll("[data-item-id]")).map((n) =>
      n.getAttribute("data-item-id"),
    );
    expect(ids).toContain("m2999");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "anchor restore with windowing"`
Expected: 第一条 FAIL（`m500` 不在挂载区，因为没有开窗）。

- [ ] **Step 3: 实现**

把 `ChatView.tsx:519-598` 那个 `useLayoutEffect` 的**查找部分**改成数据层定位：

```tsx
  useLayoutEffect(() => {
    const el = elRef.current;
    if (!el) return;
    if (restoredFor.current === threadId) return;
    // `null` = 离开时贴底，`undefined` = 从没记过：两者都走既有的"到底"。
    if (anchor == null) {
      restoredFor.current = threadId;
      return;
    }

    // 第一步：在数据层找 index。**不依赖 DOM**——锚点很可能不在挂载区，
    // 这时按 DOM 找不到会被误判成「锚点真的没了」而跳到底部。
    const index = items.findIndex((it) => it.id === anchor.itemId);
    if (index === -1) {
      // 内容还在补齐：回放分帧到达，此刻可能只收到了前几帧（不含锚点那条）。
      if (syncing || items.length === 0) return;
      // 已有完整内容却没有这条（被删/历史被截断）：回退到底部并锁定。
      restoredFor.current = threadId;
      scrollToBottom();
      return;
    }

    // 第二步：开窗，把锚点包进挂载区。用 buffer 表达，不写字面量。
    requestFocus({ index, before: DEFAULT_LIMITS.buffer, after: 3 * DEFAULT_LIMITS.buffer });

    // 第三步：等布局落定后再对位。这一段必须延后到锚点真的挂载出来，
    // 否则 querySelector 仍找不到它——所以不在本次 effect 里写 scrollTop，
    // 而是让下面那个「锚点已挂载」的 effect 接手。
    pendingAnchorRef.current = anchor;
    releaseFollow();
  }, [threadId, anchor, items.length, syncing, scrollToBottom, releaseFollow, items, requestFocus]);
```

再加一个 effect，等锚点进入 DOM 后对位（替换原来 `realign` 的入口条件）：

```tsx
  // 锚点进入挂载区后再对位。窗口化之前这里可以同步做完，因为全部条目都在 DOM；
  // 现在要等 focus 开窗渲染落地。
  useEffect(() => {
    const el = elRef.current;
    const pending = pendingAnchorRef.current;
    if (!el || !pending || restoredFor.current === threadId) return;
    const row = Array.from(el.querySelectorAll<HTMLElement>("[data-item-id]")).find(
      (n) => n.dataset.itemId === pending.itemId,
    );
    if (!row) return;
    const base = el.getBoundingClientRect().top - el.scrollTop;
    const next = restoreScrollTop(
      { id: pending.itemId, top: row.getBoundingClientRect().top - base },
      pending,
    );
    if (next === null) return;
    userTookOver.current = false;
    el.scrollTop = next;
    selfScrolling.current = el.scrollTop;
    restoredFor.current = threadId;
    heldAnchor.current = pending;
    pendingAnchorRef.current = null;
    releaseFollow();
  }, [threadId, win.start, win.end, items.length, releaseFollow]);
```

并声明 ref：

```tsx
  /** 已找到 index、等待开窗渲染落地的锚点。 */
  const pendingAnchorRef = useRef<ViewAnchor | null>(null);
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS（既有锚点恢复用例同样保持绿）

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "fix(desktop): restore reading position by index, not by DOM lookup"
```

---

### Task 11: 搜索下沉到数据层

**Files:**
- Modify: `desktop/src/hooks/useTranscriptSearch.ts`
- Modify: `desktop/src/App.tsx:864-890`
- Test: `desktop/src/hooks/useTranscriptSearch.test.tsx`（按新语义改写）

**Interfaces:**
- Consumes: `collectHits` / `prefixTops` / `estimatedRects`（`../lib/searchIndex`）、`initialActiveIndex` / `nextIndex`（`../lib/searchHits`）
- Produces: `useTranscriptSearch` 的出参签名**不变**（`total` / `active` / `next` / `prev` / `reselect`），新增入参：
  - `items: readonly AnyItem[]`
  - `structuralVersion: number`
  - `onJump?: (index: number) => void`
  - `book: HeightBook`

> **不要加 `textVersion` 参数。** 它进不了 effect 依赖（进了就等于流式期间每次
> 60ms 文本节流都重扫全量），不进依赖又只能撞 `noUnusedParameters` 或写一句
> 无意义的 `void textVersion`。而它想覆盖的情况本就被 `structuralVersion` 覆盖：
> `session.ts:410-431` 的 `insertAfterLastServerItem` 每次插入都会 bump
> `structuralVersion`；尾部正文的原地增长只改尾部那一条，而尾部不属于历史窗口，
> 不影响全量命中计数。少一个用不上的参数，测起来也更干净。

- [ ] **Step 1: 写失败测试**

改写 `desktop/src/hooks/useTranscriptSearch.test.tsx`（保留原文件的 jsdom 头与 `beforeEach`）：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, cleanup, act } from "@testing-library/react";
import { useRef, useState } from "react";
import { useTranscriptSearch } from "./useTranscriptSearch";
import { createHeightBook } from "../lib/heightBook";
import type { Item } from "../lib/protocol";

let last: { total: number; active: number; api: any } = { total: 0, active: 0, api: null };
let jumps: number[] = [];

const msg = (id: string, text: string): Item => ({ type: "agentMessage", id, text }) as Item;

function Harness({
  items,
  query,
  structuralVersion = 0,
  open = true,
}: {
  items: Item[];
  query: string;
  structuralVersion?: number;
  open?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [book] = useState(() => createHeightBook(40));
  const state = useTranscriptSearch({
    containerRef: ref,
    open,
    query,
    items,
    structuralVersion,
    book,
    onJump: (i: number) => jumps.push(i),
  });
  last = { total: state.total, active: state.active, api: state };
  return <div ref={ref} />;
}

beforeEach(() => {
  last = { total: 0, active: 0, api: null };
  jumps = [];
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(cleanup);

describe("useTranscriptSearch (data layer)", () => {
  it("命中数覆盖全量历史，不只是挂载区", () => {
    const items = Array.from({ length: 3000 }, (_, i) => msg(`m${i}`, "needle"));
    render(<Harness items={items} query="needle" />);
    // 3000 条每条一次命中
    expect(last.total).toBe(3000);
  });

  it("空查询计数为 0", () => {
    render(<Harness items={[msg("a", "x")]} query="  " />);
    expect(last.total).toBe(0);
  });

  it("next 回绕，按命中顺序推进", () => {
    const items = [msg("a", "x"), msg("b", "x"), msg("c", "x")];
    render(<Harness items={items} query="x" />);
    act(() => last.api.next());
    expect(last.active).toBe(1);
    act(() => last.api.next());
    act(() => last.api.next());
    expect(last.active).toBe(0);
  });

  it("跳转未挂载的命中会请求开窗（onJump 收到 index）", () => {
    const items = Array.from({ length: 3000 }, (_, i) =>
      msg(`m${i}`, i === 1200 ? "needle" : "hay"),
    );
    render(<Harness items={items} query="needle" />);
    expect(last.total).toBe(1);
    expect(jumps).toContain(1200);
  });

  it("同引用重渲染不重扫（流式文本 delta 不触发重扫）", () => {
    const items = [msg("a", "x"), msg("b", "y")];
    const { rerender } = render(
      <Harness items={items} query="x" structuralVersion={0} />,
    );
    // 每次重扫都会把当前命中交给 onJump，所以 jumps 的长度就是扫描次数的探针。
    // （不要用 findIndex 计数：hook 内部根本不调它，那样的探针永远通过。）
    const scansAfterFirstRender = jumps.length;
    expect(scansAfterFirstRender).toBeGreaterThan(0);
    // 流式期间：同一个 items 数组实例、原地改文本、结构版本不变。hook 只看
    // `items` 身份 / `structuralVersion` / `query`，这些都没变，所以不该重扫。
    rerender(<Harness items={items} query="x" structuralVersion={0} />);
    expect(jumps.length).toBe(scansAfterFirstRender);
  });

  it("结构性变更会重扫", () => {
    const items = [msg("a", "x")];
    const { rerender } = render(
      <Harness items={items} query="x" structuralVersion={0} />,
    );
    items.push(msg("b", "x"));
    rerender(<Harness items={items} query="x" structuralVersion={1} />);
    expect(last.total).toBe(2);
  });

  it("关闭搜索时计数清零", () => {
    const items = [msg("a", "x")];
    const { rerender } = render(<Harness items={items} query="x" open />);
    expect(last.total).toBe(1);
    rerender(<Harness items={items} query="x" open={false} />);
    expect(last.total).toBe(0);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/hooks/useTranscriptSearch.test.tsx`
Expected: FAIL — 新入参 `items` / `book` 不存在。

- [ ] **Step 3: 实现**

改写 `desktop/src/hooks/useTranscriptSearch.ts` 的核心（保留 `SEARCH_DEBOUNCE_MS`、`SEARCH_HIT_SELECTOR` 导出）：

```ts
  const hitsRef = useRef<SearchHit[]>([]);
  const totalRef = useRef(0);
  const activeRef = useRef(0);

  /**
   * 重扫：在 `session.items` 上匹配。
   *
   * 为什么不在 DOM 上扫 `mark.search-hit`：窗口化之后未挂载的历史没有 mark，
   * 搜索会静默缩小范围。高亮仍由 `rehypeSearchMark` 负责，但**计数与跳转**
   * 必须基于数据层。
   */
  const rescan = useCallback(() => {
    const hits = open && normalizeQuery(query) !== "" ? collectHits(items, query) : [];
    hitsRef.current = hits;
    totalRef.current = hits.length;
    setTotal(hits.length);
    setActive(0);
    activeRef.current = 0;
    // 换查询/结构变更后重新落在第一个命中上，并把它带进视口。窗口化之后
    // 第一个命中很可能不在挂载区，不主动开窗用户会以为「没找到」。
    if (hits.length > 0) reveal(0);
  }, [items, open, query, reveal]);

  // query / 结构性变更 -> 重扫。**纯文本 delta 刻意不重扫**：
  // 流式期间文本按 60ms 窗口更新，重扫 3000 条是纯浪费；搜索是用户主动行为，
  // 短暂滞后可接受（用户已确认的取舍）。
  useEffect(() => {
    rescan();
  }, [rescan, structuralVersion]);

  // `onJump` 必须走 ref：调用方几乎总是传内联箭头函数，它的身份每次渲染都变，
  // 若进了 `reveal`/`rescan` 的依赖，就等于**每次渲染都重扫全量历史**。
  const onJumpRef = useRef(onJump);
  onJumpRef.current = onJump;

  /** 把当前命中摆进视口：先请求开窗，再由 ChatView 对位。 */
  const reveal = useCallback((index: number) => {
    const hit = hitsRef.current[index];
    if (hit) onJumpRef.current?.(hit.index);
  }, []);

  /** 打开查找条时选「视口附近那个」命中；未挂载的用估算矩形。 */
  const reselect = useCallback(() => {
    const el = containerRef.current;
    const hits = hitsRef.current;
    if (!el || hits.length === 0) {
      setActive(0);
      activeRef.current = 0;
      return;
    }
    const tops = prefixTops(items, book);
    const rects = estimatedRects(hits, tops, book);
    const index = initialActiveIndex(rects, {
      scrollTop: el.scrollTop,
      clientHeight: el.clientHeight,
    });
    setActive(index);
    activeRef.current = index;
    reveal(index);
  }, [book, containerRef, items, reveal]);

  const step = useCallback(
    (delta: 1 | -1) => {
      const totalNow = totalRef.current;
      if (totalNow === 0) return;
      const index = nextIndex(activeRef.current, totalNow, delta);
      activeRef.current = index;
      setActive(index);
      reveal(index);
    },
    [reveal],
  );
```

`mark` 的高亮标注（`data-search-active`）保留在 `ChatView` 侧：给已挂载区间里
第 `active` 个命中加属性。为保持本任务边界清晰，这里只把「计数/当前序号/跳转」
换成数据层，`data-search-active` 的 DOM 标注留给后续（不是本设计的关键路径）。

- [ ] **Step 4: 跑测试确认通过 + `App` 接线**

`App.tsx:864` 的调用改为：

```tsx
  const search = useTranscriptSearch({
    containerRef: transcriptRef,
    open: searchOpen,
    query: searchQuery,
    items: current?.session.items ?? [],
    structuralVersion: current?.session.structuralVersion ?? 0,
    book: searchBook,
    onJump: (index) => {
      transcriptWindowRef.current?.requestFocus({
        index,
        before: DEFAULT_LIMITS.buffer,
        after: 3 * DEFAULT_LIMITS.buffer,
      });
    },
  });
```

其中 `searchBook` 用 `useMemo(() => createHeightBook(), [])`，`transcriptWindowRef`
由 `ChatView` 新增的可选 prop `windowApiRef` 回填（`requestFocus` 的引用）。

Run: `cd desktop && npx vitest run src/hooks/useTranscriptSearch.test.tsx src/App.test.tsx`
Expected: PASS

- [ ] **Step 5: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/hooks/useTranscriptSearch.ts desktop/src/hooks/useTranscriptSearch.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): search the item data layer so history stays findable"
```

---

### Task 12: 记录优化前后数字

**Files:**
- Modify: `desktop/scripts/perf/open.mjs`
- Modify: `docs/superpowers/specs/2026-11-06-transcript-window-design.md`（§6.2 补一行结论）

**Interfaces:**
- Consumes: 无（独立脚本）
- Produces: 无

- [ ] **Step 1: 在脚本里加「窗口化 + 分页」联合场景**

**必须复用生产的 `nextWindow`，不要在脚本里手写一遍翻页规则。** 手写一份等于
在测量一个和生产不同的算法，数字再漂亮也是假的。Node 23 的类型剥离默认开启，
`.mjs` 直接 `import` 生产 `.ts` 即可（无需加标志），先确认：

```bash
cd desktop && node -e \
  'import("./src/lib/transcriptWindow.ts").then(m=>console.log(typeof m.nextWindow))'
# 期望输出: function（会带一条 ExperimentalWarning，属正常）
```

> 运行环境要求 Node ≥ 22.18 / 23（默认开类型剥离）。若本机是更早的版本，
> 用 `node --experimental-strip-types` 跑；`scripts/perf/open.mjs` 的既有用法
> 不变。

在 `open.mjs` 顶部（`h` 的定义之后）加：

```js
// 复用生产的窗口规则：测的必须是真算法，不是手抄的近似版。
const {
  nextWindow,
  DEFAULT_LIMITS,
} = await import("../src/lib/transcriptWindow.ts");
```

在 `renderOnce` 之后追加：

```js
/**
 * Windowed open: 首屏只挂 `initial` 条，然后按 `nextWindow` 的真实规则一页页
 * 往上翻，直到挂载数达到 `maxMounted`。测的是「冷启动 + 上翻两页」这条用户
 * 真会走的路径，而不是一个理想化的常量窗口。
 */
async function renderWindowed(items, label) {
  const el = document.createElement("div");
  document.body.appendChild(el);
  const root = createRoot(el);
  const itemCount = items.length;
  const t0 = process.hrtime.bigint();

  let win = nextWindow({ current: null, itemCount, limits: DEFAULT_LIMITS, visible: null });
  const draw = () => {
    // `nextWindow.end` 排他且含尾部；尾部由 View 自己单独渲染，所以这里只传
    // 「历史条数」，让 View 自己算出起点。
    root.render(h(View, { items, limit: itemCount - win.start }));
  };
  draw();
  await new Promise((r) => setTimeout(r, 0));

  let frames = 1;
  // 模拟用户往上翻：视口贴住挂载区顶边时，nextWindow 会再放出一页。
  while (win.start > 0 && win.end - win.start < DEFAULT_LIMITS.maxMounted) {
    win = nextWindow({
      current: win,
      itemCount,
      limits: DEFAULT_LIMITS,
      // 贴住顶边（0），底部远在视口下方（负）。
      visible: { first: win.start, last: win.start + 10, gapTopPx: 0, gapBottomPx: -5000 },
    });
    if (win.start === 0) break; // 已经到顶
    draw();
    await new Promise((r) => setTimeout(r, 0));
    frames += 1;
  }

  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  console.log(
    `${label.padEnd(46)} ${ms.toFixed(0).padStart(6)} ms   ` +
      `frames=${String(frames).padStart(4)}  mounted=${String(
        el.querySelectorAll("[data-item-id]").length,
      ).padStart(5)}`,
  );
  root.unmount();
  el.remove();
  return ms;
}
```

并在 `main` 段（现有 `renderOnce` 调用之后）加：

```js
await renderWindowed(items, "proposed: windowed open + page up till cap");
```

- [ ] **Step 2: 跑脚本记录数字**

Run: `cd desktop && node scripts/perf/open.mjs`
Expected: 输出里出现 `proposed: windowed open + page up till cap` 一行，`mounted`
恰为 `DEFAULT_LIMITS.maxMounted`（400），耗时明显低于 ~800 ms。**只看结构
（mounted 数、frames 数）与量级，不要把某一个毫秒数写成断言。**

- [ ] **Step 3: 把实测数字写进 spec §6.2**

在 `docs/superpowers/specs/2026-11-06-transcript-window-design.md` 的 §6.2 末尾加：

```markdown
优化后实测（同一份 3251 items 日志，jsdom）：窗口化冷启动 21~22 ms / 60 条，
分页到 400 条累计 <待填实测值> ms。与 §1.1 的 773 ms 对照即可看出收益；
绝对数值仍不可等同于真机（§7 已说明诚实边界）。
```

把 `<待填实测值>` 换成 Step 2 跑出来的真实数字——**不许估**。

- [ ] **Step 4: 全量回归**

```bash
cd desktop && npx tsc --noEmit
cd desktop && npx vitest run src/components/ChatView.test.tsx src/hooks/useTranscriptWindow.test.tsx \
  src/hooks/useTranscriptSearch.test.tsx src/lib/transcriptWindow.test.ts \
  src/lib/heightBook.test.ts src/lib/searchIndex.test.ts \
  src/lib/scrollCompensation.test.ts src/lib/itemText.test.ts src/App.test.tsx
```

Expected: 全部 PASS。特别确认：
- `ChatView.test.tsx` 里既有的贴底/跟随/锚点/搜索用例全绿（窗口化最可能打坏这些）；
- `App.test.tsx` 全绿（宿主接线没断）。

- [ ] **Step 5: 提交**

```bash
cd .. && git add desktop/scripts/perf/open.mjs docs/superpowers/specs/2026-11-06-transcript-window-design.md
git commit -m "perf(desktop): record windowed open numbers against the baseline"
```

---

## 自检记录

**Spec 覆盖检查**（逐节对照设计文档）

| Spec 节 | 覆盖任务 |
|---|---|
| §2.2 纯函数内核 + 8 条规则 | Task 1 |
| §2.3 高度簿记 | Task 2 |
| §2.4 三条可证伪不变量 | Task 2（零误差 / 不劣化） |
| §3.1 `useStickToBottom` 补偿事务 | Task 6（算术）+ Task 9（接线） |
| §3.2 `viewAnchor` 数据层定位 | Task 10 |
| §3.3 `ResizeObserver` 观察 spacer | Task 8 |
| §3.4 `capture()` 限定挂载区 | Task 8 |
| §4.1 搜索新分工 | Task 4 + Task 11 |
| §4.2 `reselect` 用估算矩形 | Task 4（`estimatedRects`）+ Task 11 |
| §4.3 重扫时机（文本 delta 不重扫） | Task 11 |
| §4.4 `useTranscriptSearch` 改造边界 | Task 11 |
| §5 数据流 | Task 5 + Task 7 |
| §6.1 结构性不变量表 | Task 1 / 2 / 5 / 7 / 10 / 11 各自的测试 |
| §6.2 性能参考可复算 | Task 12 |
| §6.3 回归面 | Task 12 Step 4 |

**类型一致性**：`WindowState` 的 `following` 在 Task 1 定义、Task 5 消费、Task 9 读；
`FocusRequest` 在 Task 1 定义，Task 5/7/10/11 使用同一组字段名（`index`/`before`/`after`）；
`HeightBook` 的 `estimate`/`estimateRange`/`median`/`delta` 在 Task 2 定义，Task 4/7/11 使用同一名字。

**纯逻辑任务的实测验证**（2026-11-06，`desktop` 真实 vitest + jsdom）：

| Task | 用例 | 结果 |
|---|---|---|
| 1 `nextWindow` | 19 | 全通过 |
| 2 `HeightBook` | 10 | 全通过 |
| 6 `scrollCompensation` | 6 | 全通过 |
| 3 `itemText` | 5 | 全通过 |
| 4 `searchIndex` | 9 | 全通过 |
| 5 `useTranscriptWindow` | 5 | 全通过 |
| 11 `useTranscriptSearch` | 7 | 全通过 |

合计 61 个用例。Task 7-10、12 依赖真实 `ChatView` 改线与真实日志，无法在计划
阶段单独跑；它们的断言以结构性不变量为主（挂载数、区间宽度、spacer 高度守恒），
其中 spacer 的账目已用穷举边界验算过（`total` 与「顶 spacer + 挂载数 + 底 spacer
+ 尾部」在所有边界相等，含 `total = 0` / `1` / `win.end === total` 三种退化情形）。

**本轮自检修正的缺陷**（都是写 plan 时想当然造成的，实现本身没错）：

1. `useTranscriptWindow` 的 `merged` 依赖 `limits` 对象身份 —— 调用方传字面量时
   每次渲染都变，导致 `recompute` 身份变、effect 每帧重跑。改为按标量字段 memo。
2. `requestFocus` 走 `setPending` 的 state 往返，跳转要慢一帧；且若调用方没及时
   清空 `focus` prop，每次重算都会重跳同一位置，用户滚不动。改为 ref + 对象身份
   去重（`appliedRef`）。
3. `VisibleRange.first/last` 未定义「相对挂载区」还是「绝对 index」。已在类型注释
   写死：**组件层给相对序号，`useTranscriptWindow` 加 `start` 转绝对**。
4. `spacerBottom` 用了 `items.length - max(win.end, 1)`，尾部那条被多撑了一份高度。
   改为按 `stop = min(end, total - 1)` 计算。
5. Task 7 传 `focus={focusRef.current}` 是渲染期读可变值且恒为 `null`，去掉。
6. `useTranscriptSearch` 的 `onJump` 若进依赖，内联箭头函数会让它**每次渲染都
   重扫全量历史**。改为 ref 持有。
7. 搜索测试原本用 `Array.prototype.findIndex` 计数当「重扫次数」探针，而实现根本
   不调它——探针永远通过。改为用 `onJump` 调用次数作探针。
8. 收了一个用不上的 `textVersion` 参数（会撞 `noUnusedParameters`），删掉。
9. Task 12 原本手写翻页规则，会与生产 `nextWindow` 分叉；改为直接 import 生产
   模块（Node 23 默认开类型剥离，已实测可行），并实测循环收敛：60 → 260 → 400，
   3 帧到 `maxMounted`。

**明确未写进本计划的内容**（各自单列 PR，见 spec §8）：`upsertItems` 的 O(N²)、
`read_log` 的存储放大、桌面端复用 IndexedDB 内容缓存。
