/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsMcpTab } from "./SettingsMcpTab";

afterEach(cleanup);

const empty = { configured: false, path: "/p/.yi-agent/mcp.json", enabled: false, servers: [] };

describe("SettingsMcpTab", () => {
  it("explains how to add one when nothing is installed", async () => {
    const call = async () => empty;
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByText("未安装任何 MCP server")).toBeTruthy();
    // 路径取服务端回的真实值，不硬编码 ~/.yi-agent。
    expect(screen.getByText(/\/p\/\.yi-agent\/mcp\.json/)).toBeTruthy();
  });

  it("renders a stdio server with its command and its switch state", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        {
          name: "fs",
          kind: "stdio",
          enabled: false,
          command: "npx",
          args: ["-y", "pkg"],
          tools: null,
        },
      ],
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByText("fs")).toBeTruthy();
    // 徽章换成了开关：读屏语义是「关」。
    const sw = screen.getByRole("switch", { name: "fs 开关" });
    expect(sw.getAttribute("aria-checked")).toBe("false");
    expect(screen.getByText(/npx -y pkg/)).toBeTruthy();
  });

  it("writes a server toggle through mcp/set and adopts the server reply", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "mcp/set") {
        return {
          configured: true,
          path: "/p/.yi-agent/mcp.json",
          enabled: true,
          servers: [
            { name: "fs", kind: "stdio", enabled: true, command: "npx", tools: null },
          ],
        };
      }
      return {
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [
          { name: "fs", kind: "stdio", enabled: false, command: "npx", tools: null },
        ],
      };
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    const sw = await screen.findByRole("switch", { name: "fs 开关" });
    expect(sw.getAttribute("aria-checked")).toBe("false");

    fireEvent.click(sw);

    // 请求参数必须是 scope=server + name + enabled=true。
    const setCall = call.mock.calls.find(([m]) => m === "mcp/set");
    expect(setCall?.[1]).toMatchObject({ scope: "server", name: "fs", enabled: true });
    // 以服务端回读为准。
    await vi.waitFor(() => {
      expect(
        screen.getByRole("switch", { name: "fs 开关" }).getAttribute("aria-checked"),
      ).toBe("true");
    });
  });

  it("rolls back and reports when the write is rejected", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "mcp/set") throw new Error("denied");
      return {
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [
          { name: "fs", kind: "stdio", enabled: false, command: "npx", tools: null },
        ],
      };
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    const sw = await screen.findByRole("switch", { name: "fs 开关" });
    fireEvent.click(sw);

    // 失败必须回滚到原值，并给出可见提示——不静默。
    expect(await screen.findByRole("alert")).toBeTruthy();
    await vi.waitFor(() => {
      expect(
        screen.getByRole("switch", { name: "fs 开关" }).getAttribute("aria-checked"),
      ).toBe("false");
    });
  });

  it("toggles the master switch with scope=master", async () => {
    const call = vi.fn(async (method: string, _params: unknown) => {
      if (method === "mcp/set") {
        return {
          configured: true,
          path: "/p/.yi-agent/mcp.json",
          enabled: false,
          servers: [
            { name: "fs", kind: "stdio", enabled: true, command: "npx", tools: null },
          ],
        };
      }
      return {
        configured: true,
        path: "/p/.yi-agent/mcp.json",
        enabled: true,
        servers: [
          { name: "fs", kind: "stdio", enabled: true, command: "npx", tools: null },
        ],
      };
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    const master = await screen.findByRole("switch", { name: "全局 MCP 开关" });
    fireEvent.click(master);

    const setCall = call.mock.calls.find(([m]) => m === "mcp/set");
    expect(setCall?.[1]).toMatchObject({ scope: "master", enabled: false });
    await vi.waitFor(() => {
      expect(
        screen.getByRole("switch", { name: "全局 MCP 开关" }).getAttribute("aria-checked"),
      ).toBe("false");
    });
  });

  it("distinguishes unprobed from a probe that found no tools", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        { name: "unprobed", kind: "stdio", command: "npx", tools: null },
        { name: "empty", kind: "stdio", command: "npx", tools: [] },
      ],
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByText(/尚未探测/)).toBeTruthy();
    expect(screen.getByText(/未提供任何工具/)).toBeTruthy();
  });

  it("expands a server to list its tools", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [
        {
          name: "fs",
          kind: "stdio",
          command: "npx",
          tools: [{ name: "read_file", description: "读文件", read_only: true }],
          tool_count: 1,
          read_only_count: 1,
        },
      ],
    });
    render(<SettingsMcpTab call={call} projects={[]} />);
    fireEvent.click(await screen.findByRole("button", { name: "展开 fs 的工具清单" }));
    expect(screen.getByText("read_file")).toBeTruthy();
    expect(screen.getByText("读文件")).toBeTruthy();
  });

  it("scopes mcp/list to the selected project and re-lists on switch", async () => {
    const calls: unknown[] = [];
    const call = async (_method: string, params: unknown) => {
      calls.push(params);
      return empty;
    };
    render(<SettingsMcpTab call={call} projects={["/p/first", "/p/second"]} />);
    await screen.findByText("未安装任何 MCP server");
    expect(calls[0]).toEqual({ project: "/p/first" });
    fireEvent.change(screen.getByLabelText("MCP 设置项目"), { target: { value: "/p/second" } });
    await screen.findByText("未安装任何 MCP server");
    expect(calls[1]).toEqual({ project: "/p/second" });
  });

  it("never renders header or env values", async () => {
    const call = async () => ({
      configured: true,
      path: "/p/.yi-agent/mcp.json",
      enabled: true,
      servers: [{ name: "ws", kind: "http", url: "https://x/mcp", header_keys: ["Authorization"] }],
    });
    const { container } = render(<SettingsMcpTab call={call} projects={[]} />);
    await screen.findByText("ws");
    // 键名可见、值不可见（值根本没进前端：服务端只下发键名）。
    expect(screen.getByText(/Authorization/)).toBeTruthy();
    expect(container.textContent).not.toContain("secret-token");
  });

  it("reports a read failure instead of rendering a blank pane", async () => {
    const call = async () => {
      throw new Error("boom");
    };
    render(<SettingsMcpTab call={call} projects={[]} />);
    expect(await screen.findByRole("alert")).toBeTruthy();
  });
});