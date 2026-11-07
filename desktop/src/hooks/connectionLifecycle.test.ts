/** @vitest-environment jsdom */
import { beforeEach, describe, expect, it, vi } from "vitest";

import { ClientRegistry } from "../lib/clientRegistry";
import { LOCAL_SLOT, type Slot } from "../lib/slots";
import type { TraceConnection } from "../lib/subagentTraceSubscription";

/**
 * 这些用例只钉一件事：**轨迹来源的登记顺序与身份**。
 *
 * 交错顺序（登记在 `wireClient` 之前）不是风格问题：路由的回调闭包按 client
 * 反查来源，晚注册一步，握手后的第一段轨迹就会被当成未知来源丢掉。
 *
 * `RpcClient` 在这里被整体替换：真实实例要在 `-32010` 时才会调用 recovery 钩子，
 * 而那需要一整套假 transport 与错误帧。构造参数是可以直接捕获的接缝——我们要
 * 证明的正是「注入的钩子闭包里带着这条 client 与它的 slot」，用真实实例反而
 * 绕远。
 */
type CapturedRecover = ((raw: (m: string, a: unknown) => Promise<unknown>) => Promise<void>) | null;

const { constructed } = vi.hoisted(() => ({
  constructed: [] as Array<{
    recover: CapturedRecover;
    disposed: boolean;
  }>,
}));

vi.mock("../lib/rpc", () => ({
  RpcClient: class {
    readonly record: { recover: CapturedRecover; disposed: boolean };
    constructor(
      _transport: unknown,
      recover?: (raw: (m: string, a: unknown) => Promise<unknown>) => Promise<void>,
    ) {
      this.record = { recover: recover ?? null, disposed: false };
      constructed.push(this.record);
    }
    dispose() {
      this.record.disposed = true;
    }
  },
}));

const { createConnectionLifecycle } = await import("./connectionLifecycle");

function harness(overrides: { transportThrows?: boolean } = {}) {
  const order: string[] = [];
  const registered: TraceConnection[] = [];
  const wiring: { client: unknown; slot: Slot }[] = [];
  const registryRef = { current: new ClientRegistry<{ dispose(): void }>() };
  const lifecycle = createConnectionLifecycle({
    activeSlotRef: { current: LOCAL_SLOT },
    clientRef: { current: null },
    registryRef: registryRef as never,
    restartTimer: { current: null },
    attempt: { current: 0 },
    disposed: () => false,
    toppedUp: { current: new Set<string>() },
    setStatus: () => {},
    transportFactory: () => {
      if (overrides.transportThrows) throw new Error("bad url");
      return {} as never;
    },
    recoveryRef: { current: null },
    handshake: async () => {},
    wireClient: (client, slot) => {
      order.push("wire");
      wiring.push({ client, slot });
    },
    sideStore: {} as never,
    onTraceConnection: (connection) => {
      order.push("register");
      registered.push(connection);
    },
    serverBindingGeneration: () => 3,
    nextTraceConnectionGeneration: () => registered.length + 1,
  });
  return { lifecycle, order, registered, wiring };
}

beforeEach(() => {
  constructed.length = 0;
});

describe("connectionLifecycle 的轨迹来源登记", () => {
  it("先登记来源，再挂回调", () => {
    const { lifecycle, order, registered, wiring } = harness();
    const client = lifecycle.connect();

    expect(order).toEqual(["register", "wire"]);
    expect(registered).toHaveLength(1);
    expect(registered[0]).toMatchObject({
      slot: LOCAL_SLOT,
      serverBindingGeneration: 3,
      connectionGeneration: 1,
    });
    expect(registered[0].client).toBe(client);
    expect(wiring[0]).toEqual({ client, slot: LOCAL_SLOT });
  });

  it("中继自愈钩子带上发起它的那条连接与槽位", async () => {
    const recovery = vi.fn().mockResolvedValue(undefined);
    const order: string[] = [];
    const registryRef = { current: new ClientRegistry<{ dispose(): void }>() };
    const lifecycle = createConnectionLifecycle({
      activeSlotRef: { current: "remote-a" },
      clientRef: { current: null },
      registryRef: registryRef as never,
      restartTimer: { current: null },
      attempt: { current: 0 },
      disposed: () => false,
      toppedUp: { current: new Set<string>() },
      setStatus: () => {},
      transportFactory: () => ({}) as never,
      // 钩子是 `(raw, client, slot)`：生命周期这一层要把它闭包到具体连接上。
      recoveryRef: { current: recovery as never },
      handshake: async () => {},
      wireClient: () => {},
      sideStore: {} as never,
      onTraceConnection: () => {
        order.push("register");
      },
      serverBindingGeneration: () => 0,
      nextTraceConnectionGeneration: () => 1,
    });

    lifecycle.connect();
    const record = constructed[constructed.length - 1];
    const raw = vi.fn().mockResolvedValue(undefined);
    await record.recover!(raw);

    // 三个参数一个都不能少：少了 client/slot，恢复期间的「就绪」就无从归属。
    expect(recovery).toHaveBeenCalledTimes(1);
    expect(recovery.mock.calls[0][0]).toBe(raw);
    expect(recovery.mock.calls[0][2]).toBe("remote-a");
    expect(recovery.mock.calls[0][1]).toBeTruthy();
  });

  it("transport 建不起来时不登记来源", () => {
    const { lifecycle, registered } = harness({ transportThrows: true });
    expect(() => lifecycle.connect()).toThrow();
    expect(registered).toHaveLength(0);
  });
});
