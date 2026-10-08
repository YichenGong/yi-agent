/** @vitest-environment jsdom */
import { createElement } from "react";
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, act } from "@testing-library/react";
import { useLongPress, LONG_PRESS_MS, type LongPressTarget } from "./useLongPress";
import { resolveSidebarPressTarget } from "./sidebarPress";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

/**
 * 测试夹具：把 hook 挂在一个容器上，容器里有「会话行」和「组头」两个可长按元素。
 * `onLongPress` 记录落点，`resolver` 复用与 ThreadSidebar 相同的 dataset 读法。
 *
 * 注：本文件是 `.ts`（plan 指定的文件名，也是验收命令里的路径），esbuild 的 `ts`
 * loader 不接受 JSX 语法，故这里的 UI 用 `createElement` 表达（等价于 JSX）。
 */
function Harness({
  enabled = true,
  onLongPress,
}: {
  enabled?: boolean;
  onLongPress: (t: LongPressTarget) => void;
}) {
  const lp = useLongPress({
    enabled,
    resolveTarget: (e) => {
      // 走生产同一套落点判定：行内交互控件（button/input/a…）一律不算长按。
      const el = resolveSidebarPressTarget(e.target as Element);
      if (!el) return null;
      if (el.dataset.threadRow !== undefined) return { kind: "thread", id: el.dataset.threadRow, el };
      return { kind: "group", ws: el.dataset.wsHeader ?? "", el };
    },
    onLongPress,
  });
  return createElement(
    "div",
    { ...lp },
    createElement(
      "div",
      { "data-ws-header": "/work/projA" },
      createElement("span", { "data-testid": "header-text" }, "projA"),
    ),
    createElement(
      "div",
      { "data-thread-row": "t1" },
      createElement("span", { "data-testid": "row-text" }, "alpha"),
      createElement("button", { "data-testid": "pin" }, "pin"),
    ),
  );
}

const pressDown = (el: Element, extra: Record<string, unknown> = {}) =>
  fireEvent.pointerDown(el, { clientX: 5, clientY: 5, button: 0, ...extra });

const advance = async (ms: number) => {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
};

describe("useLongPress", () => {
  it("fires once after the threshold, on a thread row", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("row-text"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress).toHaveBeenCalledTimes(1);
    expect(onLongPress.mock.calls[0][0]).toMatchObject({ kind: "thread", id: "t1" });
  });

  it("fires on a workspace header", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("header-text"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress.mock.calls[0][0]).toMatchObject({ kind: "group", ws: "/work/projA" });
  });

  it("does not fire on a short press", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("row-text"));
    fireEvent.pointerUp(window, { pointerId: 0 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("cancels when the finger moves past the tolerance", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("row-text"));
    fireEvent.pointerMove(window, { pointerId: 0, clientX: 100, clientY: 100 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("cancels on pointercancel (the system's scroll takeover)", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("row-text"));
    fireEvent.pointerCancel(window, { pointerId: 0 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("ignores a second finger", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("row-text"));
    fireEvent.pointerDown(window, { pointerId: 7, clientX: 5, clientY: 5 });
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("swallows the click that follows a long press", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const onSelect = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    // 用行自己的 onClick 验证「吞 click」：长按后紧跟的 click 不能到 React。
    getByTestId("row-text").closest("[data-thread-row]")!.addEventListener("click", onSelect);
    pressDown(getByTestId("row-text"));
    await advance(LONG_PRESS_MS);
    fireEvent.click(getByTestId("row-text"));
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("stays inert when disabled", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { enabled: false, onLongPress }));
    pressDown(getByTestId("row-text"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });

  it("does not start from an interactive child", async () => {
    vi.useFakeTimers();
    const onLongPress = vi.fn();
    const { getByTestId } = render(createElement(Harness, { onLongPress }));
    pressDown(getByTestId("pin"));
    await advance(LONG_PRESS_MS);
    expect(onLongPress).not.toHaveBeenCalled();
  });
});
