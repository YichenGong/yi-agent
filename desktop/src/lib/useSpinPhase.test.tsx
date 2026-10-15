/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { useRef } from "react";
import { useSpinPhase } from "./useSpinPhase";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

/**
 * Stub a minimal Web Animations API on `Element.prototype` and record the
 * animations created per element, so we can assert *when* a new one is created.
 */
function stubWaapi() {
  const created: { el: Element; currentTime: number; cancelled: boolean }[] = [];
  const proto = Element.prototype as unknown as { animate?: unknown };
  proto.animate = function (this: Element) {
    const rec = { el: this, currentTime: 0, cancelled: false };
    created.push(rec);
    return {
      get currentTime() {
        return rec.currentTime;
      },
      set currentTime(v: number) {
        rec.currentTime = v;
      },
      cancel() {
        rec.cancelled = true;
      },
    } as unknown as Animation;
  };
  return created;
}

function Harness({ active }: { active: boolean }) {
  const ref = useRef<HTMLSpanElement>(null);
  useSpinPhase(ref, active);
  return <span ref={ref} data-testid="badge" />;
}

describe("useSpinPhase", () => {
  it("starts exactly one animation, phase-anchored to the wall clock", () => {
    const created = stubWaapi();
    vi.spyOn(performance, "now").mockReturnValue(4321);
    const { rerender } = render(<Harness active />);
    expect(created).toHaveLength(1);
    expect(created[0].currentTime).toBe(4321 % 1000); // 321, not 0
    // Re-rendering while still active must not recreate the animation: that is
    // what keeps the timeline continuous across a status flip.
    rerender(<Harness active />);
    expect(created).toHaveLength(1);
  });

  it("cancels the animation when it deactivates, and re-anchors on reactivation", () => {
    const created = stubWaapi();
    vi.spyOn(performance, "now").mockReturnValue(1500);
    const { rerender } = render(<Harness active />);
    expect(created).toHaveLength(1);

    rerender(<Harness active={false} />);
    expect(created[0].cancelled).toBe(true);

    vi.spyOn(performance, "now").mockReturnValue(2500);
    rerender(<Harness active />);
    expect(created).toHaveLength(2);
    // Resume in phase with the wall clock, not back at 0.
    expect(created[1].currentTime).toBe(500);
  });

  it("does nothing (no crash) when the element has no Web Animations API", () => {
    vi.spyOn(performance, "now").mockReturnValue(10);
    expect(() => render(<Harness active />)).not.toThrow();
  });
});