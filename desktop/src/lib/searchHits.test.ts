/** @vitest-environment jsdom */
import { describe, it, expect } from "vitest";
import {
  normalizeQuery,
  findMatches,
  nextIndex,
  initialActiveIndex,
} from "./searchHits";

describe("normalizeQuery", () => {
  it("trims surrounding whitespace", () => {
    expect(normalizeQuery("  hello  ")).toBe("hello");
  });
  it("keeps inner whitespace", () => {
    expect(normalizeQuery("a b")).toBe("a b");
  });
});

describe("findMatches", () => {
  it("returns nothing for an empty query", () => {
    expect(findMatches("anything", "")).toEqual([]);
    expect(findMatches("anything", "   ")).toEqual([]);
  });
  it("matches case-insensitively and reports exact positions", () => {
    expect(findMatches("Hello hello", "hello")).toEqual([
      { start: 0, end: 5 },
      { start: 6, end: 11 },
    ]);
  });
  it("finds every occurrence including overlapping boundary cases", () => {
    expect(findMatches("aaa", "aa")).toEqual([
      { start: 0, end: 2 },
      { start: 1, end: 3 },
    ]);
  });
  it("matches CJK text", () => {
    expect(findMatches("会话内搜索", "搜索")).toEqual([{ start: 3, end: 5 }]);
  });
  it("returns nothing when absent", () => {
    expect(findMatches("abc", "zzz")).toEqual([]);
  });
});

describe("nextIndex", () => {
  it("advances and wraps forward", () => {
    expect(nextIndex(0, 3, 1)).toBe(1);
    expect(nextIndex(2, 3, 1)).toBe(0);
  });
  it("moves back and wraps at zero", () => {
    expect(nextIndex(2, 3, -1)).toBe(1);
    expect(nextIndex(0, 3, -1)).toBe(2);
  });
  it("stays at zero when there is nothing to move through", () => {
    expect(nextIndex(0, 0, 1)).toBe(0);
    expect(nextIndex(0, 0, -1)).toBe(0);
  });
  it("clamps an out-of-range current index by wrapping it first", () => {
    // 先对 total 取模（9 % 3 = 0），再前进 1。语义是"位置在环上"，不是"夹到末尾"。
    expect(nextIndex(9, 3, 1)).toBe(1);
  });
});

describe("initialActiveIndex", () => {
  const viewport = { scrollTop: 100, clientHeight: 200 };

  it("prefers the first hit inside the viewport", () => {
    const rects = [
      { top: 0, height: 10 },
      { top: 120, height: 10 },
      { top: 200, height: 10 },
    ];
    expect(initialActiveIndex(rects, viewport)).toBe(1);
  });

  it("picks the nearest one below when none is visible", () => {
    const rects = [
      { top: 0, height: 10 },
      { top: 400, height: 10 },
      { top: 500, height: 10 },
    ];
    expect(initialActiveIndex(rects, viewport)).toBe(1);
  });

  it("picks the last one when everything is above", () => {
    const rects = [
      { top: 0, height: 10 },
      { top: 50, height: 10 },
    ];
    expect(initialActiveIndex(rects, viewport)).toBe(1);
  });

  it("returns 0 for an empty list", () => {
    expect(initialActiveIndex([], viewport)).toBe(0);
  });
});
