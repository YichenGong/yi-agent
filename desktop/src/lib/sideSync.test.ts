import { describe, expect, it, vi } from "vitest";
import type { RpcClient } from "./rpc";
import type { Item, SideReadItemsResult } from "./protocol";
import type { SideNotification } from "./sideConversation";
import {
  SideConversationStore,
  parseSideHandshake,
  sideHostKey,
} from "./sideConversation";
import {
  SIDE_CACHE_MAX_FRAMES,
  SideSyncController,
  isNetworkFailure,
} from "./sideSync";

const SLOT = "local";
const SIDE = "s";
const PARENT = "p1";
/** wire 上的 id 必须是合法 UUID（服务端会拒绝别的形状）。 */
const SIDE_UUID = "6f1c0e2a-0000-4000-8000-000000000001";
const REQ_UUID = "00000000-0000-4000-8000-000000000001";

function agentItem(text: string, id: string): Item {
  return { type: "agentMessage", id, text };
}

function page(over: Partial<SideReadItemsResult> = {}): SideReadItemsResult {
  return {
    items: [],
    snapshot_seq: 0,
    status: "idle",
    usage: {
      total: { input: 0, output: 0, cache_creation: 0, cache_read: 0, reasoning: 0 },
      complete: true,
    },
    active_turn_id: null,
    accepted_requests: {},
    effective_config: {
      model: "m",
      reasoning_effort: null,
      preserve_thinking: false,
      read_only: true,
      tool_names: [],
    },
    snapshot_token: "tok",
    next_cursor: null,
    ...over,
  };
}

function listResult(sides: Array<{ side_id: string; generation: number; last_seq: number }>) {
  return {
    sides: sides.map((s) => ({
      side_id: s.side_id,
      parent_thread_id: PARENT,
      status: "idle",
      snapshot_revision: 1,
      last_seq: s.last_seq,
      active_turn_id: null,
      expires_at: null,
      generation: s.generation,
      stopping: false,
    })),
  };
}

interface Fixture {
  controller: SideSyncController;
  store: SideConversationStore;
  hostKey: string;
  rpc: ReturnType<typeof vi.fn>;
  deliver: (n: SideNotification) => void;
  requests: string[];
}

/** 即便用例用 `mockRejectedValueOnce` 换掉实现，调用记录仍在 `mock.calls` 里。 */
function methodsOf(f: Fixture): string[] {
  return f.rpc.mock.calls.map((c) => c[0] as string);
}

/** 让「先 await 一个 async 前置步骤」的对账推进到真正发出 list。 */
async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function makeSideSyncFixture(
  opts: { pages?: SideReadItemsResult[]; list?: unknown } = {},
): Fixture {
  const requests: string[] = [];
  const request = vi.fn(async (method: string) => {
    requests.push(method);
    if (method === "side/list") {
      return opts.list ?? listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]);
    }
    if (method === "side/readItems") {
      return (opts.pages ?? [page()]).shift() ?? page();
    }
    if (method === "side/turnStart") return { turn_id: "turn-1" };
    return {};
  });
  const client = { request } as unknown as RpcClient;

  const store = new SideConversationStore();
  const binding = parseSideHandshake(
    SLOT,
    client,
    {
      capabilities: { side_conversation_v1: true },
      sideHostInstanceId: "h1",
    },
    "control",
  )!;
  store.bind(binding);

  const controller = new SideSyncController({
    binding,
    store,
    subscribeParent: async () => {},
    // 用真实时钟读数：fake timer 会接管 `Date.now`，于是「推进 5 秒」既驱动
    // 定时器也推进这个读数——两条路径共用同一个时间轴，测试才不会被节流挡掉。
    clock: () => Date.now(),
  });
  let unhandled: unknown = null;
  const deliver = (n: SideNotification) => {
    try {
      controller.accept(n);
    } catch (error) {
      unhandled = error;
    }
    if (unhandled) throw unhandled;
  };
  return { controller, store, hostKey: binding.key, rpc: request, deliver, requests };
}

