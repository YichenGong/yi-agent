import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { createWatchdog, SYNC_IDLE_TIMEOUT_MS } from "./syncWatchdog";

const T = 1000;

describe("createWatchdog", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("fires after the timeout with no activity", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    expect(wd.fired).toBe(false);
    vi.advanceTimersByTime(T - 1);
    expect(onTimeout).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(onTimeout).toHaveBeenCalledTimes(1);
    expect(wd.fired).toBe(true);
  });

  it("touch resets the deadline so a long replay never trips it", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    // 每半个超时续一次命，共续 6 次（跨度 3T）：总时长远超 T，但不该触发。
    for (let i = 0; i < 6; i += 1) {
      vi.advanceTimersByTime(T / 2);
      wd.touch();
    }
    expect(onTimeout).not.toHaveBeenCalled();
  });

  it("stop prevents a late fire after the resume settles", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    vi.advanceTimersByTime(T / 2);
    wd.stop();
    vi.advanceTimersByTime(T * 10);
    expect(onTimeout).not.toHaveBeenCalled();
    expect(wd.fired).toBe(false);
  });

  it("touch after fire is a no-op (no double report)", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    vi.advanceTimersByTime(T);
    expect(onTimeout).toHaveBeenCalledTimes(1);
    wd.touch();
    vi.advanceTimersByTime(T * 5);
    expect(onTimeout).toHaveBeenCalledTimes(1);
  });

  it("touch after stop does not resurrect the timer", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    wd.stop();
    wd.touch();
    vi.advanceTimersByTime(T * 5);
    expect(onTimeout).not.toHaveBeenCalled();
  });

  it("stop after fire does not throw", () => {
    const onTimeout = vi.fn();
    const wd = createWatchdog(T, onTimeout);
    vi.advanceTimersByTime(T);
    expect(() => wd.stop()).not.toThrow();
  });

  it("defaults to a 15s idle threshold", () => {
    expect(SYNC_IDLE_TIMEOUT_MS).toBe(15_000);
  });
});
