/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { ConfigCard } from "./ConfigCard";

afterEach(cleanup);

// A full `config/read` `redacted_view()` as the server sends it.
const sample: Record<string, unknown> = {
  provider: "anthropic",
  api_url: "https://api.anthropic.com",
  api_key: "***",
  model: "claude-sonnet-4",
  max_turns: 20,
  max_resident_subagents: 8,
  max_direct_children: 8,
  workdir: "/Users/me/a/very/long/project/path",
  sandbox: "WorkspaceWrite",
  sandbox_promotable: true,
  yolo: false,
  compact_threshold: 160000,
};

describe("ConfigCard", () => {
  it("titles the card and shows the three groups", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("模型与服务")).toBeTruthy();
    expect(screen.getByText("运行环境")).toBeTruthy();
    expect(screen.getByText("限额")).toBeTruthy();
  });

  it("renders the sandbox mode as its kebab-case wire name", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByText("workspace-write")).toBeTruthy();
    expect(screen.queryByText("WorkspaceWrite")).toBeNull();
  });

  it("maps every known sandbox mode", () => {
    for (const [raw, label] of [
      ["ReadOnly", "read-only"],
      ["WorkspaceWrite", "workspace-write"],
      ["DangerFullAccess", "danger-full-access"],
    ] as const) {
      const { unmount } = render(<ConfigCard config={{ ...sample, sandbox: raw }} />);
      expect(screen.getByText(label)).toBeTruthy();
      unmount();
    }
  });

  it("falls back to the raw string for a prototype key like toString", () => {
    // `in` would light up on `Object.prototype`; only own keys are real modes.
    render(<ConfigCard config={{ sandbox: "toString" }} />);
    expect(screen.getByText("toString")).toBeTruthy();
    expect(screen.queryByText(/native code/)).toBeNull();
  });

  it("renders booleans as 是 / 否", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByText("是")).toBeTruthy(); // sandbox_promotable
    expect(screen.getByText("否")).toBeTruthy(); // yolo
  });

  it("renders yolo:false as 否, not a dash", () => {
    // `false` is a value, not a missing field — this card's only boolean.
    render(<ConfigCard config={{ yolo: false }} />);
    expect(screen.getAllByText("否")).toHaveLength(1);
  });

  it("labels an unset api key honestly", () => {
    render(<ConfigCard config={{ ...sample, api_key: "" }} />);
    expect(screen.getByText("未设置")).toBeTruthy();
  });

  it("shows 已设置（***） when a key is present", () => {
    render(<ConfigCard config={sample} />);
    expect(screen.getByText("已设置（***）")).toBeTruthy();
  });

  it("shows — for a field the response omits", () => {
    // 12 known rows; with only `provider` present the other 11 are dashes.
    render(<ConfigCard config={{ provider: "anthropic" }} />);
    expect(screen.getAllByText("—")).toHaveLength(11);
  });

  it("renders max_turns 0 as 0, not a dash", () => {
    // `0` is a value, not a missing field.
    render(<ConfigCard config={{ max_turns: 0 }} />);
    expect(screen.getByText("0")).toBeTruthy();
    expect(screen.getAllByText("—")).toHaveLength(11);
  });

  it("ignores keys it does not know", () => {
    render(<ConfigCard config={{ ...sample, mystery: "leak" }} />);
    expect(screen.queryByText("mystery")).toBeNull();
    expect(screen.queryByText("leak")).toBeNull();
  });

  it("wraps a long value instead of breaking the card", () => {
    render(<ConfigCard config={sample} />);
    const path = screen.getByText("/Users/me/a/very/long/project/path");
    expect(path.className).toContain("break-all");
    expect(path.className).toContain("min-w-0");
  });

  it("shows a read failure in the same card", () => {
    render(<ConfigCard error="boom" />);
    expect(screen.getByTestId("config-card")).toBeTruthy();
    expect(screen.getByText("当前配置")).toBeTruthy();
    expect(screen.getByText("读取配置失败: boom")).toBeTruthy();
  });
});
