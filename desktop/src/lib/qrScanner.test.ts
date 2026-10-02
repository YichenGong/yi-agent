import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { startCameraScan } from "./qrScanner";

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
});
