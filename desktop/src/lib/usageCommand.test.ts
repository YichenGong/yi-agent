import { describe, it, expect } from "vitest";
import { formatUsage } from "./usageCommand";

const base = {
  model: "claude-sonnet-4-20250514",
  input: 12_345,
  output: 678,
  cacheRead: 10_000,
  cacheWrite: 0,
};

describe("formatUsage", () => {
  it("shows tokens, context occupancy and the estimated cost", () => {
    const out = formatUsage(base, null, { compactThreshold: 160_000 });
    expect(out).toContain("模型: claude-sonnet-4-20250514");
    expect(out).toContain("input: 12,345  output: 678");
    expect(out).toContain("cache read: 10,000  cache write: 0");
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
    expect(out).toMatch(/估算成本: \$0\.\d{4}|估算成本: < \$0\.01/);
  });

  it("omits the percentage when the threshold is unknown", () => {
    const out = formatUsage(base, null, { compactThreshold: null });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
    expect(out).not.toContain("/");
  });

  it("omits the percentage for a zero threshold instead of dividing by zero", () => {
    const out = formatUsage(base, null, { compactThreshold: 0 });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
  });

  it("prints an em dash for an unpriced model rather than a fake zero", () => {
    const out = formatUsage({ ...base, model: "some-local-llm" }, null, { compactThreshold: 1000 });
    expect(out).toContain("估算成本: —");
  });
});

describe("formatUsage with cumulative totals", () => {
  const totals = { input: 1_234_567, output: 8_900, cacheRead: 2_000_000, cacheWrite: 60 };

  it("prints the session's cumulative counters next to the snapshot", () => {
    const out = formatUsage(base, totals, { compactThreshold: 160_000 });
    expect(out).toContain("累计 input: 1,234,567  output: 8,900");
    expect(out).toContain("累计 cache read: 2,000,000  cache write: 60");
    // 上下文占用仍用**最后一次调用**的快照，累计值不能拿去算比例。
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
  });

  it("hides a zero cumulative cache write instead of showing a dead row", () => {
    const out = formatUsage(base, { ...totals, cacheWrite: 0 }, { compactThreshold: null });
    expect(out).toContain("累计 cache read: 2,000,000");
    expect(out).not.toContain("cache write");
  });

  it("estimates cost from the cumulative counters", () => {
    const out = formatUsage(base, totals, { compactThreshold: null });
    const snapshotCost = formatUsage(base, base, { compactThreshold: null });
    expect(out).not.toEqual(snapshotCost);
    expect(out).toMatch(/估算成本: \$/);
  });

  it("prints an em dash when a cumulative-only model is unpriced", () => {
    const out = formatUsage(
      { ...base, model: "mystery" },
      totals,
      { compactThreshold: null },
    );
    expect(out).toContain("估算成本: —");
  });

  it("falls back to the snapshot when the server sent no totals", () => {
    const out = formatUsage(base, null, { compactThreshold: 160_000 });
    expect(out).toContain("input: 12,345  output: 678");
    expect(out).not.toContain("累计");
  });
});
