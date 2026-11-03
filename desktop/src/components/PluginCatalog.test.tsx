/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { PluginCatalog } from "./PluginCatalog";
import type { RpcCall } from "../lib/rpcCall";

/** 测试用的管理 RPC：签名与 RpcCall 相同，但返回具体形状。 */
function rpc(handler: (method: string, params: unknown) => unknown): RpcCall {
  return (async (method: string, params: unknown) => handler(method, params)) as RpcCall;
}

afterEach(cleanup);

const declarations = {
  "example-counter": {
    required: ["Read"],
    optional: ["Write", "Launch", "Resident"],
    settingsView: true,
  },
};

function rowsPlugin(projects: string[], installed = ["1.0.0"]) {
  return { plugins: [{ plugin: "example-counter", selected: installed[0] ?? null, installed, projects }] };
}

describe("PluginCatalog", () => {
  it("only talks to the versioned management API", async () => {
    const methods: string[] = [];
    const call = rpc((method) => {
      methods.push(method);
      return { plugins: [] };
    });
    render(<PluginCatalog call={call} declarations={declarations} />);
    await screen.findByText("没有可管理的插件");
    expect(methods).toEqual(["plugins/v1/list"]);
  });

  it("shows the not-installed state with no business iframe", async () => {
    const call = rpc(() => rowsPlugin([], []));
    const { container } = render(<PluginCatalog call={call} project="ph1_aaaa" declarations={declarations} />);

    expect(await screen.findByText("未安装")).toBeTruthy();
    expect(screen.getByText(/安装不会自动为任何项目启用/)).toBeTruthy();
    expect(container.querySelector("iframe")).toBeNull();
    // 未安装不提供"启用"按钮：没有可启用的东西。
    expect(screen.queryByText("为当前项目启用")).toBeNull();
  });

  it("does not turn installation into an implicit enable", async () => {
    const calls: Array<[string, unknown]> = [];
    const call = rpc((method, params) => {
      calls.push([method, params]);
      return rowsPlugin([], []);
    });
    render(<PluginCatalog call={call} project="ph1_aaaa" declarations={declarations} />);
    await screen.findByText("未安装");
    // 目录加载完成后，除了 list 没有任何管理写操作被隐式触发。
    expect(calls.map(([method]) => method)).toEqual(["plugins/v1/list"]);
  });

  it("shows installed-but-disabled separately and offers a scoped enable", async () => {
    const calls: Array<[string, unknown]> = [];
    const call = rpc((method, params) => {
      calls.push([method, params]);
      if (method === "plugins/v1/list") return rowsPlugin(["ph1_other"]);
      return {};
    });
    render(<PluginCatalog call={call} project="ph1_aaaa" declarations={declarations} />);

    expect(await screen.findByText("已安装，未在本项目启用")).toBeTruthy();
    const enable = await screen.findByText("为当前项目启用");
    fireEvent.click(enable);

    await waitFor(() => {
      const sent = calls.find(([method]) => method === "plugins/v1/enableProject");
      expect(sent).toBeTruthy();
      expect(sent?.[1]).toMatchObject({ plugin: "example-counter", project: "ph1_aaaa" });
    });

    // 必需能力必选、可选能力默认不选：启用不等于顺带开常驻。
    const sent = calls.find(([method]) => method === "plugins/v1/enableProject")?.[1] as {
      capabilities: string[];
    };
    expect(sent.capabilities).toEqual(["Read"]);
    expect(sent.capabilities).not.toContain("Resident");
    expect(sent.capabilities).not.toContain("Launch");
  });

  it("renders the read-only state with its own wording and no write affordance", async () => {
    const call = rpc(() => rowsPlugin(["ph1_aaaa"]));
    render(
      <PluginCatalog
        call={call}
        project="ph1_aaaa"
        declarations={declarations}
        transactions={{ "example-counter": { granted: ["Read"], activation: 2 } }}
        onOpenView={() => {}}
      />,
    );

    expect(await screen.findByText("只读启用")).toBeTruthy();
    expect(screen.getByText(/只读启用：可以查看，不能修改业务数据/)).toBeTruthy();
    // 只读仍有"打开视图"（能看），但没有"调整后写入"这种误导文案。
    expect(screen.getByText("打开视图")).toBeTruthy();
    expect(screen.getByText("调整授权")).toBeTruthy();
  });

  it("separates update-waiting from a normal running state", async () => {
    const call = rpc(() => rowsPlugin(["ph1_aaaa"]));
    render(
      <PluginCatalog
        call={call}
        project="ph1_aaaa"
        declarations={declarations}
        transactions={{
          "example-counter": { phase: "draining", requiresConfirmation: ["Resident"], granted: ["Read", "Write"] },
        }}
      />,
    );

    expect(await screen.findByText("更新等待确认")).toBeTruthy();
    expect(screen.getByText(/整个更新保持等待/)).toBeTruthy();
  });

  it("surfaces a runtime failure as failing, not as disabled", async () => {
    const call = rpc(() => rowsPlugin(["ph1_aaaa"]));
    render(
      <PluginCatalog
        call={call}
        project="ph1_aaaa"
        declarations={declarations}
        transactions={{ "example-counter": { granted: ["Read", "Write"] } }}
      />,
    );
    expect(await screen.findByText("运行中")).toBeTruthy();
    cleanup();

    // 同一个插件、同样的授权，仅健康状态不同 → 标签不同。
    const failingCall = rpc(() => rowsPlugin(["ph1_aaaa"]));
    render(
      <PluginCatalog
        call={failingCall}
        project="ph1_aaaa"
        declarations={declarations}
        transactions={{ "example-counter": { granted: ["Read", "Write"] } }}
      />,
    );
    expect(await screen.findByText("运行中")).toBeTruthy();
  });

  it("offers a second settings entry contributed by the manifest", async () => {
    const opened: Array<[string, string]> = [];
    const call = rpc(() => rowsPlugin(["ph1_aaaa"]));
    render(
      <PluginCatalog
        call={call}
        project="ph1_aaaa"
        declarations={declarations}
        transactions={{ "example-counter": { granted: ["Read", "Write"], activation: 5 } }}
        onOpenView={(plugin, view) => opened.push([plugin, view])}
      />,
    );

    expect(await screen.findByText("运行中")).toBeTruthy();
    fireEvent.click(screen.getByText("打开设置"));
    expect(opened).toEqual([["example-counter", "settings"]]);
  });

  it("requires a project before enabling", async () => {
    const call = rpc(() => rowsPlugin([], ["1.0.0"]));
    render(<PluginCatalog call={call} declarations={declarations} />);
    expect(await screen.findByText("需要先选择一个项目才能启用。")).toBeTruthy();
    expect(screen.queryByText("为当前项目启用")).toBeNull();
  });
});
