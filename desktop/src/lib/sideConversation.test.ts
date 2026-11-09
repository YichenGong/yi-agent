import { describe, expect, it } from "vitest";
import type { RpcClient } from "./rpc";
import { LOCAL_SLOT, type Slot } from "./slots";
import type { SideNotification } from "./sideConversation";
import {
  SideConversationRouter,
  SideConversationStore,
  isSideNotification,
  parseSideHandshake,
  sideHostKey,
} from "./sideConversation";
import type { Notification } from "./generated/Notification";
import type { Item } from "./generated/Item";

/** 只需要身份比较，不需要真的发请求。 */
function fakeClient(name: string): RpcClient {
  return { name } as unknown as RpcClient;
}

const SLOT: Slot = LOCAL_SLOT;

function handshake(over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    serverInfo: { name: "yi-agent-app-server", version: "0.0.0" },
    protocolVersion: 1,
    capabilities: { side_conversation_v1: true, side_snapshot_lifecycle_v1: true },
    sideHostInstanceId: "h1",
    syncPlan: [],
    ...over,
  };
}

function binding(
  client: RpcClient,
  opts: { slot?: Slot; instanceId?: string; supported?: boolean } = {},
) {
  const parsed = parseSideHandshake(
    opts.slot ?? SLOT,
    client,
    handshake(
      opts.instanceId === undefined
        ? {}
        : { sideHostInstanceId: opts.instanceId },
    ),
    "control",
  );
  if (!parsed) throw new Error("handshake did not parse");
  return opts.supported === false ? { ...parsed, supported: false } : parsed;
}

let nextId = 0;
function agentItem(text: string, id?: string): Item {
  return { type: "agentMessage", id: id ?? `i${nextId++}`, text };
}

function changed(
  sideId: string,
  seq: number,
  opts: { generation?: number; lastSeq?: number } = {},
): SideNotification {
  return {
    method: "side/changed",
    params: {
      parent_thread_id: "p1",
      side_id: sideId,
      generation: opts.generation ?? 1,
      seq,
      summary: {
        side_id: sideId,
        parent_thread_id: "p1",
        status: "running",
        snapshot_revision: 1,
        last_seq: opts.lastSeq ?? seq,
        active_turn_id: "t1",
        expires_at: null,
        generation: opts.generation ?? 1,
        stopping: false,
      },
    },
  };
}

function itemUpdated(
  sideId: string,
  seq: number,
  item: Item,
  generation = 1,
): SideNotification {
  return {
    method: "side/itemUpdated",
    params: {
      parent_thread_id: "p1",
      side_id: sideId,
      generation,
      seq,
      turn_id: "t1",
      item,
    },
  };
}

function turnCompleted(
  sideId: string,
  seq: number,
  generation = 1,
): SideNotification {
  return {
    method: "side/turnCompleted",
    params: {
      parent_thread_id: "p1",
      side_id: sideId,
      generation,
      seq,
      turn_id: "t1",
      status: "idle",
      reason: null,
      error: null,
      usage: {
        total: {
          input: 10,
          output: 20,
          cache_creation: 0,
          cache_read: 0,
          reasoning: 0,
        },
        complete: true,
      },
    },
  };
}

function closed(
  sideId: string,
  seq: number,
  generation = 1,
  reason = "user_closed",
): SideNotification {
  return {
    method: "side/closed",
    params: {
      parent_thread_id: "p1",
      side_id: sideId,
      generation,
      seq,
      reason,
    },
  };
}

describe("sideHostKey", () => {
  it("binds identities to slot and host, not global reader", () => {
    expect(sideHostKey(SLOT, "h1")).not.toBe(sideHostKey(SLOT, "h2"));
    expect(sideHostKey("proj-a", "h1")).not.toBe(sideHostKey("proj-b", "h1"));
  });

  it("does not conflate a colon inside slot or instance id", () => {
    // 拼接以冒号分隔会让 ("a:b","c") 与 ("a","b:c") 撞键。
    expect(sideHostKey("a:b", "c")).not.toBe(sideHostKey("a", "b:c"));
  });
});

