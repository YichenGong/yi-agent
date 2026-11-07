/** @vitest-environment jsdom */
import { describe, expect, it, vi } from "vitest";

import { createNotificationRouter } from "./useNotificationRouter";
import { createSubagentTraceController } from "./useSubagentTrace";
import { SubagentRailStore } from "../lib/subagents";
import { LOCAL_SLOT } from "../lib/slots";
import type { RpcClient } from "../lib/rpc";

/**
 * 路由层只负责一件事：**把帧连同送它来的那条连接一起交给控制器**。
 *
 * 归属核对本身在控制器里（它才知道订阅是谁开的）。这里要钉的是接线错误——
 * 传错 client、传错 slot、或者干脆沿用「当前打开的任务」那套 React 影子。
 * 那类错误在控制器单测里永远看不见，因为它们看起来只是「一帧合法输入」。
 */
const target = {
  slot: LOCAL_SLOT,
  serverBindingGeneration: 0,
  threadId: "t1",
  taskId: "task-1",
};

function harness() {
  const trace = createSubagentTraceController();
  const railStore = new SubagentRailStore();
  const handlers = {
    notification: null as null | ((n: unknown) => void),
    status: null as null | ((s: unknown) => void),
  };
  // 一个客户端对象同时扮演两个角色：路由器挂回调的那条连接，也是控制器认识的
  // 那个来源。真实代码里这就是同一个 `RpcClient` 实例，测试里必须同样只用一个，
  // 否则归属判据永远对不上（那正是这类接线错误最典型的形态）。
  const client = {
    request: vi.fn().mockResolvedValue({ rows: [], highWaterId: 0 }),
    onNotification: (cb: (n: unknown) => void) => {
      handlers.notification = cb;
      return () => {};
    },
    onApproval: () => () => {},
    onStatus: (cb: (s: unknown) => void) => {
      handlers.status = cb;
      return () => {};
    },
  };
  const connection = {
    slot: LOCAL_SLOT,
    serverBindingGeneration: 0,
    connectionGeneration: 1,
    client: client as never,
  };
  trace.setConnection(connection);
  trace.setConnectionReady(connection, true);
  const router = createNotificationRouter({
    activeSlotRef: { current: LOCAL_SLOT },
    sess: {
      groupsRef: { current: [] },
      pinnedRef: { current: [] },
      currentIdRef: { current: "t1" },
      refreshThreads: () => {},
    } as never,
    prefs: { repairingRef: { current: false }, repairDismissedRef: { current: false } } as never,
    diff: {} as never,
    proc: {} as never,
    railStore,
    store: {} as never,
    subWindow: { current: {} } as never,
    toppedUp: { current: new Set<string>() },
    warm: { current: new Set<string>() },
    setStatus: () => {},
    setTagNames: () => {},
    force: () => {},
    markThreadRead: () => {},
    schedulePersist: () => {},
    selectThread: async () => {},
    notifySyncActivity: () => {},
    disposed: false,
    scheduleReconnect: () => {},
    sideStore: {} as never,
    acceptSideNotification: () => {},
    trace,
    changeDetailTab: () => {},
  });

  // 把 router 挂到那条客户端上，并取出它注册的回调。
  router(client as unknown as RpcClient, LOCAL_SLOT);

  return { trace, railStore, handlers, connection, client, router };
}

/** 等 watch 落定：帧只在订阅进入 ready 之后才会被接受。 */
async function ready(trace: ReturnType<typeof createSubagentTraceController>) {
  for (let i = 0; i < 20; i += 1) {
    if (trace.getSnapshot().subscription.phase === "ready") return;
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
  throw new Error("subscription never became ready");
}

describe("通知路由与轨迹控制器的接线", () => {
  it("轨迹帧带着来源交给控制器，落进当前任务", async () => {
    const { trace, handlers } = harness();
    trace.open(target);
    await ready(trace);

    handlers.notification!({
      method: "agent/trace/event",
      params: {
        threadId: "t1",
        taskId: "task-1",
        row: {
          eventId: 1,
          taskId: "task-1",
          kind: "assistant_text",
          payloadJson: JSON.stringify({ type: "assistant_text", text: "进展" }),
        },
      },
    });

    expect(trace.getSnapshot().subscription.rows).toHaveLength(1);
  });

  it("未登记来源的连接送来的帧被丢弃", async () => {
    const { trace, router } = harness();
    trace.open(target);
    await ready(trace);
    const before = trace.getSnapshot().subscription.rows.length;

    // 另一条从未登记过的连接：它自己的通知回调送来的帧必须被丢弃。
    const outsider = { onNotification: null as null | ((n: unknown) => void) };
    const other = {
      request: () => Promise.resolve({}),
      onNotification: (cb: (n: unknown) => void) => {
        outsider.onNotification = cb;
        return () => {};
      },
      onApproval: () => () => {},
      onStatus: () => () => {},
    };
    router(other as unknown as RpcClient, LOCAL_SLOT);
    outsider.onNotification!({
      method: "agent/trace/event",
      params: {
        threadId: "t1",
        taskId: "task-1",
        row: {
          eventId: 9,
          taskId: "task-1",
          kind: "assistant_text",
          payloadJson: JSON.stringify({ type: "assistant_text", text: "别人的帧" }),
        },
      },
    });

    expect(trace.getSnapshot().subscription.rows).toHaveLength(before);
  });

  it("children 更新把最新状态同步给打开的任务", async () => {
    const { trace, railStore, handlers } = harness();
    trace.open(target);
    await ready(trace);

    handlers.notification!({
      method: "agent/children/updated",
      params: {
        threadId: "t1",
        children: [
          {
            taskId: "task-1",
            state: "failed",
            objective: "整理日志",
            lastStep: null,
            parentTaskId: null,
          },
        ],
      },
    });

    // 行列表与动作记录里的终态一致：控制器据此关掉停止入口。
    expect(railStore.get("t1")[0].finished).toBe(true);
  });
});
