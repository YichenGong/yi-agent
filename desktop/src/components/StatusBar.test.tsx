/** @vitest-environment jsdom */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { StatusBar } from "./StatusBar";

afterEach(cleanup);

/**
 * 默认「没有 subagent」的 StatusBar。
 *
 * 几乎每个用例都只关心主会话那三个数字；把 subagent 两项收敛到这里，新增
 * prop 时不必逐个调用点补。需要 subagent 的用例显式覆盖即可。
 */
function bar(
  props: Omit<Parameters<typeof StatusBar>[0], "subagentTotals" | "subagentCount"> &
    Partial<Pick<Parameters<typeof StatusBar>[0], "subagentTotals" | "subagentCount">>,
) {
  return (
    <StatusBar
      subagentTotals={{ input: 0, output: 0, cacheRead: 0 }}
      subagentCount={0}
      {...props}
    />
  );
}

describe("StatusBar", () => {
  it("renders the connection status and cwd", () => {
    render(bar({ cwd: "/w", status: "connected", totals: null, fallbackUsage: null }));
    expect(screen.getByText("connected")).toBeTruthy();
    expect(screen.getByText("/w")).toBeTruthy();
  });

  it("no longer renders the model text", () => {
    // 模型已从状态栏移出（改由会话输入区的下拉承担）。这里不再传任何 model，
    // 并钉死一段绝不会来自状态栏的模型字样。`toBeInTheDocument` 不可用
    // （本工作区没有 @testing-library/jest-dom），用等价的 `toBeNull`。
    render(bar({ cwd: "/w", status: "connected", totals: null, fallbackUsage: null }));
    expect(screen.queryByText("claude-x")).toBeNull();
  });

  it("carries the class the phone CSS hooks on to keep the bar one line", () => {
    // 手机端「状态栏单行」的 CSS 门禁（见 index.css 的
    // `[data-mobile="true"] .app-statusbar`）挂在根元素这个类名上：改名或漏加，
    // 用量文字就会在窄屏折行、把底栏再撑高。这里让 JS 侧与 CSS 侧的锚点对齐。
    const { container } = render(bar({ cwd: "/w", status: "connected", totals: null, fallbackUsage: null }));
    expect(container.firstElementChild?.className).toContain("app-statusbar");
  });

  it("offers a re-pair entry only when the host passes one", () => {
    const { rerender } = render(bar({ cwd: "/w", status: "connecting", totals: null, fallbackUsage: null }));
    expect(screen.queryByRole("button", { name: "重新配对" })).toBeNull();

    rerender(bar({ cwd: "/w", status: "connecting", totals: null, fallbackUsage: null, onRepair: () => {} }));
    expect(screen.getByRole("button", { name: "重新配对" })).toBeTruthy();
  });

  it("invokes the re-pair callback when clicked", () => {
    const onRepair = vi.fn();
    render(bar({ cwd: "/w", status: "connecting", totals: null, fallbackUsage: null, onRepair: onRepair }));
    fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
    expect(onRepair).toHaveBeenCalledTimes(1);
  });

  it("shows cumulative usage through rolling digits plus one screen-reader sentence", () => {
    const { container } = render(
      bar({ cwd: "/w", status: "connected", totals: { input: 1_234_567, output: 8_900, cacheRead: 2_000_000, cacheWrite: 0 }, fallbackUsage: null, usageId: "thread-1" }),
    );
    // 折叠态是三组数字：累计 input / 累计 cache read / 累计 output。
    expect(container.querySelectorAll(".rolling-number")).toHaveLength(3);
    // 数字位与单位是分开的节点（逐位滚动 + 独立单位），按结构断言。
    const units = Array.from(container.querySelectorAll(".rolling-unit")).map((e) => e.textContent);
    expect(units).toEqual(["M", "M", "k"]);
    const digitCounts = Array.from(container.querySelectorAll(".rolling-number")).map(
      (n) => n.querySelectorAll(".rolling-digit").length,
    );
    // 1.23M / 2.00M / 8.9k → 3 / 3 / 2 位。
    expect(digitCounts).toEqual([3, 3, 2]);
    expect(container.querySelectorAll(".rolling-point")).toHaveLength(3);
    // 完整文本另有一份，读屏与文本查询都能拿到精确数字。
    const sentence = container.querySelector(".sr-only");
    expect(sentence?.textContent).toBe(
      "1,234,567 cumulative input / 2,000,000 cumulative cache read / 8,900 cumulative output",
    );
    expect(screen.getByRole("button", { name: "Token usage details" })).toBeTruthy();
  });

  it("shows the sum of session and subagent usage when collapsed", () => {
    const { container } = render(
      bar({
        cwd: "/w",
        status: "connected",
        totals: { input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 },
        subagentTotals: { input: 500, output: 50, cacheRead: 5_000 },
        subagentCount: 2,
        fallbackUsage: null,
        usageId: "thread-1",
      }),
    );
    // 折叠态是合计：1000+500 / 10000+5000 / 100+50。
    // 紧凑写法下 1.5k / 15.0k / 150 —— 直接读每个数字的完整文本更稳。
    // 1.5k / 15.0k 有单位；150 是整数，单位为空因而没有对应节点。
    const units = Array.from(container.querySelectorAll(".rolling-unit")).map((e) => e.textContent);
    expect(units).toEqual(["k", "k"]);
    // 读屏句子是精确值，用它钉死「合计」而不只是主会话：
    // output 是 100+50=150（主会话单独的 100 会在这里露馅）。
    const sentence = container.querySelector(".sr-only")?.textContent ?? "";
    expect(sentence).toContain("1,500 cumulative input");
    expect(sentence).toContain("15,000 cumulative cache read");
    expect(sentence).toContain("150 cumulative output");
    expect(sentence).toContain("including subagents, 2 subagents");
  });

  it("keeps the existing sentence when there are no subagents", () => {
    const { container } = render(
      bar({
        cwd: "/w",
        status: "connected",
        totals: { input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 },
        fallbackUsage: null,
        usageId: "thread-1",
      }),
    );
    expect(container.querySelector(".sr-only")?.textContent).toBe(
      "1,000 cumulative input / 10,000 cumulative cache read / 100 cumulative output",
    );
  });

  it("splits session and subagent usage in the expanded panel", () => {
    render(
      bar({
        cwd: "/w",
        status: "connected",
        totals: { input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 },
        subagentTotals: { input: 500, output: 50, cacheRead: 5_000 },
        subagentCount: 2,
        // 给个能定价的模型：这样主会话成本是真数字，唯一的「—」必定来自
        // subagent 那行，断言才有区分度。
        fallbackUsage: { model: "claude-sonnet-4-20250514", input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        usageId: "thread-1",
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    // 两组标题都在，且各自的数字是分开的那一份。
    expect(screen.getByText("Session")).toBeTruthy();
    expect(screen.getByText("Subagents")).toBeTruthy();
    expect(screen.getByText("1,000 tokens")).toBeTruthy();
    expect(screen.getByText("500 tokens")).toBeTruthy();
    expect(screen.getByText("5,000 tokens")).toBeTruthy();
    /*
     * 成本按组归属，而不是给合计加一句「仅主会话」的脚注：
     * 两组各有自己的 Cost 行，Subagents 那份是 —。
     *
     * 之所以是 —：subagent 的模型名没随用量上报（`AgentChildUsage` 只有三个
     * token 字段），没有价目就无从估价。破折号说的是「算不出来」，比把成本
     * 钉在合计旁边、再补一句排除说明更贴近事实，也让两组结构对称。
     */
    // 两组各有一行成本：主会话一行、subagent 一行（后者是 —）。
    expect(screen.getAllByText("Cost")).toHaveLength(2);
    expect(screen.getByText("—")).toBeTruthy();
    // 旧的脚注式限定语必须消失，它与「成本归属到组」是两套表达，同时出现会打架。
    expect(screen.queryByText("（仅主会话）")).toBeNull();
  });

  it("hides the subagent section when there are no subagents", () => {
    render(
      bar({
        cwd: "/w",
        status: "connected",
        totals: { input: 1_000, output: 100, cacheRead: 10_000, cacheWrite: 0 },
        fallbackUsage: null,
        usageId: "thread-1",
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    // 没有 subagent 就不留死行，也不出现成本限定语。
    expect(screen.queryByText("Subagents")).toBeNull();
    expect(screen.queryByText("Session")).toBeNull();
    // 没有 subagent 时只有一行成本（就是全部），不需要限定语，也没有第二组。
    // 不给 model 时这一行的值本身是 —（未知模型），与 subagent 的缺失是两回事。
    expect(screen.getAllByText("Cost")).toHaveLength(1);
    expect(screen.queryByText("（仅主会话）")).toBeNull();
  });

  it("falls back to the last-call snapshot when the server sent no totals", () => {
    const { container } = render(
      bar({ cwd: "/w", status: "connected", totals: null, fallbackUsage: { model: "m", input: 1234, output: 5678, cacheRead: 10, cacheWrite: 0 }, usageId: "thread-1" }),
    );
    // 老服务端没有累计值：显示最后一轮的数，不是空白——但也不能冒充累计。
    expect(container.querySelectorAll(".rolling-number")).toHaveLength(3);
    // 1,234 → 1.2k，单位是 k。
    expect(container.querySelector(".rolling-unit")?.textContent).toBe("k");
    expect(container.querySelector(".sr-only")?.textContent).toContain("in /");
  });

  it("shows nothing when there is neither totals nor a snapshot", () => {
    const { container } = render(
      bar({ cwd: "/w", status: "connected", totals: null, fallbackUsage: null }),
    );
    expect(container.querySelectorAll(".rolling-number")).toHaveLength(0);
    expect(screen.queryByRole("button", { name: "Token usage details" })).toBeNull();
  });

  it("opens the usage panel on click", () => {
    render(
      bar({ cwd: "/w", status: "connected", totals: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }, fallbackUsage: null, usageId: "thread-1" }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("opens the usage panel upward, since the bar is the window's last row", () => {
    // 状态栏是 `h-screen` 外壳的最后一行，浮层若向下（`top-full`）展开就整个落到
    // 视口之外：面板撑高文档，右侧因此出现滚动条/黑边（真实 Chrome 实测 800x600
    // 时面板 bottom 超出视口 169px、文档溢出 169px）。必须向上（`bottom-full`）。
    const { container } = render(
      bar({ cwd: "/w", status: "connected", totals: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }, fallbackUsage: null, usageId: "thread-1" }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    const panel = container.querySelector("#usage-panel");
    expect(panel).toBeTruthy();
    const classes = panel?.className.split(/\s+/) ?? [];
    expect(classes).toContain("bottom-full");
    expect(classes).not.toContain("top-full");
  });
});
