import { describe, it, expect } from "vitest";
import {
  nextReconnectDelay,
  RECONNECT_BASE_MS,
  RECONNECT_MAX_MS,
} from "./reconnect";

describe("nextReconnectDelay", () => {
  it("follows the exact exponential schedule 500/1000/2000/4000/8000", () => {
    expect([0, 1, 2, 3, 4].map(nextReconnectDelay)).toEqual([500, 1000, 2000, 4000, 8000]);
  });

  it("caps at RECONNECT_MAX_MS once doubling would overshoot", () => {
    // attempt 5 would be 16000 uncapped; everything at or beyond it is 10000.
    expect(nextReconnectDelay(5)).toBe(RECONNECT_MAX_MS);
    expect(nextReconnectDelay(6)).toBe(RECONNECT_MAX_MS);
    expect(nextReconnectDelay(50)).toBe(RECONNECT_MAX_MS);
  });

  it("exposes the base/max constants the App schedules with", () => {
    expect(RECONNECT_BASE_MS).toBe(500);
    expect(RECONNECT_MAX_MS).toBe(10000);
    expect(nextReconnectDelay(0)).toBe(RECONNECT_BASE_MS);
  });

  it("clamps negative attempts to the base delay", () => {
    // A negative counter is a bug, but it must never yield a negative timeout
    // (window.setTimeout treats negatives as 0, i.e. a hot retry loop).
    expect(nextReconnectDelay(-1)).toBe(RECONNECT_BASE_MS);
    expect(nextReconnectDelay(-1000)).toBe(RECONNECT_BASE_MS);
  });

  it("clamps non-finite input instead of returning NaN or Infinity", () => {
    expect(nextReconnectDelay(NaN)).toBe(RECONNECT_BASE_MS);
    expect(nextReconnectDelay(-Infinity)).toBe(RECONNECT_BASE_MS);
    expect(nextReconnectDelay(Infinity)).toBe(RECONNECT_MAX_MS);
  });

  it("clamps huge finite attempts without overflowing", () => {
    expect(nextReconnectDelay(1024)).toBe(RECONNECT_MAX_MS);
    expect(nextReconnectDelay(Number.MAX_SAFE_INTEGER)).toBe(RECONNECT_MAX_MS);
  });

  it("floors fractional attempts so the schedule stays on integers", () => {
    expect(nextReconnectDelay(1.9)).toBe(1000);
    expect(nextReconnectDelay(0.5)).toBe(RECONNECT_BASE_MS);
  });

  it("never returns a value outside [base, max]", () => {
    for (const attempt of [-5, 0, 1, 2, 3, 4, 5, 10, 1e6, NaN, Infinity, -Infinity]) {
      const delay = nextReconnectDelay(attempt);
      expect(delay).toBeGreaterThanOrEqual(RECONNECT_BASE_MS);
      expect(delay).toBeLessThanOrEqual(RECONNECT_MAX_MS);
    }
  });
});
