/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { RollingNumber } from "./RollingNumber";
import { BASE_MS, MAX_MS, PER_STEP_MS, STRIP_BASE, stripY } from "../lib/rollingDigits";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function strips(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(".rolling-strip"));
}

describe("RollingNumber", () => {
  it("shows the value immediately on mount, without a transition", () => {
    const { container } = render(<RollingNumber value={1234} />);
    expect(strips(container).map((el) => el.style.transform)).toEqual([
      stripY(STRIP_BASE + 1),
      stripY(STRIP_BASE + 2),
      stripY(STRIP_BASE + 3),
      stripY(STRIP_BASE + 4),
    ]);
    expect(strips(container).every((el) => el.style.transition === "none")).toBe(true);
  });

  it("hides the mechanical digits from assistive tech", () => {
    const { container } = render(<RollingNumber value={12} />);
    expect(container.firstElementChild?.getAttribute("aria-hidden")).toBe("true");
  });

  it("rolls only the digits whose face changed", () => {
    const { container, rerender } = render(<RollingNumber value={1234} />);
    rerender(<RollingNumber value={1235} />);
    const [thousands, hundreds, tens, units] = strips(container);
    expect(thousands.style.transition).toBe("none");
    expect(hundreds.style.transition).toBe("none");
    expect(tens.style.transition).toBe("none");
    expect(units.style.transform).toBe(stripY(STRIP_BASE + 5));
    expect(units.style.transition).toContain(`${BASE_MS}ms`);
  });

  it("rolls downward when the value decreases", () => {
    const { container, rerender } = render(<RollingNumber value={1234} />);
    rerender(<RollingNumber value={1230} />);
    const units = strips(container)[3];
    // 4 -> 0 going down: four steps, landing four positions lower.
    expect(units.style.transform).toBe(stripY(STRIP_BASE + 0));
    expect(units.style.transition).toContain(`${BASE_MS + 3 * PER_STEP_MS}ms`);
  });

  it("caps the duration of a long walk", () => {
    const { container, rerender } = render(<RollingNumber value={10} />);
    rerender(<RollingNumber value={19} />);
    const units = strips(container)[1];
    expect(units.style.transition).toContain(`${MAX_MS}ms`);
  });

  it("keeps the same strip elements across updates so the browser can retarget", () => {
    const { container, rerender } = render(<RollingNumber value={1234} />);
    const before = strips(container);
    rerender(<RollingNumber value={1235} />);
    const after = strips(container);
    expect(after).toHaveLength(4);
    expect(after[3]).toBe(before[3]);
  });

  it("jumps without a transition when the usage key changes", () => {
    const { container, rerender } = render(<RollingNumber value={1234} usageKey="a" />);
    rerender(<RollingNumber value={7} usageKey="b" />);
    expect(strips(container).every((el) => el.style.transition === "none")).toBe(true);
    expect(strips(container)[0].style.transform).toBe(stripY(STRIP_BASE + 7));
  });

  it("jumps without a transition under prefers-reduced-motion", () => {
    vi.stubGlobal("matchMedia", (query: string) => ({
      matches: query.includes("prefers-reduced-motion"),
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    }));
    const { container, rerender } = render(<RollingNumber value={1234} />);
    rerender(<RollingNumber value={1239} />);
    expect(strips(container).every((el) => el.style.transition === "none")).toBe(true);
    expect(strips(container)[3].style.transform).toBe(stripY(STRIP_BASE + 9));
  });

  it("adds a new leading digit at its final face without animating it", () => {
    const { container, rerender } = render(<RollingNumber value={99} />);
    rerender(<RollingNumber value={100} />);
    const [hundreds, tens, units] = strips(container);
    expect(hundreds.style.transform).toBe(stripY(STRIP_BASE + 1));
    expect(hundreds.style.transition).toBe("none");
    // Both lower digits roll 9 -> 0 upward: one step, landing one position higher
    // (the neighbouring `0` in the strip).
    expect(tens.style.transform).toBe(stripY(STRIP_BASE + 10));
    expect(tens.style.transition).toContain(`${BASE_MS}ms`);
    expect(units.style.transform).toBe(stripY(STRIP_BASE + 10));
  });

  it("re-bases a digit to the middle of the strip once its roll settles", () => {
    const { container, rerender } = render(<RollingNumber value={99} />);
    rerender(<RollingNumber value={100} />);
    const tens = strips(container)[1];
    fireEvent.transitionEnd(tens);
    // Same face (`0`), but back in the middle band so positions cannot drift away.
    expect(tens.style.transform).toBe(stripY(STRIP_BASE + 0));
    expect(tens.style.transition).toBe("none");
  });

  it("does not cancel an in-flight roll when a later value leaves its face unchanged", () => {
    const { container, rerender } = render(<RollingNumber value={1284} />);
    // 4 -> 0 while the value grows: six steps downward.
    rerender(<RollingNumber value={1290} />);
    const rolling = strips(container)[3];
    const midTransform = rolling.style.transform;
    const midTransition = rolling.style.transition;
    expect(midTransition).toContain(`${BASE_MS + 5 * PER_STEP_MS}ms`);

    // 1290 -> 1300: the value changed, but this digit's face stays `0`.
    // Switching its transition off here would snap it to the destination.
    rerender(<RollingNumber value={1300} />);
    const after = strips(container)[3];
    expect(after).toBe(rolling);
    expect(after.style.transform).toBe(midTransform);
    expect(after.style.transition).toBe(midTransition);
  });
});
