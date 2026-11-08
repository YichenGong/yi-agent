/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { StrictMode, useCallback, useState } from "react";
import { render, cleanup, act } from "@testing-library/react";
import { useTranscriptWindow } from "./useTranscriptWindow";
import { createHeightBook } from "../lib/heightBook";
import type { FocusRequest, WindowState } from "../lib/transcriptWindow";
import type { WindowMeasurement } from "../lib/transcriptGeometry";

let last: WindowState = { start: 0, end: 0, following: true };
let requestFocus: (f: FocusRequest) => void = () => {};
let nudge: () => void = () => {};
/**
 * 构造一份「历史区边界 + 可选可见行」的测量。
 *
 * 新口径下 `history.first/last` 是**绝对** index，gap 只由历史区行计算：
 * 测试不再自己写相对序号，也就不会再固化成旧的 `window.start + 相对` 口径。
 */
function edge(args: {
  first?: number;
  last?: number;
  gapTopPx: number;
  gapBottomPx: number;
  visible?: { first: number; last: number } | null;
  tailVisible?: boolean;
}): WindowMeasurement {
  const first = args.first ?? 0;
  const last = args.last ?? first + 10;
  return {
    // 三态：`undefined` = 默认「首末行都可视」；显式 `null` = 视口内没有任何行，
    // 必须原样保留。用 `??` 会把 null 也当缺省，令「视口落进 spacer」的用例
    // 静默退化成已覆盖，红证据就消失了。
    visible: args.visible === undefined ? { first, last } : args.visible,
    history: { first, last, gapTopPx: args.gapTopPx, gapBottomPx: args.gapBottomPx },
    tailVisible: args.tailVisible ?? false,
    viewportTop: 0,
    viewportBottom: 800,
  };
}

let measure: () => WindowMeasurement | null = () => null;
let currentFocus: FocusRequest | null = null;

let externalBook: ReturnType<typeof createHeightBook> | null = null;

