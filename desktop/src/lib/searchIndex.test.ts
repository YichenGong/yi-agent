import { describe, it, expect } from "vitest";
import { collectHits, prefixTops, estimatedRects } from "./searchIndex";
import { createHeightBook } from "./heightBook";
import type { Item } from "./protocol";

const msg = (id: string, text: string): Item => ({ type: "agentMessage", id, text }) as Item;

describe("collectHits", () => {
  it("空查询不产生命中", () => {
    expect(collectHits([msg("a", "hello")], "  ")).toEqual([]);
  });

  it("大小写不敏感", () => {
    const hits = collectHits([msg("a", "Hello World")], "hello");
    expect(hits).toHaveLength(1);
    expect(hits[0]).toEqual({ index: 0, itemId: "a", offset: 0 });
  });

  it("同一条里的多个命中各自成项，offset 用序号区分", () => {
    const hits = collectHits([msg("a", "aa aa aa")], "aa");
    expect(hits.map((h) => h.offset)).toEqual([0, 1, 2]);
    expect(hits.every((h) => h.index === 0 && h.itemId === "a")).toBe(true);
  });

  it("能命中很靠后的未挂载项（这正是下沉到数据层的目的）", () => {
    const items = Array.from({ length: 3000 }, (_, i) => msg(`m${i}`, `row ${i}`));
    items[10] = msg("m10", "needle here");
    const hits = collectHits(items, "needle");
    expect(hits).toHaveLength(1);
    expect(hits[0].index).toBe(10);
  });

  it("命中总数覆盖全量历史，与挂载区间无关", () => {
    const items = Array.from({ length: 500 }, (_, i) => msg(`m${i}`, "x"));
    expect(collectHits(items, "x")).toHaveLength(500);
  });
});

describe("prefixTops", () => {
  it("第 0 条顶部为 0，逐条累加估计高度", () => {
    const book = createHeightBook(50);
    const items = [msg("a", ""), msg("b", ""), msg("c", "")];
    expect(prefixTops(items, book)).toEqual([0, 50, 100]);
  });

  it("已测量的项用真实高度", () => {
    const book = createHeightBook(50);
    book.record("a", 100);
    const items = [msg("a", ""), msg("b", "")];
    expect(prefixTops(items, book)).toEqual([0, 100]);
  });
});

describe("estimatedRects", () => {
  it("每个命中给一个 top/height，top 单调不减", () => {
    const book = createHeightBook(50);
    const items = [msg("a", "hit"), msg("b", "hit hit"), msg("c", "nope")];
    const tops = prefixTops(items, book);
    const rects = estimatedRects(collectHits(items, "hit"), tops, book);
    expect(rects).toHaveLength(3);
    expect(rects[0].top).toBeLessThanOrEqual(rects[1].top);
    expect(rects.every((r) => r.height > 0)).toBe(true);
  });

  it("空命中返回空数组", () => {
    const book = createHeightBook();
    expect(estimatedRects([], [], book)).toEqual([]);
  });
});
