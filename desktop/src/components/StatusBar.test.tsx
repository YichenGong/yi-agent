/** @vitest-environment jsdom */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { StatusBar } from "./StatusBar";

afterEach(cleanup);

describe("StatusBar", () => {
  it("renders the connection status and cwd", () => {
    render(<StatusBar cwd="/w" status="connected" totals={null} fallbackUsage={null} />);
    expect(screen.getByText("connected")).toBeTruthy();
    expect(screen.getByText("/w")).toBeTruthy();
  });

  it("no longer renders the model text", () => {
    // 模型已从状态栏移出（改由会话输入区的下拉承担）。这里不再传任何 model，
    // 并钉死一段绝不会来自状态栏的模型字样。`toBeInTheDocument` 不可用
    // （本工作区没有 @testing-library/jest-dom），用等价的 `toBeNull`。
    render(<StatusBar cwd="/w" status="connected" totals={null} fallbackUsage={null} />);
    expect(screen.queryByText("claude-x")).toBeNull();
  });

  it("carries the class the phone CSS hooks on to keep the bar one line", () => {
    // 手机端「状态栏单行」的 CSS 门禁（见 index.css 的
    // `[data-mobile="true"] .app-statusbar`）挂在根元素这个类名上：改名或漏加，
    // 用量文字就会在窄屏折行、把底栏再撑高。这里让 JS 侧与 CSS 侧的锚点对齐。
    const { container } = render(<StatusBar cwd="/w" status="connected" totals={null} fallbackUsage={null} />);
    expect(container.firstElementChild?.className).toContain("app-statusbar");
  });

  it("offers a re-pair entry only when the host passes one", () => {
    const { rerender } = render(<StatusBar cwd="/w" status="connecting" totals={null} fallbackUsage={null} />);
    expect(screen.queryByRole("button", { name: "重新配对" })).toBeNull();

    rerender(<StatusBar cwd="/w" status="connecting" totals={null} fallbackUsage={null} onRepair={() => {}} />);
    expect(screen.getByRole("button", { name: "重新配对" })).toBeTruthy();
  });

  it("invokes the re-pair callback when clicked", () => {
    const onRepair = vi.fn();
    render(<StatusBar cwd="/w" status="connecting" totals={null} fallbackUsage={null} onRepair={onRepair} />);
    fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
    expect(onRepair).toHaveBeenCalledTimes(1);
  });

  it("shows cumulative usage through rolling digits plus one screen-reader sentence", () => {
    const { container } = render(
      <StatusBar
        cwd="/w"
        status="connected"
        totals={{ input: 1_234_567, output: 8_900, cacheRead: 2_000_000, cacheWrite: 0 }}
        fallbackUsage={null}
        usageId="thread-1"
      />,
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

  it("falls back to the last-call snapshot when the server sent no totals", () => {
    const { container } = render(
      <StatusBar
        cwd="/w"
        status="connected"
        totals={null}
        fallbackUsage={{ model: "m", input: 1234, output: 5678, cacheRead: 10, cacheWrite: 0 }}
        usageId="thread-1"
      />,
    );
    // 老服务端没有累计值：显示最后一轮的数，不是空白——但也不能冒充累计。
    expect(container.querySelectorAll(".rolling-number")).toHaveLength(3);
    // 1,234 → 1.2k，单位是 k。
    expect(container.querySelector(".rolling-unit")?.textContent).toBe("k");
    expect(container.querySelector(".sr-only")?.textContent).toContain("in /");
  });

  it("shows nothing when there is neither totals nor a snapshot", () => {
    const { container } = render(
      <StatusBar cwd="/w" status="connected" totals={null} fallbackUsage={null} />,
    );
    expect(container.querySelectorAll(".rolling-number")).toHaveLength(0);
    expect(screen.queryByRole("button", { name: "Token usage details" })).toBeNull();
  });

  it("opens the usage panel on click", () => {
    render(
      <StatusBar
        cwd="/w"
        status="connected"
        totals={{ input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }}
        fallbackUsage={null}
        usageId="thread-1"
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    expect(screen.getByRole("dialog")).toBeTruthy();
  });

  it("opens the usage panel upward, since the bar is the window's last row", () => {
    // 状态栏是 `h-screen` 外壳的最后一行，浮层若向下（`top-full`）展开就整个落到
    // 视口之外：面板撑高文档，右侧因此出现滚动条/黑边（真实 Chrome 实测 800x600
    // 时面板 bottom 超出视口 169px、文档溢出 169px）。必须向上（`bottom-full`）。
    const { container } = render(
      <StatusBar
        cwd="/w"
        status="connected"
        totals={{ input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }}
        fallbackUsage={null}
        usageId="thread-1"
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Token usage details" }));
    const panel = container.querySelector("#usage-panel");
    expect(panel).toBeTruthy();
    const classes = panel?.className.split(/\s+/) ?? [];
    expect(classes).toContain("bottom-full");
    expect(classes).not.toContain("top-full");
  });
});
