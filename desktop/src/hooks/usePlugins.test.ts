import { describe, expect, it } from "vitest";

import type { RpcCall } from "../lib/rpcCall";
import {
  deriveState,
  loadManagedPlugins,
  stateAllowsWrite,
  stateLabel,
  stateRendersView,
  type ManagedPluginRow,
} from "./usePlugins";

function row(overrides: Partial<ManagedPluginRow> = {}): ManagedPluginRow {
  return {
    plugin: "example-counter",
    selected: "1.0.0",
    installed: ["1.0.0"],
    projects: [],
    pendingCleanup: [],
    ...overrides,
  };
}

describe("state derivation keeps every case distinct", () => {
  // 待清理记录来自磁盘：重启后界面仍要显示它。只在卸载的返回值里报一次，
  // 等于重启之后没人知道还有残留。
  it("reads a persisted pending cleanup from the row itself", () => {
    const state = deriveState(
      row({ pendingCleanup: [{ project: "/mnt/usb/proj", remaining: ["manifest still on disk"] }] }),
      { project: "ph1_aaaa" },
    );
    expect(state.kind).toBe("pending-cleanup");
    expect(stateLabel(state)).toBe("待清理");
  });

  it("reports not-installed before looking at grants", () => {
    const state = deriveState(row({ installed: [], selected: null }), { project: "ph1_aaaa" });
    expect(state.kind).toBe("not-installed");
    expect(stateLabel(state)).toBe("未安装");
    expect(stateRendersView(state)).toBe(false);
  });

  it("reports installed-disabled when the project has no grant", () => {
    const state = deriveState(row({ projects: ["ph1_other"] }), { project: "ph1_aaaa" });
    expect(state.kind).toBe("installed-disabled");
    expect(stateAllowsWrite(state)).toBe(false);
    // 未启用时不渲染业务视图：安装前只允许静态介绍。
    expect(stateRendersView(state)).toBe(false);
  });

  it("reports read-only as its own state, never as enabled", () => {
    const state = deriveState(row({ projects: ["ph1_aaaa"] }), {
      project: "ph1_aaaa",
      transaction: { granted: ["Read"], activation: 3 },
    });
    expect(state.kind).toBe("read-only");
    expect(stateLabel(state)).toBe("只读启用");
    expect(stateAllowsWrite(state)).toBe(false);
    // 只读仍可查看：视图要渲染，但写入口必须关掉。
    expect(stateRendersView(state)).toBe(true);
  });

  it("reports running only when a write capability was granted", () => {
    const state = deriveState(row({ projects: ["ph1_aaaa"] }), {
      project: "ph1_aaaa",
      transaction: { granted: ["Read", "Write"], activation: 4 },
    });
    expect(state.kind).toBe("running");
    expect(stateAllowsWrite(state)).toBe(true);
    if (state.kind === "running") expect(state.activation).toBe(4);
  });

  it("reports a failing runtime as failing, not as disabled", () => {
    const state = deriveState(row({ projects: ["ph1_aaaa"] }), {
      project: "ph1_aaaa",
      transaction: { granted: ["Read", "Write"] },
      health: { phase: "failing", detail: "健康检查超时" },
    });
    expect(state.kind).toBe("failing");
    expect(stateLabel(state)).toBe("运行故障");
    expect(stateAllowsWrite(state)).toBe(false);
  });

  it("reports an update that needs new capabilities as waiting", () => {
    const state = deriveState(row({ projects: ["ph1_aaaa"] }), {
      project: "ph1_aaaa",
      transaction: { phase: "draining", requiresConfirmation: ["Resident"] },
    });
    expect(state.kind).toBe("update-waiting");
    expect(stateLabel(state)).toBe("更新等待确认");
    if (state.kind === "update-waiting") expect(state.requires).toEqual(["Resident"]);
  });

  it("reports pending-cleanup for an unreachable project", () => {
    const state = deriveState(row({ projects: ["ph1_gone"] }), {
      project: "ph1_aaaa",
      health: { phase: "pending-cleanup", project: "ph1_gone", detail: "项目目录不可达" },
    });
    expect(state.kind).toBe("pending-cleanup");
    expect(stateLabel(state)).toBe("待清理");
  });

  it("never collapses two states into one label", () => {
    const labels = new Set(
      [
        { installed: [], selected: null, projects: [] },
        { projects: ["ph1_other"] },
        { projects: ["ph1_aaaa"] },
      ].map((overrides) => stateLabel(deriveState(row(overrides as Partial<ManagedPluginRow>), { project: "ph1_aaaa" }))),
    );
    expect(labels.size).toBe(3);
  });
});

describe("loading rows", () => {
  it("asks the versioned management API, not the legacy supervisor one", async () => {
    const methods: string[] = [];
    const call = (async (method: string) => {
      methods.push(method);
      return { plugins: [] };
    }) as RpcCall;
    await loadManagedPlugins(call);
    expect(methods).toEqual(["plugins/v1/list"]);
  });

  it("drops malformed rows instead of inventing data", async () => {
    const call = (async () => ({
      plugins: [{ plugin: "a", installed: ["1.0.0"], projects: [] }, { nope: true }, "junk"],
    })) as RpcCall;
    const rows = await loadManagedPlugins(call);
    expect(rows).toHaveLength(1);
    expect(rows[0].plugin).toBe("a");
  });
});
