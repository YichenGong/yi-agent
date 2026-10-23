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
    const out = formatUsage(base, null, null, { compactThreshold: 160_000 });
    expect(out).toContain("模型: claude-sonnet-4-20250514");
    expect(out).toContain("input: 12,345  output: 678");
    expect(out).toContain("cache read: 10,000  cache write: 0");
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
    expect(out).toMatch(/估算成本: \$0\.\d{4}|估算成本: < \$0\.01/);
  });

  it("omits the percentage when the threshold is unknown", () => {
    const out = formatUsage(base, null, null, { compactThreshold: null });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
    expect(out).not.toContain("/");
  });

  it("omits the percentage for a zero threshold instead of dividing by zero", () => {
    const out = formatUsage(base, null, null, { compactThreshold: 0 });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
  });

  it("prints an em dash for an unpriced model rather than a fake zero", () => {
    const out = formatUsage({ ...base, model: "some-local-llm" }, null, null, { compactThreshold: 1000 });
    expect(out).toContain("估算成本: —");
  });
});

describe("formatUsage with cumulative totals", () => {
  const totals = { input: 1_234_567, output: 8_900, cacheRead: 2_000_000, cacheWrite: 60 };

  it("prints the session's cumulative counters next to the snapshot", () => {
    const out = formatUsage(base, totals, null, { compactThreshold: 160_000 });
    expect(out).toContain("累计 input: 1,234,567  output: 8,900");
    expect(out).toContain("累计 cache read: 2,000,000  cache write: 60");
    // 上下文占用仍用**最后一次调用**的快照，累计值不能拿去算比例。
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
  });

  it("hides a zero cumulative cache write instead of showing a dead row", () => {
    const out = formatUsage(base, { ...totals, cacheWrite: 0 }, null, { compactThreshold: null });
    expect(out).toContain("累计 cache read: 2,000,000");
    expect(out).not.toContain("cache write");
  });

  it("estimates cost from the cumulative counters", () => {
    const out = formatUsage(base, totals, null, { compactThreshold: null });
    const snapshotCost = formatUsage(base, base, null, { compactThreshold: null });
    expect(out).not.toEqual(snapshotCost);
    expect(out).toMatch(/估算成本: \$/);
  });

  it("prints an em dash when a cumulative-only model is unpriced", () => {
    const out = formatUsage(
      { ...base, model: "mystery" },
      totals, null, { compactThreshold: null },
    );
    expect(out).toContain("估算成本: —");
  });

  it("falls back to the snapshot when the server sent no totals", () => {
    const out = formatUsage(base, null, null, { compactThreshold: 160_000 });
    expect(out).toContain("input: 12,345  output: 678");
    expect(out).not.toContain("累计");
  });
});

describe("formatUsage with subagents", () => {
  const totals = { input: 1_234, output: 56, cacheRead: 7_890, cacheWrite: 0 };
  const subagent = { input: 500, output: 60, cacheRead: 4_000 };

  it("prints a separate subagent section", () => {
    const out = formatUsage(base, totals, subagent, { compactThreshold: null });
    expect(out).toContain("主会话 input: 1,234  output: 56");
    expect(out).toContain("主会话 cache read: 7,890");
    expect(out).toContain("subagent input: 500  output: 60");
    expect(out).toContain("subagent cache read: 4,000");
    expect(out).toContain("（主会话，不含 subagent）");
  });

  it("keeps the single-section format when there are no subagents", () => {
    const out = formatUsage(base, totals, { input: 0, output: 0, cacheRead: 0 }, {
      compactThreshold: null,
    });
    // 无 subagent 时既有文案逐字不变（含"累计"字样、不含"主会话"）。
    // 这不是遗漏：改文案会连带改掉既有测试，且没有信息增益。
    expect(out).toContain("累计 input: 1,234  output: 56");
    expect(out).toContain("累计 cache read: 7,890");
    expect(out).not.toContain("主会话");
    expect(out).not.toContain("subagent");
  });

  it("treats a missing subagent total as none", () => {
    const out = formatUsage(base, totals, null, { compactThreshold: null });
    expect(out).not.toContain("subagent");
    expect(out).toContain("累计 input: 1,234  output: 56");
  });
});