describe("parseSideHandshake", () => {
  it("keeps capability and host instance id from the same response", () => {
    const client = fakeClient("c");
    const b = parseSideHandshake(SLOT, client, handshake(), "control");
    expect(b).not.toBeNull();
    expect(b!.supported).toBe(true);
    expect(b!.instanceId).toBe("h1");
    expect(b!.key).toBe(sideHostKey(SLOT, "h1"));
    expect(b!.scope).toBe("control");
    expect(b!.connected).toBe(true);
  });

  it("reports unsupported instead of falling back when capability is missing", () => {
    const b = parseSideHandshake(
      SLOT,
      fakeClient("c"),
      handshake({ capabilities: {} }),
      "control",
    );
    expect(b).not.toBeNull();
    expect(b!.supported).toBe(false);
  });

  it("requires both capability bits for the disposable lifecycle", () => {
    const both = parseSideHandshake(SLOT, fakeClient("c"), handshake(), "control");
    expect(both!.snapshotLifecycle).toBe(true);

    // 只声明旧位：分页/通知还能用（supported），但「退出即销毁 + 票据准入」
    // 不在，入口必须不开放。
    const legacy = parseSideHandshake(
      SLOT,
      fakeClient("c"),
      handshake({ capabilities: { side_conversation_v1: true } }),
      "control",
    );
    expect(legacy!.supported).toBe(true);
    expect(legacy!.snapshotLifecycle).toBe(false);

    // 只声明新位：没有旧分页契约，同样不开放。
    const onlyNew = parseSideHandshake(
      SLOT,
      fakeClient("c"),
      handshake({ capabilities: { side_snapshot_lifecycle_v1: true } }),
      "control",
    );
    expect(onlyNew!.supported).toBe(false);
    expect(onlyNew!.snapshotLifecycle).toBe(true);
  });

  it("treats a missing host id as unsupported", () => {
    const b = parseSideHandshake(
      SLOT,
      fakeClient("c"),
      handshake({ sideHostInstanceId: undefined }),
      "control",
    );
    expect(b!.supported).toBe(false);
  });

  it("returns null only when the handshake result is not an object", () => {
    expect(parseSideHandshake(SLOT, fakeClient("c"), null, "control")).toBeNull();
    expect(parseSideHandshake(SLOT, fakeClient("c"), 7, "control")).toBeNull();
  });

  it("carries the observe scope through so Control RPCs can be hidden", () => {
    const b = parseSideHandshake(SLOT, fakeClient("c"), handshake(), "observe");
    expect(b!.scope).toBe("observe");
  });
});

describe("isSideNotification", () => {
  it("narrows only the four side frames", () => {
    expect(isSideNotification(changed("s", 1))).toBe(true);
    expect(isSideNotification(closed("s", 2))).toBe(true);
    const parent: Notification = {
      method: "thread/started",
      params: { thread_id: "p1", cwd: "/tmp", model: "m" },
    };
    expect(isSideNotification(parent)).toBe(false);
  });
});

