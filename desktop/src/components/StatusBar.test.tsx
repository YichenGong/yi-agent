/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { StatusBar } from "./StatusBar";

afterEach(cleanup);

describe("StatusBar", () => {
  it("renders the connection status and cwd", () => {
    render(<StatusBar cwd="/w" status="connected" usage={null} />);
    expect(screen.getByText("connected")).toBeTruthy();
    expect(screen.getByText("/w")).toBeTruthy();
  });

  it("no longer renders the model text", () => {
    // 模型已从状态栏移出（改由会话输入区的下拉承担）。这里不再传任何 model，
    // 并钉死一段绝不会来自状态栏的模型字样。`toBeInTheDocument` 不可用
    // （本工作区没有 @testing-library/jest-dom），用等价的 `toBeNull`。
    render(<StatusBar cwd="/w" status="connected" usage={null} />);
    expect(screen.queryByText("claude-x")).toBeNull();
  });
});
