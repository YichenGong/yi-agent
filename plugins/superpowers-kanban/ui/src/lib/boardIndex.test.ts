import { describe, expect, it } from "vitest";
import { summarize } from "./boardIndex";

describe("summarize", () => {
  it("数出还在队列里的每一类，包括合并卡", () => {
    const cards = [
      { state: "queued" },
      { state: "queued" },
      { state: "running" },
      { state: "merging" },
      { state: "awaiting_merge" },
      { state: "needs_you" },
    ];
    expect(summarize(cards)).toBe("2 排队 · 2 运行中 · 1 待合并 · 1 待处理");
  });

  it("合并卡不再被算成 0", () => {
    // 用户报的原始 case：3 张等待合并 + 2 张待处理，旧口径显示「0 排队 · 0 运行中」，
    // 于是卡住的两张合并卡在侧栏完全隐形。
    const cards = [
      { state: "awaiting_merge" },
      { state: "awaiting_merge" },
      { state: "awaiting_merge" },
      { state: "needs_you" },
      { state: "needs_you" },
    ];
    expect(summarize(cards)).toBe("3 待合并 · 2 待处理");
  });

  it("已确认合并、正在排队的卡单独算一行", () => {
    // merge_queued 不需要用户再做任何事，但也绝不能消失成「无待办」。
    expect(summarize([{ state: "merge_queued" }, { state: "merge_queued" }])).toBe(
      "2 合并排队中",
    );
  });

  it("没有待办时明说，不留空", () => {
    expect(summarize([{ state: "done" }, { state: "failed" }])).toBe("无待办");
  });

  it("只有预排队的看板也算有待办", () => {
    // 预排队是最左列，侧栏摘要却整类漏掉它：一块只放预排队卡的看板会显示
    // 「无待办」，预排队追踪等于白做。
    const cards = [{ state: "pre_queued" }];
    const summary = summarize(cards);
    expect(summary).not.toBe("无待办");
    expect(summary).toContain("预排队");
    expect(summary).toBe("1 预排队");
  });

  it("预排队排在摘要最前，与最左列的顺序一致", () => {
    const cards = [{ state: "pre_queued" }, { state: "queued" }];
    expect(summarize(cards)).toBe("1 预排队 · 1 排队");
  });

  it("大小写不敏感（服务端可能回大写）", () => {
    expect(summarize([{ state: "QUEUED" }, { state: "Running" }])).toBe("1 排队 · 1 运行中");
  });
});
