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
    // `inspectLegacy` 是只读探测，允许在加载时调用；写方法一个都不许有。
    expect(methods).toContain("plugins/v1/list");
    expect(methods.every((method) => method === "plugins/v1/list" || method === "plugins/v1/inspectLegacy")).toBe(
      true,
    );
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
    // 目录加载完成后，除了 list（与只读的 inspectLegacy）没有任何管理**写**
    // 操作被隐式触发。这里按"写方法"判断而不是枚举调用序列：探测是只读的，
    // 允许它出现；而 enable/grant/activate/adopt 出现一次就是真 bug。
    const writes = calls
      .map(([method]) => method)
      .filter((method) => method !== "plugins/v1/list" && method !== "plugins/v1/inspectLegacy");
    expect(writes).toEqual([]);
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

describe("PluginCatalog legacy adoption", () => {
  const legacyReport = {
    findings: [
      { kind: "boardData", path: "/p/.yi-agent/superpowers-kanban", project: "/p", detail: "can be adopted" },
      { kind: "userSkill", path: "/h/skills/superpowers-kanban/SKILL.md", detail: "a user copy" },
      { kind: "pathBinary", path: "/usr/local/bin/superpowers-kanban", detail: "on PATH" },
    ],
    rejectedInputs: [],
  };

  function legacyRpc(calls: Array<[string, unknown]>) {
    return rpc((method, params) => {
      calls.push([method, params]);
      // 必须"已安装"：旧布局的规则来自那份插件的清单，没装就没有可查的自述。
      if (method === "plugins/v1/list") return rowsPlugin([], ["1.0.0"]);
      if (method === "plugins/v1/inspectLegacy") return legacyReport;
      if (method === "plugins/v1/adoptLegacy") return { activation: 1, backup: "/home/plugins/backups/tx-1" };
      return {};
    });
  }

  it("separates what can be adopted from what is left untouched", async () => {
    const calls: Array<[string, unknown]> = [];
    render(
      <PluginCatalog call={legacyRpc(calls)} project="ph1_aaaa" declarations={declarations} />,
    );

    expect(await screen.findByText("旧安装")).toBeTruthy();
    // 用户自定义 skill 与 PATH 上的旧程序被单列，并明确写着不会删除。
    // 文案被 `<strong>` 拆成多段，且祖先元素也包含它；用最内层的那一个。
    const untouchedNotice = screen
      .getAllByText((_, element) => element?.textContent?.includes("不会被删除或改动") ?? false)
      .find((element) => element.tagName === "P");
    expect(untouchedNotice).toBeTruthy();
    expect(screen.getByText("/h/skills/superpowers-kanban/SKILL.md")).toBeTruthy();
    expect(screen.getByText("/usr/local/bin/superpowers-kanban")).toBeTruthy();
  });

  it("sends the capabilities the user ticked, not the legacy switch", async () => {
    const calls: Array<[string, unknown]> = [];
    render(
      <PluginCatalog call={legacyRpc(calls)} project="ph1_aaaa" declarations={declarations} />,
    );
    await screen.findByText("旧安装");

    // 取消勾选 Write，只留 Read：请求体必须如实只带 Read。
    fireEvent.click(screen.getByLabelText(/写入数据/));
    fireEvent.click(screen.getByText("接管这个项目"));

    await waitFor(() => {
      const adopt = calls.find(([method]) => method === "plugins/v1/adoptLegacy");
      expect(adopt).toBeTruthy();
      expect(adopt?.[1]).toEqual({
        plugin: "example-counter",
        project: "ph1_aaaa",
        capabilities: ["Read"],
      });
    });
  });

  it("does not offer adoption when no project is selected", async () => {
    const calls: Array<[string, unknown]> = [];
    render(<PluginCatalog call={legacyRpc(calls)} declarations={declarations} />);

    await screen.findByText("旧安装");
    expect(screen.getByText("需要先选择一个项目才能接管。")).toBeTruthy();
    expect(screen.queryByText("接管这个项目")).toBeNull();
  });

  it("asks the host about each installed plugin by name", async () => {
    const calls: Array<[string, unknown]> = [];
    render(
      <PluginCatalog call={legacyRpc(calls)} project="ph1_aaaa" declarations={declarations} />,
    );
    await screen.findByText("旧安装");

    // 规则来自清单，所以必须带上具体插件名；不传插件等于让宿主猜该找什么。
    const probes = calls.filter(([method]) => method === "plugins/v1/inspectLegacy");
    expect(probes).toEqual([
      ["plugins/v1/inspectLegacy", { plugin: "example-counter", projects: ["ph1_aaaa"] }],
    ]);
  });
});

describe("PluginCatalog uninstall", () => {
  /** 一个"装了、且当前项目已授权"的实例，用来露出卸载入口。 */
  function installedRpc(calls: Array<[string, unknown]>, uninstall: unknown) {
    return rpc((method, params) => {
      calls.push([method, params]);
      if (method === "plugins/v1/list") return rowsPlugin(["ph1_aaaa"], ["1.0.0"]);
      if (method === "plugins/v1/uninstall") return uninstall;
      if (method === "plugins/v1/transaction") return { phase: "idle", granted: ["Read", "Write"] };
      return {};
    });
  }

  it("uninstalls without asking to delete data", async () => {
    const calls: Array<[string, unknown]> = [];
    render(
      <PluginCatalog
        call={installedRpc(calls, { versionsRemoved: 1, grantsRevoked: 1, retained: [], pending: [] })}
        project="ph1_aaaa"
        declarations={declarations}
      />,
    );

    fireEvent.click(await screen.findByText("卸载"));

    await waitFor(() => {
      const call = calls.find(([method]) => method === "plugins/v1/uninstall");
      // 数据默认保留：删数据是另一个显式动作，不该被顺带做掉。
      expect(call?.[1]).toEqual({ plugin: "example-counter", removeData: false });
    });
  });

  it("reports the projects that still need cleanup instead of claiming success", async () => {
    const calls: Array<[string, unknown]> = [];
    render(
      <PluginCatalog
        call={installedRpc(calls, {
          versionsRemoved: 1,
          grantsRevoked: 1,
          retained: [],
          pending: [{ project: "/mnt/usb/proj", remaining: ["the project directory is not reachable"] }],
        })}
        project="ph1_aaaa"
        declarations={declarations}
      />,
    );

    fireEvent.click(await screen.findByText("卸载"));

    expect(await screen.findByText("/mnt/usb/proj")).toBeTruthy();
    expect(screen.getByText(/还有 1 个项目没有清干净/)).toBeTruthy();
    expect(screen.getByText("the project directory is not reachable")).toBeTruthy();
  });
});
