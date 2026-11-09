/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook } from "@testing-library/react";
import type { RpcClient } from "../lib/rpc";
import {
  SideConversationStore,
  sideHostKey,
  type SideSubmitReceipt,
} from "../lib/sideConversation";
import type { SideStatusWire } from "../lib/protocol";
import type { SideSummaryWire } from "../lib/generated/SideSummaryWire";
import type { SideSyncController } from "../lib/sideSync";
import { SideExitCoordinator } from "../lib/sideExit";
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
  /** 已签发的票据序号，保证每次 prepare 拿到不同的 entryId。 */
  let prepared = 0;
  const request = vi.fn(async (method: string, params: unknown) => {
    requests.push({ method, params });
    const handler = responses[method];
    if (handler) return handler(params);
    if (method === "side/list") return { sides: [] };
    // 每次入场先签票。默认实现按请求的父回一张合法票据，测试只需要关心
    // 票据**之后**的行为；要测票据本身（错宿主、迟到、缺字段）的用例自己覆盖。
    if (method === "side/prepare") {
      // 每次 prepare 签一张新票：生产端就是这么做的，测试替身照抄，否则
      // 「迟到回包属于哪一次入场」这类断言会失去意义。
      prepared += 1;
      return {
        hostInstanceId: INSTANCE,
        parentThreadId: (params as { parentThreadId: string }).parentThreadId,
        entryId: `entry-${prepared}`,
      };
    }
    if (method === "side/exit") {
      return { ...(params as object), state: "closed" };
    }
    return {};
  });
  return { requests, request } as unknown as RpcClient & { requests: Request[] };
}

/** 只实现本 hook 用到的那几个方法。 */
function fakeController() {
  return {
    // 真控制器的 `submit` 返回三态收据（Task 2）：fake 必须同样返回，否则
    // hook 里 `receipt?.kind === "accepted"` 的判定在测试中永远为假。
    submit: vi.fn(
      async (
        _sideId: string,
        _text: string,
        requestId: string,
      ): Promise<SideSubmitReceipt> => ({ kind: "accepted", requestId }),
    ),
    reconcile: vi.fn(async () => {}),
    // 退出会要求控制器让那一代失效（停缓存、停在途对账）。假的也要有，否则
    // 「退出时不该再做别的事」这条断言会以 TypeError 的形态失败，而不是断言失败。
    invalidateGeneration: vi.fn(),
    client: null as unknown,
    dispose: vi.fn(),
  };
}

