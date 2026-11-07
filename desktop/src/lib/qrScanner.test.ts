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

/** 本文件局部的可控 Promise；后续组件测试各自声明，不抽成生产工具。 */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

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

  // ---- 扫描结果接收契约（A06/A09/A10/A17）------------------------------

  it("clears srcObject on acceptance when the stream is ours", async () => {
    const stream = {
      getTracks: () => [{ stop: vi.fn() }],
    } as unknown as MediaStream;
    const video = fakeVideo();
    startCameraScan(video, () => true, vi.fn(), {
      getUserMedia: async () => stream,
      grabFrame: () => frame,
      decode: () => "text",
    });
    await vi.advanceTimersByTimeAsync(200);
    expect(video.srcObject).toBeNull();
  });

  it("A06 keeps rejected text alive and stops on acceptance", async () => {
    const trackStop = vi.fn();
    const stream = { getTracks: () => [{ stop: trackStop }] } as unknown as MediaStream;
    const result = vi
      .fn<(text: string) => boolean | void>()
      .mockReturnValueOnce(false)
      .mockReturnValueOnce(true);
    const decode = vi.fn(() => "text");
    const stop = startCameraScan(fakeVideo(), result, vi.fn(), {
      getUserMedia: async () => stream,
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(200);
    expect(result).toHaveBeenCalledTimes(1);
    expect(trackStop).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(200);
    expect(result).toHaveBeenCalledTimes(2);
    expect(trackStop).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(600);
    expect(decode).toHaveBeenCalledTimes(2);
    stop();
    expect(trackStop).toHaveBeenCalledTimes(1);
  });

  it("A09 releases late streams without assigning video", async () => {
    const pending = deferred<MediaStream>();
    const stops = [vi.fn(), vi.fn()];
    const stream = {
      getTracks: () => stops.map((stop) => ({ stop })),
    } as unknown as MediaStream;
    const video = fakeVideo();
    const before = video.srcObject;
    const result = vi.fn();
    const error = vi.fn();
    const stop = startCameraScan(video, result, error, {
      getUserMedia: () => pending.promise,
    });
    stop();
    pending.resolve(stream);
    await vi.advanceTimersByTimeAsync(400);
    for (const s of stops) expect(s).toHaveBeenCalledTimes(1);
    expect(video.srcObject).toBe(before);
    expect(video.play).not.toHaveBeenCalled();
    expect(result).not.toHaveBeenCalled();
    expect(error).not.toHaveBeenCalled();
  });

  it.each([
    ["get-stop-reject", 0],
    ["play-stop-reject", 0],
    ["play-accept-reject", 0],
    ["get-current-reject", 1],
    ["play-current-reject", 1],
    ["play-sync-throw", 1],
    ["result-sync-throw", 1],
  ] as const)("A10/A17 %s errors=%s", async (scenario, expectedErrors) => {
    const pendingStream = deferred<MediaStream>();
    const pendingPlay = deferred<void>();
    const trackStop = vi.fn();
    const stream = { getTracks: () => [{ stop: trackStop }] } as unknown as MediaStream;
    const error = vi.fn();
    const decode = vi.fn(() => "hit");
    const result = vi.fn(() => {
      if (scenario === "result-sync-throw") throw new Error("result failed");
      return scenario === "play-accept-reject";
    });
    const video = fakeVideo();
    video.play = vi.fn(() => {
      if (scenario === "play-sync-throw") throw new Error("play failed");
      return pendingPlay.promise;
    });
    const usesPendingStream = scenario.startsWith("get-");
    const stop = startCameraScan(video, result, error, {
      getUserMedia: () => (usesPendingStream ? pendingStream.promise : Promise.resolve(stream)),
      grabFrame: () => frame,
      decode,
    });
    await vi.advanceTimersByTimeAsync(0);
    if (scenario === "get-stop-reject") {
      stop();
      pendingStream.reject(new Error("old get"));
    }
    if (scenario === "get-current-reject") pendingStream.reject(new Error("current get"));
    if (scenario === "play-stop-reject") {
      stop();
      pendingPlay.reject(new Error("old play"));
    }
    if (scenario === "play-current-reject") pendingPlay.reject(new Error("current play"));
    if (scenario === "play-accept-reject") {
      await vi.advanceTimersByTimeAsync(200);
      expect(result).toHaveBeenCalledTimes(1);
      pendingPlay.reject(new Error("accepted scan play"));
    }
    await vi.advanceTimersByTimeAsync(400);
    expect(error).toHaveBeenCalledTimes(expectedErrors);
    expect(trackStop).toHaveBeenCalledTimes(usesPendingStream ? 0 : 1);
    const calls = decode.mock.calls.length;
    await vi.advanceTimersByTimeAsync(600);
    expect(decode).toHaveBeenCalledTimes(calls);
    stop();
  });
});
