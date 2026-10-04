/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen, act } from "@testing-library/react";
import { SettingsPluginsTab } from "./SettingsPluginsTab";

afterEach(cleanup);

describe("SettingsPluginsTab", () => {
  it("shows the empty state when nothing is installed", async () => {
    const call = async () => ({ plugins: [] });
    render(<SettingsPluginsTab call={call} />);
    expect(await screen.findByText("未安装任何插件")).toBeTruthy();
  });

  it("renders a neutral row for a plugin without a registered panel", async () => {
    const call = async () => ({
      plugins: [{ name: "mystery", queryable: true, switch_key: "mystery_on" }],
    });
    render(<SettingsPluginsTab call={call} />);
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
    render(<SettingsPluginsTab call={call} />);
    expect(await screen.findByLabelText("默认并发上限")).toBeTruthy();
    expect(await screen.findByLabelText("推进间隔秒数")).toBeTruthy();
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
    render(<SettingsPluginsTab call={call} />);
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
    const { rerender } = render(<SettingsPluginsTab call={makeCall()} />);
    const input = (await screen.findByLabelText("默认并发上限")) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "7" } });
    expect(input.value).toBe("7");

    // 宿主重渲染换新接缝身份：看板的 2 秒轮询会反复触发这一情形。
    rerender(<SettingsPluginsTab call={makeCall()} />);
    await act(async () => {
      await Promise.resolve();
    });

    // 未保存的编辑不得被重新读回来的服务端值冲掉。
    expect((screen.getByLabelText("默认并发上限") as HTMLInputElement).value).toBe("7");
  });
});
