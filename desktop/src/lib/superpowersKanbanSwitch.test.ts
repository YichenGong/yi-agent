import { describe, expect, it, vi } from "vitest";
import {
  enqueueBoardCard,
  fetchBoard,
  formatSwitch,
  pluginIsUnavailable,
  readBoardSwitch,
  resolveSwitch,
  setBoardSwitch,
} from "./superpowersKanbanSwitch";

describe("resolveSwitch", () => {
  it("lets the project layer win over the global layer", () => {
    expect(resolveSwitch(true, false)).toEqual({ value: false, source: "project" });
    expect(resolveSwitch(false, true)).toEqual({ value: true, source: "project" });
  });

  it("inherits the global layer when the project layer is unset", () => {
    expect(resolveSwitch(true, null)).toEqual({ value: true, source: "global" });
  });

  it("defaults to disabled when both layers are unset", () => {
    expect(resolveSwitch(null, null)).toEqual({ value: false, source: "default" });
  });
});

describe("formatSwitch", () => {
  it("names the feature and shows the source", () => {
    const text = formatSwitch(true, "project");
    expect(text).toContain("Superpowers 看板");
    expect(text).toContain("on");
    expect(text).toContain("project");
  });

  it("says off when disabled", () => {
    expect(formatSwitch(false, "default")).toContain("off");
  });
});

describe("board RPC wrappers", () => {
  type Rpc = Parameters<typeof fetchBoard>[0];
  const asRpc = (fn: unknown) => fn as unknown as Rpc;

  it("routes every call through the generic plugin channel", async () => {
    const inner = vi.fn(async () => ({ cards: [] }));
    await fetchBoard(asRpc(inner));
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      method: "list",
      params: {},
    });
  });

  it("fetchBoard unwraps the cards array", async () => {
    const rpc = asRpc(vi.fn(async () => ({
      cards: [{ id: "c1", state: "queued", progress: null, detail: "/w" }],
    })));
    const cards = await fetchBoard(rpc);
    expect(cards).toHaveLength(1);
    expect(cards[0].id).toBe("c1");
  });

  it("fetchBoard tolerates a missing cards field", async () => {
    expect(await fetchBoard(asRpc(vi.fn(async () => ({}))))).toEqual([]);
  });

  it("readBoardSwitch returns the resolved switch", async () => {
    const rpc = asRpc(vi.fn(async () => ({ on: true, source: "project" })));
    expect(await readBoardSwitch(rpc)).toEqual({ on: true, source: "project" });
  });

  it("readBoardSwitch asks for switch.read", async () => {
    const inner = vi.fn(async () => ({ on: true, source: "project" }));
    await readBoardSwitch(asRpc(inner));
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      method: "switch.read",
      params: {},
    });
  });

  it("setBoardSwitch writes the requested value", async () => {
    const inner = vi.fn(async () => ({ on: true }));
    await setBoardSwitch(asRpc(inner), true);
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      method: "switch.write",
      params: { on: true },
    });
  });

  it("enqueueBoardCard passes both paths through the channel", async () => {
    const inner = vi.fn(async () => ({ id: "c1" }));
    await enqueueBoardCard(asRpc(inner), "a.spec.md", "a.plan.md");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      method: "enqueue",
      params: { spec_path: "a.spec.md", plan_path: "a.plan.md" },
    });
  });
});

describe("pluginIsUnavailable", () => {
  it("recognises the daemon refusing a plugin it does not supervise", () => {
    // Verbatim shape the daemon produces: NotFound + this message line.
    expect(
      pluginIsUnavailable(
        new Error(
          "the plugin rejected the query: NotFound plugin superpowers-kanban is not available",
        ),
      ),
    ).toBe(true);
  });

  it("recognises the structured unavailable error code", () => {
    expect(pluginIsUnavailable(new Error("PluginUnavailable { plugin: \"x\" }"))).toBe(true);
  });

  it("does not mistake an unrelated failure for a missing plugin", () => {
    expect(pluginIsUnavailable(new Error("daemon is unavailable: connection refused"))).toBe(
      false,
    );
  });
});
