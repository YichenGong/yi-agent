import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startCameraScan, type ScanDeps } from "./qrScanner";

function fakeVideo(): HTMLVideoElement {
  return { srcObject: null, play: vi.fn(async () => {}) } as unknown as HTMLVideoElement;
}
function fakeStream(): MediaStream {
  const stop = vi.fn();
  return { getTracks: () => [{ stop }] } as unknown as MediaStream;
}
const frame = {} as ImageData;

describe("startCameraScan", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("stops and reports on the first successful decode", async () => {
    const onResult = vi.fn();
    const decode = vi.fn().mockReturnValue("yiagent://pair?x");
    const grabFrame = vi.fn().mockReturnValue(frame);
    const stop = startCameraScan(fakeVideo(), onResult, vi.fn(), {
      getUserMedia: async () => fakeStream(),
      grabFrame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(250);
    expect(onResult).toHaveBeenCalledWith("yiagent://pair?x");
    const callsAfterHit = decode.mock.calls.length;
    await vi.advanceTimersByTimeAsync(600);
    expect(decode.mock.calls.length).toBe(callsAfterHit); // 命中即停
    stop();
  });

  it("keeps scanning until a frame decodes", async () => {
    const onResult = vi.fn();
    let n = 0;
    const decode = vi.fn(() => (++n >= 3 ? "hit" : null));
    startCameraScan(fakeVideo(), onResult, vi.fn(), {
      getUserMedia: async () => fakeStream(),
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(700);
    expect(onResult).toHaveBeenCalledWith("hit");
  });

  it("keeps scanning when the decoder throws", async () => {
    const onResult = vi.fn();
    let n = 0;
    const decode = vi.fn(() => {
      if (++n === 1) throw new Error("boom");
      return n >= 3 ? "hit" : null;
    });
    startCameraScan(fakeVideo(), onResult, vi.fn(), {
      getUserMedia: async () => fakeStream(),
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(700);
    expect(onResult).toHaveBeenCalledWith("hit");
  });

  it("reports an error and does not loop when getUserMedia fails", async () => {
    const onError = vi.fn();
    const decode = vi.fn();
    startCameraScan(fakeVideo(), vi.fn(), onError, {
      getUserMedia: async () => {
        throw new Error("denied");
      },
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(600);
    expect(onError).toHaveBeenCalled();
    expect(decode).not.toHaveBeenCalled();
  });

  it("reports an error instead of throwing when mediaDevices is missing", async () => {
    // iOS 的 WKWebView 在非安全上下文里会把整个 `navigator.mediaDevices` 藏起来
    // （`MediaDevices` 标了 `[SecureContext]`），不是抛权限异常而是属性不存在。
    // 此时默认实现里 `navigator.mediaDevices.getUserMedia` 会**同步**抛 TypeError，
    // 而它落在 `useEffect` 里、`.catch` 又接不住同步异常 → React 卸载整棵树 = 黑屏。
    const onError = vi.fn();
    const original = Object.getOwnPropertyDescriptor(globalThis.navigator, "mediaDevices");
    Object.defineProperty(globalThis.navigator, "mediaDevices", {
      value: undefined,
      configurable: true,
    });
    try {
      // 不注入 `getUserMedia`：走真实默认实现，复现线上那条路径。
      expect(() => startCameraScan(fakeVideo(), vi.fn(), onError)).not.toThrow();
      await vi.advanceTimersByTimeAsync(600);
      expect(onError).toHaveBeenCalled();
    } finally {
      if (original) Object.defineProperty(globalThis.navigator, "mediaDevices", original);
      else delete (globalThis.navigator as { mediaDevices?: unknown }).mediaDevices;
    }
  });

  it("survives a getUserMedia that throws synchronously", async () => {
    // 注入的实现同样可能同步抛（老 webview、polyfill）。同步异常必须被接住并
    // 归到 `onError`，而不是从 `startCameraScan` 逃逸出去。
    const onError = vi.fn();
    expect(() =>
      startCameraScan(fakeVideo(), vi.fn(), onError, {
        getUserMedia: (() => {
          throw new Error("sync boom");
        }) as unknown as ScanDeps["getUserMedia"],
      }),
    ).not.toThrow();
    await vi.advanceTimersByTimeAsync(600);
    expect(onError).toHaveBeenCalled();
  });

  it("reports an error when the video element refuses to play", async () => {
    // `play()` 的 reject 此前被 `void` 丢弃：预览停在第一帧（黑屏）且失败无声。
    const onError = vi.fn();
    const video = {
      srcObject: null,
      play: vi.fn(async () => {
        throw new Error("NotAllowedError");
      }),
    } as unknown as HTMLVideoElement;
    startCameraScan(video, vi.fn(), onError, {
      getUserMedia: async () => fakeStream(),
      grabFrame: () => frame,
      decode: () => null,
    });
    await vi.advanceTimersByTimeAsync(50);
    expect(onError).toHaveBeenCalled();
  });
});
