/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsPluginsTab } from "./SettingsPluginsTab";

afterEach(cleanup);

describe("SettingsPluginsTab", () => {
  it("shows the empty state when nothing is installed", async () => {
    const call = async () => ({ plugins: [] });
    render(<SettingsPluginsTab call={call} projects={[]} />);
    expect(await screen.findByText("未安装任何插件")).toBeTruthy();
  });

  it("renders a neutral row for any plugin, without a host-side panel registry", async () => {
    // 主程序不再有「插件名 → 组件」的表：任何名字都走同一条中性行，设置由插件
    // 自己提供的受管视图打开。
    const call = async () => ({
      plugins: [{ name: "mystery", queryable: true, switch_key: "mystery_on" }],
    });
    render(<SettingsPluginsTab call={call} projects={[]} />);
    expect(await screen.findByText("mystery")).toBeTruthy();
    expect(
      screen.getByText("该插件的设置由它自己提供，见上方「受管插件」里的设置入口。"),
    ).toBeTruthy();
  });

  it("treats a plugin the host knows nothing about like any other", async () => {
    // 回归：曾经有一个插件名在注册表里、其余走中性行；现在两者必须一模一样，
    // 否则"加一个插件不用改主程序"就是假的。
    const call = async () => ({
      plugins: [
        { name: "superpowers-kanban", queryable: true, switch_key: "a" },
        { name: "example-counter", queryable: true, switch_key: "b" },
      ],
    });
    const { container } = render(<SettingsPluginsTab call={call} projects={[]} />);
    await screen.findByText("superpowers-kanban");
    const rows = Array.from(container.querySelectorAll("section"));
    expect(rows).toHaveLength(2);
    for (const row of rows) {
      expect(
        row.textContent,
      ).toContain("该插件的设置由它自己提供");
    }
  });

  it("scopes plugins/list to the selected project", async () => {
    const calls: Array<[string, unknown]> = [];
    const call = async (method: string, params: unknown) => {
      calls.push([method, params]);
      if (method === "plugins/list") return { plugins: [] };
      return {};
    };
    render(<SettingsPluginsTab call={call} projects={["/p/proj"]} />);
    await screen.findByText("未安装任何插件");
    // 根因回归：不再去宿主 workdir（home）找清单，而是带着项目问。
    expect(calls).toContainEqual(["plugins/list", { project: "/p/proj" }]);
  });

  it("re-lists plugins when the user switches project", async () => {
    const calls: unknown[] = [];
    const call = async (method: string, params: unknown) => {
      if (method === "plugins/list") {
        calls.push(params);
        const project = (params as { project?: string }).project;
        return project === "/p/second"
          ? { plugins: [{ name: "second-only", queryable: true, switch_key: "k" }] }
          : { plugins: [] };
      }
      return {};
    };
    render(<SettingsPluginsTab call={call} projects={["/p/first", "/p/second"]} />);
    await screen.findByText("未安装任何插件");
    expect(calls[0]).toEqual({ project: "/p/first" });

    fireEvent.change(screen.getByLabelText("插件设置项目"), {
      target: { value: "/p/second" },
    });

    // 换项目 = 换作用域：重新枚举，且这次能看到该项目里的插件。
    expect(await screen.findByText("second-only")).toBeTruthy();
    expect(calls[1]).toEqual({ project: "/p/second" });
  });

  it("marks a switched-off plugin without hiding it", async () => {
    const call = async (method: string) => {
      if (method === "plugins/list") {
        return {
          plugins: [
            { name: "superpowers-kanban", queryable: true, switch_key: "k", enabled: false },
          ],
        };
      }
      return {};
    };
    render(<SettingsPluginsTab call={call} projects={["/p/proj"]} />);
    expect(await screen.findByText(/已停用/)).toBeTruthy();
    // 仍列出来：用户得能从这一页看出它为什么没在跑，而不是整块消失。
    expect(screen.getByText("superpowers-kanban")).toBeTruthy();
  });

  it("treats an old host that omits enabled as running", async () => {
    const call = async (method: string) => {
      if (method === "plugins/list") {
        return { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "k" }] };
      }
      return {};
    };
    render(<SettingsPluginsTab call={call} projects={["/p/proj"]} />);
    await screen.findByText("superpowers-kanban");
    expect(screen.queryByText(/已停用/)).toBeNull();
  });
});

describe("SettingsPluginsTab managed section", () => {
  it("shows managed plugins only when a project handle is supplied", async () => {
    const call = async (method: string) => {
      if (method === "plugins/list") return { plugins: [] };
      if (method === "plugins/v1/list") {
        return {
          plugins: [
            { plugin: "example-counter", selected: "1.0.0", installed: ["1.0.0"], projects: ["ph1_aaaa"] },
          ],
        };
      }
      return {};
    };

    const { unmount } = render(<SettingsPluginsTab call={call} projects={[]} />);
    await screen.findByText("未安装任何插件");
    // 没有句柄时不显示受管分区：路径不能冒充句柄。
    expect(screen.queryByText("受管插件")).toBeNull();
    unmount();

    render(<SettingsPluginsTab call={call} projects={[]} projectHandle="ph1_aaaa" />);
    expect(await screen.findByText("受管插件")).toBeTruthy();
    expect(await screen.findByText("example-counter")).toBeTruthy();
  });
});
