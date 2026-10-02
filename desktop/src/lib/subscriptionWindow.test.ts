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
