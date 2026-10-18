/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook, act, cleanup } from "@testing-library/react";
import { useRef } from "react";
import { useDragDropPaths, dragHitsTarget, type DragDropPayload } from "./useDragDropPaths";

afterEach(cleanup);

/** 造一个带固定 rect 的元素，模拟 jsdom 里"落在卡片上"的几何。 */
function elWithRect(rect: { left: number; top: number; right: number; bottom: number }) {
  const el = document.createElement("div");
  el.getBoundingClientRect = () => ({ ...rect, width: rect.right - rect.left, height: rect.bottom - rect.top, x: rect.left, y: rect.top, toJSON: () => ({}) }) as DOMRect;
  document.body.appendChild(el);
  return el;
}

/** 假订阅：捕获 handler 供测试直接喂事件，并记录 unlisten 是否被调用。 */
function fakeSubscribe() {
  const handlerBox: { fn: ((e: DragDropPayload) => void) | null } = { fn: null };
  const unlisten = vi.fn();
  const subscribe = async (handler: (e: DragDropPayload) => void) => {
    handlerBox.fn = handler;
    return unlisten;
  };
  return { subscribe, handlerBox, unlisten };
}

function renderHookWithTarget(opts: {
  isRemote: boolean;
  onDrop: (paths: string[]) => void;
  rect?: { left: number; top: number; right: number; bottom: number };
}) {
  const el = elWithRect(opts.rect ?? { left: 0, top: 0, right: 100, bottom: 100 });
  const sub = fakeSubscribe();
  const view = renderHook(() => {
    const ref = useRef<HTMLElement | null>(el);
    return useDragDropPaths({
      subscribe: sub.subscribe,
      isRemote: opts.isRemote,
      targetRef: ref,
      onDrop: opts.onDrop,
    });
  });
  return { ...sub, el, view };
}

describe("dragHitsTarget", () => {
  it("物理像素除以 devicePixelRatio 后落在矩形内即为命中", () => {
    const el = elWithRect({ left: 10, top: 20, right: 110, bottom: 120 });
    // ratio=2：物理 (120, 100) → CSS (60, 50)，在矩形内。
    expect(dragHitsTarget(el, { x: 120, y: 100 }, 2)).toBe(true);
  });

  it("右/下边界为开区间，左上边界为闭区间", () => {
    const el = elWithRect({ left: 10, top: 20, right: 110, bottom: 120 });
    expect(dragHitsTarget(el, { x: 110, y: 20 }, 1)).toBe(false); // 右边界外
    expect(dragHitsTarget(el, { x: 10, y: 20 }, 1)).toBe(true); // 左上角计入
  });

  it("元素为 null 时不命中", () => {
    expect(dragHitsTarget(null, { x: 0, y: 0 }, 1)).toBe(false);
  });
});

describe("useDragDropPaths", () => {
  it("enter 落在卡片上时 active 为真", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    expect(view.result.current.active).toBe(false);
    act(() => handlerBox.fn!({ type: "enter", paths: ["/a"], position: { x: 50, y: 50 } }));
    expect(view.result.current.active).toBe(true);
  });

  it("over 移出卡片时 active 转假", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "over", position: { x: 50, y: 50 } }));
    expect(view.result.current.active).toBe(true);
    act(() => handlerBox.fn!({ type: "over", position: { x: 500, y: 500 } }));
    expect(view.result.current.active).toBe(false);
  });

  it("drop 落在卡片上时回传 paths 并清掉高亮", async () => {
    const onDrop = vi.fn();
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "drop", paths: ["/a", "/b"], position: { x: 50, y: 50 } }));
    expect(onDrop).toHaveBeenCalledWith(["/a", "/b"]);
    expect(view.result.current.active).toBe(false);
  });

  it("drop 落在卡片之外时什么都不做", async () => {
    const onDrop = vi.fn();
    const { handlerBox } = renderHookWithTarget({ isRemote: false, onDrop });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "drop", paths: ["/a"], position: { x: 900, y: 900 } }));
    expect(onDrop).not.toHaveBeenCalled();
  });

  it("drop 的 paths 为空时不回调", async () => {
    const onDrop = vi.fn();
    const { handlerBox } = renderHookWithTarget({ isRemote: false, onDrop });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "drop", paths: [], position: { x: 50, y: 50 } }));
    expect(onDrop).not.toHaveBeenCalled();
  });

  it("leave 清掉高亮", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    act(() => handlerBox.fn!({ type: "enter", paths: [], position: { x: 50, y: 50 } }));
    expect(view.result.current.active).toBe(true);
    act(() => handlerBox.fn!({ type: "leave" }));
    expect(view.result.current.active).toBe(false);
  });

  it("远端（iOS）完全不订阅", async () => {
    const { handlerBox, view } = renderHookWithTarget({ isRemote: true, onDrop: vi.fn() });
    await act(async () => {});
    expect(handlerBox.fn).toBeNull();
    expect(view.result.current.active).toBe(false);
  });

  it("卸载时调用 unlisten", async () => {
    const { unlisten, view } = renderHookWithTarget({ isRemote: false, onDrop: vi.fn() });
    await act(async () => {});
    view.unmount();
    expect(unlisten).toHaveBeenCalled();
  });
});
