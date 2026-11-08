/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach } from "vitest";
import { StrictMode, useCallback, useState } from "react";
import { render, cleanup, act } from "@testing-library/react";
import { useTranscriptWindow } from "./useTranscriptWindow";
import { createHeightBook } from "../lib/heightBook";
import type { FocusRequest, VisibleRange, WindowState } from "../lib/transcriptWindow";

let last: WindowState = { start: 0, end: 0, following: true };
let requestFocus: (f: FocusRequest) => void = () => {};
let nudge: () => void = () => {};
let measure: () => VisibleRange | null = () => null;
let currentFocus: FocusRequest | null = null;

let externalBook: ReturnType<typeof createHeightBook> | null = null;

function Harness({ itemCount, threadId }: { itemCount: number; threadId?: string }) {
  // `book` 必须稳定：内联新建会让它每次渲染都是新对象，`merged`/`recompute`
  // 的身份随之改变，依赖它的 effect 就会每次渲染都重跑。
  const [book] = useState(() => externalBook ?? createHeightBook(40));
  // `measure` 必须是**稳定引用**，与 `ChatView` 里 `useCallback(measureVisible, [])`
  // 一致。若这里每次渲染都新建箭头函数，`recompute` 的身份就会变，依赖它的 effect
  // 每帧重跑——那样即使 hook 根本不支持滚动触发，测试也会「碰巧」通过（重算被
  // 渲染循环免费带起来了）。这个稳定性是本测试断言有效性的前提。
  const measureStable = useCallback(() => measure(), []);
  const state = useTranscriptWindow({
    itemCount,
    threadId,
    measure: measureStable,
    focus: currentFocus,
    book,
  });
  last = state.window;
  requestFocus = state.requestFocus;
  nudge = state.nudge;
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
  externalBook = null;
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
    //
    // `first`/`last` 是**挂载区相对序号**（组件层数不出前面还有多少条），
    // 由 hook 加上 `window.start` 变成绝对 index。这里写 `last.start` 会把
    // 基准重复计一次，虽然当前判定只用 gap、不会失败，但会把错误口径固化下来。
    measure = () => ({
      first: 0,
      last: 10,
      // 有符号：翻页后挂载区顶边在被翻到视口上方 -> 负值；底边远在视口下方 -> 负值。
      gapTopPx: last.start === initialStart ? 0 : -5000,
      gapBottomPx: -5000,
    });
    // 真实场景里「贴顶」是用户滚出来的：滚动事件驱动 `nudge`。
    await act(async () => {
      nudge();
    });
    await tick();
    expect(last.end - last.start).toBe(260);
    expect(last.start).toBe(initialStart - 200);
    expect(last.following).toBe(false);
  });

  it("滚动本身不会重算：必须显式 nudge，否则永远不翻页", async () => {
    render(<Harness itemCount={3000} />);
    await tick();
    const initialStart = last.start;
    measure = () => ({
      first: 0,
      last: 10,
      gapTopPx: last.start === initialStart ? 0 : -5000,
      gapBottomPx: -5000,
    });
    // 只有 `measure` 变了，没有任何 React 依赖变化：不调用 nudge 就不该翻页。
    await tick();
    expect(last.start).toBe(initialStart);

    // 滚动事件到达后才会重算。
    await act(async () => {
      nudge();
    });
    await tick();
    expect(last.start).toBe(initialStart - 200);
  });

  it("nudge 每帧最多触发一次重算", async () => {
    render(<Harness itemCount={3000} />);
    await tick();
    let calls = 0;
    measure = () => {
      calls += 1;
      return null;
    };
    await act(async () => {
      nudge();
      nudge();
      nudge();
    });
    await tick();
    expect(calls).toBe(1);
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

  it("中位数变大后像素阈值跟着变（bufferPx 不能冻结在冷启动值）", async () => {
    // 冷启动 fallback = 40px/条 -> bufferPx = 20*40 = 800。
    // 记录 50 条 400px 的高项后中位数变 400 -> bufferPx = 20*400 = 8000。
    // gap 取 -2000：旧阈值下 -2000 >= -800 不成立，不翻页；新阈值下成立，翻页。
    externalBook = createHeightBook(40);
    const book = externalBook;
    render(<Harness itemCount={3000} />);
    await tick();
    expect(last.end - last.start).toBe(60);

    for (let i = 0; i < 50; i += 1) book.record(`h${i}`, 400);
    expect(book.median()).toBe(400);

    measure = () => ({ first: 0, last: 10, gapTopPx: -2000, gapBottomPx: -5000 });
    await act(async () => {
      nudge();
    });
    await tick();
    expect(last.end - last.start).toBe(260);
    externalBook = null;
  });

  it("measure 返回 null（尚未布局）时保持跟随，不越界", async () => {
    render(<Harness itemCount={10} />);
    await tick();
    expect(last).toEqual({ start: 0, end: 10, following: true });
  });
  it("换了会话就必须重新从尾部开窗，不能继承上一条会话的窗口", async () => {
    // 真实浏览器实测（长会话 1024 items）：在 A 里上翻到历史中间后切到等长的 B，
    // B 打开时窗口停在**同一条中间区间**（following=false），用户看到的是 B 的
    // 中段而不是最新内容。窗口状态是「这次访问的位置」，不属于组件生命周期。
    const { rerender } = render(<Harness itemCount={3000} threadId="A" />);
    await tick();
    expect(last).toEqual({ start: 2940, end: 3000, following: true });

    // 用户在 A 里上翻：窗口扩一页并放弃跟随。
    measure = () => ({ first: 0, last: 10, gapTopPx: 0, gapBottomPx: -5000 });
    await act(async () => {
      nudge();
    });
    await tick();
    const scrolled = { ...last };
    expect(scrolled.following).toBe(false);
    expect(scrolled.start).toBeLessThan(2940);

    // 换个等长会话：必须回到「跟随尾部、只挂首屏」，而不是沿用上面的中间窗口。
    measure = () => null;
    rerender(<Harness itemCount={3000} threadId="B" />);
    await tick();
    expect(last).toEqual({ start: 2940, end: 3000, following: true });

    // 残留的请求会在**下一次重算**时才露头（切会话后的首次渲染看不出问题），
    // 所以必须再推一帧滚动：若 `pendingRef` 没被清掉，B 会被拽到 A 的 index 480。
    measure = () => ({ first: 0, last: 10, gapTopPx: -5000, gapBottomPx: 0 });
    await act(async () => {
      nudge();
    });
    await tick();
    expect(last.start).toBeGreaterThan(2000);
  });

  it("换会话时丢弃上一条会话「已排队但还没跑」的锚点开窗请求", async () => {
    // 真实时序：`requestFocus` 只把请求塞进 ref 并排一帧 rAF。若在这帧跑之前
    // 用户就切走了会话（点击很快、会话缓存命中时不刷新），这条属于 A 的请求
    // 会作用到 B 上：B 一打开就跳到 A 的 index，用户看不到最新内容。
    const { rerender } = render(<Harness itemCount={3000} threadId="A" />);
    await tick();
    expect(last).toEqual({ start: 2940, end: 3000, following: true });

    // 排一次请求，**不推进帧**，立刻切会话。
    await act(async () => {
      requestFocus({ index: 500, before: 20, after: 60 });
    });
    rerender(<Harness itemCount={3000} threadId="B" />);
    await tick();

    expect(last).toEqual({ start: 2940, end: 3000, following: true });
  });

  it("StrictMode 下挂载→卸载→再挂载后仍能翻页（cleanup 必须清掉 frameRef）", async () => {
    // 应用根部是 `<React.StrictMode>`（`main.tsx:15`），开发期每个 effect 都会
    // 跑一遍 挂载→卸载→再挂载。若 cleanup 只 `cancelAnimationFrame` 而不把
    // `frameRef.current` 置回 null，再挂载时 `recompute` 会因「已排帧」永久提前
    // 返回：翻页、focus、nudge 全部失效，而窗口停在冷启动那 60 条上。
    // 不开 StrictMode 的用例结构性地看不见这个缺陷。
    render(
      <StrictMode>
        <Harness itemCount={600} />
      </StrictMode>,
    );
    await tick();
    expect(last.end - last.start).toBe(60);

    // 触顶：必须真的能追加一页。若 frameRef 被作废帧号粘住，这里恒为 60。
    // `gapTopPx: 0` 即「挂载区顶边贴住视口顶」，与真实触顶一致。
    measure = () => ({ first: 0, last: 10, gapTopPx: 0, gapBottomPx: -5000 });
    await act(async () => {
      nudge();
    });
    await tick();
    expect(last.end - last.start).toBe(260);
  });
});
