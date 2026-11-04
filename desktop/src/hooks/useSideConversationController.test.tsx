/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";
import type { RpcClient } from "../lib/rpc";
import { SideConversationStore, sideHostKey } from "../lib/sideConversation";
import type { SideSyncController } from "../lib/sideSync";
import {
  parentStatusText,
  useSideConversationController,
} from "./useSideConversationController";

afterEach(cleanup);

const SLOT = "local" as never;
const INSTANCE = "host-1";

type Request = { method: string; params: unknown };

/** 最小 RPC 客户端：记录请求，按 method 回放应答。 */
function fakeClient(responses: Record<string, (params: any) => unknown> = {}) {
  const requests: Request[] = [];
  const request = vi.fn(async (method: string, params: unknown) => {
    requests.push({ method, params });
    const handler = responses[method];
    if (handler) return handler(params);
    if (method === "side/list") return { sides: [] };
    return {};
  });
  return { requests, request } as unknown as RpcClient & { requests: Request[] };
}

/** 只实现本 hook 用到的那几个方法。 */
function fakeController() {
  return {
    // 真控制器的 `submit` 返回三态收据（Task 2）：fake 必须同样返回，否则
    // hook 里 `receipt?.kind === "accepted"` 的判定在测试中永远为假。
    submit: vi.fn(async (_sideId: string, _text: string, requestId: string) => ({
      kind: "accepted" as const,
      requestId,
    })),
    reconcile: vi.fn(async () => {}),
    client: null as unknown,
    dispose: vi.fn(),
  };
}

function setup(options: {
  responses?: Record<string, (params: any) => unknown>;
  supported?: boolean;
  scope?: "observe" | "control" | "admin";
} = {}) {
  const store = new SideConversationStore();
  const client = fakeClient(options.responses);
  const binding = store.bind({
    key: sideHostKey(SLOT, INSTANCE),
    slot: SLOT,
    instanceId: INSTANCE,
    client,
    supported: options.supported ?? true,
    scope: options.scope ?? "control",
    connected: true,
  });
  const controllers: Array<ReturnType<typeof fakeController>> = [];
  const controllerFor = vi.fn(() => {
    const c = fakeController();
    c.client = binding.client;
    controllers.push(c);
    return c as unknown as SideSyncController;
  });
  const onChange = vi.fn();
  let seq = 0;
  const hook = renderHook(() =>
    useSideConversationController({
      store,
      controllerFor,
      onChange,
      newRequestId: () => `00000000-0000-4000-a000-${String(++seq).padStart(12, "0")}`,
    }),
  );
  return { store, binding, client, controllerFor, controllers, onChange, hook };
}

describe("useSideConversationController.open", () => {
  it("opens, adopts the returned generation and reconciles", async () => {
    const s = setup({
      responses: {
        "side/open": () => ({
          side_id: "s1",
          parent_thread_id: "t1",
          status: "idle",
          snapshot_revision: 1,
          last_seq: 0,
          active_turn_id: null,
          expires_at: null,
          generation: 3,
          stopping: false,
        }),
      },
    });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    expect(s.hook.result.current.active).toEqual({
      kind: "side",
      hostKey: s.binding.key,
      parentId: "t1",
      sideId: "s1",
      generation: 3,
    });
    expect(s.controllers[0].reconcile).toHaveBeenCalledWith("t1");
  });

  it("sends the question as the first turn with a generated request id", async () => {
    const s = setup({
      responses: {
        "side/open": () => ({
          side_id: "s1",
          parent_thread_id: "t1",
          status: "idle",
          snapshot_revision: 1,
          last_seq: 0,
          active_turn_id: null,
          expires_at: null,
          generation: 0,
          stopping: false,
        }),
      },
    });
    s.store.installBaseline(
      s.binding.key,
      "s1",
      { side_id: "s1", snapshot_seq: 0, accepted_requests: {} } as never,
      [],
    );
    s.store.adopt(s.binding.key, "s1", "t1", 0);
    s.store.setSync(s.binding.key, "s1", "synced");

    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "why slow?");
    });
    expect(s.controllers[0].submit).toHaveBeenCalledWith(
      "s1",
      "why slow?",
      "00000000-0000-4000-a000-000000000001",
    );
  });

  it("merges a repeated open into the same side instead of opening twice", async () => {
    const s = setup({
      responses: {
        "side/open": () => ({
          side_id: "s1",
          parent_thread_id: "t1",
          status: "idle",
          snapshot_revision: 1,
          last_seq: 0,
          active_turn_id: null,
          expires_at: null,
          generation: 0,
          stopping: false,
        }),
      },
    });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // 真实对账会把基线装进 store；这里用假控制器，手动补上等价的一步。
    s.store.installBaseline(
      s.binding.key,
      "s1",
      { side_id: "s1", snapshot_seq: 0, accepted_requests: {} } as never,
      [],
    );
    s.store.adopt(s.binding.key, "s1", "t1", 0);

    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // 同一父上已有未关闭的旁支：第二次直接复用，不再请求服务端。
    expect(
      s.client.requests.filter((r) => r.method === "side/open"),
    ).toHaveLength(1);
  });

  it("refuses to open on a host without the capability", async () => {
    const s = setup({ supported: false });
    let ok = true;
    await act(async () => {
      ok = await s.hook.result.current.open(s.binding, "t1", "");
    });
    expect(ok).toBe(false);
    expect(s.client.requests.some((r) => r.method === "side/open")).toBe(false);
    expect(s.hook.result.current.error).toContain("不支持");
  });
});

