import { describe, expect, it } from "vitest";
import type { Item } from "./protocol";
import {
  applyTrim,
  estimateItemsBytes,
  MAX_ENTRY_BYTES,
  selectEvictions,
} from "./contentStore";

const msg = (id: string, text: string): Item => ({ type: "agentMessage", id, text });

describe("estimateItemsBytes", () => {
  it("grows with content and is > 0 for a non-empty list", () => {
    const one = estimateItemsBytes([msg("a", "hello")]);
    const two = estimateItemsBytes([msg("a", "hello"), msg("b", "world")]);
    expect(one).toBeGreaterThan(0);
    expect(two).toBeGreaterThan(one);
  });
});

describe("applyTrim", () => {
  it("returns items unchanged when under the budget", () => {
    const items = [msg("a", "hello"), msg("b", "world")];
    expect(applyTrim(items, 1_000_000)).toEqual({ items, trimmed: false });
  });

  it("drops from the head (oldest) until it fits, keeping the tail", () => {
    const items = [msg("a", "x".repeat(200)), msg("b", "y".repeat(200)), msg("c", "z".repeat(200))];
    const budget = estimateItemsBytes([items[2]]); // only the last one fits
    const out = applyTrim(items, budget);
    expect(out.trimmed).toBe(true);
    expect(out.items).toEqual([items[2]]); // newest preserved
    expect(estimateItemsBytes(out.items)).toBeLessThanOrEqual(budget);
  });

  it("keeps at least one item even if it alone exceeds the budget", () => {
    const big = msg("a", "x".repeat(5000));
    const out = applyTrim([big], 10);
    expect(out.items).toEqual([big]);
  });

  it("an empty list stays empty and untrimmed", () => {
    expect(applyTrim([], MAX_ENTRY_BYTES)).toEqual({ items: [], trimmed: false });
  });
});

describe("selectEvictions", () => {
  const meta = (threadId: string, savedAt: number, bytes: number) => ({ threadId, savedAt, bytes });

  it("evicts nothing when under both bounds", () => {
    const metas = [meta("a", 1, 100), meta("b", 2, 100)];
    expect(selectEvictions(metas, { maxCount: 10, maxBytes: 1000 })).toEqual([]);
  });

  it("evicts the oldest first to satisfy the count bound", () => {
    const metas = [meta("old", 1, 10), meta("mid", 2, 10), meta("new", 3, 10)];
    expect(selectEvictions(metas, { maxCount: 2, maxBytes: 10_000 })).toEqual(["old"]);
  });

  it("evicts oldest until the byte bound is met too", () => {
    const metas = [meta("old", 1, 600), meta("mid", 2, 600), meta("new", 3, 600)];
    expect(selectEvictions(metas, { maxCount: 10, maxBytes: 1000 })).toEqual(["old", "mid"]);
  });

  it("stops as soon as both bounds hold (no over-eviction)", () => {
    const metas = [meta("old", 1, 10), meta("mid", 2, 10), meta("new", 3, 10)];
    expect(selectEvictions(metas, { maxCount: 2, maxBytes: 1000 })).toHaveLength(1);
  });
});
