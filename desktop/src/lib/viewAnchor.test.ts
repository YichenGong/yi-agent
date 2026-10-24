import { describe, it, expect } from "vitest";
import { findAnchor, restoreScrollTop, isAnchorableId, type ItemBox } from "./viewAnchor";

const boxes = (tops: Record<string, number>): ItemBox[] =>
  Object.entries(tops).map(([id, top]) => ({ id, top }));

describe("isAnchorableId", () => {
  it("accepts server ids", () => {
    expect(isAnchorableId("item-1")).toBe(true);
  });
  it("rejects local optimistic ids, which are rebuilt on reopen", () => {
    expect(isAnchorableId("local-3")).toBe(false);
  });
  it("rejects missing and empty ids", () => {
    expect(isAnchorableId(undefined)).toBe(false);
    expect(isAnchorableId("")).toBe(false);
  });
});

describe("findAnchor", () => {
  it("picks the last message whose top is at or above the viewport top", () => {
    // 视口顶在 250：item-b 从 100 开始、item-c 从 300 开始 ⇒ b 是顶部那条。
    const got = findAnchor(boxes({ "item-a": 0, "item-b": 100, "item-c": 300 }), 250);
    expect(got).toEqual({ itemId: "item-b", offset: -150 });
  });

  it("records a zero offset when a message starts exactly at the viewport top", () => {
    expect(findAnchor(boxes({ "item-a": 0, "item-b": 200 }), 200)).toEqual({
      itemId: "item-b",
      offset: 0,
    });
  });

  it("never anchors on a local id", () => {
    const got = findAnchor(boxes({ "item-a": 0, "local-1": 100 }), 150);
    expect(got).toEqual({ itemId: "item-a", offset: -150 });
  });

  it("returns null when nothing is above the viewport top", () => {
    expect(findAnchor(boxes({ "item-a": 400 }), 100)).toBeNull();
    expect(findAnchor([], 0)).toBeNull();
  });
});

describe("restoreScrollTop", () => {
  it("puts the anchored message back at the offset it had", () => {
    // 锚点偏移 -150 表示该消息顶边在视口上方 150px ⇒ 回滚到 250+150。
    expect(restoreScrollTop({ id: "item-b", top: 250 }, { itemId: "item-b", offset: -150 })).toBe(400);
  });

  it("clamps to zero instead of using a negative scrollTop", () => {
    // offset 为正 = 这条消息顶边在视口顶部**下方** 50px；它现在离内容顶端只有 10px，
    // 想把顶边放到视口下方 50px 会算出 -40，必须夹到 0。
    expect(restoreScrollTop({ id: "item-b", top: 10 }, { itemId: "item-b", offset: 50 })).toBe(0);
  });

  it("returns null when the anchored message is gone", () => {
    expect(restoreScrollTop(undefined, { itemId: "item-b", offset: 0 })).toBeNull();
  });
});
