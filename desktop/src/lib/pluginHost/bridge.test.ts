import { describe, expect, it, vi } from "vitest";

import { BridgeFailure, PluginBridge, stableStringify, type BridgeBackend, type BridgePresent } from "./bridge";
import type { HostBinding, InitializationRequest } from "./types";

/**
 * 这些测试刻意**不** mock 身份校验。`fakeFrame()` 返回的是一个真实不同的对象，
 * 令牌来自真实的 `initialize`，桥用 WeakMap 做 frame → 绑定。T18-a 之所以重要，
 * 正是因为"把校验 mock 掉再断言它"什么也证明不了。
 */

function fakeFrame(label: string): { label: string; contentWindow: object } {
  return { label, contentWindow: { name: `frame-${label}` } };
}

function binding(overrides: Partial<HostBinding> = {}): HostBinding {
  return {
    host: "host-local",
    plugin: "example-counter",
    project: "ph1_aaaa",
    activation: 1,
    grant: 1,
    uiApi: "v1",
    ...overrides,
  };
}

function backend(overrides: Partial<BridgeBackend> = {}): BridgeBackend & {
  forward: ReturnType<typeof vi.fn>;
} {
  const forward = vi.fn(async (_binding: HostBinding, req: { method: string; params: unknown }) => ({
    result: { echoed: req.method, params: req.params },
  }));
  return {
    forward,
    snapshot: async (_binding, topic) => ({ topic, revision: 0, state: { seeded: true } }),
    ...overrides,
  } as BridgeBackend & { forward: ReturnType<typeof vi.fn> };
}

function present(): BridgePresent & { openThread: ReturnType<typeof vi.fn> } {
  return {
    notify: vi.fn(async () => {}),
    confirm: vi.fn(async () => true),
    openThread: vi.fn(async () => {}),
  };
}

function makeBridge(overrides: { backend?: ReturnType<typeof backend>; limits?: Record<string, number> } = {}) {
  const b = overrides.backend ?? backend();
  const p = present();
  let counter = 0;
  const bridge = new PluginBridge({
    backend: b,
    present: p,
    limits: overrides.limits,
    randomToken: () => `tok-${++counter}`,
    now: () => 1000,
  });
  return { bridge, backend: b, present: p };
}

const initMsg: InitializationRequest = { type: "init", nonce: "nonce-a", uiApi: "v1" };

function setupPair() {
  const frameA = fakeFrame("a");
  const frameB = fakeFrame("b");
  const harness = makeBridge();
  harness.bridge.registerFrame(frameA, {
    binding: binding(),
    methods: ["counter/read", "counter/increment"],
    plugin: { id: "example-counter", version: "0.1.0" },
    host: { id: "host-local", version: "1.0.0" },
  });
  harness.bridge.registerFrame(frameB, {
    binding: binding({ plugin: "other-plugin", project: "ph1_bbbb" }),
    methods: ["counter/read"],
    plugin: { id: "other-plugin", version: "0.1.0" },
    host: { id: "host-local", version: "1.0.0" },
  });
  const initA = harness.bridge.initialize(frameA, initMsg);
  const initB = harness.bridge.initialize(frameB, { ...initMsg, nonce: "nonce-b" });
  return { ...harness, frameA, frameB, tokenA: initA.token, tokenB: initB.token };
}

describe("T18-a: identity comes from the frame, never from the message", () => {
  it("does not trust plugin-supplied identity", async () => {
    const { bridge, backend: b, frameA, frameB, tokenB } = setupPair();

    // 拿 A 的令牌，从 B 这个真实不同的 frame 发出去。
    const reply = await bridge.receive(frameB, {
      requestId: "r-1",
      method: "counter/read",
      params: {},
      token: tokenB,
    });

    // 关键：即使报文自称属于 A，桥只看 frameB 的绑定。B 没有声明 increment，
    // 因此越权请求不可能打到 A 的绑定上。
    expect(reply.ok).toBe(true);
    expect(b.forward).toHaveBeenCalledTimes(1);
    const [usedBinding] = b.forward.mock.calls[0];
    expect(usedBinding.plugin).toBe("other-plugin");
    expect(usedBinding.plugin).not.toBe("example-counter");

    const denied = await bridge.receive(frameB, {
      requestId: "r-2",
      method: "counter/increment",
      params: {},
      token: tokenB,
    });
    expect(denied).toMatchObject({ ok: false, error: { kind: "NotSupported" } });

    // 用 A 的令牌去驱动 B 的 frame：令牌与 frame 不匹配，直接拒绝。
    const wrongToken = await bridge.receive(frameB, {
      requestId: "r-3",
      method: "counter/read",
      params: {},
      token: "tok-A-forged",
    });
    expect(wrongToken).toMatchObject({ ok: false, error: { kind: "PermissionDenied" } });
    expect(frameA.contentWindow).not.toBe(frameB.contentWindow);
    expect(b.forward).toHaveBeenCalledTimes(1);
  });

  it("rejects a message from an unregistered frame", async () => {
    const { bridge, backend: b, tokenA } = setupPair();
    const stray = fakeFrame("stray");
    const reply = await bridge.receive(stray, {
      requestId: "r-stray",
      method: "counter/read",
      params: {},
      token: tokenA,
    });
    expect(reply).toMatchObject({ ok: false, error: { kind: "PermissionDenied" } });
    expect(b.forward).not.toHaveBeenCalled();
  });

  it("never carries management methods across the bridge", async () => {
    const { bridge, backend: b, frameA, tokenA } = setupPair();
    const reply = await bridge.receive(frameA, {
      requestId: "r-mgmt",
      method: "plugins/v1/install",
      params: { plugin: "evil" },
      token: tokenA,
    });
    expect(reply).toMatchObject({ ok: false, error: { kind: "PermissionDenied" } });
    expect(b.forward).not.toHaveBeenCalled();
  });
});

