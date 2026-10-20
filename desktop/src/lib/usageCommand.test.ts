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
    const out = formatUsage(base, { compactThreshold: 160_000 });
    expect(out).toContain("模型: claude-sonnet-4-20250514");
    expect(out).toContain("input: 12,345  output: 678");
    expect(out).toContain("cache read: 10,000  cache write: 0");
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
    expect(out).toMatch(/估算成本: \$0\.\d{4}|估算成本: < \$0\.01/);
  });

  it("omits the percentage when the threshold is unknown", () => {
    const out = formatUsage(base, { compactThreshold: null });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
    expect(out).not.toContain("/");
  });

  it("omits the percentage for a zero threshold instead of dividing by zero", () => {
    const out = formatUsage(base, { compactThreshold: 0 });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
  });

  it("prints an em dash for an unpriced model rather than a fake zero", () => {
    const out = formatUsage({ ...base, model: "some-local-llm" }, { compactThreshold: 1000 });
    expect(out).toContain("估算成本: —");
  });
});