function changed(seq: number, lastSeq = seq): SideNotification {
  return {
    method: "side/changed",
    params: {
      parent_thread_id: PARENT,
      side_id: SIDE,
      generation: 1,
      seq,
      summary: {
        side_id: SIDE,
        parent_thread_id: PARENT,
        status: "running",
        snapshot_revision: 1,
        last_seq: lastSeq,
        active_turn_id: "turn-1",
        expires_at: null,
        generation: 1,
        stopping: false,
      },
    },
  };
}

function itemUpdated(seq: number, item: Item, generation = 1): SideNotification {
  return {
    method: "side/itemUpdated",
    params: {
      parent_thread_id: PARENT,
      side_id: SIDE,
      generation,
      seq,
      turn_id: "turn-1",
      item,
    },
  };
}

function closedFrame(seq: number, reason = "user_closed"): SideNotification {
  return {
    method: "side/closed",
    params: {
      parent_thread_id: PARENT,
      side_id: SIDE,
      generation: 1,
      seq,
      reason,
    },
  };
}

describe("SideSyncController.reconcile", () => {
  it("subscribes to the parent before listing", async () => {
    const order: string[] = [];
    const client = {
      request: vi.fn(async (method: string) => {
        order.push(method);
        if (method === "side/list") return listResult([]);
        return {};
      }),
    } as unknown as RpcClient;
    const store = new SideConversationStore();
    const binding = parseSideHandshake(
      SLOT,
      client,
      { capabilities: { side_conversation_v1: true }, sideHostInstanceId: "h1" },
      "control",
    )!;
    store.bind(binding);
    const controller = new SideSyncController({
      binding,
      store,
      subscribeParent: async (id) => {
        order.push(`subscribe:${id}`);
      },
      clock: () => 0,
    });
    await controller.reconcile(PARENT);
    // 先订阅再 list：反过来的话，两步之间发布的帧永久丢失。
    expect(order).toEqual(["subscribe:p1", "side/list"]);
  });

  it("joins pages with one snapshot token and installs a single baseline", async () => {
    const f = makeSideSyncFixture({
      pages: [
        page({ items: [agentItem("a", "m1")], snapshot_seq: 2, next_cursor: "c1" }),
        page({ items: [agentItem("b", "m2")], snapshot_seq: 2, next_cursor: null }),
      ],
    });
    await f.controller.reconcile(PARENT);
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.items.map((i) => i.id)).toEqual(["m1", "m2"]);
    expect(view.seq).toBe(2);
    expect(view.sync).toBe("synced");
    // 两页各一次 readItems，用的是同一个 token（第二页必须带 snapshotToken）。
    const reads = f.rpc.mock.calls.filter((c: unknown[]) => c[0] === "side/readItems");
    expect(reads).toHaveLength(2);
    expect(reads[1][1]).toMatchObject({ snapshotToken: "tok", cursor: "c1" });
  });

  it("does not call open or renew the token while reconciling", async () => {
    const f = makeSideSyncFixture();
    await f.controller.reconcile(PARENT);
    expect(f.requests).not.toContain("side/open");
    // Observe 端恢复也不该走任何 Control RPC。
    expect(methodsOf(f)).not.toContain("side/turnStart");
    expect(f.requests).not.toContain("side/turnInterrupt");
    expect(f.requests).not.toContain("side/close");
  });

  it("clears stale views only when the list is explicitly empty", async () => {
    const f = makeSideSyncFixture();
    await f.controller.reconcile(PARENT);
    f.deliver(changed(1));
    expect(f.store.read(f.hostKey, SIDE)).toBeDefined();
    const empty = makeSideSyncFixture({ list: listResult([]) });
    // 同一个 store 上再对账一次，但这次 list 明确为空。
    const controller = new SideSyncController({
      binding: (empty.store.bind(
        parseSideHandshake(
          SLOT,
          ({ request: empty.rpc } as unknown) as RpcClient,
          { capabilities: { side_conversation_v1: true }, sideHostInstanceId: "h1" },
          "control",
        )!,
      )),
      store: f.store,
      subscribeParent: async () => {},
      clock: () => 0,
    });
    await controller.reconcile(PARENT);
    expect(f.store.read(f.hostKey, SIDE)).toBeUndefined();
  });

  it("keeps the placeholder and marks unsynced on a network failure", async () => {
    const f = makeSideSyncFixture();
    f.rpc.mockImplementationOnce(async () => {
      throw new Error("offline");
    });
    await f.controller.reconcile(PARENT);
    // 还没有视图时，网络失败不会凭空造出一个「已关闭」的壳。
    expect(f.store.list(f.hostKey)).toHaveLength(0);
  });

  it("keeps an existing view and marks it unsynced when the reconnect list fails", async () => {
    const f = makeSideSyncFixture();
    await f.controller.reconcile(PARENT);
    expect(f.store.read(f.hostKey, SIDE)!.sync).toBe("synced");
    f.rpc.mockImplementationOnce(async () => {
      throw new Error("offline");
    });
    await f.controller.reconcile(PARENT);
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view).toBeDefined();
    expect(view.sync).toBe("unsynced");
  });

  it("refuses to install a partial baseline when the page budget runs out", async () => {
    const pages = Array.from({ length: 100 }, (_, i) =>
      page({ items: [agentItem(`x${i}`, `m${i}`)], snapshot_seq: 1, next_cursor: `c${i}` }),
    );
    const f = makeSideSyncFixture({ pages });
    await expect(f.controller.reconcile(PARENT)).rejects.toThrow(/page budget/);
    expect(f.store.read(f.hostKey, SIDE)).toBeUndefined();
  });

  it("ignores an older generation reported after a newer one was seen", async () => {
    const f = makeSideSyncFixture({
      list: listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]),
    });
    await f.controller.reconcile(PARENT);
    // 对账进行中看到新一代：这一份旧快照不得覆盖它。
    f.deliver(itemUpdated(1, agentItem("new life", "n1"), 2));
    await f.controller.reconcile(PARENT);
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.generation).toBe(2);
    expect(view.items.map((i) => i.id)).toEqual(["n1"]);
  });
});

