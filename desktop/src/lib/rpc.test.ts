import { describe, it, expect, vi } from "vitest";
import { RpcClient, type Transport } from "./rpc";

function fakeTransport(opts: { failSend?: boolean } = {}) {
  const sent: any[] = [];
  // Model the transport as a set of subscribers (like Tauri's event listeners),
  // so a test can prove that a client which forgets to dispose keeps receiving
  // frames alongside its replacement.
  const messageListeners = new Set<(m: unknown) => void>();
  const transport: Transport = {
    send: async (m) => {
      if (opts.failSend) throw new Error("sidecar down");
      sent.push(m);
    },
    respond: async () => {},
    onMessage: (cb) => {
      messageListeners.add(cb);
      return () => {
        messageListeners.delete(cb);
      };
    },
    onRequest: () => () => {},
    onStatus: () => () => {},
    dispose: () => {},
  };
  return {
    transport,
    sent,
    emit: (m: unknown) => {
      for (const cb of [...messageListeners]) cb(m);
    },
    messageListenerCount: () => messageListeners.size,
  };
}

describe("RpcClient", () => {
  it("allocates increasing ids and resolves on matching response", async () => {
    const { transport, sent, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const p = client.request("initialize", {});
    expect(sent[0].id).toBe(1);
    emit({ jsonrpc: "2.0", id: 1, result: { ok: true } });
    await expect(p).resolves.toEqual({ ok: true });
  });

  it("rejects on error response", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const p = client.request("nope", {});
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32601, message: "no" } });
    await expect(p).rejects.toMatchObject({ code: -32601 });
  });

  it("rejects with a buffered error response when the matching request is made later", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    // Error response arrives before any request allocates this id.
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32601, message: "no" } });
    const p = client.request("initialize", {});
    await expect(p).rejects.toMatchObject({ code: -32601 });
  });

  it("drains a buffered response when the matching request is made later", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    // A response arrives for an id with no pending entry yet: it gets buffered.
    emit({ jsonrpc: "2.0", id: 1, result: "early" });
    // The next request allocates id 1 and must consume the buffered response
    // (rather than registering a pending entry that would hang forever).
    const first = client.request("initialize", {});
    await expect(first).resolves.toBe("early");
    // The buffer was drained, not merely left in place: a fresh request still
    // correlates normally.
    const second = client.request("thread/list", {});
    emit({ jsonrpc: "2.0", id: 2, result: ["t1"] });
    await expect(second).resolves.toEqual(["t1"]);
  });

  it("fans notifications out to subscribers and stops after unsubscribe", () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const seen: unknown[] = [];
    const off = client.onNotification((n) => seen.push(n));
    emit({ jsonrpc: "2.0", method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    expect(seen).toHaveLength(1);
    off();
    emit({ jsonrpc: "2.0", method: "turn/started", params: { thread_id: "t", turn_id: "u2" } });
    expect(seen).toHaveLength(1);
  });

  it("releases the transport subscription on dispose so a reconnect cannot double-deliver", () => {
    // The App rebuilds its client on every reconnect; the old transport is still
    // alive in the Tauri/ws case and keeps delivering frames. Unless the old
    // client drops its transport-level listener, every notification (e.g. an
    // append-only `item/delta`) is applied once per live client and the text
    // duplicates until `item/completed` replaces the item wholesale.
    const { transport, emit, messageListenerCount } = fakeTransport();
    const client = new RpcClient(transport);
    // `onNotification` is a fan-out on top of the transport listener.
    client.onNotification(() => {});
    expect(messageListenerCount()).toBe(1);
    client.dispose();
    expect(messageListenerCount()).toBe(0);
    // A frame arriving afterward must reach nobody.
    const seen: unknown[] = [];
    emit({ jsonrpc: "2.0", method: "turn/started", params: {} });
    expect(seen).toHaveLength(0);
  });

  it("rejects and leaves no pending entry when the transport send fails", async () => {
    const { transport } = fakeTransport({ failSend: true });
    const client = new RpcClient(transport);
    await expect(client.request("initialize", {})).rejects.toThrow("sidecar down");
    // The failed request must not leak its pending entry.
    const pending = (client as unknown as { pending: Map<unknown, unknown> }).pending;
    expect(pending.size).toBe(0);
  });

  it("keeps the buffered response when the send fails", async () => {
    const { transport, emit } = fakeTransport({ failSend: true });
    const client = new RpcClient(transport);
    // Response arrives before any request allocates the id -> buffered.
    emit({ jsonrpc: "2.0", id: 1, result: "early" });
    await expect(client.request("initialize", {})).rejects.toThrow("sidecar down");
    // The buffered entry is dropped only after a successful send, so it must
    // survive a failed send instead of losing the response.
    const buffered = (client as unknown as { buffered: Map<unknown, unknown> }).buffered;
    expect(buffered.size).toBe(1);
  });
  // --- 中继桥接重启后的自动恢复 ---------------------------------------------
  // 经中继时，全 session 只有**一条**电脑侧桥接连接被所有手机共享。app-server
  // 换新进程后桥接重连，服务端为这条新连接铸一个新的 `ws-<uuid>`，其
  // `initialized=false`；而手机的 socket 挂在中继上从未断开，也就不会重发
  // `initialize`。于是手机在切 session 时发的 `thread/subscribe`/`thread/resume`
  // 会被拒为 `-32010 server not initialized`（实测复现）。修法：RpcClient 收到
  // `-32010` 时先重新握手，再把该请求**自动重发一次**——覆盖所有调用点，不依赖
  // 用户切前后台。

  it("re-handshakes and retries once when refused with -32010", async () => {
    const { transport, sent, emit } = fakeTransport();
    const recover = vi.fn(async () => {});
    const client = new RpcClient(transport, recover);
    const p = client.request("thread/subscribe", { threadIds: ["t1"] });
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32010, message: "server not initialized" } });
    await vi.waitFor(() => expect(sent).toHaveLength(2));
    expect(recover).toHaveBeenCalledTimes(1);
    // 重发的必须是**同一个**请求，而非别的东西。
    expect(sent[1].method).toBe("thread/subscribe");
    expect(sent[1].params).toEqual({ threadIds: ["t1"] });
    emit({ jsonrpc: "2.0", id: 2, result: { ok: true } });
    await expect(p).resolves.toEqual({ ok: true });
  });

  it("never auto-recovers `initialize` itself, so recovery cannot recurse", async () => {
    const { transport, emit } = fakeTransport();
    const recover = vi.fn(async () => {});
    const client = new RpcClient(transport, recover);
    const p = client.request("initialize", {});
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32010, message: "server not initialized" } });
    await expect(p).rejects.toMatchObject({ code: -32010 });
    expect(recover).not.toHaveBeenCalled();
  });

  it("retries only once: a second -32010 after recovery is fatal", async () => {
    const { transport, sent, emit } = fakeTransport();
    const recover = vi.fn(async () => {});
    const client = new RpcClient(transport, recover);
    const p = client.request("thread/resume", { threadId: "t1" });
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32010, message: "server not initialized" } });
    await vi.waitFor(() => expect(sent).toHaveLength(2));
    emit({ jsonrpc: "2.0", id: 2, error: { code: -32010, message: "server not initialized" } });
    await expect(p).rejects.toMatchObject({ code: -32010 });
    expect(recover).toHaveBeenCalledTimes(1);
    expect(sent).toHaveLength(2); // 不得第三次重发（防死循环）
  });

  it("shares one recovery across concurrent -32010 refusals", async () => {
    const { transport, sent, emit } = fakeTransport();
    let release!: () => void;
    const recover = vi.fn(() => new Promise<void>((r) => { release = r; }));
    const client = new RpcClient(transport, recover);
    const a = client.request("thread/subscribe", { threadIds: ["a"] });
    const b = client.request("thread/resume", { threadId: "b" });
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32010, message: "server not initialized" } });
    emit({ jsonrpc: "2.0", id: 2, error: { code: -32010, message: "server not initialized" } });
    // 两个拒绝同时到达：只允许跑一次握手（否则是握手风暴）。
    await vi.waitFor(() => expect(recover).toHaveBeenCalledTimes(1));
    await Promise.resolve();
    expect(recover).toHaveBeenCalledTimes(1);
    release();
    await vi.waitFor(() => expect(sent).toHaveLength(4));
    emit({ jsonrpc: "2.0", id: 3, result: 1 });
    emit({ jsonrpc: "2.0", id: 4, result: 2 });
    await expect(a).resolves.toBe(1);
    await expect(b).resolves.toBe(2);
  });

  it("hands the recovery hook a raw request fn so its own requests cannot re-enter recovery", async () => {
    // 恢复钩子会重放订阅（`thread/subscribe`）。若那些请求走的是会自动恢复的
    // `request`，而它们恰好又被拒为 -32010，就会去 await 自己正在跑的那次恢复
    // ——死锁。钩子拿到的必须是**裸**请求：被拒就直接失败，不再触发恢复。
    const { transport, sent, emit } = fakeTransport();
    let innerRefused = false;
    const recover = vi.fn(async (raw: (m: string, a: unknown) => Promise<unknown>) => {
      const p = raw("thread/subscribe", { threadIds: ["t1"] });
      await vi.waitFor(() => expect(sent.length).toBeGreaterThan(0));
      const last = sent[sent.length - 1].id;
      emit({ jsonrpc: "2.0", id: last, error: { code: -32010, message: "server not initialized" } });
      // 裸请求直接抛出 -32010，不触发第二次恢复。
      await expect(p).rejects.toMatchObject({ code: -32010 });
      innerRefused = true;
    });
    const client = new RpcClient(transport, recover);
    const outer = client.request("thread/resume", { threadId: "t1" });
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32010, message: "server not initialized" } });
    await vi.waitFor(() => expect(innerRefused).toBe(true));
    // 恢复钩子只跑了一次（内部请求没有再触发一次恢复）。
    expect(recover).toHaveBeenCalledTimes(1);
    // 外层请求重发；这里让它成功收尾。
    const resent = sent[sent.length - 1].id;
    emit({ jsonrpc: "2.0", id: resent, result: { ok: 1 } });
    await expect(outer).resolves.toEqual({ ok: 1 });
  });

  it("passes non-32010 errors straight through without recovering", async () => {
    const { transport, emit } = fakeTransport();
    const recover = vi.fn(async () => {});
    const client = new RpcClient(transport, recover);
    const p = client.request("thread/resume", { threadId: "t1" });
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32011, message: "unknown thread: t1" } });
    await expect(p).rejects.toMatchObject({ code: -32011 });
    expect(recover).not.toHaveBeenCalled();
  });

  it("leaves -32010 untouched when no recovery hook is configured", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    const p = client.request("thread/listAll", {});
    emit({ jsonrpc: "2.0", id: 1, error: { code: -32010, message: "server not initialized" } });
    await expect(p).rejects.toMatchObject({ code: -32010 });
  });

});
