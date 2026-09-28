/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import {
  DEFAULT_SIDEBAR_WIDTH,
  MIN_SIDEBAR_WIDTH,
  MAX_SIDEBAR_WIDTH,
  clampSidebarWidth,
} from "./sidebarWidth";

afterEach(() => localStorage.clear());

describe("clampSidebarWidth", () => {
  it("returns an in-range value unchanged", () => {
    expect(clampSidebarWidth(300)).toBe(300);
  });

  it("clamps below MIN up to MIN", () => {
    expect(clampSidebarWidth(10)).toBe(MIN_SIDEBAR_WIDTH);
  });

  it("clamps above MAX down to MAX", () => {
    expect(clampSidebarWidth(9999)).toBe(MAX_SIDEBAR_WIDTH);
  });

  it("falls back to the default for non-finite input", () => {
    expect(clampSidebarWidth(NaN)).toBe(DEFAULT_SIDEBAR_WIDTH);
    expect(clampSidebarWidth(Infinity)).toBe(DEFAULT_SIDEBAR_WIDTH);
    expect(clampSidebarWidth(-Infinity)).toBe(DEFAULT_SIDEBAR_WIDTH);
  });
});
