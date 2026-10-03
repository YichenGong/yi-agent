import { describe, expect, it } from "vitest";
import { withToken, wsTransport } from "./wsTransport";

/**
 * Minimal stand-in for the browser `WebSocket` so the transport can be driven
 * deterministically: `open()` settles the handshake, `receive()` pushes a frame,
 * `close()` fires the close handler.
 *
 * Note the frames the transport writes are captured verbatim in `sent` *after*
 * `open()`: a real socket throws when you `send()` while still CONNECTING, so
 * the transport must await the open before writing.
 */
class FakeWebSocket {
  readyState = 0; // CONNECTING
  sent: string[] = [];
  onopen: ((ev?: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code?: number }) => void) | null = null;
  onerror: ((ev?: unknown) => void) | null = null;

  constructor(readonly url: string) {}

  send(data: string): void {
    this.sent.push(data);
  }

  close(code = 1000): void {
    this.readyState = 3;
    this.onclose?.({ code });
  }

  open(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  receive(data: string): void {
    this.onmessage?.({ data });
  }
}

function make(url = "wss://relay.test/ws", token = "yia_tok") {
  const fake = new FakeWebSocket(url);
  let captured = url;
  const transport = wsTransport(url, token, (u) => {
    captured = u;
    return fake as unknown as WebSocket;
  });
  return { fake, transport, url: () => captured };
}

describe("withToken", () => {
  it("carries the token in the URL query (the WKWebView-compatible handshake)", () => {
    const url = new URL(withToken("wss://relay.test/ws", "yia_abc"));
    expect(url.searchParams.get("token")).toBe("yia_abc");
    expect(url.pathname).toBe("/ws");
  });

  it("preserves an existing query on the relay url", () => {
    const url = new URL(withToken("wss://relay.test/ws?x=1", "yia_abc"));
    expect(url.searchParams.get("x")).toBe("1");
    expect(url.searchParams.get("token")).toBe("yia_abc");
  });
});

describe("wsTransport", () => {
  it("connects to the token-bearing url", () => {
    const { url } = make();
    expect(new URL(url()).searchParams.get("token")).toBe("yia_tok");
  });

  it("sends a JSON-RPC frame containing the method, after open", async () => {
    const { fake, transport } = make();
    fake.open();
    await transport.send({ id: 1, method: "initialize", params: {} });
    expect(fake.sent).toHaveLength(1);
    const frame = JSON.parse(fake.sent[0]);
    expect(frame).toMatchObject({ jsonrpc: "2.0", id: 1, method: "initialize" });
  });

  it("queues a send issued before the socket opens", async () => {
    const { fake, transport } = make();
    const pending = transport.send({ id: 7, method: "thread/list", params: {} });
    expect(fake.sent).toHaveLength(0); // nothing on the wire yet
    fake.open();
    await pending;
    expect(JSON.parse(fake.sent[0]).method).toBe("thread/list");
  });

  it("rejects a send if the socket closes before opening", async () => {
    const { fake, transport } = make();
    const pending = transport.send({ id: 1, method: "initialize", params: {} });
    fake.close(4401);
    await expect(pending).rejects.toThrow(/4401/);
  });

  it("delivers an incoming response to onMessage", () => {
    const { fake, transport } = make();
    const seen: unknown[] = [];
    transport.onMessage((m) => seen.push(m));
    fake.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: { ok: true } }));
    expect(seen).toEqual([{ jsonrpc: "2.0", id: 1, result: { ok: true } }]);
  });

  it("routes a reverse request (method + id) to onRequest", () => {
    const { fake, transport } = make();
    const requests: unknown[] = [];
    const messages: unknown[] = [];
    transport.onRequest((r) => requests.push(r));
    transport.onMessage((m) => messages.push(m));
    fake.receive(
      JSON.stringify({
        jsonrpc: "2.0",
        id: "perm-0",
        method: "item/toolCall/requestApproval",
        params: { tool_name: "bash" },
      }),
    );
    expect(requests).toHaveLength(1);
    expect((requests[0] as { method: string }).method).toBe("item/toolCall/requestApproval");
    expect(messages).toHaveLength(0);
  });

  it("routes a notification (method, no id) to onMessage", () => {
    const { fake, transport } = make();
    const messages: unknown[] = [];
    transport.onMessage((m) => messages.push(m));
    fake.receive(
      JSON.stringify({ jsonrpc: "2.0", method: "item/delta", params: { delta: "x" } }),
    );
    expect(messages).toHaveLength(1);
  });

  it("answers a reverse request by correlating respond(id, result)", async () => {
    const { fake, transport } = make();
    fake.open();
    await transport.respond("perm-0", { decision: "allow_once" });
    expect(JSON.parse(fake.sent[0])).toEqual({
      jsonrpc: "2.0",
      id: "perm-0",
      result: { decision: "allow_once" },
    });
  });

  it("reports an exited status (with the close code) on close", () => {
    const { fake, transport } = make();
    const statuses: Array<{ state: string; code?: number | null }> = [];
    transport.onStatus((s) => statuses.push(s));
    fake.close(4401);
    expect(statuses).toEqual([{ state: "exited", code: 4401 }]);
  });

  it("unsubscribes handlers", () => {
    const { fake, transport } = make();
    const seen: unknown[] = [];
    const off = transport.onMessage((m) => seen.push(m));
    off();
    fake.receive(JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }));
    expect(seen).toHaveLength(0);
  });

  it("dispose stops delivery, closes the socket, and does not report an exit", () => {
    const { fake, transport } = make();
    const messages: unknown[] = [];
    const statuses: unknown[] = [];
    transport.onMessage((m) => messages.push(m));
    transport.onStatus((s) => statuses.push(s));
    fake.open();

    transport.dispose();
    // A deliberately closed socket must not look like a server disconnect
    // (that would make the App schedule another reconnect for its own teardown).
    expect(statuses).toHaveLength(0);
    // Frames arriving after dispose (a race with the close) reach nobody.
    fake.receive(JSON.stringify({ jsonrpc: "2.0", method: "item/delta", params: { delta: "x" } }));
    expect(messages).toHaveLength(0);
  });

  it("ignores a non-JSON frame without throwing", () => {
    const { fake, transport } = make();
    const seen: unknown[] = [];
    transport.onMessage((m) => seen.push(m));
    expect(() => fake.receive("not json")).not.toThrow();
    expect(seen).toHaveLength(0);
  });
});
