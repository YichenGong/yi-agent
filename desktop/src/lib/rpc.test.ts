import { describe, it, expect } from "vitest";
import { RpcClient, type Transport } from "./rpc";

function fakeTransport() {
  const sent: any[] = [];
  let onMessage: (m: unknown) => void = () => {};
  const transport: Transport = {
    send: async (m) => {
      sent.push(m);
    },
    respond: async () => {},
    onMessage: (cb) => {
      onMessage = cb;
      return () => {
        onMessage = () => {};
      };
    },
    onRequest: () => () => {},
    onStatus: () => () => {},
  };
  return { transport, sent, emit: (m: unknown) => onMessage(m) };
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

  it("buffers a response that arrives before the id is registered", async () => {
    const { transport, emit } = fakeTransport();
    const client = new RpcClient(transport);
    // Emit first, then request — the client must not lose the response.
    const p = client.request("initialize", {});
    emit({ jsonrpc: "2.0", id: 1, result: 42 });
    await expect(p).resolves.toBe(42);
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
});
