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
let observers: Array<{ cb: RoCallback; targets: Element[] }> = [];
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

let api: {
  scrollToBottom: () => void;
  followIfAtBottom: () => void;
  releaseFollow: () => void;
} | null = null;

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
    api = {
      scrollToBottom: stick.scrollToBottom,
      followIfAtBottom: stick.followIfAtBottom,
      releaseFollow: stick.releaseFollow,
    };
  }, [stick.scrollToBottom, stick.followIfAtBottom, stick.releaseFollow]);
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

/**
 * 与 `installMetrics` 相同，但**按真实布局语义夹取 `scrollTop`**。
 *
 * 真实浏览器里 `scrollTop` 永远落在 `[0, scrollHeight - clientHeight]`：写
 * `scrollTop = scrollHeight` 实际只会得到当时的最大值。这就是「贴底」之后再
 * 长高内容时 `gap` 会变成增量，而不是 0 的原因——用不夹取的替身会把这个缺陷
 * 掩盖掉（测试恒绿，线上恒错）。
 */
function installClampedMetrics(
  el: HTMLElement,
  init: { scrollHeight: number; clientHeight: number; scrollTop?: number },
) {
  const state = {
    scrollHeight: init.scrollHeight,
    clientHeight: init.clientHeight,
    scrollTop: init.scrollTop ?? 0,
  };
  const maxTop = () => Math.max(0, state.scrollHeight - state.clientHeight);
  Object.defineProperty(el, "scrollHeight", { configurable: true, get: () => state.scrollHeight });
  Object.defineProperty(el, "clientHeight", { configurable: true, get: () => state.clientHeight });
  Object.defineProperty(el, "scrollTop", {
    configurable: true,
    get: () => state.scrollTop,
    set: (v: number) => {
      state.scrollTop = Math.min(Math.max(0, v), maxTop());
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

  it("does not follow content growth once the user scrolled up", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 100 });
    fireEvent.scroll(el); // user is reading history
    // Content grew (a streamed delta): the viewport must stay put.
    state.scrollHeight = 1200;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(100);
  });

  it("follows content growth while the user is still at the bottom", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el); // user is at the bottom
    state.scrollHeight = 1200;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(1200);
  });

  /**
   * 真实浏览器实测（Chrome，真实布局）：`scrollTop = scrollHeight` 写入后，浏览器
   * 在**后续的 scroll steps** 才派发 `scroll` 事件；若这之前内容又长高了（流式
   * throttle 窗口内新增的文本），事件送达时 `gap` 已经是新增的高度（实测 126px）。
   * 用当下的绝对 gap 重算意图会把「我们自己刚贴的底」判成「用户上翻」，跟随就此
   * 永久关闭——正是「生成到某处停住、后面内容继续长但视口不贴底」的现场。
   */
  it("keeps following when the scroll event lands after the content already grew", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installClampedMetrics(el, {
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 600,
    });
    fireEvent.scroll(el); // 用户贴底
    act(() => api!.scrollToBottom()); // 我们写 scrollTop = scrollHeight -> 夹到 600
    expect(state.scrollTop).toBe(600);
    // 内容在事件送达之前增长：scrollHeight 1000 -> 1726，scrollTop 仍停在 600。
    state.scrollHeight = 1726;
    fireEvent.scroll(el); // 浏览器派发的 scroll 事件，此刻 gap = 726
    // 意图必须仍是「贴底」：这次增长来自我们自己贴底之后的内容，不是用户上翻。
    state.scrollHeight = 2000;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(1600);
  });

  it("still stops following when the user actually scrolls up", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installClampedMetrics(el, {
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 600,
    });
    fireEvent.scroll(el); // 贴底
    // 用户真的往上滑了：scrollTop 变小、内容高度没变。
    state.scrollTop = 100;
    fireEvent.scroll(el);
    state.scrollHeight = 1200;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(100);
  });

  it("accumulates sub-pixel upward steps so a slow scroll-up still releases follow", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installClampedMetrics(el, {
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 600,
    });
    fireEvent.scroll(el); // 贴底
    // 触控板慢速上滑：每个 scroll 事件只移动 0.4px，低于噪声下限。
    // 累计 8px 后必须越过 4px 容差、真的放弃跟随；若把基线每次重置到当前值，
    // 每步的 0.4px 都低于下限，就永远触发不了上翻。
    for (let i = 1; i <= 20; i += 1) {
      state.scrollTop = 600 - i * 0.4;
      fireEvent.scroll(el);
    }
    state.scrollHeight = 1400;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBeCloseTo(592, 1);
  });

  it("does not release follow on a small upward jitter that is still inside the threshold", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installClampedMetrics(el, {
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 600,
    });
    fireEvent.scroll(el); // 贴底
    // 行高取整 / 图片解码 / 上方内容重排都可能让 scrollTop 突然少 3px——
    // 这仍在 4px 容差内，用户没有表达任何上翻意图。
    state.scrollTop = 597;
    fireEvent.scroll(el);
    state.scrollHeight = 1400;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(1000); // 必须继续跟随
  });

  it("ignores sub-pixel jitter while still at the bottom", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installClampedMetrics(el, {
      scrollHeight: 1000,
      clientHeight: 400,
      scrollTop: 600,
    });
    fireEvent.scroll(el); // 贴底
    // 设备像素比带来的浮点抖动：0.5px 级别、来回跳。
    state.scrollTop = 599.5;
    fireEvent.scroll(el);
    state.scrollTop = 600;
    fireEvent.scroll(el);
    state.scrollTop = 599.6;
    fireEvent.scroll(el);
    state.scrollHeight = 1400;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(1000); // 仍在跟随，贴到新的底部
  });

  it("forces the bottom even when the user scrolled up (explicit intent)", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 100 });
    fireEvent.scroll(el); // user is reading history
    // Sending a message / switching threads is an explicit request to see the
    // newest content, so the command must override the scrolled-up state.
    act(() => api!.scrollToBottom());
    expect(state.scrollTop).toBe(1000);
    // ...and the next growth keeps following without another explicit call.
    state.scrollHeight = 1200;
    act(() => api!.followIfAtBottom());
    expect(state.scrollTop).toBe(1200);
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
    // 同一帧内两次尺寸变化只应留下**一个**待执行的 rAF（第二次取消了第一次）。
    expect(rafCallbacks.size).toBe(1);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(1000);
  });

  it("abandons the scheduled pin when the user scrolls up before the frame runs", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el); // user is at the bottom
    fireResize(400);
    state.clientHeight = 300;
    fireResize(300); // schedules the pin for the next frame
    expect(rafCallbacks.size).toBe(1);
    // 键盘收起 / `dvh` 动画期间：用户在这一帧内上翻历史（贴底判定在 rAF 里重读）。
    state.scrollTop = 100;
    fireEvent.scroll(el);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(100);
  });

  it("stops reacting after unmount", () => {
    const { getByTestId, unmount } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el);
    fireResize(400);
    unmount();
    // 卸载时观察者被断开（fake 的 `disconnect()` 会清空 `targets`）。
    expect(observers[0].targets).toEqual([]);
    state.clientHeight = 300;
    fireResize(300);
    act(() => flushRaf());
    expect(state.scrollTop).toBe(600);
  });

  it("ignores a resize that reports the same height", () => {
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 600 });
    fireEvent.scroll(el);
    fireResize(400); // baseline
    fireResize(400); // same height again: not a change
    expect(rafCallbacks.size).toBe(0);
    expect(state.scrollTop).toBe(600);
  });

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

  it("degrades gracefully when ResizeObserver is unavailable", () => {
    vi.stubGlobal("ResizeObserver", undefined);
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 0 });
    expect(() => act(() => api!.scrollToBottom())).not.toThrow();
    expect(state.scrollTop).toBe(1000);
  });
});
