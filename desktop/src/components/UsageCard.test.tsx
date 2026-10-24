/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { UsageCard } from "./UsageCard";
import type { UsageReport } from "../lib/usageCommand";

afterEach(cleanup);

const sessionRows = [
  { label: "Input", value: 1_234_567 },
  { label: "Output", value: 8_900 },
  { label: "Cache read", value: 2_000_000 },
];

function report(over: Partial<UsageReport> = {}): UsageReport {
  return {
    title: "Token 用量",
    model: "claude-sonnet-4-20250514",
    context: { used: 12_345, limit: 160_000 },
    session: { rows: sessionRows, isSnapshot: false },
    subagent: null,
    cost: "$1.23",
    ...over,
  };
}

describe("UsageCard", () => {
  it("renders the model, the context occupancy and the session rows", () => {
    const { container } = render(<UsageCard report={report()} />);
    expect(container.querySelector('[data-testid="usage-card"]')).toBeTruthy();
    expect(screen.getByText("Token 用量")).toBeTruthy();
    expect(screen.getByText("claude-sonnet-4-20250514")).toBeTruthy();
    // 千分位与百分比是渲染层的职责：数据层只给数字。
    expect(screen.getByText("12,345 / 160,000 (7.7%)")).toBeTruthy();
    expect(screen.getByText("1,234,567")).toBeTruthy();
    expect(screen.getByText("2,000,000")).toBeTruthy();
    expect(screen.getByText("$1.23")).toBeTruthy();
  });

  it("shows no group headings when there are no subagents", () => {
    render(<UsageCard report={report()} />);
    // 无对照对象时「主会话」是空指涉，只会让人问「子会话在哪」。
    expect(screen.queryByText("主会话")).toBeNull();
    expect(screen.queryByText("Subagents")).toBeNull();
  });

  it("splits into session and subagent groups when subagents exist", () => {
    render(
      <UsageCard
        report={report({
          subagent: [
            { label: "Input", value: 500 },
            { label: "Output", value: 60 },
            { label: "Cache read", value: 4_000 },
          ],
        })}
      />,
    );
    // `getByText` 默认精确匹配，「主会话」与成本行的「（仅主会话）」是两个节点，不会撞。
    expect(screen.getByText("主会话")).toBeTruthy();
    expect(screen.getByText("Subagents")).toBeTruthy();
    expect(screen.getByText("500")).toBeTruthy();
    expect(screen.getByText("4,000")).toBeTruthy();
  });

  it("gives each group its own cost line when subagents exist", () => {
    render(
      <UsageCard report={report({ subagent: [{ label: "Input", value: 500 }] })} />,
    );
    /*
     * 成本按组归属：主会话一行（有价目 → 真数字），subagent 一行（—）。
     * 这取代了原先给合计加「（仅主会话）」脚注的做法 —— 脚注要求读者把那句话
     * 与被限定的数字对上，成本一多就说不清是哪一个。
     */
    expect(screen.getAllByText("成本")).toHaveLength(2);
    expect(screen.getByText("$1.23")).toBeTruthy();
    // subagent 的模型名没随用量上报，无从估价。
    expect(screen.getByText("—")).toBeTruthy();
    expect(screen.queryByText("仅主会话")).toBeNull();
  });

  it("has a single cost line when there are no subagents", () => {
    render(<UsageCard report={report()} />);
    expect(screen.getAllByText("成本")).toHaveLength(1);
    expect(screen.getByText("$1.23")).toBeTruthy();
    expect(screen.queryByText("仅主会话")).toBeNull();
  });

  it("says the numbers are one turn when there are no cumulative totals", () => {
    render(
      <UsageCard
        report={report({ title: "本轮用量", session: { rows: sessionRows, isSnapshot: true } })}
      />,
    );
    expect(screen.getByText("本轮用量")).toBeTruthy();
  });

  it("omits the denominator when the context limit is unknown", () => {
    render(<UsageCard report={report({ context: { used: 12_345, limit: null } })} />);
    expect(screen.getByText("12,345")).toBeTruthy();
    // 用函数匹配器：字符串参数是字面量，`/%/` 永远匹配不到任何节点。
    expect(screen.queryByText((t) => t.includes("%"))).toBeNull();
  });

  it("renders a cache write row when the data carries one", () => {
    render(
      <UsageCard
        report={report({
          session: {
            rows: [...sessionRows, { label: "Cache write", value: 60 }],
            isSnapshot: false,
          },
        })}
      />,
    );
    expect(screen.getByText("Cache write")).toBeTruthy();
    expect(screen.getByText("60")).toBeTruthy();
  });
});
