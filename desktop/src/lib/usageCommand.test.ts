import { describe, it, expect } from "vitest";
import { buildUsageReport } from "./usageCommand";

const base = {
  model: "claude-sonnet-4-20250514",
  input: 12_345,
  output: 678,
  cacheRead: 10_000,
  cacheWrite: 0,
};

const totals = { input: 1_234_567, output: 8_900, cacheRead: 2_000_000, cacheWrite: 60 };

describe("buildUsageReport", () => {
  it("reports the session's cumulative counters and names itself as such", () => {
    const r = buildUsageReport(base, totals, null, { compactThreshold: 160_000 });
    expect(r.title).toBe("Token 用量");
    expect(r.model).toBe("claude-sonnet-4-20250514");
    expect(r.session.isSnapshot).toBe(false);
    expect(r.session.rows).toEqual([
      { label: "Input", value: 1_234_567 },
      { label: "Output", value: 8_900 },
      { label: "Cache read", value: 2_000_000 },
      { label: "Cache write", value: 60 },
    ]);
  });

  it("omits the cache write row when it is zero", () => {
    const r = buildUsageReport(base, { ...totals, cacheWrite: 0 }, null, {
      compactThreshold: null,
    });
    expect(r.session.rows.map((x) => x.label)).toEqual(["Input", "Output", "Cache read"]);
  });

  it("falls back to the snapshot and says so in the title", () => {
    const r = buildUsageReport(base, null, null, { compactThreshold: null });
    // 老服务端没有累计值：标题必须主动说明这是「一轮」，不能让人当成会话总量。
    expect(r.title).toBe("本轮用量");
    expect(r.session.isSnapshot).toBe(true);
    // cache write 为 0 时同样不出行：这条规则与口径无关，快照分支也要遵守。
    expect(r.session.rows).toEqual([
      { label: "Input", value: 12_345 },
      { label: "Output", value: 678 },
      { label: "Cache read", value: 10_000 },
    ]);
  });

  it("carries the context limit when it is known", () => {
    const r = buildUsageReport(base, totals, null, { compactThreshold: 160_000 });
    // 分子必须是快照：累计输入量与「当前上下文还剩多少」无关。
    expect(r.context).toEqual({ used: 12_345, limit: 160_000 });
  });

  it("reports a null context limit for an unknown or zero threshold", () => {
    expect(buildUsageReport(base, totals, null, { compactThreshold: null }).context).toEqual({
      used: 12_345,
      limit: null,
    });
    // 0 会让每一轮都触发压缩，不是真实上限，不能当分母。
    expect(buildUsageReport(base, totals, null, { compactThreshold: 0 }).context).toEqual({
      used: 12_345,
      limit: null,
    });
  });

  it("has no subagent group when there are none", () => {
    expect(buildUsageReport(base, totals, null, { compactThreshold: null }).subagent).toBeNull();
    expect(
      buildUsageReport(base, totals, { input: 0, output: 0, cacheRead: 0 }, {
        compactThreshold: null,
      }).subagent,
    ).toBeNull();
  });

  it("lists the three subagent counters", () => {
    const r = buildUsageReport(base, totals, { input: 500, output: 60, cacheRead: 4_000 }, {
      compactThreshold: null,
    });
    expect(r.subagent).toEqual([
      { label: "Input", value: 500 },
      { label: "Output", value: 60 },
      { label: "Cache read", value: 4_000 },
    ]);
  });

  it("estimates the cost from the cumulative counters", () => {
    const r = buildUsageReport(base, totals, null, { compactThreshold: null });
    expect(r.cost).toMatch(/^\$|^< \$/);
  });

  it("prints an em dash for an unpriced model rather than a fake zero", () => {
    const r = buildUsageReport({ ...base, model: "mystery" }, totals, null, {
      compactThreshold: null,
    });
    expect(r.cost).toBe("—");
  });
});