describe("useSideConversationController.back", () => {
  it("returns to main without stopping or closing the side", async () => {
    const s = setup({
      responses: {
        "side/open": () => ({
          side_id: "s1",
          parent_thread_id: "t1",
          status: "running",
          snapshot_revision: 1,
          last_seq: 0,
          active_turn_id: "turn-1",
          expires_at: null,
          generation: 0,
          stopping: false,
        }),
      },
    });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    act(() => s.hook.result.current.back());

    expect(s.hook.result.current.active).toEqual({
      kind: "main",
      hostKey: s.binding.key,
      parentId: "t1",
    });
    const methods = s.client.requests.map((r) => r.method);
    expect(methods).not.toContain("side/turnInterrupt");
    expect(methods).not.toContain("side/close");
  });
});

describe("useSideConversationController.stop", () => {
  it("interrupts the side's own turn, never the parent", async () => {
    const s = setup();
    s.store.installBaseline(
      s.binding.key,
      "s1",
      {
        side_id: "s1",
        snapshot_seq: 0,
        accepted_requests: {},
        status: "running",
        active_turn_id: "turn-7",
      } as never,
      [],
    );
    s.store.adopt(s.binding.key, "s1", "t1", 0);
    s.store.setSync(s.binding.key, "s1", "synced");
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 0));

    await act(async () => {
      await s.hook.result.current.stop();
    });
    expect(s.client.requests).toContainEqual({
      method: "side/turnInterrupt",
      params: { sideId: "s1", turnId: "turn-7" },
    });
    expect(
      s.client.requests.some((r) => r.method === "turn/interrupt"),
    ).toBe(false);
  });

  it("does nothing when the side is idle", async () => {
    const s = setup();
    s.store.installBaseline(
      s.binding.key,
      "s1",
      { side_id: "s1", snapshot_seq: 0, accepted_requests: {}, active_turn_id: null } as never,
      [],
    );
    s.store.adopt(s.binding.key, "s1", "t1", 0);
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 0));
    await act(async () => {
      await s.hook.result.current.stop();
    });
    expect(s.client.requests.some((r) => r.method === "side/turnInterrupt")).toBe(false);
  });
});

describe("useSideConversationController.close", () => {
  it("reads the server's current turn before confirming", async () => {
    const s = setup({
      responses: {
        "side/list": () => ({
          sides: [
            {
              side_id: "s1",
              parent_thread_id: "t1",
              status: "running",
              snapshot_revision: 2,
              last_seq: 9,
              active_turn_id: "turn-9",
              expires_at: null,
              generation: 0,
              stopping: false,
            },
          ],
        }),
      },
    });
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 0));
    let confirmation: unknown = null;
    await act(async () => {
      confirmation = await s.hook.result.current.requestClose();
    });
    expect(confirmation).toEqual({
      sideId: "s1",
      activeTurnId: "turn-9",
      running: true,
    });
  });

  it("passes the confirmed turn id through, and returns to main", async () => {
    const s = setup();
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 0));
    let ok = false;
    await act(async () => {
      ok = await s.hook.result.current.confirmClose({
        sideId: "s1",
        activeTurnId: "turn-9",
        running: true,
      });
    });
    expect(ok).toBe(true);
    expect(s.client.requests).toContainEqual({
      method: "side/close",
      // Idle 时传 null，Running 时传用户确认过的那一轮——不是"随便关掉"。
      params: { sideId: "s1", expectedActiveTurnId: "turn-9" },
    });
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
  });

  it("returns to main when the side was already closed elsewhere", async () => {
    const s = setup({ responses: { "side/list": () => ({ sides: [] }) } });
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 0));
    let confirmation: unknown = "unset";
    await act(async () => {
      confirmation = await s.hook.result.current.requestClose();
    });
    // 另一端已经关掉了：没有可确认的对象，也不该再发一次 close。
    expect(confirmation).toBeNull();
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
    expect(s.client.requests.some((r) => r.method === "side/close")).toBe(false);
  });
});

describe("parentStatusText", () => {
  it("translates the parent's state for the header", () => {
    expect(parentStatusText("running")).toBe("运行中");
    expect(parentStatusText("awaiting_approval")).toBe("等待审批");
    expect(parentStatusText("idle")).toBe("空闲");
    expect(parentStatusText(null)).toBe("未知");
  });
});