describe("SideConversationStore", () => {
  it("scopes the same side id to its own host", () => {
    const store = new SideConversationStore();
    const a = store.bind(
      binding(fakeClient("a"), { slot: LOCAL_SLOT, instanceId: "h1" }),
    );
    const b = store.bind(
      binding(fakeClient("b"), { slot: "proj-a", instanceId: "h2" }),
    );
    store.apply(a.key, changed("same", 1));
    expect(store.read(a.key, "same")).toBeDefined();
    expect(store.read(b.key, "same")).toBeUndefined();
  });

  it("keeps the same side id apart across slots with equal host ids", () => {
    const store = new SideConversationStore();
    const local = store.bind(
      binding(fakeClient("l"), { slot: LOCAL_SLOT, instanceId: "h" }),
    );
    const remote = store.bind(
      binding(fakeClient("r"), { slot: "proj-a", instanceId: "h" }),
    );
    store.apply(local.key, changed("s", 1));
    expect(store.read(remote.key, "s")).toBeUndefined();
  });

  it("upserts items instead of appending duplicate text", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, itemUpdated("s", 1, agentItem("he", "m1")));
    store.apply(b.key, itemUpdated("s", 2, agentItem("hello", "m1")));
    const view = store.read(b.key, "s")!;
    expect(view.items).toHaveLength(1);
    expect(view.items[0]).toMatchObject({ id: "m1", text: "hello" });
  });

  it("ignores a repeated revision", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, itemUpdated("s", 1, agentItem("one", "m1")));
    store.apply(b.key, itemUpdated("s", 1, agentItem("stale", "m1")));
    expect(store.read(b.key, "s")!.items[0]).toMatchObject({ text: "one" });
  });

  it("does not resurrect a closed generation with a later item frame", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, itemUpdated("s", 1, agentItem("before", "m1")));
    store.apply(b.key, closed("s", 2));
    store.apply(b.key, itemUpdated("s", 3, agentItem("after", "m2")));
    const view = store.read(b.key, "s")!;
    expect(view.closed).toBe(true);
    expect(view.items).toHaveLength(1);
  });

  it("applies a closed frame even when it skips sequence numbers", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    store.apply(b.key, closed("s", 9, 1, "expired"));
    const view = store.read(b.key, "s")!;
    expect(view.closed).toBe(true);
    expect(view.error?.reason).toBe("expired");
  });

  it("drops frames from an older generation", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 5, { generation: 2 }));
    store.apply(b.key, itemUpdated("s", 6, agentItem("old", "m1"), 1));
    expect(store.read(b.key, "s")!.items).toHaveLength(0);
  });

  it("resets the transcript when a newer generation opens", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, itemUpdated("s", 1, agentItem("first life", "m1"), 1));
    store.apply(b.key, changed("s", 1, { generation: 2 }));
    const view = store.read(b.key, "s")!;
    expect(view.generation).toBe(2);
    expect(view.items).toHaveLength(0);
  });

  it("clears every view of the host when the host restarts", () => {
    const store = new SideConversationStore();
    const old = store.bind(binding(fakeClient("c1"), { instanceId: "h1" }));
    store.apply(old.key, changed("s", 1));
    const fresh = store.bind(binding(fakeClient("c2"), { instanceId: "h2" }));
    expect(store.binding(old.key)).toBeUndefined();
    expect(store.read(old.key, "s")).toBeUndefined();
    expect(store.binding(fresh.key)).toBeDefined();
  });

  it("keeps views across a disconnect but marks them unsynced", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    const view = store.read(b.key, "s")!;
    view.sync = "synced";
    store.setConnected(b.key, false);
    expect(store.read(b.key, "s")).toBeDefined();
    expect(store.read(b.key, "s")!.sync).toBe("unsynced");
    expect(store.binding(b.key)!.connected).toBe(false);
  });

  it("turns an in-flight submission uncertain on disconnect", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    store.read(b.key, "s")!.pending = {
      requestId: "00000000-0000-4000-8000-000000000001",
      text: "q",
      draftVersion: 1,
      state: "submitting",
    };
    store.setConnected(b.key, false);
    expect(store.read(b.key, "s")!.pending!.state).toBe("uncertain");
  });

  it("writes a draft version only when the text actually changed", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    store.setDraft(b.key, "s", "question");
    const before = store.read(b.key, "s")!.draftVersion;
    expect(before).toBeGreaterThan(0);
    // 受控输入框在无关重渲染时会重复写同一个值：那不算一次编辑。
    store.setDraft(b.key, "s", "question");
    expect(store.read(b.key, "s")!.draftVersion).toBe(before);
    // 改走再改回来是**两次**变化：只比正文的话，一个在途提交会误以为草稿没动过。
    store.setDraft(b.key, "s", "other");
    store.setDraft(b.key, "s", "question");
    expect(store.read(b.key, "s")!.draftVersion).toBe(before + 2);
  });

  it("keeps the local draft version when a baseline is installed again", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    store.setDraft(b.key, "s", "half-written");
    const version = store.read(b.key, "s")!.draftVersion;
    store.read(b.key, "s")!.pending = {
      requestId: "00000000-0000-4000-8000-000000000001",
      text: "q",
      draftVersion: version,
      state: "submitting",
    };
    // 基线替换的是正文，不是用户此刻正在写的东西。
    store.installBaseline(
      b.key,
      "s",
      {
        items: [],
        snapshot_seq: 3,
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
        snapshot_token: "t",
        next_cursor: null,
      },
      [],
    );
    const view = store.read(b.key, "s")!;
    expect(view.draftVersion).toBe(version);
    expect(view.draft).toBe("half-written");
    expect(view.pending?.requestId).toBe(
      "00000000-0000-4000-8000-000000000001",
    );
  });

  it("drops the previous generation's pending, rejected and draft version", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    store.setDraft(b.key, "s", "old question");
    const view = store.read(b.key, "s")!;
    view.pending = {
      requestId: "00000000-0000-4000-8000-000000000001",
      text: "old question",
      draftVersion: view.draftVersion,
      state: "uncertain",
    };
    view.rejectedSubmission = {
      requestId: "00000000-0000-4000-8000-000000000002",
      text: "rejected",
      message: "boom",
    };
    store.apply(b.key, changed("s", 1, { generation: 2 }));
    const fresh = store.read(b.key, "s")!;
    // 新一代是宿主给的另一条旁支：上一代的提交不得跟着搬过去。
    expect(fresh.draft).toBe("");
    expect(fresh.draftVersion).toBe(0);
    expect(fresh.pending).toBeNull();
    expect(fresh.rejectedSubmission).toBeNull();
  });

  it("records the usage and error of a finished turn", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1));
    store.apply(b.key, turnCompleted("s", 2));
    const view = store.read(b.key, "s")!;
    expect(view.usage.total.output).toBe(20);
    expect(view.summary.active_turn_id).toBeNull();
    expect(view.error).toBeNull();
  });

  it("drops a notification whose host is not bound", () => {
    const store = new SideConversationStore();
    // 没有 bind：帧无处可归，不能退回到「当前连接」去猜。
    store.apply(sideHostKey(SLOT, "h1"), changed("s", 1));
    expect(store.read(sideHostKey(SLOT, "h1"), "s")).toBeUndefined();
  });

  it("lists the views of one host only", () => {
    const store = new SideConversationStore();
    const a = store.bind(
      binding(fakeClient("a"), { slot: LOCAL_SLOT, instanceId: "h1" }),
    );
    const b = store.bind(
      binding(fakeClient("b"), { slot: "proj-a", instanceId: "h2" }),
    );
    store.apply(a.key, changed("s1", 1));
    store.apply(b.key, changed("s2", 1));
    expect(store.list(a.key).map((v) => v.sideId)).toEqual(["s1"]);
  });
});

