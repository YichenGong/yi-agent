import { describe, expect, it, vi } from "vitest";
import { REMOTE_STORAGE_KEY, type StorageLike } from "./lib/remoteConfig";

/**
 * These tests exercise the *decision* only. `tauriTransport` and `wsTransport`
 * are spied so no real bridge/socket is touched; each returns a tagged sentinel
 * we can assert on.
 */
vi.mock("./tauriTransport", () => ({ tauriTransport: vi.fn(() => ({ kind: "tauri" })) }));
vi.mock("./wsTransport", () => ({
  wsTransport: vi.fn((url: string, token: string) => ({ kind: "ws", url, token })),
}));

import { transportFactory } from "./transportFactory";
import { tauriTransport } from "./tauriTransport";
import { wsTransport } from "./wsTransport";

function storages(entries: Record<string, string> = {}): StorageLike {
  const map = new Map(Object.entries(entries));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  };
}

const remote = (url: string, token: string) => ({ [REMOTE_STORAGE_KEY]: JSON.stringify({ url, token }) });

describe("transportFactory", () => {
  it("uses tauriTransport when there is no persisted remote config (desktop path)", () => {
    expect(transportFactory({ storage: storages() })).toEqual({ kind: "tauri" });
    expect(tauriTransport).toHaveBeenCalled();
  });

  it("uses wsTransport with the persisted url+token when configured (remote path)", () => {
    expect(transportFactory({ storage: storages(remote("wss://relay.test/ws", "yia_tok")) })).toEqual({
      kind: "ws",
      url: "wss://relay.test/ws",
      token: "yia_tok",
    });
    expect(wsTransport).toHaveBeenCalledWith("wss://relay.test/ws", "yia_tok");
  });

  it("lets an injected config win over storage (for tests / callers)", () => {
    const config = { url: "wss://relay.test/ws", token: "yia_inj" };
    expect(transportFactory({ storage: storages(), config })).toEqual({
      kind: "ws",
      url: "wss://relay.test/ws",
      token: "yia_inj",
    });
  });

  // 槽位只在桌面桥接上有意义：ws 客户端连的是中继上的一个 session。
  it("passes the slot through to tauriTransport", () => {
    transportFactory({ storage: storages(), slot: "proj-foo" });
    expect(tauriTransport).toHaveBeenCalledWith("proj-foo");
  });

  it("defaults the slot to local on the desktop path", () => {
    transportFactory({ storage: storages() });
    expect(tauriTransport).toHaveBeenCalledWith("local");
  });

  it("does not pass a slot to the ws transport", () => {
    transportFactory({ storage: storages(remote("wss://relay.test/ws", "t")), slot: "proj-foo" });
    expect(wsTransport).toHaveBeenLastCalledWith("wss://relay.test/ws", "t");
  });

  it("falls back to tauriTransport when storage access throws", () => {
    const denied: StorageLike = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {},
      removeItem: () => {},
    };
    expect(transportFactory({ storage: denied })).toEqual({ kind: "tauri" });
  });
});
