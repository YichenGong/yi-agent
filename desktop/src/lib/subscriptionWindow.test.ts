import { describe, expect, it } from "vitest";
import { SUBSCRIPTION_WINDOW_SIZE, SubscriptionWindow } from "./subscriptionWindow";

describe("SubscriptionWindow", () => {
  it("keeps the most recent ids, most-recent first", () => {
    const w = new SubscriptionWindow();
    expect(w.touch("a")).toEqual(["a"]);
    expect(w.touch("b")).toEqual(["b", "a"]);
    expect(w.touch("a")).toEqual(["a", "b"]); // 重访移到最前
  });

  it("caps at the window size, evicting the least recent", () => {
    const w = new SubscriptionWindow();
    for (let i = 0; i < SUBSCRIPTION_WINDOW_SIZE + 3; i++) w.touch(`t${i}`);
    const ids = w.current();
    expect(ids).toHaveLength(SUBSCRIPTION_WINDOW_SIZE);
    expect(ids[0]).toBe(`t${SUBSCRIPTION_WINDOW_SIZE + 2}`); // 最新在最前
    expect(ids).not.toContain("t0"); // 最旧的被淘汰
  });

  it("never exceeds the server cap of 16", () => {
    expect(SUBSCRIPTION_WINDOW_SIZE).toBeLessThanOrEqual(16);
  });

  it("reports membership and returns copies", () => {
    const w = new SubscriptionWindow();
    w.touch("a");
    expect(w.has("a")).toBe(true);
    expect(w.has("z")).toBe(false);
    const snap = w.current();
    snap.push("x");
    expect(w.has("x")).toBe(false); // 返回的是副本，外部改动不影响内部
  });
});

describe("SubscriptionWindow.forget", () => {
  it("removes the id so the early-return guard no longer matches", () => {
    const w = new SubscriptionWindow(3);
    w.touch("t1");
    expect(w.has("t1")).toBe(true);
    w.forget("t1");
    expect(w.has("t1")).toBe(false);
  });

  it("keeps the other ids in most-recent-first order", () => {
    const w = new SubscriptionWindow(3);
    w.touch("a");
    w.touch("b");
    w.touch("c");
    w.forget("b");
    expect(w.current()).toEqual(["c", "a"]);
  });

  it("is a no-op for an unknown id", () => {
    const w = new SubscriptionWindow(3);
    w.touch("a");
    expect(() => w.forget("zzz")).not.toThrow();
    expect(w.current()).toEqual(["a"]);
  });

  it("does not break the LRU cap afterwards", () => {
    const w = new SubscriptionWindow(2);
    w.touch("a");
    w.touch("b");
    w.forget("a");
    w.touch("c");
    w.touch("d");
    expect(w.current()).toEqual(["d", "c"]);
  });
});
