import { describe, expect, it } from "vitest";
import {
  listPlugins,
  readKanbanSettings,
  writeKanbanSettings,
  pluginErrorKind,
  pluginProjectPaths,
} from "./pluginSettings";

// `PluginRpc` returns a generic `Promise<T>`, so a concrete stub is not
// assignable without a cast. Same idiom as `superpowersKanbanBoards.test.ts`.
type Rpc = Parameters<typeof listPlugins>[0];

describe("pluginSettings", () => {
  it("maps plugins/list rows", async () => {
    const rpc = async () => ({
      plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "superpowers_kanban" }],
    });
    expect(await listPlugins(rpc as unknown as Rpc)).toEqual([
      { name: "superpowers-kanban", queryable: true, switchKey: "superpowers_kanban" },
    ]);
  });

  it("scopes plugins/list to the project when one is given", async () => {
    const calls: unknown[] = [];
    const rpc = async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { plugins: [] };
    };
    await listPlugins(rpc as unknown as Rpc, "/p/proj");
    expect(calls[0]).toEqual(["plugins/list", { project: "/p/proj" }]);

    // 缺省不带 project：宿主回落到自身 workdir（TUI / 旧调用方的原语义）。
    await listPlugins(rpc as unknown as Rpc);
    expect(calls[1]).toEqual(["plugins/list", {}]);
  });

  it("reads kanban settings through the plugin channel", async () => {
    const calls: unknown[] = [];
    const rpc = async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    const settings = await readKanbanSettings(rpc as unknown as Rpc);
    expect(settings.interval_secs).toBe(10);
    expect(calls[0]).toEqual([
      "plugin/settings/read",
      { plugin: "superpowers-kanban" },
    ]);
  });

  it("scopes plugin/settings read and write to the project", async () => {
    const calls: unknown[] = [];
    const rpc = async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    await readKanbanSettings(rpc as unknown as Rpc, "/p/proj");
    expect(calls[0]).toEqual([
      "plugin/settings/read",
      { plugin: "superpowers-kanban", project: "/p/proj" },
    ]);

    const settings = { default_max_tasks: 5, interval_secs: 20, windows: [] };
    await writeKanbanSettings(rpc as unknown as Rpc, settings, "/p/proj");
    expect(calls[1]).toEqual([
      "plugin/settings/write",
      { plugin: "superpowers-kanban", settings, project: "/p/proj" },
    ]);
  });

  it("writes kanban settings", async () => {
    const calls: unknown[] = [];
    const rpc = async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { ok: true };
    };
    const settings = { default_max_tasks: 5, interval_secs: 20, windows: [] };
    await writeKanbanSettings(rpc as unknown as Rpc, settings);
    expect(calls[0]).toEqual([
      "plugin/settings/write",
      { plugin: "superpowers-kanban", settings },
    ]);
  });

  it("lists board projects first, then recent dirs, deduped and ordered", () => {
    expect(
      pluginProjectPaths(
        [{ path: "/w/a" }, { path: "/w/b" }, { path: "/w/a" }, { path: "  " }],
        ["/w/c", "/w/a"],
      ),
    ).toEqual(["/w/c", "/w/a", "/w/b"]);
    expect(pluginProjectPaths([], [])).toEqual([]);
  });

  it("distinguishes not_installed from not_running", () => {
    expect(pluginErrorKind({ data: { code: "plugin_not_installed" } })).toBe("not_installed");
    expect(pluginErrorKind({ data: { code: "plugin_unavailable" } })).toBe("not_running");
    expect(pluginErrorKind(new Error("boom"))).toBe("other");
  });

  it("gives the plugin's own rejection its own kind", () => {
    expect(pluginErrorKind({ data: { code: "plugin_rejected" } })).toBe("plugin_rejected");
    // 文本退路也要认出来，免得结构化码缺失时又退回 "other"。
    expect(
      pluginErrorKind(new Error("the plugin rejected the query: validation")),
    ).toBe("plugin_rejected");
  });
});
