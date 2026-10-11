import { describe, expect, it } from "vitest";
import type { Item } from "./protocol";
import {
  applyTrim,
  estimateItemsBytes,
  evict,
  MAX_ENTRY_BYTES,
  MemoryContentStore,
  persistContent,
  selectEvictions,
} from "./contentStore";
import { Session } from "./session";

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

describe("MemoryContentStore", () => {
  it("round-trips an entry, deletes, and lists keys", async () => {
    const store = new MemoryContentStore();
    const entry = {
      threadId: "t1",
      savedAt: 1,
      lastServerItemId: "i1",
      items: [msg("i1", "hi")],
      trimmed: false,
      bytes: 3,
    };
    await store.write(entry);
    expect(await store.read("t1")).toEqual(entry);
    expect(await store.keys()).toEqual(["t1"]);
    await store.delete("t1");
    expect(await store.read("t1")).toBeNull();
    expect(await store.keys()).toEqual([]);
  });

  it("copies on write so later mutation of the source does not leak in", async () => {
    const store = new MemoryContentStore();
    const items = [msg("i1", "hi")];
    await store.write({ threadId: "t1", savedAt: 1, lastServerItemId: null, items, trimmed: false, bytes: 3 });
    items.push(msg("i2", "later"));
    const back = await store.read("t1");
    expect(back?.items).toHaveLength(1);
  });

  it("reads a missing key as null", async () => {
    expect(await new MemoryContentStore().read("nope")).toBeNull();
  });
});

describe("evict", () => {
  const write = async (store: MemoryContentStore, threadId: string, savedAt: number, bytes: number) =>
    store.write({ threadId, savedAt, lastServerItemId: null, items: [], trimmed: false, bytes });

  it("deletes the least recently written entries past the count bound", async () => {
    const store = new MemoryContentStore();
    await write(store, "old", 1, 10);
    await write(store, "mid", 2, 10);
    await write(store, "new", 3, 10);
    await evict(store, { maxCount: 2, maxBytes: 10_000 });
    expect((await store.keys()).sort()).toEqual(["mid", "new"]);
  });
});

describe("persistContent", () => {
  it("stores only server items (drops local echoes and notices)", async () => {
    const store = new MemoryContentStore();
    const session = new Session();
    session.notice("command output");           // local-* id
    session.addUserMessage("hi");               // local-* optimistic echo
    session.apply({ method: "item/completed", params: { item: msg("s1", "server text") } } as never);
    await persistContent(store, "t1", session);
    const entry = await store.read("t1");
    expect(entry?.items.map((i) => i.id)).toEqual(["s1"]);
    expect(entry?.lastServerItemId).toBe("s1");
  });
});