describe("T18-b: replayed initialization", () => {
  it("refuses a second init on the same frame", () => {
    const { bridge, frameA } = setupPair();
    expect(() => bridge.initialize(frameA, { ...initMsg, nonce: "nonce-2" })).toThrow(BridgeFailure);
    try {
      bridge.initialize(frameA, { ...initMsg, nonce: "nonce-2" });
    } catch (error) {
      expect((error as BridgeFailure).kind).toBe("PermissionDenied");
    }
  });

  it("refuses a reused nonce on a fresh frame", () => {
    const { bridge } = setupPair();
    const fresh = fakeFrame("fresh");
    bridge.registerFrame(fresh, {
      binding: binding(),
      methods: ["counter/read"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    expect(() => bridge.initialize(fresh, { text: "init", nonce: "nonce-a", uiApi: "v1" } as never)).toThrow();
  });

  it("refuses a message before the handshake completes", async () => {
    const { bridge, frameA, tokenA } = setupPair();
    const pending = fakeFrame("pending");
    bridge.registerFrame(pending, {
      binding: binding(),
      methods: ["counter/read"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    const reply = await bridge.receive(pending, {
      requestId: "r-early",
      method: "counter/read",
      params: {},
      token: tokenA,
    });
    expect(reply).toMatchObject({ ok: false, error: { kind: "PermissionDenied" } });
    expect(frameA).toBeTruthy();
  });
});

describe("T18-c: responses from an outdated grant are discarded", () => {
  it("drops an in-flight response once the grant advanced", async () => {
    const frameA = fakeFrame("a");
    let release!: (value: { result: unknown }) => void;
    const forward = vi.fn(
      () =>
        new Promise<{ result: unknown }>((resolve) => {
          release = resolve;
        }),
    );
    const bridge = new PluginBridge({
      backend: { forward, snapshot: async (_b, topic) => ({ topic, revision: 0, state: null }) },
      present: present(),
      randomToken: (() => {
        let n = 0;
        return () => `tok-${++n}`;
      })(),
    });
    bridge.registerFrame(frameA, {
      binding: binding({ grant: 1 }),
      methods: ["counter/increment"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    const { token } = bridge.initialize(frameA, initMsg);

    const inFlight = bridge.receive(frameA, {
      requestId: "r-grant",
      method: "counter/increment",
      params: { by: 1 },
      token,
    });

    // 授权递增：旧通道失效。
    bridge.rebind(frameA, binding({ grant: 2 }));
    release({ result: { total: 1 } });

    const reply = await inFlight;
    expect(reply).toMatchObject({ ok: false, error: { kind: "ResultUnknown" } });
    expect(bridge.result(frameA, "r-grant").status).toBe("unknown");

    // 新页面拿到同样的 requestId 也不能被喂进旧响应。
    const reread = bridge.result(frameA, "r-grant");
    expect(reread.result).toBeNull();
  });
});

describe("T18-d: reload never replays a write", () => {
  it("marks an unsettled write unknown and never resends it", async () => {
    const frameA = fakeFrame("a");
    const forward = vi.fn(async () => ({ result: { ok: true } }));
    const bridge = new PluginBridge({
      backend: { forward, snapshot: async (_b, topic) => ({ topic, revision: 0, state: null }) },
      present: present(),
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: ["counter/increment"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    const { token } = bridge.initialize(frameA, initMsg);

    // 先在途，然后 frame 被销毁（页面重载前的等价事件）。
    const hanging = new Promise<never>(() => {});
    forward.mockImplementationOnce(() => hanging as never);
    void bridge.receive(frameA, {
      requestId: "r-reload",
      method: "counter/increment",
      params: { by: 1 },
      token,
    });

    expect(bridge.dropFrame(frameA)).toBe(0);
    expect(bridge.result(frameA, "r-reload").status).toBe("unknown");
    expect(forward).toHaveBeenCalledTimes(1);

    // 重载后的新 frame 用同一 requestId 再发：桥不再执行，直接给出 unknown
    // 语义上的拒绝（未确认的写不允许自动重放）。
    const reloaded = fakeFrame("reloaded");
    bridge.registerFrame(reloaded, {
      binding: binding(),
      methods: ["counter/increment"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    const init2 = bridge.initialize(reloaded, { ...initMsg, nonce: "nonce-reload" });
    const retry = await bridge.receive(reloaded, {
      requestId: "r-reload",
      method: "counter/increment",
      params: { by: 1 },
      token: init2.token,
    });
    expect(retry).toMatchObject({ ok: false, error: { kind: "ResultUnknown" } });
    expect(forward).toHaveBeenCalledTimes(1);
  });
});

describe("T18-e: same request id with different params", () => {
  it("returns the original result for an identical retry and Conflict for a changed one", async () => {
    const { bridge, backend: b, frameA, tokenA } = setupPair();

    const first = await bridge.receive(frameA, {
      requestId: "r-idem",
      method: "counter/increment",
      params: { by: 1, note: "x" },
      token: tokenA,
    });
    expect(first.ok).toBe(true);

    // 键序不同但语义相同：必须当成同一个请求。
    const retry = await bridge.receive(frameA, {
      requestId: "r-idem",
      method: "counter/increment",
      params: { note: "x", by: 1 },
      token: tokenA,
    });
    expect(retry.ok).toBe(true);
    expect(b.forward).toHaveBeenCalledTimes(1);

    const changed = await bridge.receive(frameA, {
      requestId: "r-idem",
      method: "counter/increment",
      params: { by: 2, note: "x" },
      token: tokenA,
    });
    expect(changed).toMatchObject({ ok: false, error: { kind: "Conflict" } });
    expect(b.forward).toHaveBeenCalledTimes(1);
  });

  it("stableStringify ignores key order at every depth", () => {
    expect(stableStringify({ a: 1, b: { c: 2, d: [1, 2] } })).toBe(
      stableStringify({ b: { d: [1, 2], c: 2 }, a: 1 }),
    );
  });
});

describe("T18-f: limits and unknown methods", () => {
  it("rejects an oversized message with TooLarge", async () => {
    const { bridge, backend: b, frameA, tokenA } = setupPair();
    const reply = await bridge.receive(frameA, {
      requestId: "r-big",
      method: "counter/read",
      params: { blob: "x".repeat(300 * 1024) },
      token: tokenA,
    });
    expect(reply).toMatchObject({ ok: false, error: { kind: "TooLarge" } });
    expect(b.forward).not.toHaveBeenCalled();
  });

  it("rejects concurrency above the in-flight limit with Busy", async () => {
    const frameA = fakeFrame("a");
    const resolves: Array<() => void> = [];
    const forward = vi.fn(
      () => new Promise<{ result: unknown }>((resolve) => resolves.push(() => resolve({ result: 1 }))),
    );
    const bridge = new PluginBridge({
      backend: { forward, snapshot: async (_b, topic) => ({ topic, revision: 0, state: null }) },
      present: present(),
      limits: { maxInFlight: 2 },
      now: () => 1000,
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: ["counter/read"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    const { token } = bridge.initialize(frameA, initMsg);

    void bridge.receive(frameA, { requestId: "r-1", method: "counter/read", params: {}, token });
    void bridge.receive(frameA, { requestId: "r-2", method: "counter/read", params: {}, token });
    const third = await bridge.receive(frameA, {
      requestId: "r-3",
      method: "counter/read",
      params: {},
      token,
    });
    expect(third).toMatchObject({ ok: false, error: { kind: "Busy" } });
    expect(forward).toHaveBeenCalledTimes(2);
  });

  it("rejects above the per-second rate limit with RateLimited", async () => {
    const { bridge, frameA, tokenA } = setupPair();
    for (let i = 0; i < 60; i += 1) {
      const reply = await bridge.receive(frameA, {
        requestId: `r-rate-${i}`,
        method: "counter/read",
        params: { i },
        token: tokenA,
      });
      expect(reply.ok).toBe(true);
    }
    const over = await bridge.receive(frameA, {
      requestId: "r-rate-over",
      method: "counter/read",
      params: { i: 60 },
      token: tokenA,
    });
    expect(over).toMatchObject({ ok: false, error: { kind: "RateLimited" } });
  });

  it("answers NotSupported for an undeclared method", async () => {
    const { bridge, backend: b, frameA, tokenA } = setupPair();
    const reply = await bridge.receive(frameA, {
      requestId: "r-unknown",
      method: "counter/deleteEverything",
      params: {},
      token: tokenA,
    });
    expect(reply).toMatchObject({ ok: false, error: { kind: "NotSupported" } });
    expect(b.forward).not.toHaveBeenCalled();
  });
});

describe("T18-g: an event gap forces a snapshot first", () => {
  it("delivers the snapshot and continues from its revision", async () => {
    const frameA = fakeFrame("a");
    const bridge = new PluginBridge({
      backend: {
        forward: async () => ({ result: null }),
        snapshot: async (_b, topic) => ({ topic, revision: 4, state: { total: 4 } }),
      },
      present: present(),
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);

    for (let i = 0; i < 6; i += 1) bridge.publish("counter.state", { total: i });

    const seen: Array<{ revision: number; payload: unknown }> = [];
    const handle = await bridge.subscribe(frameA, "counter.state", 2, (event) => {
      seen.push({ revision: event.revision, payload: event.payload });
    });

    expect(handle.snapshot?.state).toEqual({ total: 4 });
    // 补发只含快照之后的 5、6。
    expect(seen.map((event) => event.revision)).toEqual([5, 6]);
  });

  it("subscribes directly when after is continuous", async () => {
    const frameA = fakeFrame("a");
    const bridge = new PluginBridge({
      backend: { forward: async () => ({ result: null }) },
      present: present(),
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);
    bridge.publish("counter.state", { total: 1 });
    bridge.publish("counter.state", { total: 2 });

    const handle = await bridge.subscribe(frameA, "counter.state", 2, () => {});
    expect(handle.snapshot).toBeNull();
    expect(handle.revision).toBe(2);
  });

  it("fails closed when there is no snapshot source", async () => {
    const frameA = fakeFrame("a");
    const bridge = new PluginBridge({
      backend: { forward: async () => ({ result: null }) },
      present: present(),
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);
    bridge.publish("counter.state", { total: 1 });

    await expect(bridge.subscribe(frameA, "counter.state", 0, () => {})).rejects.toMatchObject({
      kind: "RecoveryRequired",
    });
  });
});

describe("T18-h: subscription cleanup is counted, not assumed", () => {
  it("removes every subscription when the frame is dropped", async () => {
    const frameA = fakeFrame("a");
    const bridge = new PluginBridge({
      backend: {
        forward: async () => ({ result: null }),
        snapshot: async (_b, topic) => ({ topic, revision: 0, state: null }),
      },
      present: present(),
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);

    await bridge.subscribe(frameA, "counter.state", 0, () => {});
    await bridge.subscribe(frameA, "counter.log", 0, () => {});
    expect(bridge.subscriberCount()).toBe(2);

    expect(bridge.dropFrame(frameA)).toBe(2);
    expect(bridge.subscriberCount()).toBe(0);
  });

  it("removes every subscription when the project is switched", async () => {
    const frameA = fakeFrame("a");
    const bridge = new PluginBridge({
      backend: {
        forward: async () => ({ result: null }),
        snapshot: async (_b, topic) => ({ topic, revision: 0, state: null }),
      },
      present: present(),
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);

    await bridge.subscribe(frameA, "counter.state", 0, () => {});
    await bridge.subscribe(frameA, "counter.state", 0, () => {});
    expect(bridge.subscriberCount("counter.state")).toBe(2);

    bridge.rebind(frameA, binding({ project: "ph1_cccc", grant: 2 }));
    expect(bridge.subscriberCount("counter.state")).toBe(0);
  });

  it("enforces the subscription limit", async () => {
    const frameA = fakeFrame("a");
    const bridge = new PluginBridge({
      backend: {
        forward: async () => ({ result: null }),
        snapshot: async (_b, topic) => ({ topic, revision: 0, state: null }),
      },
      present: present(),
      limits: { maxSubscriptions: 1 },
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);
    await bridge.subscribe(frameA, "a", 0, () => {});
    await expect(bridge.subscribe(frameA, "b", 0, () => {})).rejects.toMatchObject({
      kind: "Busy",
    });
  });
});

describe("openThread scope", () => {
  it("refuses a thread outside the plugin's project scope", async () => {
    const frameA = fakeFrame("a");
    const p = present();
    const bridge = new PluginBridge({
      backend: { forward: async () => ({ result: null }) },
      present: p,
      randomToken: () => "tok",
    });
    bridge.registerFrame(frameA, {
      binding: binding(),
      methods: [],
      threads: ["thread-1"],
      plugin: { id: "example-counter", version: "0.1.0" },
      host: { id: "host-local", version: "1.0.0" },
    });
    bridge.initialize(frameA, initMsg);

    await bridge.openThread(frameA, "thread-1");
    expect(p.openThread).toHaveBeenCalledWith("thread-1");

    await expect(bridge.openThread(frameA, "thread-2")).rejects.toMatchObject({
      kind: "PermissionDenied",
    });
    expect(p.openThread).toHaveBeenCalledTimes(1);
  });
});