function setup(options: {
  responses?: Record<string, (params: any) => unknown>;
  supported?: boolean;
  scope?: "observe" | "control" | "admin";
  /** 绑定的连接状态。断网用例把它设成 false 来复现「退出时连不上」。 */
  connected?: () => boolean;
} = {}) {
  const store = new SideConversationStore();
  const client = fakeClient(options.responses);
  const binding = store.bind({
    key: sideHostKey(SLOT, INSTANCE),
    slot: SLOT,
    instanceId: INSTANCE,
    client,
    supported: options.supported ?? true,
    snapshotLifecycle: true,
    scope: options.scope ?? "control",
    connected: options.connected?.() ?? true,
  });
  const controllers: Array<ReturnType<typeof fakeController>> = [];
  const controllerFor = vi.fn(() => {
    const c = fakeController();
    c.client = binding.client;
    controllers.push(c);
    return c as unknown as SideSyncController;
  });
  const onChange = vi.fn();
  const exits = new SideExitCoordinator(() => {});
  let seq = 0;
  const hook = renderHook(() =>
    useSideConversationController({
      store,
      controllerFor,
      onChange,
      exits,
      newRequestId: () => `00000000-0000-4000-a000-${String(++seq).padStart(12, "0")}`,
    }),
  );
  return { store, binding, client, controllerFor, controllers, onChange, exits, hook };
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
    // 提交只认完整来源身份（hostKey/parentId/sideId/generation），不认「当前视图」：
    // 生产里 `controllerFor` 按 (binding, parentId) 记忆化，这里是每次新建，故取最后一个。
    const controller = s.controllers[s.controllers.length - 1];
    expect(controller.submit).toHaveBeenCalledWith(
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
    // 同一父上已有未关闭的旁支：第二次仍要签票 + open，让服务端把这张新票
    // 绑定到**同一代次**（spec §6.2.2）。这是本机获得退出权的唯一途径，
    // 少走一步就等于放弃了「Control 端可以销毁共享环境」。
    expect(
      s.client.requests.filter((r) => r.method === "side/prepare"),
    ).toHaveLength(2);
    expect(
      s.client.requests.filter((r) => r.method === "side/open"),
    ).toHaveLength(2);
    // 第二次 open 里的票与第一次不是同一张：每次入场都是一张新票。
    const tickets = s.client.requests
      .filter((r) => r.method === "side/open")
      .map((r) => (r.params as { entryId: string }).entryId);
    expect(new Set(tickets).size).toBe(2);
    // 服务端把它绑到同一代：本地视图仍是同一条旁支，没有被换成新的。
    expect(s.hook.result.current.active).toMatchObject({ kind: "side", sideId: "s1" });
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

describe("useSideConversationController navigation intent", () => {
  function summary(over: Record<string, unknown> = {}): SideSummaryWire {
    return {
      side_id: "s1",
      parent_thread_id: "t1",
      status: "idle" as SideStatusWire,
      snapshot_revision: 1,
      last_seq: 0,
      active_turn_id: null,
      expires_at: null,
      generation: 1,
      stopping: false,
      ...over,
    } as SideSummaryWire;
  }

  it("does not yank the user back into a side that opened after they left", async () => {
    let release!: (value: unknown) => void;
    const wait = new Promise((resolve) => {
      release = resolve;
    });
    const s = setup({ responses: { "side/open": () => wait } });
    let opening!: Promise<boolean>;
    act(() => {
      opening = s.hook.result.current.open(s.binding, "t1", "");
    });
    // 让 prepare 先回来，这样「在途」的确实是被 `back` 打断的那次 open。
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    // 用户等不及网络，先返回了主会话。
    act(() => s.hook.result.current.back());
    await act(async () => {
      release(summary());
      await opening;
    });
    // 迟到的 open 回包不得抢当前视图。
    expect(s.hook.result.current.active?.kind).not.toBe("side");
    // 也不能把它留下（spec §6.2.5「open 的迟到回包必须使本代销毁」）：
    // 不做对账（免得把即将销毁的帧拉回来），而是再补一个精确退出。
    expect(s.controllers[0].reconcile).not.toHaveBeenCalled();
    // 两次退出，同一张票，各有分工：`back` 那次是「票可能还没被消费，先撤销」，
    // open 迟到成功那次才是「这一代真的建起来了，销毁它」。
    const exits = s.client.requests.filter((r) => r.method === "side/exit");
    expect(exits).toHaveLength(2);
    expect(new Set(exits.map((r) => (r.params as { entryId: string }).entryId)).size).toBe(1);
    expect(s.controllers[0].invalidateGeneration).toHaveBeenCalledWith("s1", 1);
  });

  it("re-enters only after the previous generation is confirmed gone", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    expect(s.hook.result.current.active).toMatchObject({ kind: "side", sideId: "s1" });

    act(() => s.hook.result.current.back());
    // 退出意图刚被记录：宿主还没确认销毁，此时不能建第二套。
    act(() => {
      void s.hook.result.current.open(s.binding, "t1", "");
    });
    expect(s.hook.result.current.notice).toContain("正在退出");

    // 补发拿到 `closed`（fake 默认回 closed），屏障解除。
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(s.exits.blocked(INSTANCE, "t1")).toBe(false);

    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // 重新进入是一次**新的**入场：重新签票、重新 open，不复用旧环境。
    expect(
      s.client.requests.filter((r) => r.method === "side/prepare"),
    ).toHaveLength(2);
    expect(s.client.requests.filter((r) => r.method === "side/open")).toHaveLength(2);
    expect(s.hook.result.current.active).toMatchObject({ kind: "side", sideId: "s1" });
  });

  it("keeps the automatic first question as a draft when the user left first", async () => {
    let release!: (value: unknown) => void;
    const wait = new Promise((resolve) => {
      release = resolve;
    });
    const s = setup({ responses: { "side/open": () => wait } });
    let opening!: Promise<boolean>;
    act(() => {
      opening = s.hook.result.current.open(s.binding, "t1", "why slow?");
    });
    act(() => s.hook.result.current.back());
    await act(async () => {
      release(summary());
      await opening;
    });
    // 返回发生在首问下发之前：不替用户在后台发出这句问题……
    expect(
      s.controllers.some((c) =>
        (c.submit as unknown as { mock: { calls: unknown[][] } }).mock.calls.length > 0,
      ),
    ).toBe(false);
  });

  it("routes a submission to the side and host it was captured from", async () => {
    const s = setup();
    const other = s.store.bind({
      key: sideHostKey("proj-b" as never, "host-2"),
      slot: "proj-b" as never,
      instanceId: "host-2",
      client: s.client,
      supported: true,
      snapshotLifecycle: true,
      scope: "control",
      connected: true,
    });
    const baseline = (hostKey: string) => {
      s.store.installBaseline(hostKey, "s1", { snapshot_seq: 0 } as never, []);
      s.store.adopt(hostKey, "s1", "t1", 1);
      s.store.setSync(hostKey, "s1", "synced");
    };
    baseline(s.binding.key);
    baseline(other.key);
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    let submitting!: Promise<unknown>;
    act(() => {
      submitting = s.hook.result.current.submit("hello A");
    });
    // 提交在路上时用户切到 B 宿主上的同名 s1。
    act(() => s.hook.result.current.enter(other, "t1", "s1", 1));
    await act(async () => {
      await submitting;
    });
    // 提交只认出发时的身份：A 的控制器收到它，B 的控制器一次都没收到。
    const aController = s.controllers[0];
    expect(aController.submit).toHaveBeenCalledWith(
      "s1",
      "hello A",
      expect.any(String),
    );
    expect(s.controllers.slice(1).every((c) => c.submit.mock.calls.length === 0)).toBe(true);
  });

  it("returns a receipt from submit and rejects when no side is open", async () => {
    const s = setup();
    const receipt = await s.hook.result.current.submit("hello");
    expect(receipt).toEqual({
      kind: "rejected",
      requestId: "",
      message: "当前没有打开的旁问",
    });
    // 没有旁支就没有请求，也不该凭空造一个 requestId。
    expect(s.client.requests).toHaveLength(0);
  });

  it("rejects a submission aimed at a generation that has already been replaced", async () => {
    const s = setup();
    s.store.installBaseline(s.binding.key, "s1", { snapshot_seq: 0 } as never, []);
    s.store.adopt(s.binding.key, "s1", "t1", 1);
    s.store.setSync(s.binding.key, "s1", "synced");
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    // 宿主换代：同一 sideId 已经是另一条旁支。
    s.store.apply(s.binding.key, {
      method: "side/changed",
      params: {
        parent_thread_id: "t1",
        side_id: "s1",
        generation: 2,
        seq: 1,
        summary: summary({ generation: 2 }),
      },
    });
    const receipt = await s.hook.result.current.submit("stale");
    expect(receipt.kind).toBe("rejected");
    expect(receipt.requestId).toBe("");
    expect(s.controllers.every((c) => c.submit.mock.calls.length === 0)).toBe(true);
  });

  it("keeps a local return independent from an in-flight submission lock", async () => {
    const s = setup();
    s.store.installBaseline(s.binding.key, "s1", { snapshot_seq: 0 } as never, []);
    s.store.adopt(s.binding.key, "s1", "t1", 1);
    s.store.setSync(s.binding.key, "s1", "synced");
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    let release!: (v: unknown) => void;
    s.controllers.length = 0;
    s.controllerFor.mockImplementationOnce(() => {
      const c = fakeController();
      c.submit.mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            release = resolve as (v: unknown) => void;
          }),
      );
      s.controllers.push(c);
      return c as unknown as SideSyncController;
    });
    let submitting!: Promise<unknown>;
    act(() => {
      submitting = s.hook.result.current.submit("slow");
    });
    // 本地返回不依赖那条 RPC：网络还在路上也要立刻生效。
    act(() => s.hook.result.current.back());
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
    await act(async () => {
      release({ kind: "accepted", requestId: "x" });
      await submitting;
    });
    expect(s.client.requests.some((r) => r.method === "side/turnStart")).toBe(false);
  });
});

