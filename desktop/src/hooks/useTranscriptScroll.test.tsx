/** @vitest-environment jsdom */
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useTranscriptScroll } from "./useTranscriptScroll";

/**
 * `useTranscriptScroll` 的 DOM 接线层判别性测试。
 *
 * 纯决策逻辑在 `lib/transcriptScroll.test.ts`；这里只钉那些**只有在真实事件流
 * 下才暴露**的问题：程序化写入的回声、监听是否真的注册、owner 是否稳定。
 */

interface Metrics {
  scrollHeight: number;
  clientHeight: number;
  scrollTop: number;
}

/**
 * 挂载滚动容器并装上可读写的滚动度量（jsdom 没有布局引擎）。
 *
 * **先装度量、再挂 ref**：真实浏览器里容器的尺寸本就先于任何监听存在。反过来
 * 的话，监听注册那一刻读到的是空几何（`maxTop = 0`），「是否贴底」会被算成
 * true，所有跟随意图的断言都会失去判别力。
 */
function mount(
  api: { ref: (el: HTMLDivElement | null) => void },
  init: Omit<Metrics, "scrollTop"> & { scrollTop?: number },
): { el: HTMLDivElement; state: Metrics } {
  const el = document.createElement("div");
  document.body.appendChild(el);
  const state: Metrics = {
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
      // 浏览器会 clamp 到 [0, maxTop]。
      state.scrollTop = Math.max(0, Math.min(v, state.scrollHeight - state.clientHeight));
    },
  });
  act(() => {
    api.ref(el);
  });
  return { el, state };
}

const scroll = (el: HTMLElement) => {
  act(() => {
    el.dispatchEvent(new Event("scroll"));
  });
};

describe("useTranscriptScroll: 监听确实注册了", () => {
  it("用户滚离底部后不再跟随", () => {
    // 回归：`ref` 是 callback ref，首帧渲染时节点在 `useEffect` 之后才挂上。
    // effect 只依赖 `ownerKey` 的话，监听与观察器永远不会注册，`scroll` 事件
    // 拿不到、`followingIntent` 停在初始值 true —— 用户上翻历史完全不生效，
    // 每个 delta 都把人拽回底部。
    const { result } = renderHook(() => useTranscriptScroll({ ownerKey: "t1" }));
    const { el, state } = mount(result.current, {
      scrollHeight: 3000,
      clientHeight: 400,
      scrollTop: 2600, // 贴底
    });
    expect(result.current.isFollowing()).toBe(true);

    act(() => {
      state.scrollTop = 1500; // 用户上翻
    });
    scroll(el);

    expect(result.current.isFollowing()).toBe(false);
  });
});

describe("useTranscriptScroll: 自身写入的回声", () => {
  it("程序化写入触发的 scroll 事件不被读成用户接管", () => {
    // 真实浏览器在写完 `scrollTop` 之后会异步派发 scroll，且可能派发多次。
    // 若把它读成用户滚动，`acceptUserScroll` 会清掉阅读锚点，此后锚点上方内容
    // 长高就再也没人纠回原位。这里验证：写到非底部位置后紧接的回声不改变意图。
    const { result } = renderHook(() => useTranscriptScroll({ ownerKey: "t1" }));
    const { el, state } = mount(result.current, {
      scrollHeight: 3000,
      clientHeight: 400,
      scrollTop: 2600,
    });

    // 一次真实程序化写入（`planLayout` 之外的唯一入口）。
    act(() => {
      result.current.scrollToBottom(); // 贴底：位置不变、意图不变
    });
    state.scrollHeight = 5000;
    state.scrollTop = 4600;
    scroll(el);

    // 回声没有把「跟随」误判成其他状态，也没有反过来把用户意图改掉。
    expect(result.current.isFollowing()).toBe(true);
  });
});

describe("useTranscriptScroll: owner 稳定性", () => {
  it("ownerKey 不变时保留已放弃的跟随意图", () => {
    // `ownerKey` 曾经拼进 `textVersion` / `structuralVersion`：流式每个 delta、
    // 每次追加 item 都会换 key，协调器随之重建，`followingIntent` 复位成 true。
    // 用户上翻历史后，只要还有东西流进来就会被判成「在跟随」并拽回底部。
    const { result, rerender } = renderHook(
      ({ key }: { key: string }) => useTranscriptScroll({ ownerKey: key }),
      { initialProps: { key: "t1" } },
    );
    mount(result.current, { scrollHeight: 3000, clientHeight: 400, scrollTop: 500 });

    act(() => {
      result.current.releaseFollow();
    });
    expect(result.current.isFollowing()).toBe(false);

    // 内容继续增长：owner 仍是 t1，协调器不能被重建。
    rerender({ key: "t1" });

    expect(result.current.isFollowing()).toBe(false);
  });

  it("换会话时重建，不继承上一条的跟随意图", () => {
    const { result, rerender } = renderHook(
      ({ key }: { key: string }) => useTranscriptScroll({ ownerKey: key }),
      { initialProps: { key: "t1" } },
    );
    mount(result.current, { scrollHeight: 3000, clientHeight: 400, scrollTop: 500 });

    act(() => {
      result.current.releaseFollow();
    });
    expect(result.current.isFollowing()).toBe(false);

    rerender({ key: "t2" });
    // 新会话默认跟随（冷会话打开时落在最新内容上，除非有入场锚点）。
    expect(result.current.isFollowing()).toBe(true);
  });
});
