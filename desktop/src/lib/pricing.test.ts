import { describe, it, expect } from "vitest";
import { estimateCost, formatCost, priceFor } from "./pricing";

const usage = (over: Partial<Parameters<typeof estimateCost>[0]>) => ({
  model: "claude-sonnet-4-20250514",
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
  ...over,
});

describe("priceFor", () => {
  it("matches by longest prefix", () => {
    // shorter prefixes are listed BEFORE their longer extensions, so a broken
    // first-match-wins implementation would fail these assertions.
    expect(priceFor("gpt-4o-mini")?.input).toBe(0.15);
    expect(priceFor("gpt-4o")?.input).toBe(2.5);
    expect(priceFor("o1-mini")?.input).toBe(1.1);
    expect(priceFor("o1")?.input).toBe(15);
    expect(priceFor("o3-mini")?.input).toBe(1.1);
    expect(priceFor("o3")?.input).toBe(10);
  });

  it("returns null for unknown models", () => {
    expect(priceFor("llama-3")).toBeNull();
  });
});

describe("estimateCost", () => {
  it("weights all four token kinds", () => {
    // sonnet-4: input 3, output 15, cacheRead 0.3, cacheWrite 3.75 (USD/1M)
    const cost = estimateCost(
      usage({ input: 1_000_000, output: 1_000_000, cacheRead: 1_000_000, cacheWrite: 1_000_000 }),
    );
    expect(cost).toBeCloseTo(3 + 15 + 0.3 + 3.75, 6);
  });

  it("returns null for unknown models", () => {
    expect(estimateCost(usage({ model: "llama-3" }))).toBeNull();
  });
});

describe("formatCost", () => {
  it("shows em dash for null", () => {
    expect(formatCost(null)).toBe("—");
  });
  it("shows < $0.01 for tiny non-zero costs", () => {
    expect(formatCost(0.0001)).toBe("< $0.01");
  });
  it("shows four decimals otherwise", () => {
    expect(formatCost(0.0123)).toBe("$0.0123");
  });
});
