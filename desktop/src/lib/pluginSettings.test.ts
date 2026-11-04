import { describe, expect, it } from "vitest";
import { pluginErrorKind, pluginProjectPaths } from "./pluginSettings";

// `PluginRpc` returns a generic `Promise<T>`, so a concrete stub is not
// assignable without a cast.
describe("pluginSettings", () => {
  it("lists extra projects first, then recent dirs, deduped and ordered", () => {
    expect(
      pluginProjectPaths(
        [{ path: "/w/a" }, { path: "/w/b" }, { path: "/w/a" }, { path: "  " }],
        ["/w/c", "/w/a"],
      ),
    ).toEqual(["/w/c", "/w/a", "/w/b"]);
    expect(pluginProjectPaths([], [])).toEqual([]);
    // 缺省不额外点名任何项目：只用最近目录。
    expect(pluginProjectPaths([{ path: "/w/a" }])).toEqual(["/w/a"]);
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
