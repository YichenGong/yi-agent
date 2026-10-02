import { describe, expect, it } from "vitest";
import { redeemPairCode } from "./pairing";

/** See `wsTransport.test.ts` for the shape of this fake; kept local to avoid a shared test helper. */
class FakeWebSocket {
  readyState = 0;
  onopen: ((ev?: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code?: number }) => void) | null = null;
  onerror: ((ev?: unknown) => void) | null = null;

  constructor(readonly url: string) {}

  send(): void {}

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

const REDEEMED = {
  jsonrpc: "2.0",
  method: "pair/redeemed",
  params: { device_id: "dev-1", token: "yia_token", scope: "control" },
};

describe("redeemPairCode", () => {
  it("connects with the pair code and device name in the query", () => {
    let captured = "";
    const fake = new FakeWebSocket("");
    void redeemPairCode("wss://relay.test/ws", "CODE-123", "iPhone 15", (u) => {
      captured = u;
      return fake as unknown as WebSocket;
    }).catch(() => {});
    const url = new URL(captured);
    expect(url.searchParams.get("pair")).toBe("CODE-123");
    expect(url.searchParams.get("device_name")).toBe("iPhone 15");
  });

  it("resolves the device credentials from a pair/redeemed frame", async () => {
    const fake = new FakeWebSocket("");
    let captured = "";
    const promise = redeemPairCode("wss://relay.test/ws", "CODE-123", "iPhone", (u) => {
      captured = u;
      return fake as unknown as WebSocket;
    });
    const url = new URL(captured);
    expect(url.searchParams.get("pair")).toBe("CODE-123");
    expect(url.searchParams.get("device_name")).toBe("iPhone");
    fake.receive(JSON.stringify(REDEEMED));
    await expect(promise).resolves.toEqual({
      device_id: "dev-1",
      token: "yia_token",
      scope: "control",
    });
  });

  it("stays resolved when the server closes 4403 after delivery", async () => {
    const fake = new FakeWebSocket("");
    const promise = redeemPairCode("wss://relay.test/ws", "CODE-123", "iPhone", () =>
      fake as unknown as WebSocket,
    );
    fake.receive(JSON.stringify(REDEEMED));
    fake.close(4403); // pairing delivered
    await expect(promise).resolves.toMatchObject({ token: "yia_token" });
  });

  it("rejects on a 4401 close (invalid or used pair code)", async () => {
    const fake = new FakeWebSocket("");
    const promise = redeemPairCode("wss://relay.test/ws", "bad", "iPhone", () =>
      fake as unknown as WebSocket,
    );
    fake.close(4401);
    await expect(promise).rejects.toThrow(/4401|unauthor/i);
  });

  it("rejects when the socket closes before delivering a frame", async () => {
    const fake = new FakeWebSocket("");
    const promise = redeemPairCode("wss://relay.test/ws", "CODE-123", "iPhone", () =>
      fake as unknown as WebSocket,
    );
    fake.close(1006);
    await expect(promise).rejects.toThrow(/closed|1006/i);
  });

  it("rejects a malformed frame without hanging", async () => {
    const fake = new FakeWebSocket("");
    const promise = redeemPairCode("wss://relay.test/ws", "CODE-123", "iPhone", () =>
      fake as unknown as WebSocket,
    );
    fake.receive("not json");
    fake.close(1000);
    await expect(promise).rejects.toThrow();
  });
});
