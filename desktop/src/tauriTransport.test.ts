import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn(async () => undefined);
const listeners = new Map<string, (e: { payload: unknown }) => void>();
const listen = vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
  listeners.set(event, cb);
  return () => listeners.delete(event);
});

vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...(args as [])) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: (...args: unknown[]) => listen(...(args as [string, never])) }));

const { LOCAL_SLOT, eventName } = await import("./lib/slots");
const { tauriTransport } = await import("./tauriTransport");

describe("event naming", () => {
  it("matches the Rust side format", () => {
    expect(eventName("message", LOCAL_SLOT)).toBe("app-server://message/local");
  });

  it("puts the slot last so per-slot subscription is a string match", () => {
    expect(eventName("status", "proj-foo")).toBe("app-server://status/proj-foo");
  });

  it("local slot is the literal the Rust side reserves", () => {
    expect(LOCAL_SLOT).toBe("local");
  });
});

describe("tauriTransport", () => {
  beforeEach(() => {
    invoke.mockClear();
    listen.mockClear();
    listeners.clear();
  });

  it("defaults to the local slot", async () => {
    const transport = tauriTransport();
    await transport.send({ id: 1, method: "ping", params: {} });
    expect(invoke).toHaveBeenCalledWith("rpc", { id: 1, method: "ping", params: {}, slot: "local" });
    transport.onMessage(() => {});
    await vi.waitFor(() => expect(listeners.has("app-server://message/local")).toBe(true));
    transport.dispose();
  });

  it("addresses a remote slot on send and respond", async () => {
    const transport = tauriTransport("alpha");
    await transport.send({ id: 7, method: "thread/start", params: { a: 1 } });
    await transport.respond("req-1", { kind: "accept" } as never);
    expect(invoke).toHaveBeenNthCalledWith(1, "rpc", {
      id: 7,
      method: "thread/start",
      params: { a: 1 },
      slot: "alpha",
    });
    expect(invoke).toHaveBeenNthCalledWith(2, "rpc_respond", {
      id: "req-1",
      result: { kind: "accept" },
      slot: "alpha",
    });
    transport.dispose();
  });

  it("subscribes every hook to the slot-scoped event", async () => {
    const transport = tauriTransport("alpha");
    const seen: unknown[] = [];
    transport.onMessage((m) => seen.push(m));
    transport.onRequest(() => {});
    transport.onStatus(() => {});
    await vi.waitFor(() => expect(listeners.size).toBe(3));
    expect([...listeners.keys()].sort()).toEqual([
      "app-server://message/alpha",
      "app-server://request/alpha",
      "app-server://status/alpha",
    ]);
    listeners.get("app-server://message/alpha")?.({ payload: "hello" });
    expect(seen).toEqual(["hello"]);
    transport.dispose();
    await vi.waitFor(() => expect(listeners.size).toBe(0));
  });
});
