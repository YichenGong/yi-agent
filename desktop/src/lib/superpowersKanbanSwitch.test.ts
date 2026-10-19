import { describe, expect, it, vi } from "vitest";
import {
  enqueueBoardCard,
  enqueuePrequeuedCard,
  fetchBoard,
  formatSwitch,
  pluginIsUnavailable,
  promoteBoardCard,
  readBoardSwitch,
  readBoardWatchman,
  resolveSwitch,
  setBoardSwitch,
  writeBoardWatchman,
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

  it("enqueuePrequeuedCard sends the trimmed title as enqueue_prequeued", async () => {
    const inner = vi.fn(async () => ({ id: "prequeued-1" }));
    await enqueuePrequeuedCard(asRpc(inner), "/proj", "  先记个标题  ", null);
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "enqueue_prequeued",
      params: { title: "先记个标题" },
    });
  });

  it("enqueuePrequeuedCard sends the discussion session when given", async () => {
    const inner = vi.fn(async () => ({ id: "prequeued-1" }));
    await enqueuePrequeuedCard(asRpc(inner), "/proj", "先记个标题", "thread-abc");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "enqueue_prequeued",
      params: { title: "先记个标题", origin_thread_id: "thread-abc" },
    });
  });

  it("enqueuePrequeuedCard omits the session when there is none", async () => {
    const inner = vi.fn(async () => ({ id: "prequeued-1" }));
    await enqueuePrequeuedCard(asRpc(inner), "/proj", "先记个标题", null);
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "enqueue_prequeued",
      params: { title: "先记个标题" },
    });
  });

  it("enqueuePrequeuedCard refuses a blank title without sending a request", async () => {
    // 插件也会拒，但调用方不该指望 UI 先 trim 好：空白标题在客户端就地挡下。
    for (const title of ["", "   ", "\t\n"]) {
      const inner = vi.fn(async () => ({ id: "prequeued-1" }));
      await expect(
        enqueuePrequeuedCard(asRpc(inner), "/proj", title, null),
      ).rejects.toThrow();
      expect(inner).not.toHaveBeenCalled();
    }
  });

  it("promoteBoardCard passes id and both paths through the channel", async () => {
    const inner = vi.fn(async () => ({ id: "c1" }));
    await promoteBoardCard(asRpc(inner), "/proj", "c1", "a.spec.md", "a.plan.md");
    expect(inner).toHaveBeenCalledWith("plugin/query", {
      plugin: "superpowers-kanban",
      project: "/proj",
      method: "promote",
      params: { id: "c1", spec_path: "a.spec.md", plan_path: "a.plan.md" },
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

describe("background watchman settings", () => {
  type Rpc = Parameters<typeof readBoardWatchman>[0];
  const asRpc = (fn: unknown) => fn as unknown as Rpc;

  it("reads through the host channel, with no project and no plugin", async () => {
    // 宿主级设置：直接走 ui/settings/read。若误走 plugin/query，就会带上 project
    // 去问某个项目的插件——那是另一个问题。
    const inner = vi.fn(async () => ({ resident_daemon_watchman_enabled: true }));
    const on = await readBoardWatchman(asRpc(inner));
    expect(inner).toHaveBeenCalledWith("ui/settings/read", {});
    expect(on).toBe(true);
  });

  it("falls back to the legacy field name an older host still returns", async () => {
    // 旧宿主只回旧名。读不到新名时若直接套缺省，用户关掉的值守会翻回开。
    expect(
      await readBoardWatchman(asRpc(vi.fn(async () => ({ board_watchman_enabled: false })))),
    ).toBe(false);
    // 两个都在时以新名为准：新名一旦写下，旧名就是过期的影子。
    expect(
      await readBoardWatchman(
        asRpc(vi.fn(async () => ({ resident_daemon_watchman_enabled: false, board_watchman_enabled: true }))),
      ),
    ).toBe(false);
  });

  it("defaults to on when the key is unset or not a boolean", async () => {
    // 老配置没有这个键：缺省为开，否则会把用户本来一直在跑的值守悄悄关掉。
    expect(await readBoardWatchman(asRpc(vi.fn(async () => ({}))))).toBe(true);
    expect(
      await readBoardWatchman(asRpc(vi.fn(async () => ({ resident_daemon_watchman_enabled: "yes" })))),
    ).toBe(true);
    expect(
      await readBoardWatchman(asRpc(vi.fn(async () => ({ resident_daemon_watchman_enabled: false })))),
    ).toBe(false);
  });

  it("writes the generic field name the host reads", async () => {
    const inner = vi.fn(async () => ({ warning: "未能在登录项中安装值守" }));
    const result = await writeBoardWatchman(asRpc(inner), false);
    // 宿主只把新名当正式字段（旧名仅兼容读）。写旧名会让「拨开关」在升级后的
    // 宿主上变成写进一个影子键。
    expect(inner).toHaveBeenCalledWith("ui/settings/write", {
      resident_daemon_watchman_enabled: false,
    });
    expect(result.warning).toBe("未能在登录项中安装值守");
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