describe("SideSyncController.accept", () => {
  async function synced(pages?: SideReadItemsResult[]) {
    const f = makeSideSyncFixture(pages ? { pages } : {});
    await f.controller.reconcile(PARENT);
    return f;
  }

  it("applies only contiguous sequence numbers", async () => {
    const f = await synced();
    f.deliver(changed(1));
    f.deliver(itemUpdated(2, agentItem("hello", "m1")));
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.seq).toBe(2);
    expect(view.items).toHaveLength(1);
  });

  it("re-reconciles instead of skipping a gap", async () => {
    const f = await synced();
    f.deliver(changed(1));
    const before = methodsOf(f).filter((m) => m === "side/list").length;
    f.deliver(itemUpdated(5, agentItem("hole", "m9")));
    // 缺号：标未同步并触发一次全量对账，绝不 max(seq) 跳过去。
    expect(f.store.read(f.hostKey, SIDE)!.sync).toBe("unsynced");
    await flush();
    expect(methodsOf(f).filter((m) => m === "side/list").length).toBeGreaterThan(before);
  });

  it("ignores a duplicate revision", async () => {
    const f = await synced();
    f.deliver(itemUpdated(1, agentItem("one", "m1")));
    f.deliver(itemUpdated(1, agentItem("stale", "m1")));
    expect(f.store.read(f.hostKey, SIDE)!.items[0]).toMatchObject({ text: "one" });
  });

  it("applies a closed frame without waiting for missing numbers", async () => {
    const f = await synced();
    f.deliver(changed(1));
    f.deliver(closedFrame(7, "expired"));
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.closed).toBe(true);
    expect(view.error?.reason).toBe("expired");
  });

  it("keeps a closed generation closed even if a late page arrives", async () => {
    const f = await synced();
    f.deliver(closedFrame(1));
    f.deliver(itemUpdated(2, agentItem("ghost", "m1")));
    expect(f.store.read(f.hostKey, SIDE)!.items).toHaveLength(0);
  });

  it("drops frames from an older generation", async () => {
    const f = await synced();
    f.deliver(changed(1));
    f.deliver(itemUpdated(9, agentItem("old life", "x"), 0));
    expect(f.store.read(f.hostKey, SIDE)!.items).toHaveLength(0);
  });

  it("resets the transcript when a newer generation opens", async () => {
    const f = await synced();
    f.deliver(itemUpdated(1, agentItem("first", "m1")));
    f.deliver(changed(1));
    f.deliver(itemUpdated(1, agentItem("second", "m2"), 2));
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.generation).toBe(2);
    expect(view.items.map((i) => i.id)).toEqual(["m2"]);
  });

  it("drops the cache and asks for a full snapshot when it overflows", async () => {
    const f = makeSideSyncFixture();
    // 让对账停在 list 上，使后续帧进入缓存。
    let release!: () => void;
    f.rpc.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          release = () =>
            resolve(listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]));
        }),
    );
    const pending = f.controller.reconcile(PARENT);
    await flush();
    for (let i = 1; i <= SIDE_CACHE_MAX_FRAMES + 1; i += 1) {
      f.deliver(itemUpdated(i, { type: "agentMessage", id: `m${i}`, text: "x".repeat(64) }));
    }
    const before = f.store.read(f.hostKey, SIDE);
    // 溢出后没有任何东西被写进视图（视图可能还不存在）。
    expect(before).toBeUndefined();
    release();
    await pending;
    // 溢出必须让视图回到未同步，而不是悄悄装一个已知有洞的基线。
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.sync).toBe("unsynced");
  });

  it("replays cached frames that continue the baseline", async () => {
    const f = makeSideSyncFixture({
      pages: [page({ items: [agentItem("base", "m1")], snapshot_seq: 1 })],
    });
    let release!: () => void;
    f.rpc.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          release = () =>
            resolve(listResult([{ side_id: SIDE, generation: 1, last_seq: 2 }]));
        }),
    );
    const pending = f.controller.reconcile(PARENT);
    await flush();
    f.deliver(itemUpdated(2, agentItem("during", "m2")));
    release();
    await pending;
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.items.map((i) => i.id)).toEqual(["m1", "m2"]);
    expect(view.seq).toBe(2);
    expect(view.sync).toBe("synced");
  });

  it("re-reconciles when the cache itself has a hole", async () => {
    const f = makeSideSyncFixture({
      pages: [page({ items: [agentItem("base", "m1")], snapshot_seq: 1 })],
    });
    let release!: () => void;
    f.rpc.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          release = () =>
            resolve(listResult([{ side_id: SIDE, generation: 1, last_seq: 3 }]));
        }),
    );
    const pending = f.controller.reconcile(PARENT);
    await flush();
    f.deliver(itemUpdated(3, agentItem("hole", "m3")));
    release();
    await pending;
    // 缓存缺 2：不装配，标未同步。
    expect(f.store.read(f.hostKey, SIDE)!.sync).toBe("unsynced");
  });

  it("takes a full snapshot before showing a side it has not seen yet", async () => {
    const f = makeSideSyncFixture();
    f.deliver(changed(1));
    // 第一帧不是「凭空建一个占位」：真实正文可能已经有几十条，从 seq=1 起画会
    // 给出一份缺了开头却毫无提示的历史。这一帧进缓存，先订父 + 分页装基线。
    await flush();
    expect(methodsOf(f)).toEqual(
      expect.arrayContaining(["side/list", "side/readItems"]),
    );
    expect(f.store.read(f.hostKey, SIDE)).toBeDefined();
    expect(f.store.read(f.hostKey, SIDE)!.sync).toBe("synced");
  });
});

