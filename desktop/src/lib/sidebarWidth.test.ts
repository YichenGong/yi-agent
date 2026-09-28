/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import {
  DEFAULT_SIDEBAR_WIDTH,
  MIN_SIDEBAR_WIDTH,
  MAX_SIDEBAR_WIDTH,
  SIDEBAR_WIDTH_STORAGE_KEY,
  clampSidebarWidth,
  loadSidebarWidth,
  saveSidebarWidth,
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

describe("loadSidebarWidth / saveSidebarWidth", () => {
  it("returns the default when nothing is stored", () => {
    expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
  });

  it("returns the default for a corrupt stored value", () => {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "abc");
    expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "");
    expect(loadSidebarWidth()).toBe(DEFAULT_SIDEBAR_WIDTH);
  });

  it("clamps an out-of-range stored value", () => {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, "9999");
    expect(loadSidebarWidth()).toBe(MAX_SIDEBAR_WIDTH);
  });

  it("round-trips a saved value", () => {
    saveSidebarWidth(300);
    expect(loadSidebarWidth()).toBe(300);
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe("300");
  });

  it("clamps before saving", () => {
    saveSidebarWidth(1);
    expect(localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY)).toBe(String(MIN_SIDEBAR_WIDTH));
  });
});
