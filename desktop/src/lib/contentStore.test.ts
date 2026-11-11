import { describe, expect, it, vi } from "vitest";
import type { Item } from "./protocol";
import {
  applyTrim,
  contentFingerprintOf,
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

  it("serializes at most linear item volume while trimming a long history", () => {
    const items = Array.from({ length: 300 }, (_, i) => msg(String(i), "长历史🙂".repeat(100)));
    const budget = estimateItemsBytes([items[299]]);
    const stringify = vi.spyOn(JSON, "stringify");
    let out: ReturnType<typeof applyTrim>;
    let serializedItems: number;
    try {
      out = applyTrim(items, budget);
      serializedItems = stringify.mock.calls.reduce((sum, [value]) =>
        sum + (Array.isArray(value) ? value.length : 1), 0);
    } finally { stringify.mockRestore(); }
    expect(out.items).toEqual([items[299]]);
    expect(serializedItems).toBeLessThanOrEqual(items.length + 1);
  });

  it("uses exact UTF-8 bytes including array brackets and separators", () => {
    const items = [msg("a", "旧🙂"), msg("b", "新é"), msg("c", "尾巴")];
    const exact = estimateItemsBytes(items.slice(1));
    expect(applyTrim(items, exact).items).toEqual(items.slice(1));
    expect(applyTrim(items, exact - 1).items).toEqual(items.slice(2));
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
  it("round-trips contentEpoch and leaves a legacy entry undefined", async () => {
    const store = new MemoryContentStore();
    await store.write({
      threadId: "t1",
      savedAt: 1,
      lastServerItemId: "i1",
      contentEpoch: 4,
      items: [],
      trimmed: false,
      bytes: 1,
    });
    expect((await store.read("t1"))?.contentEpoch).toBe(4);

    // 老条目（本字段出现前写入）没有该字段：读回 undefined，由调用方解释为
    // "未知" ⇒ 保守全量（spec §4.3）。
    await store.write({
      threadId: "t2",
      savedAt: 1,
      lastServerItemId: null,
      items: [],
      trimmed: false,
      bytes: 1,
    });
    expect((await store.read("t2"))?.contentEpoch).toBeUndefined();
  });

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

  it("counts writes and deletes without changing the stored shape", async () => {
    const store = new MemoryContentStore();
    expect(store.writes).toBe(0);
    expect(store.deletes).toBe(0);
    await store.write({
      threadId: "t1", savedAt: 1, lastServerItemId: null,
      items: [], trimmed: false, bytes: 0,
    });
    expect(store.writes).toBe(1);
    // 计数不写进条目本身：读回来逐字段比对仍与写入值一致。
    expect(await store.read("t1")).toEqual({
      threadId: "t1", savedAt: 1, lastServerItemId: null,
      items: [], trimmed: false, bytes: 0,
    });
    await store.delete("t1");
    expect(store.deletes).toBe(1);
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

  it("writes nothing when there is no server content and it is not authoritative", async () => {
    const store = new MemoryContentStore();
    const session = new Session();
    // 只有本地回显与 notice：服务端尚未回帧。
    session.notice("command output");
    session.addUserMessage("hi");
    const outcome = await persistContent(store, "t1", session);
    expect(outcome).toEqual({ action: "skipped" });
    expect(await store.keys()).toEqual([]);
  });

  it("deletes the entry on an authoritative empty result", async () => {
    const store = new MemoryContentStore();
    await store.write({
      threadId: "t1", savedAt: 1, lastServerItemId: "old",
      items: [{ type: "agentMessage", id: "old", text: "stale" }],
      trimmed: false, bytes: 10,
    });
    const session = new Session();          // 全量 resume 后确认为空
    const outcome = await persistContent(store, "t1", session, { authoritative: true });
    expect(outcome).toEqual({ action: "deleted" });
    expect(await store.keys()).toEqual([]);
  });

  it("does not delete when authoritative but only local echoes exist", async () => {
    const store = new MemoryContentStore();
    await store.write({
      threadId: "t1", savedAt: 1, lastServerItemId: "done",
      items: [{ type: "agentMessage", id: "done", text: "keep" }],
      trimmed: false, bytes: 10,
    });
    const session = new Session();
    session.addUserMessage("just sent, server has not echoed yet");
    const outcome = await persistContent(store, "t1", session, { authoritative: true });
    expect(outcome).toEqual({ action: "skipped" });
    expect(await store.keys()).toEqual(["t1"]);   // 好缓存必须留着
  });

  it("returns a fingerprint computed from the trimmed entry, not raw items", async () => {
    const store = new MemoryContentStore();
    const session = new Session();
    // 一条 notice + 三条服务端项：归一化前 4 条，归一化后 3 条。
    session.notice("notice");
    for (const id of ["a", "b", "c"]) {
      session.apply({ method: "item/completed", params: { item: msg(id, `text-${id}`) } } as never);
    }
    const outcome = await persistContent(store, "t1", session);
    expect(outcome.action).toBe("written");
    if (outcome.action !== "written") throw new Error("unreachable");
    const entry = await store.read("t1");
    expect(entry).not.toBeNull();
    // 关键：指纹必须与「从盘上条目反算」逐字相等，否则冷启后永远判「变了」。
    expect(outcome.fingerprint).toBe(
      contentFingerprintOf(entry!.items, entry!.lastServerItemId, entry!.contentEpoch),
    );
    // 且不能等于用原始 items 算出来的那个（那会把 notice 也数进去）。
    expect(outcome.fingerprint).not.toBe(
      contentFingerprintOf(session.items as never, session.lastServerItemId, session.contentEpoch),
    );
  });

  it("reports which threads the write evicted from the LRU", async () => {
    const store = new MemoryContentStore();
    // 先放满上限（CONTENT_LIMITS.maxCount = 10），再写第 11 条。
    // `savedAt` 递增：old0 最旧，必是第一个被淘汰的。
    for (let i = 0; i < 10; i++) {
      await store.write({
        threadId: `old${i}`, savedAt: i + 1, lastServerItemId: null,
        items: [msg(`i${i}`, "x")], trimmed: false, bytes: 1,
      });
    }
    const session = new Session();
    session.apply({ method: "item/completed", params: { item: msg("fresh", "y") } } as never);
    const outcome = await persistContent(store, "fresh", session);
    expect(outcome.action).toBe("written");
    if (outcome.action !== "written") throw new Error("unreachable");
    // 写第 11 条后共 11 条 > 10，必须淘汰 1 条：`old0`（savedAt 最小）。
    // 它要在返回值里报出来——调用方靠它清 persistedFp，否则该会话既已不在
    // 盘上、又会被 `flushAll` 判「没变」而永不重写。
    expect(outcome.evicted).toEqual(["old0"]);
    expect(await store.keys()).not.toContain("old0");
    expect(await store.keys()).toContain("fresh");
  });
});