describe("SideConversationStore.dropGeneration", () => {
  it("drops exactly one generation and leaves the next one alone", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("s", 1, { generation: 1 }));
    expect(store.read(b.key, "s")!.generation).toBe(1);

    // 旧代退出：按精确代删除。
    expect(store.dropGeneration(b.key, "s", 1)).toBe(true);
    expect(store.read(b.key, "s")).toBeUndefined();

    // 重进得到新的一代：那份新视图不该被一次迟到的旧代删除带走。
    store.apply(b.key, changed("s", 1, { generation: 2 }));
    expect(store.dropGeneration(b.key, "s", 1)).toBe(false);
    expect(store.read(b.key, "s")!.generation).toBe(2);
  });

  it("does not touch another parent on the same host", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, changed("mine", 1));
    store.apply(
      b.key,
      {
        method: "side/changed",
        params: {
          ...changed("theirs", 1).params,
          parent_thread_id: "p2",
          side_id: "theirs",
        },
      } as SideNotification,
    );
    store.dropGeneration(b.key, "mine", 1);
    expect(store.read(b.key, "mine")).toBeUndefined();
    expect(store.read(b.key, "theirs")).toBeDefined();
  });

  it("keeps a late frame from resurrecting the dropped generation", () => {
    const store = new SideConversationStore();
    const b = store.bind(binding(fakeClient("c")));
    store.apply(b.key, itemUpdated("s", 1, agentItem("before", "m1")));
    store.dropGeneration(b.key, "s", 1);
    // 旧代迟到的正文帧不得重建视图。
    store.apply(b.key, itemUpdated("s", 2, agentItem("late", "m2")));
    expect(store.read(b.key, "s")).toBeUndefined();
  });
});

describe("SideConversationRouter", () => {
  it("routes by the source client rather than the active slot", () => {
    const store = new SideConversationStore();
    const clientA = fakeClient("a");
    const clientB = fakeClient("b");
    store.bind(binding(clientA, { slot: LOCAL_SLOT, instanceId: "ha" }));
    store.bind(binding(clientB, { slot: "proj-a", instanceId: "hb" }));
    const router = new SideConversationRouter(store);

    // 当前活动槽位是本地，但帧来自远端连接：仍要写进远端宿主的视图。
    const remoteBinding = router.bindingFor(clientB, "proj-a");
    router.accept(remoteBinding, changed("s", 1));
    expect(store.read(sideHostKey("proj-a", "hb"), "s")).toBeDefined();
    expect(store.read(sideHostKey(LOCAL_SLOT, "ha"), "s")).toBeUndefined();
  });

  it("does not confuse two clients on the same slot", () => {
    const store = new SideConversationStore();
    const stale = fakeClient("stale");
    const fresh = fakeClient("fresh");
    store.bind(binding(fresh));
    const router = new SideConversationRouter(store);
    expect(router.bindingFor(stale, SLOT)).toBeNull();
    expect(router.bindingFor(fresh, SLOT)).not.toBeNull();
  });

  it("discards a frame when no binding matches the client", () => {
    const store = new SideConversationStore();
    const router = new SideConversationRouter(store);
    router.accept(null, changed("s", 1));
    expect(store.list(sideHostKey(SLOT, "h1"))).toHaveLength(0);
  });
});