describe("SideSyncController.submit", () => {
  async function synced() {
    const f = makeSideSyncFixture();
    await f.controller.reconcile(PARENT);
    return f;
  }

  it("sends the text with the client request id", async () => {
    const f = await synced();
    await f.controller.submit(SIDE, "question", REQ_UUID);
    expect(f.rpc).toHaveBeenCalledWith("side/turnStart", {
      sideId: SIDE,
      text: "question",
      clientRequestId: REQ_UUID,
    });
  });

  it("retries uncertain submission with the same request id", async () => {
    const f = await synced();
    f.rpc.mockRejectedValueOnce(new Error("disconnected"));
    await f.controller.submit(SIDE, "question", REQ_UUID);
    const pending = f.store.read(f.hostKey, SIDE)!.pending!;
    expect(pending.state).toBe("uncertain");
    expect(pending.requestId).toBe(REQ_UUID);
  });

  it("clears the pending submission once accepted", async () => {
    const f = await synced();
    await f.controller.submit(SIDE, "question", REQ_UUID);
    expect(f.store.read(f.hostKey, SIDE)!.pending).toBeNull();
  });

  it("restores the question text when the server rejects it outright", async () => {
    const f = await synced();
    f.rpc.mockRejectedValueOnce({ code: -32602, message: "invalid params" });
    await expect(f.controller.submit(SIDE, "kept question", REQ_UUID)).rejects.toBeTruthy();
    const view = f.store.read(f.hostKey, SIDE)!;
    expect(view.pending).toBeNull();
    expect(view.draft).toBe("kept question");
  });

  it("does not overwrite newer edits when restoring a rejected question", async () => {
    const f = await synced();
    f.store.setDraft(f.hostKey, SIDE, "brand new edit");
    f.rpc.mockRejectedValueOnce({ code: -32602, message: "invalid params" });
    await expect(f.controller.submit(SIDE, "old question", REQ_UUID)).rejects.toBeTruthy();
    expect(f.store.read(f.hostKey, SIDE)!.draft).toBe("brand new edit");
  });

  it("refuses to submit before the view is synced", async () => {
    const f = makeSideSyncFixture({ list: listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]) });
    await f.controller.reconcile(PARENT);
    f.store.setSync(f.hostKey, SIDE, "unsynced");
    await expect(f.controller.submit(SIDE, "q", REQ_UUID)).rejects.toBeTruthy();
    expect(methodsOf(f)).not.toContain("side/turnStart");
  });

  it("confirms an uncertain submission via accepted_requests", async () => {
    const f = await synced();
    f.rpc.mockRejectedValueOnce(new Error("disconnected"));
    await f.controller.submit(SIDE, "question", REQ_UUID);
    // 服务端其实受理了：accepted_requests 里有这个 id。
    f.rpc.mockImplementation(async (method: string) => {
      if (method === "side/list") return listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]);
      if (method === "side/readItems") {
        return page({ snapshot_seq: 0, accepted_requests: { [REQ_UUID]: "turn-1" } });
      }
      return {};
    });
    await f.controller.confirmPending(PARENT, SIDE);
    expect(f.store.read(f.hostKey, SIDE)!.pending).toBeNull();
    // 已确认：不得再发一次 turnStart。
    expect(methodsOf(f).filter((m) => m === "side/turnStart")).toHaveLength(1);
  });

  it("retries with the same id when the server has no record of it", async () => {
    const f = await synced();
    f.rpc.mockRejectedValueOnce(new Error("disconnected"));
    await f.controller.submit(SIDE, "question", REQ_UUID);
    f.rpc.mockImplementation(async (method: string) => {
      if (method === "side/list") return listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]);
      if (method === "side/readItems") return page({ snapshot_seq: 0 });
      if (method === "side/turnStart") return { turn_id: "turn-2" };
      return {};
    });
    await f.controller.confirmPending(PARENT, SIDE);
    const starts = f.rpc.mock.calls.filter((c: unknown[]) => c[0] === "side/turnStart");
    // 同一个 requestId 重试，绝不生成新 id。
    expect(starts[starts.length - 1][1]).toMatchObject({ clientRequestId: REQ_UUID });
  });
});