describe("useSideConversationController error attribution", () => {
  it("only surfaces an error for the side that produced it", async () => {
    const s = setup();
    s.store.installBaseline(s.binding.key, "s1", { snapshot_seq: 0 } as never, []);
    s.store.adopt(s.binding.key, "s1", "t1", 1);
    s.store.setSync(s.binding.key, "s1", "synced");
    s.controllerFor.mockImplementationOnce(() => {
      const c = fakeController();
      c.submit.mockResolvedValueOnce({
        kind: "rejected",
        requestId: "r",
        message: "模型拒绝了这条问题",
      });
      return c as unknown as SideSyncController;
    });
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    await act(async () => {
      await s.hook.result.current.submit("hello");
    });
    expect(s.hook.result.current.visibleError).toBe("模型拒绝了这条问题");
    expect(s.hook.result.current.errorTarget).toMatchObject({
      hostKey: s.binding.key,
      sideId: "s1",
    });
    // 回到主会话：那条失败仍然属于原旁支，但不再是「当前」可展示的错误。
    act(() => s.hook.result.current.back());
    expect(s.hook.result.current.visibleError).toBeNull();
    expect(s.hook.result.current.error).toBe("模型拒绝了这条问题");
  });

  it("does not let one host's failure show up as another host's", async () => {
    const s = setup();
    const other = s.store.bind({
      key: sideHostKey("proj-b" as never, "host-2"),
      slot: "proj-b" as never,
      instanceId: "host-2",
      client: s.client,
      supported: true,
      snapshotLifecycle: true,
      scope: "control",
      connected: true,
    });
    for (const hostKey of [s.binding.key, other.key]) {
      s.store.installBaseline(hostKey, "s1", { snapshot_seq: 0 } as never, []);
      s.store.adopt(hostKey, "s1", "t1", 1);
      s.store.setSync(hostKey, "s1", "synced");
    }
    s.controllerFor.mockImplementationOnce(() => {
      const c = fakeController();
      c.submit.mockResolvedValueOnce({
        kind: "rejected",
        requestId: "r",
        message: "A 失败了",
      });
      return c as unknown as SideSyncController;
    });
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    await act(async () => {
      await s.hook.result.current.submit("hello");
    });
    // 用户切到 B 宿主上的同名 s1：A 的失败不能当成 B 的错误横幅。
    act(() => s.hook.result.current.enter(other, "t1", "s1", 1));
    expect(s.hook.result.current.visibleError).toBeNull();
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

describe("useSideConversationController.leaveSide", () => {
  /** 一次成功的入场：prepare 由 fixture 默认给出，open 返回这一代。 */
  function summary(over: Record<string, unknown> = {}): SideSummaryWire {
    return {
      side_id: "s1",
      parent_thread_id: "t1",
      status: "idle" as SideStatusWire,
      snapshot_revision: 1,
      last_seq: 0,
      active_turn_id: null,
      expires_at: null,
      generation: 1,
      stopping: false,
      ...over,
    } as SideSummaryWire;
  }

  it("records the entry ticket and asks the host to destroy this generation", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // 条款顺序：先 prepare 拿票，再带票 open。
    const methods = s.client.requests.map((r) => r.method);
    expect(methods.indexOf("side/prepare")).toBeGreaterThanOrEqual(0);
    expect(methods.indexOf("side/open")).toBeGreaterThan(methods.indexOf("side/prepare"));
    const prepared = s.client.requests.find((r) => r.method === "side/prepare")!;
    expect(prepared.params).toEqual({ parentThreadId: "t1" });
    const opened = s.client.requests.find((r) => r.method === "side/open")!;
    expect(opened.params).toEqual({ parentThreadId: "t1", entryId: "entry-1" });

    await act(async () => {
      s.hook.result.current.back();
    });
    // 返回 = 退出：一张精确票据，三个身份齐全。
    const exits = s.client.requests.filter((r) => r.method === "side/exit");
    expect(exits).toHaveLength(1);
    expect(exits[0].params).toEqual({
      hostInstanceId: INSTANCE,
      parentThreadId: "t1",
      entryId: "entry-1",
    });
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
  });

  it("never sends the parent's interrupt or the old side/close", async () => {
    const s = setup({ responses: { "side/open": () => summary({ status: "running", active_turn_id: "t9" }) } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    await act(async () => {
      s.hook.result.current.back();
    });
    const methods = s.client.requests.map((r) => r.method);
    // 退出自带取消语义：不需要先 interrupt，也不走旧的 close 二次确认。
    expect(methods).not.toContain("side/turnInterrupt");
    expect(methods).not.toContain("turn/interrupt");
    expect(methods).not.toContain("side/close");
  });

  it("backs out during open without submitting a late first question", async () => {
    let resolveOpen!: (value: unknown) => void;
    const pending = new Promise((resolve) => {
      resolveOpen = resolve;
    });
    const s = setup({
      responses: {
        "side/prepare": () => ({
          hostInstanceId: INSTANCE,
          parentThreadId: "t1",
          entryId: "e1",
        }),
        "side/open": () => pending,
        "side/exit": (params: any) => ({ ...params, state: "closing" }),
      },
    });
    let opening!: Promise<boolean>;
    act(() => {
      opening = s.hook.result.current.open(s.binding, "t1", "首问");
    });
    await act(async () => {
      await Promise.resolve();
    });
    act(() => s.hook.result.current.back());
    await act(async () => {
      resolveOpen(summary());
      await opening;
    });
    // 用户已经走了：这次 open 迟到，必须在服务端销毁这一代，且**不提问**。
    expect(s.client.requests.some((r) => r.method === "side/exit")).toBe(true);
    expect(s.client.requests.some((r) => r.method === "side/turnStart")).toBe(false);
    expect(s.controllers.every((c) => c.submit.mock.calls.length === 0)).toBe(true);
    expect(s.hook.result.current.active?.kind).toBe("main");
  });

  it("only revokes the ticket when prepare lands after the user left", async () => {
    let resolvePrepare!: (value: unknown) => void;
    const pending = new Promise((resolve) => {
      resolvePrepare = resolve;
    });
    const s = setup({ responses: { "side/prepare": () => pending } });
    let opening!: Promise<boolean>;
    act(() => {
      opening = s.hook.result.current.open(s.binding, "t1", "");
    });
    act(() => s.hook.result.current.back());
    await act(async () => {
      resolvePrepare({
        hostInstanceId: INSTANCE,
        parentThreadId: "t1",
        entryId: "late-ticket",
      });
      await opening;
    });
    // 票还没被消费：只撤销它，绝不建环境。
    expect(s.client.requests.some((r) => r.method === "side/open")).toBe(false);
    // 撤票走的是 side/exit（票据图的同一个入口），回 `closed` 后屏障解除。
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(s.exits.blocked(INSTANCE, "t1")).toBe(false);
  });

  it("rejects a prepare receipt that describes another host or parent", async () => {
    const s = setup({
      responses: {
        "side/prepare": () => ({
          hostInstanceId: "another-host",
          parentThreadId: "t1",
          entryId: "e1",
        }),
      },
    });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // 不匹配的票既不能 open，也不能留着占屏障——它描述的不是这次停留。
    expect(s.client.requests.some((r) => r.method === "side/open")).toBe(false);
    expect(s.exits.blocked("another-host", "t1")).toBe(false);
    expect(s.hook.result.current.error).toContain("不匹配");
  });

  it("refuses to open while the previous generation is still exiting", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // 让宿主的 exit 一直停在 closing：屏障必须保持。
    s.exits.record({
      hostInstanceId: INSTANCE,
      parentThreadId: "t1",
      entryId: "still-exiting",
    });
    act(() => s.hook.result.current.back());
    const before = s.client.requests.filter((r) => r.method === "side/prepare").length;
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "第二个问题");
    });
    expect(
      s.client.requests.filter((r) => r.method === "side/prepare").length,
    ).toBe(before);
    expect(s.hook.result.current.notice).toContain("正在退出");
  });

  it("drops the generation and its draft on the way out", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    s.store.installBaseline(s.binding.key, "s1", { snapshot_seq: 0 } as never, []);
    s.store.adopt(s.binding.key, "s1", "t1", 1);
    s.store.setSync(s.binding.key, "s1", "synced");
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    expect(s.store.read(s.binding.key, "s1")).toBeDefined();
    await act(async () => {
      s.hook.result.current.back();
    });
    // 退出即丢弃：不留可续聊的正文、草稿或用量。
    expect(s.store.read(s.binding.key, "s1")).toBeUndefined();
  });

  it("stops the departing generation's controller without reconciling it back", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    const controller = s.controllers[s.controllers.length - 1];
    const before = controller.reconcile.mock.calls.length;
    await act(async () => {
      s.hook.result.current.back();
    });
    // 退出不是「重新对齐」：再对账一次会把即将销毁的正文拉回来。
    expect(controller.reconcile.mock.calls.length).toBe(before);
  });

  it("leaves through navigation without fighting the new view", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    act(() => s.hook.result.current.selectMain(s.binding, "t2"));
    // 切到另一条普通会话也是退出：意图已记录，视图交给导航。
    expect(s.exits.blocked(INSTANCE, "t1")).toBe(true);
    expect(s.hook.result.current.active).toMatchObject({ kind: "main", parentId: "t2" });
  });

  /**
   * 断网时退出：本地正文立刻丢弃，退出意图留着；重连后**先**补发再谈别的
   * （spec §6.4）。这条不能靠「反正下次握手会好」——环境还在宿主上占着槽位。
   */
  it("keeps the exit intent through a disconnect and sends it on reconnect", async () => {
    let connected = true;
    const s = setup({
      connected: () => connected,
      responses: { "side/open": () => summary() },
    });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    s.client.requests.length = 0;
    // 连接状态变了：真实路径走 `setConnected`（原地改，视图保留、禁发）。
    connected = false;
    s.store.setConnected(s.binding.key, false);

    act(() => s.hook.result.current.back());
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    // 意图仍在：屏障挡着重入，断线期间不发（发了也是白费）。
    expect(s.exits.blocked(INSTANCE, "t1")).toBe(true);
    expect(s.client.requests.filter((r) => r.method === "side/exit")).toHaveLength(0);

    connected = true;
    await act(async () => {
      await s.exits.flush({ ...s.binding, connected: true });
    });
    expect(s.client.requests.filter((r) => r.method === "side/exit")).toHaveLength(1);
    expect(s.exits.blocked(INSTANCE, "t1")).toBe(false);
  });

  /**
   * 宿主换了一代（重连到另一个进程）：旧票在新宿主上永远无法确认。此时不能
   * 拿新连接去发旧 exit——那会命中一个无关的环境（spec §6.2.3）。
   */
  it("drops old tickets instead of sending them to a replaced host", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    act(() => s.hook.result.current.back());
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    s.client.requests.length = 0;

    s.exits.hostReplaced(INSTANCE);
    await act(async () => {
      await s.exits.flush(s.binding);
    });
    expect(s.client.requests).toHaveLength(0);
    expect(s.exits.blocked(INSTANCE, "t1")).toBe(false);
  });

  it("leaves the local view without an exit RPC on a read-only device", async () => {
    const s = setup({ scope: "observe" });
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    act(() => s.hook.result.current.back());
    expect(s.client.requests).toHaveLength(0);
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
  });

  it("closeBackground exits the entry when it is the one on this device", async () => {
    const s = setup({ responses: { "side/open": () => summary() } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    await act(async () => {
      await s.hook.result.current.closeBackground(s.binding, "t1");
    });
    expect(s.client.requests.some((r) => r.method === "side/exit")).toBe(true);
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
  });

  it("does not pretend to close a side another device opened", async () => {
    const s = setup();
    // 本地有视图，但本次停留不是它（换句说：不是本机开的，没有票据）。
    s.store.installBaseline(s.binding.key, "s1", { snapshot_seq: 0 } as never, []);
    s.store.adopt(s.binding.key, "s1", "t1", 1);
    await act(async () => {
      await s.hook.result.current.closeBackground(s.binding, "t1");
    });
    // 没有票据就没有精确定位：如实说明，不虚构一次关闭。
    expect(s.client.requests.some((r) => r.method === "side/exit")).toBe(false);
    expect(s.hook.result.current.error).toContain("无法从本机退出");
  });

  it("refuses to close at all on a read-only device", async () => {
    const s = setup({ scope: "observe" });
    await act(async () => {
      await s.hook.result.current.closeBackground(s.binding, "t1");
    });
    expect(s.client.requests).toHaveLength(0);
    expect(s.hook.result.current.error).toContain("只读设备");
  });
});

