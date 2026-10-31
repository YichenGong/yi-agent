import { describe, expect, it, vi } from "vitest";
import { REMOTE_STORAGE_KEY, type StorageLike } from "./lib/remoteConfig";

/**
 * These tests exercise the *decision* only. `tauriTransport` and `wsTransport`
 * are spied so no real bridge/socket is touched; each returns a tagged sentinel
 * we can assert on.
 */
vi.mock("./tauriTransport", () => ({
  tauriTransport: vi.fn(() => ({ kind: "tauri" })),
}));
vi.mock("./wsTransport", () => ({
  wsTransport: vi.fn((url: string, token: string) => ({
    kind: "ws",
    url,
    token,
  })),
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

const remote = (url: string, token: string) => ({
  [REMOTE_STORAGE_KEY]: JSON.stringify({ url, token }),
});

describe("transportFactory", () => {
  it("uses tauriTransport when there is no persisted remote config (desktop path)", () => {
    expect(transportFactory({ storage: storages() })).toEqual({
      kind: "tauri",
    });
    expect(tauriTransport).toHaveBeenCalled();
  });

  it("uses wsTransport with the persisted url+token when configured (remote path)", () => {
    expect(
      transportFactory({
        storage: storages(remote("wss://relay.test/ws", "yia_tok")),
      }),
    ).toEqual({
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

  /**
   * 远端 slot 必须拨**它自己**的 session。回落到 base URL 会开第二条连到默认
   * session 的连接——两个槽位共用一个 app-server，正是多连接要防的那件事。
   */
  it("dials the project's own session for a remote slot", () => {
    const config = {
      url: "wss://relay.test/ws",
      token: "t",
      projects: [{ profileId: "proj-foo", session: "base-proj-foo" }],
    };
    transportFactory({ config, slot: "proj-foo" });
    expect(wsTransport).toHaveBeenLastCalledWith(
      "wss://relay.test/ws?session=base-proj-foo",
      "t",
    );
  });

  it("replaces rather than appends when the stored url already carries a session", () => {
    const config = {
      url: "wss://relay.test/ws?session=base",
      token: "t",
      projects: [{ profileId: "proj-foo", session: "base-proj-foo" }],
    };
    transportFactory({ config, slot: "proj-foo" });
    const url = vi.mocked(wsTransport).mock.lastCall?.[0] as string;
    expect(url.match(/session=/g)).toHaveLength(1);
    expect(url).toContain("session=base-proj-foo");
  });

  /**
   * 没有绑定就抛错，而不是悄悄连回默认 session：那会让用户在一个「看起来是
   * 项目 B」的界面上操作项目 A 的对话。
   */
  it("refuses a remote slot with no binding instead of falling back", () => {
    const config = { url: "wss://relay.test/ws", token: "t", projects: [] };
    expect(() => transportFactory({ config, slot: "proj-foo" })).toThrow(
      /no relay binding/,
    );
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