function Harness({
  itemCount,
  threadId,
  anchorIds,
  following,
}: {
  itemCount: number;
  threadId?: string;
  anchorIds?: readonly string[];
  following?: () => boolean;
}) {
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
    anchorIds,
    following,
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
    // 翻出一页后，新挂载的 200 条落到视口上方：历史区顶边因此远在视口上方
    // （负 gap）。桩必须模拟这件事，否则每帧都会被判「仍在顶端」而无限翻页。
    measure = () =>
      edge({
        // 有符号：翻页后历史顶边在被翻到视口上方 -> 负值；底边远在视口下方 -> 负值。
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
    measure = () => edge({ gapTopPx: last.start === initialStart ? 0 : -5000, gapBottomPx: -5000, });
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

  it("提交后自驱动收敛：视口落进 spacer 时不需要第二次外部 scroll", async () => {
    // 真实缺陷（D02）：`scrollTop=0` 直接落进 41004px 的顶 spacer，窗口仍是
    // [201,600)。此时浏览器已经贴在顶部，不会再产生 scroll 事件；只靠
    // 「滚动事件触发重算」的调度就永远停在这一帧，屏幕全空——用户只能切走再
    // 切回（重挂载强制回尾）才看到内容。修复后必须自己继续开窗，直到覆盖。
    render(<Harness itemCount={3000} />);
    await tick();
    expect(last).toEqual({ start: 2940, end: 3000, following: true });

    // 脚本化几何：第一次测量 = 视口整个落在顶 spacer 里（无相交行）；此后每次
    // 被问到就往前走一页，直到视口落在挂载区中间。方向是「向下」——窗口在
    // 视口上方，需要把 start 往后推，而不是继续向上扩。
    let asked = 0;
    const scripted = (): WindowMeasurement => {
      asked += 1;
      if (asked === 1) {
        return edge({ first: 201, last: 599, gapTopPx: 41004, gapBottomPx: -489396, visible: null });
      }
      const step = asked - 1;
      const start = Math.min(2500, 201 + 200 * step);
      const covered = start <= 2400;
      return edge({
        first: start,
        last: Math.min(start + 399, 2998),
        gapTopPx: covered ? -900 : 41004 - 204 * 200 * step,
        gapBottomPx: covered ? -600000 : -600000,
        visible: covered ? { first: 2450, last: 2470 } : null,
      });
    };
    measure = scripted;
    await act(async () => {
      nudge();
    });
    await tick();
    const afterFirst = { ...last };
    expect(afterFirst.following).toBe(false);

    // **关键断言**：此后不再派发 scroll / nudge，只推进帧。没有自驱动调度时
    // `asked` 会停在第一次（这就是修复前的行为）。
    await tick(10);
    expect(asked).toBeGreaterThan(1);
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

    measure = () => edge({ gapTopPx: -2000, gapBottomPx: -5000 });
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
    measure = () => edge({ gapTopPx: 0, gapBottomPx: -5000 });
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
    measure = () => edge({ gapTopPx: -5000, gapBottomPx: 0 });
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
    measure = () => edge({ gapTopPx: 0, gapBottomPx: -5000 });
    await act(async () => {
      nudge();
    });
    await tick();
    expect(last.end - last.start).toBe(260);
  });
});

describe("useTranscriptWindow: 视口落在未挂载区间", () => {
  it("stalled 时按估算位置发一次 focus，而不是放弃", async () => {
    // 场景：长会话里用户直接把滚动条拖到中段，视口落在**未挂载的 spacer** 里。
    // 此刻 `measureTranscript` 的 `history` 为 null（没有任何历史行与视口相交），
    // `estimateFocusIndex` 无从估算，旧实现只能 `stalled` —— 用户看到一片空白，
    // 而且不滚动就不会有任何东西再来救（Spec §4.4 第 6 条）。
    //
    // 正确行为：用「当前 scrollTop 对应哪一条」的估算索引发**一次** focus，
    // 把那一窗挂上来。
    const book = createHeightBook();
    // 每条 100px，3000 条；视口在 60_000px 处 ≈ 第 600 条。
    for (let i = 0; i < 3000; i += 1) book.record(`m${i}`, 100);
    externalBook = book;
    render(<Harness itemCount={3000} anchorIds={Array.from({ length: 3000 }, (_, i) => `m${i}`)} />);
    await tick();

    // 视口在 spacer 里：没有任何行相交，history 为 null。
    measure = () =>
      ({
        visible: null,
        history: null,
        tailVisible: false,
        viewportTop: 60_000,
        viewportBottom: 60_800,
      }) as WindowMeasurement;

    await act(async () => {
      nudge();
    });
    await tick(6);

    // 窗口必须被搬到估算位置附近（第 600 条左右），而不是停在尾部首屏。
    const expected = 600;
    expect(last.start).toBeLessThanOrEqual(expected);
    expect(last.end).toBeGreaterThanOrEqual(expected);
    // 不应报「无法显示」——那会让用户以为内容坏了。
    expect(last.following).toBe(false);
  });
});

describe("useTranscriptWindow: 页面不可见时暂停调度", () => {
  it("hidden 期间不排帧，可见后由 visibilitychange 自动恢复", async () => {
    // Spec A2：滚动/提交触发的续测只在页面可见时有意义。标签页在后台时 rAF 被
    // 浏览器节流甚至完全停发，却仍可能在切回时一次性连跑多帧，把已经过期的
    // 窗口结论应用到新布局上。正确行为：不可见时**不排帧**，切回可见时补一次。
    const book = createHeightBook();
    for (let i = 0; i < 3000; i += 1) book.record(`m${i}`, 100);
    externalBook = book;
    render(<Harness itemCount={3000} />);
    await tick();

    const hidden = vi.spyOn(document, "visibilityState", "get");
    try {
      hidden.mockReturnValue("hidden");
      const before = last.start;
      measure = () => edge({ first: before, last: before + 5, gapTopPx: 0, gapBottomPx: -5000 });
      await act(async () => {
        nudge();
      });
      await tick(4);
      // 不可见时不推进。
      expect(last.start).toBe(before);

      // 切回可见：不需要任何外部 scroll，也要自己续上。
      hidden.mockReturnValue("visible");
      await act(async () => {
        document.dispatchEvent(new Event("visibilitychange"));
      });
      await tick(6);
      expect(last.start).toBeLessThan(before);
    } finally {
      hidden.mockRestore();
    }
  });
});

describe("useTranscriptWindow: 贴底意图要让窗口回到尾部", () => {
  it("用户拖到底部而窗口还停在中段时，窗口必须收回尾部", async () => {
    // 真实回归（D03）：读者把滚动条一路拖到底（`following` 意图为真），但窗口
    // 仍停在早先 `focus(100)` 开出来的中段区间。此刻视口落在**下方 spacer** 里，
    // 于是屏幕空白；而「跟随中不补页」分支直接判 `settled`，连补页都不做，
    // 窗口永远停在原处——不滚动就一直是空白。
    //
    // 正确行为：贴底意图为真且视口没被覆盖时，窗口本身就应当回到尾部
    // （`following: true` 的窗口），而不是靠补页无限往后追。
    let following = false;
    const book = createHeightBook();
    for (let i = 0; i < 3000; i += 1) book.record(`m${i}`, 100);
    externalBook = book;
    render(<Harness itemCount={3000} following={() => following} />);
    await tick();

    // 先往前开一窗，制造「窗口在中段」的状态。
    measure = () => edge({ first: 100, last: 105, gapTopPx: -100, gapBottomPx: -5000 });
    await act(async () => {
      requestFocus({ index: 100, before: 20, after: 60 });
    });
    await tick(4);
    expect(last.start).toBeLessThan(200);
    expect(last.following).toBe(false);

    // 用户拖到底：意图变成贴底，但视口没有任何相交行（落在下方 spacer 里）。
    following = true;
    measure = () =>
      ({
        visible: null,
        history: { first: 0, last: 300, gapTopPx: -500_000, gapBottomPx: 500_000 },
        tailVisible: false,
        viewportTop: 500_000,
        viewportBottom: 500_800,
      }) as WindowMeasurement;
    await act(async () => {
      nudge();
    });
    await tick(6);

    // 窗口必须回到尾部（跟随态），覆盖视口可见的末尾内容。
    expect(last.following).toBe(true);
    expect(last.end).toBe(3000);
  });
});
