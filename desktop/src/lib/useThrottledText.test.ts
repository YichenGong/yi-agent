/** @vitest-environment jsdom */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useThrottledText, THROTTLE_MS } from "./useThrottledText";

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("useThrottledText", () => {
  it("returns the first value immediately", () => {
    const { result } = renderHook(({ text }) => useThrottledText(text, THROTTLE_MS), {
      initialProps: { text: "a" },
    });
    expect(result.current).toBe("a");
  });

  it("collapses rapid updates into one, then flushes the last one (trailing edge)", () => {
    const { result, rerender } = renderHook(
      ({ text }) => useThrottledText(text, THROTTLE_MS),
      { initialProps: { text: "" } },
    );
    // Ten deltas inside one window.
    for (let i = 1; i <= 10; i += 1) {
      rerender({ text: "x".repeat(i) });
    }
    // The FIRST change renders immediately (leading edge), so the reply starts
    // appearing at once instead of waiting out a whole window; the remaining
    // nine collapse into one trailing flush. Ten deltas ⇒ two renders.
    expect(result.current).toBe("x");
    act(() => {
      vi.advanceTimersByTime(THROTTLE_MS);
    });
    // Trailing edge: the LAST value is shown, not the first.
    expect(result.current).toBe("x".repeat(10));
  });

  it("does not swallow the final value when the stream ends mid-window", () => {
    const { result, rerender } = renderHook(
      ({ text }) => useThrottledText(text, THROTTLE_MS),
      { initialProps: { text: "" } },
    );
    rerender({ text: "final answer" });
    // The authoritative `item/completed` text must render even though the
    // window has not elapsed — a swallowed final value is a correctness bug.
    expect(result.current).toBe("final answer");
  });
});
