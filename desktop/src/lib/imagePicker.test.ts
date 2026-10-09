/** @vitest-environment jsdom */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PICKER_FOCUS_FALLBACK_MS, pickImageFiles } from "./imagePicker";

/**
 * 远端（iOS）图片选择器。
 *
 * 这里只管它自己的 DOM 生命周期：选中的文件要对、`input` 用完即摘、`window` 上
 * 那条 focus 兜底监听不能赖着不走。用假定时器把「兜底窗口」快进过去，才能断言
 * 清理是**定时**发生的（而不是碰巧被某个事件带走）。
 */

/** 当前挂在 `window` 上的 focus 监听数（afterEach 会加一层计数包装）。 */
function focusListenerCount(): number {
  return (window as unknown as { __focusListeners?: number }).__focusListeners ?? 0;
}

describe("pickImageFiles", () => {
  let realAdd: typeof window.addEventListener;
  let realRemove: typeof window.removeEventListener;

  beforeEach(() => {
    vi.useFakeTimers();
    realAdd = window.addEventListener.bind(window);
    realRemove = window.removeEventListener.bind(window);
    (window as unknown as { __focusListeners?: number }).__focusListeners = 0;
    // 计数包装：读的是「有没有摘干净」，不关心回调是谁。
    vi.spyOn(window, "addEventListener").mockImplementation((type, listener, opts) => {
      if (type === "focus") {
        const c = window as unknown as { __focusListeners?: number };
        c.__focusListeners = (c.__focusListeners ?? 0) + 1;
      }
      realAdd(type, listener, opts);
    });
    vi.spyOn(window, "removeEventListener").mockImplementation((type, listener, opts) => {
      if (type === "focus") {
        const c = window as unknown as { __focusListeners?: number };
        c.__focusListeners = Math.max(0, (c.__focusListeners ?? 0) - 1);
      }
      realRemove(type, listener, opts);
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
    document.body.innerHTML = "";
  });

  function inputInDom(): HTMLInputElement | null {
    return document.body.querySelector('input[type="file"]');
  }

  /** 让 `input.click()` 同步派发一个带文件的 `change`（系统选择器选完了）。 */
  function clickSelects(files: File[]) {
    vi.spyOn(HTMLInputElement.prototype, "click").mockImplementation(function (this: HTMLInputElement) {
      Object.defineProperty(this, "files", { value: files, configurable: true });
      this.dispatchEvent(new Event("change"));
    });
  }

  it("window focus 兜底监听在兜底窗口内被摘掉（picker 一个事件都不回也不泄漏）", async () => {
    vi.spyOn(HTMLInputElement.prototype, "click").mockImplementation(() => {});
    pickImageFiles();
    expect(focusListenerCount()).toBe(1);

    // 快进整个兜底窗口：监听必须已经摘掉，否则它会挂到会话结束。
    await vi.advanceTimersByTimeAsync(PICKER_FOCUS_FALLBACK_MS);

    expect(focusListenerCount()).toBe(0);
    // 摘掉兜底不等于放弃这次选择：input 仍在文档里，晚到的 change 还能收尾。
    expect(inputInDom()).not.toBeNull();
  });

  it("change 到手即收尾：resolve 选中的文件，并摘掉 input 与 focus 兜底", async () => {
    const file = new File([new Uint8Array([1, 2, 3])], "a.png", { type: "image/png" });
    clickSelects([file]);

    const files = await pickImageFiles();

    expect(files).toEqual([file]);
    expect(inputInDom()).toBeNull();
    expect(focusListenerCount()).toBe(0);
  });

  it("老 webview 上窗口重新获焦即收尾（取消路径，读到空列表）", async () => {
    vi.spyOn(HTMLInputElement.prototype, "click").mockImplementation(() => {});
    const pending = pickImageFiles();
    expect(focusListenerCount()).toBe(1);

    window.dispatchEvent(new Event("focus"));
    await vi.advanceTimersByTimeAsync(0);

    await expect(pending).resolves.toEqual([]);
    expect(inputInDom()).toBeNull();
    expect(focusListenerCount()).toBe(0);
  });

  it("cancel 事件即取消（Safari 16.4+），不留下监听", async () => {
    vi.spyOn(HTMLInputElement.prototype, "click").mockImplementation(() => {});
    const pending = pickImageFiles();

    inputInDom()!.dispatchEvent(new Event("cancel"));
    await expect(pending).resolves.toEqual([]);
    expect(inputInDom()).toBeNull();
    expect(focusListenerCount()).toBe(0);
  });
});
