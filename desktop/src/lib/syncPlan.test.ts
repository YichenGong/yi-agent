import { describe, it, expect } from "vitest";
import { parseSyncPlan, resolveSyncPlan } from "./syncPlan";
import type { ContentEntry } from "./contentStore";
import type { ThreadSummary } from "./protocol";

const entry = (over: Partial<ContentEntry> = {}): ContentEntry => ({
  threadId: "t1",
  savedAt: 1,
  lastServerItemId: null,
  items: [],
  trimmed: false,
  bytes: 0,
  ...over,
});

const summary = (over: Partial<ThreadSummary> = {}): ThreadSummary => ({
  thread_id: "t1",
  cwd: "/w",
  model: "m",
  created_at: 0,
  updated_at: 0,
  title: null,
  ...over,
});

describe("parseSyncPlan", () => {
  it("parses entries and drops an unknown action rather than guessing", () => {
    const plan = parseSyncPlan({
      syncPlan: [
        { threadId: "t1", action: "full", epoch: 2, reason: "epoch_changed" },
        { threadId: "t2", action: "none", epoch: 1, reason: "match" },
        { threadId: "t3", action: "bogus", epoch: 1 },
        { action: "full", epoch: 1 }, // 缺 threadId
      ],
    });
    expect(plan.get("t1")).toEqual({ action: "full", epoch: 2 });
    expect(plan.get("t2")).toEqual({ action: "none", epoch: 1 });
    expect(plan.has("t3")).toBe(false);
    expect(plan.size).toBe(2);
  });

  it("tolerates a missing or malformed plan", () => {
    expect(parseSyncPlan({}).size).toBe(0);
    expect(parseSyncPlan(undefined).size).toBe(0);
    expect(parseSyncPlan({ syncPlan: "nope" }).size).toBe(0);
  });
});

describe("resolveSyncPlan", () => {
  it("uses the plan's verdict for the thread", () => {
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "full", epoch: 3 }] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 3, trimmed: false }),
      }),
    ).toBe("full");
  });

  it("forces a full resume when the cached entry was trimmed", () => {
    // 服务端正确地说 none（同 epoch），但本地缓存丢过头部——照 none 跳过 resume
    // 会让用户静默看到被截断的会话（spec §4.2 例外）。
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "none", epoch: 3 }] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 3, trimmed: true }),
      }),
    ).toBe("full");
  });

  it("lets the server's full verdict win even when the epochs look equal", () => {
    // 服务端在服务端日志里发现锚点已缺失（同 epoch 也会发生：客户端可能报了一个
    // 只存在于未落盘 partial 检查点的锚点）⇒ 必须全量，绝不因"epoch 相等"降级为 none。
    const plan = parseSyncPlan({
      syncPlan: [{ threadId: "t1", action: "full", epoch: 3, reason: "anchor_missing" }],
    });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 3, trimmed: false }),
      }),
    ).toBe("full");
  });

  it("lets the server's none verdict win when the epochs look equal", () => {
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "none", epoch: 3 }] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 3, trimmed: false }),
      }),
    ).toBe("none");
  });

  it("does not let a plan entry without an epoch shadow the listing's version", () => {
    // plan 覆盖该会话时以服务端裁决为准；此处 action 为 full，即使 summary 的
    // 版本字段缺失也必须 full（缺字段绝不当作"一致"）。
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "full" }] });
    expect(resolveSyncPlan({ plan, summary: summary(), cached: entry({ contentEpoch: 0 }) })).toBe(
      "full",
    );
  });

  it("falls back to the summary when the plan covers nothing", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 1 }),
      }),
    ).toBe("full");
  });

  it("degrades to a full resume when the server omits the version", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    // 旧服务端：没有 content_epoch。绝不当作"一致"。
    expect(resolveSyncPlan({ plan, summary: summary(), cached: entry({ contentEpoch: 1 }) })).toBe(
      "full",
    );
  });

  it("degrades to a full resume when we do not know our own epoch", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    expect(
      resolveSyncPlan({ plan, summary: summary({ content_epoch: 3 }), cached: entry() }),
    ).toBe("full");
  });

  it("says none only when a known epoch matches and nothing is trimmed", () => {
    const plan = parseSyncPlan({ syncPlan: [] });
    expect(
      resolveSyncPlan({
        plan,
        summary: summary({ content_epoch: 3 }),
        cached: entry({ contentEpoch: 3, trimmed: false }),
      }),
    ).toBe("none");
  });

  it("honours an explicit drop even when epochs look equal", () => {
    const plan = parseSyncPlan({ syncPlan: [{ threadId: "t1", action: "drop", epoch: 0 }] });
    expect(resolveSyncPlan({ plan, summary: summary({ content_epoch: 0 }), cached: entry() })).toBe(
      "drop",
    );
  });
});