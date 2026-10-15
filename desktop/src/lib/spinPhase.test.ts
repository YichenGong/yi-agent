import { describe, it, expect } from "vitest";
import {
  SPIN_PERIOD_MS,
  spinPhaseMs,
  SPIN_KEYFRAMES,
  SPIN_TIMING,
  supportsWaapi,
  anchorSpin,
} from "./spinPhase";

/**
 * 这些是「转圈不因列表重排而回退」的相位内核。真正的机制（CSS 动画会在节点被
 * 移动时重启、WAAPI 不会）在真实引擎里取证（见 `docs/bug-list.md` 对应条目与
 * `desktop/src/components/ThreadSidebar.tsx` 的 `StatusBadge` 注释）；这里钉住的是
 * 让修复成立的两个纯函数性质：相位锚到墙钟、且在支持 WAAPI 时用 WAAPI 起动画。
 */
describe("spinPhaseMs", () => {
  it("maps a wall-clock reading into one turn's phase", () => {
    expect(spinPhaseMs(0)).toBe(0);
    expect(spinPhaseMs(250)).toBe(250);
    expect(spinPhaseMs(SPIN_PERIOD_MS)).toBe(0);
    expect(spinPhaseMs(SPIN_PERIOD_MS + 123)).toBe(123);
  });

  it("stays within [0, SPIN_PERIOD_MS) for arbitrary readings", () => {
    for (const t of [1, 999, 1000, 1001, 12345.678, 1e9]) {
      const p = spinPhaseMs(t);
      expect(p).toBeGreaterThanOrEqual(0);
      expect(p).toBeLessThan(SPIN_PERIOD_MS);
    }
  });

  it("never yields a negative phase (a negative clock must wrap forward)", () => {
    // Two badges mounted 400ms apart on the SAME wall clock land on the SAME spin,
    // which is what makes a remount visually seamless.
    const a = spinPhaseMs(1234);
    const b = spinPhaseMs(1234 + SPIN_PERIOD_MS * 3);
    expect(a).toBe(b);
    expect(spinPhaseMs(-1)).toBe(SPIN_PERIOD_MS - 1);
  });

  it("advances with the wall clock (a full turn per period)", () => {
    const start = spinPhaseMs(500);
    const halfLater = spinPhaseMs(500 + SPIN_PERIOD_MS / 2);
    expect(halfLater).toBe((start + SPIN_PERIOD_MS / 2) % SPIN_PERIOD_MS);
  });
});

describe("spin constants", () => {
  it("describes exactly one full clockwise turn, linear, looping", () => {
    expect(SPIN_PERIOD_MS).toBe(1000);
    expect(SPIN_KEYFRAMES).toEqual([
      { transform: "rotate(0deg)" },
      { transform: "rotate(360deg)" },
    ]);
    expect(SPIN_TIMING).toMatchObject({
      duration: SPIN_PERIOD_MS,
      iterations: Infinity,
      easing: "linear",
    });
  });
});

describe("anchorSpin", () => {
  it("returns null when the environment has no Web Animations API", () => {
    // jsdom is exactly this case: no `element.animate`, so the caller falls back
    // to the CSS `animate-spin` class. A plain object stands in for such an
    // element (no dependency on a DOM global in this suite).
    const el = {} as unknown as Element;
    expect(supportsWaapi()).toBe(false);
    expect(anchorSpin(el, 1234)).toBeNull();
  });

  it("starts a looping animation and anchors its currentTime to the wall-clock phase", () => {
    // Stand in for a browser: capture how `animate` was called.
    const calls: { keyframes: unknown; options: unknown }[] = [];
    const fake = {
      currentTime: 0,
      cancel() {},
      animate(keyframes: unknown, options: unknown) {
        calls.push({ keyframes, options });
        return this as unknown as Animation;
      },
    };
    const anim = anchorSpin(fake as unknown as Element, 777);
    expect(calls).toHaveLength(1);
    expect(calls[0].keyframes).toEqual(SPIN_KEYFRAMES);
    expect(calls[0].options).toMatchObject({ duration: SPIN_PERIOD_MS, iterations: Infinity });
    // The phase is the wall-clock remainder, NOT 0 — a remount therefore resumes
    // in phase instead of snapping back to 12 o'clock.
    expect(anim!.currentTime).toBe(777);
    expect(anchorSpin(fake as unknown as Element, SPIN_PERIOD_MS + 42)!.currentTime).toBe(42);
  });
});