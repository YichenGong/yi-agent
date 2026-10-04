/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsPluginsTab } from "./SettingsPluginsTab";

afterEach(cleanup);

describe("SettingsPluginsTab", () => {
  it("shows the empty state when nothing is installed", async () => {
    const call = async () => ({ plugins: [] });
    render(<SettingsPluginsTab call={call} projects={[]} />);
    expect(await screen.findByText("未安装任何插件")).toBeTruthy();
  });

  it("renders a neutral row for a plugin without a registered panel", async () => {
    const call = async () => ({
      plugins: [{ name: "mystery", queryable: true, switch_key: "mystery_on" }],
    });
    render(<SettingsPluginsTab call={call} projects={[]} />);
    expect(await screen.findByText("mystery")).toBeTruthy();
    expect(screen.getByText("该插件暂无可配置项")).toBeTruthy();
  });

  it("renders the kanban panel for the kanban plugin", async () => {
    const call = async (method: string) => {
      if (method === "plugins/list") {
        return { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "k" }] };
      }
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    render(<SettingsPluginsTab call={call} projects={[]} />);
    expect(await screen.findByLabelText("默认并发上限")).toBeTruthy();
    expect(await screen.findByLabelText("推进间隔秒数")).toBeTruthy();
  });

  it("scopes plugins/list to the selected project", async () => {
    const calls: Array<[string, unknown]> = [];
    const call = async (method: string, params: unknown) => {
      calls.push([method, params]);
      if (method === "plugins/list") return { plugins: [] };
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
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
          ? { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "k" }] }
          : { plugins: [] };
      }
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    render(
      <SettingsPluginsTab call={call} projects={["/p/first", "/p/second"]} />,
    );
    await screen.findByText("未安装任何插件");
    expect(calls[0]).toEqual({ project: "/p/first" });

    fireEvent.change(screen.getByLabelText("插件设置项目"), {
      target: { value: "/p/second" },
    });

    // 换项目 = 换作用域：重新枚举，且这次能看到该项目里的看板面板。
    expect(await screen.findByLabelText("默认并发上限")).toBeTruthy();
    expect(calls[1]).toEqual({ project: "/p/second" });
  });

  it("keeps the user's input and shows an inline error when saving fails", async () => {
    const call = async (method: string) => {
      if (method === "plugins/list") {
        return { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "k" }] };
      }
      // 写入失败：插件不可用。
      if (method === "plugin/settings/write") {
        throw new Error("plugin_unavailable: the plugin is not available");
      }
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    render(<SettingsPluginsTab call={call} projects={[]} />);
    const input = (await screen.findByLabelText("默认并发上限")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "7" } });
    expect(input.value).toBe("7");

    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toBeTruthy();
    // 失败绝不乐观更新：用户改的值必须还在，且显示的就是他的输入。
    expect((screen.getByLabelText("默认并发上限") as HTMLInputElement).value).toBe("7");
  });

  it("keeps unsaved edits when the call seam identity changes on re-render", async () => {
    const makeCall = () => async (method: string) => {
      if (method === "plugins/list") {
        return { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "k" }] };
      }
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    const { rerender } = render(
      <SettingsPluginsTab call={makeCall()} projects={["/p/proj"]} />,
    );
    const input = (await screen.findByLabelText("默认并发上限")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "7" } });
    expect(input.value).toBe("7");

    // 宿主重渲染换新接缝身份：看板的 2 秒轮询会反复触发这一情形。
    rerender(<SettingsPluginsTab call={makeCall()} projects={["/p/proj"]} />);
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());

    // 未保存的编辑不得被重新读回来的服务端值冲掉。
    expect((screen.getByLabelText("默认并发上限") as HTMLInputElement).value).toBe("7");
  });
});
