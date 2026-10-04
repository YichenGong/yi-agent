import { describe, expect, it } from "vitest";
import {
  listPlugins,
  readKanbanSettings,
  writeKanbanSettings,
  pluginErrorKind,
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

  it("distinguishes not_installed from not_running", () => {
    expect(pluginErrorKind({ data: { code: "plugin_not_installed" } })).toBe("not_installed");
    expect(pluginErrorKind({ data: { code: "plugin_unavailable" } })).toBe("not_running");
    expect(pluginErrorKind(new Error("boom"))).toBe("other");
  });
});
