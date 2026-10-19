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

let api: { scrollToBottom: () => void; followIfAtBottom: () => void } | null = null;

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
    api = { scrollToBottom: stick.scrollToBottom, followIfAtBottom: stick.followIfAtBottom };
  }, [stick.scrollToBottom, stick.followIfAtBottom]);
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

  it("degrades gracefully when ResizeObserver is unavailable", () => {
    vi.stubGlobal("ResizeObserver", undefined);
    const { getByTestId } = render(createElement(Harness));
    const el = getByTestId("scroller");
    const state = installMetrics(el, { scrollHeight: 1000, clientHeight: 400, scrollTop: 0 });
    expect(() => act(() => api!.scrollToBottom())).not.toThrow();
    expect(state.scrollTop).toBe(1000);
  });
});