describe("useSideConversationController.observe", () => {
  it("lists existing sides instead of opening one", async () => {
    const s = setup({
      scope: "observe",
      responses: {
        "side/list": () => ({
          sides: [
            {
              side_id: "s1",
              parent_thread_id: "t1",
              status: "idle",
              snapshot_revision: 1,
              last_seq: 0,
              active_turn_id: null,
              expires_at: null,
              generation: 1,
              stopping: false,
            },
          ],
        }),
      },
    });
    s.store.installBaseline(s.binding.key, "s1", { snapshot_seq: 0 } as never, []);
    s.store.adopt(s.binding.key, "s1", "t1", 1);
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    // Observe 的「打开」是一次纯读取：绝不能创建 runner，也不能发问。
    const methods = s.client.requests.map((r) => r.method);
    expect(methods).not.toContain("side/open");
    expect(methods).not.toContain("side/turnStart");
    expect(s.hook.result.current.active).toMatchObject({
      kind: "side",
      hostKey: s.binding.key,
      sideId: "s1",
      generation: 1,
    });
  });

  it("says there is nothing to look at when the parent has no side", async () => {
    const s = setup({ scope: "observe", responses: { "side/list": () => ({ sides: [] }) } });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "");
    });
    expect(s.hook.result.current.notice).toBe("这条会话还没有旁问");
    expect(s.hook.result.current.active).toBeNull();
    expect(s.client.requests.some((r) => r.method === "side/open")).toBe(false);
  });

  it("rejects an observe-only request to start a conversation", async () => {
    const s = setup({ scope: "observe" });
    await act(async () => {
      await s.hook.result.current.open(s.binding, "t1", "为什么不问我？");
    });
    expect(s.hook.result.current.error).toContain("只能查看");
    expect(s.client.requests).toHaveLength(0);
  });

  it("still lets a read-only device return locally", async () => {
    const s = setup({ scope: "observe" });
    act(() => s.hook.result.current.enter(s.binding, "t1", "s1", 1));
    act(() => s.hook.result.current.back());
    // 返回是纯本地动作：权限、连接、同步态都不该拦住它。
    expect(s.hook.result.current.active).toMatchObject({ kind: "main" });
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