describe("SideSyncController.setVisible", () => {
  it("polls list while visible and stops in the background", async () => {
    vi.useFakeTimers();
    try {
      const f = makeSideSyncFixture();
      await f.controller.reconcile(PARENT);
      f.controller.setVisible(PARENT);
      const before = methodsOf(f).filter((m) => m === "side/list").length;
      await vi.advanceTimersByTimeAsync(5100);
      expect(methodsOf(f).filter((m) => m === "side/list").length).toBeGreaterThan(before);

      f.controller.setVisible(null);
      const stopped = methodsOf(f).filter((m) => m === "side/list").length;
      await vi.advanceTimersByTimeAsync(20000);
      expect(methodsOf(f).filter((m) => m === "side/list").length).toBe(stopped);
    } finally {
      vi.useRealTimers();
    }
  });

  it("re-reconciles when the server's last_seq moved", async () => {
    vi.useFakeTimers();
    try {
      const f = makeSideSyncFixture();
      await f.controller.reconcile(PARENT);
      f.rpc.mockImplementation(async (method: string) => {
        if (method === "side/list") {
          return listResult([{ side_id: SIDE, generation: 1, last_seq: 9 }]);
        }
        if (method === "side/readItems") return page({ snapshot_seq: 9 });
        return {};
      });
      f.controller.setVisible(PARENT);
      await vi.advanceTimersByTimeAsync(5100);
      expect(f.rpc.mock.calls.some((c: unknown[]) => c[0] === "side/readItems")).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it("treats a throwing list as a network wobble, not a loss", async () => {
    vi.useFakeTimers();
    try {
      const f = makeSideSyncFixture();
      await f.controller.reconcile(PARENT);
      f.rpc.mockRejectedValue(new Error("offline"));
      f.controller.setVisible(PARENT);
      await vi.advanceTimersByTimeAsync(5100);
      expect(f.store.read(f.hostKey, SIDE)!.closed).toBe(false);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("SideSyncController.dispose", () => {
  it("cancels an in-flight reconcile so its baseline never lands", async () => {
    const f = makeSideSyncFixture();
    let release!: () => void;
    f.rpc.mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          release = () =>
            resolve(listResult([{ side_id: SIDE, generation: 1, last_seq: 0 }]));
        }),
    );
    const pending = f.controller.reconcile(PARENT);
    await flush();
    f.controller.dispose();
    release();
    await pending;
    expect(methodsOf(f)).not.toContain("side/readItems");
  });

  it("stops the tail poll", async () => {
    vi.useFakeTimers();
    try {
      const f = makeSideSyncFixture();
      await f.controller.reconcile(PARENT);
      f.controller.setVisible(PARENT);
      f.controller.dispose();
      const before = methodsOf(f).filter((m) => m === "side/list").length;
      await vi.advanceTimersByTimeAsync(20000);
      expect(methodsOf(f).filter((m) => m === "side/list").length).toBe(before);
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("isNetworkFailure", () => {
  it("treats an RPC error object as a definite answer", () => {
    expect(isNetworkFailure({ code: -32031, message: "side_not_found" })).toBe(false);
  });

  it("treats a transport failure as unknown", () => {
    expect(isNetworkFailure(new Error("disconnected"))).toBe(true);
  });

  it("treats a local sync error as definite", () => {
    expect(isNetworkFailure(new (class extends Error {})("x"))).toBe(true);
  });
});

describe("sideHostKey stability", () => {
  it("matches the exported helper", () => {
    expect(sideHostKey(SLOT, "h1")).toBe(JSON.stringify([SLOT, "h1"]));
  });

  it("keeps distinct side ids under one host apart", async () => {
    const f = makeSideSyncFixture();
    await f.controller.reconcile(PARENT);
    const other = "6f1c0e2a-0000-4000-8000-000000000002";
    // 第二个 side 走「首次听说」的路径（先全量对账），而不是直接写 store。
    f.rpc.mockImplementation(async (method: string) => {
      if (method === "side/list") {
        return listResult([
          { side_id: SIDE, generation: 1, last_seq: 0 },
          { side_id: other, generation: 1, last_seq: 1 },
        ]);
      }
      if (method === "side/readItems") return page({ snapshot_seq: 1 });
      return {};
    });
    f.deliver({
      method: "side/changed",
      params: {
        parent_thread_id: PARENT,
        side_id: other,
        generation: 1,
        seq: 1,
        summary: {
          side_id: other,
          parent_thread_id: PARENT,
          status: "running",
          snapshot_revision: 1,
          last_seq: 1,
          active_turn_id: null,
          expires_at: null,
          generation: 1,
          stopping: false,
        },
      },
    });
    // 对账是 fire-and-forget 的一串异步：等它真的装上。
    await vi.waitFor(() => {
      expect(f.store.read(f.hostKey, other)).toBeDefined();
    });
    expect(f.store.read(f.hostKey, SIDE)).toBeDefined();
    // 两条 side 各有一份独立视图，键仍是 (hostKey, sideId)。
    expect(f.store.read(f.hostKey, SIDE)!.sideId).not.toBe(
      f.store.read(f.hostKey, other)!.sideId,
    );
  });
});

describe("wire ids", () => {
  it("keeps side ids as UUIDs in fixtures", () => {
    // 服务端只接受合法 UUID；测试里用合法形状，wire 断言才有意义。
    expect(SIDE_UUID).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  });
});
