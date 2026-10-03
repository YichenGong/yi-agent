import { describe, it, expect } from "vitest";
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
});
