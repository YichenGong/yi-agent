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
    await fetchBoard(asRpc(inner), "/proj");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "list",
      params: {},
    });
  });

  it("fetchBoard unwraps the cards array", async () => {
    const rpc = asRpc(vi.fn(async () => ({
      cards: [{ id: "c1", state: "queued", progress: null, detail: "/w" }],
    })));
    const cards = await fetchBoard(rpc, "/proj");
    expect(cards).toHaveLength(1);
    expect(cards[0].id).toBe("c1");
  });

  it("fetchBoard tolerates a missing cards field", async () => {
    expect(await fetchBoard(asRpc(vi.fn(async () => ({}))), "/proj")).toEqual([]);
  });

  it("readBoardSwitch returns the resolved switch", async () => {
    const rpc = asRpc(vi.fn(async () => ({ on: true, source: "project" })));
    expect(await readBoardSwitch(rpc, "/proj")).toEqual({ on: true, source: "project" });
  });

  it("readBoardSwitch asks for switch.read", async () => {
    const inner = vi.fn(async () => ({ on: true, source: "project" }));
    await readBoardSwitch(asRpc(inner), "/proj");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "switch.read",
      params: {},
    });
  });

  it("setBoardSwitch writes the requested value", async () => {
    const inner = vi.fn(async () => ({ on: true }));
    await setBoardSwitch(asRpc(inner), "/proj", true);
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "switch.write",
      params: { on: true },
    });
  });

  it("enqueueBoardCard passes both paths through the channel", async () => {
    const inner = vi.fn(async () => ({ id: "c1" }));
    await enqueueBoardCard(asRpc(inner), "/proj", "a.spec.md", "a.plan.md");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "enqueue",
      params: { spec_path: "a.spec.md", plan_path: "a.plan.md" },
    });
  });

  it("每个项目各自成问：project 是参数不是环境", async () => {
    // 同一个客户端连问两个项目，两次请求的 project 必须不同——否则两个项目
    // 会读到同一个看板（这正是「点了没反应」的根因）。
    const inner = vi.fn(async (_method: string, _params: unknown) => ({ on: false, source: "default" }));
    const rpc = asRpc(inner);
    await readBoardSwitch(rpc, "/a");
    await readBoardSwitch(rpc, "/b");
    expect(inner.mock.calls.map((call) => (call[1] as { project: string }).project)).toEqual([
      "/a",
      "/b",
    ]);
  });
});

describe("queryPlugin", () => {
  it("把 project 带进 plugin/query 的参数", async () => {
    const calls: unknown[] = [];
    const rpc = async <T,>(m: string, p: unknown) => { calls.push([m, p]); return {} as T; };
    await readBoardSwitch(rpc, "/proj");
    expect(calls[0]).toEqual(["plugin/query", { plugin: "superpowers-kanban", project: "/proj", method: "switch.read", params: {} }]);
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

  it("recognises a raw RpcError object (the client does not wrap it in Error)", () => {
    // RpcClient 对 JSON-RPC 错误抛的是裸 RpcError 对象，不是 Error 实例。
    expect(
      pluginIsUnavailable({
        code: -32022,
        message: "the plugin rejected the query: NotFound plugin superpowers-kanban is not available",
        data: { code: "plugin_unavailable" },
      }),
    ).toBe(true);
    // 结构化码单独也够：宿主之后改写措辞也不会漏判。
    expect(pluginIsUnavailable({ code: -32022, message: "no", data: { code: "plugin_unavailable" } })).toBe(true);
  });

  it("does not mistake an unrelated failure for a missing plugin", () => {
    expect(pluginIsUnavailable(new Error("daemon is unavailable: connection refused"))).toBe(
      false,
    );
  });
});
