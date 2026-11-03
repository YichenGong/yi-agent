import { describe, expect, it } from "vitest";
import {
  REMOTE_STORAGE_KEY,
  clearRemoteConfig,
  parseRemoteConfig,
  saveRemoteConfig,
  storedRemoteConfig,
  type StorageLike,
} from "./remoteConfig";

/** In-memory stand-in for `localStorage`; `entries` seeds it. */
function storages(entries: Record<string, string> = {}): StorageLike {
  const map = new Map(Object.entries(entries));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  };
}

describe("parseRemoteConfig", () => {
  it("parses a well-formed config", () => {
    expect(
      parseRemoteConfig('{"url":"wss://relay.test/ws","token":"yia_tok"}'),
    ).toEqual({
      url: "wss://relay.test/ws",
      token: "yia_tok",
      projects: [],
    });
  });

  it("returns null for null / empty / malformed input", () => {
    expect(parseRemoteConfig(null)).toBeNull();
    expect(parseRemoteConfig("")).toBeNull();
    expect(parseRemoteConfig("{not json")).toBeNull();
  });

  it("returns null when url or token is missing or blank", () => {
    expect(parseRemoteConfig('{"url":"wss://x/ws"}')).toBeNull();
    expect(parseRemoteConfig('{"token":"yia_tok"}')).toBeNull();
    expect(parseRemoteConfig('{"url":"wss://x/ws","token":""}')).toBeNull();
  });
});

describe("storedRemoteConfig", () => {
  it("reads and parses the persisted entry", () => {
    const s = storages({
      [REMOTE_STORAGE_KEY]: '{"url":"wss://x/ws","token":"t"}',
    });
    expect(storedRemoteConfig(s)).toEqual({
      url: "wss://x/ws",
      token: "t",
      projects: [],
    });
  });

  it("returns null when nothing is persisted", () => {
    expect(storedRemoteConfig(storages())).toBeNull();
  });

  it("returns null (never throws) when storage access is denied", () => {
    const denied: StorageLike = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {},
      removeItem: () => {},
    };
    expect(storedRemoteConfig(denied)).toBeNull();
  });
});

describe("parseRemoteConfig · projects", () => {
  /**
   * 旧版本写的是 `{url, token}`。它必须读出 `projects: []` 而**不是** null：
   * 否则连 `url`/`token` 一起丢，等于把已配对的设备变成没配对——用户降级一次
   * 就要重新扫码。
   */
  it("reads a legacy single-binding config as an empty project list", () => {
    const config = parseRemoteConfig(
      JSON.stringify({ url: "wss://r/", token: "t" }),
    );
    expect(config).toEqual({ url: "wss://r/", token: "t", projects: [] });
  });

  it("reads a multi-project config", () => {
    const config = parseRemoteConfig(
      JSON.stringify({
        url: "wss://r/",
        token: "t",
        projects: [
          { profileId: "proj-foo", session: "wss://r/?session=base-proj-foo" },
          { profileId: "proj-bar", session: "wss://r/?session=base-proj-bar" },
        ],
      }),
    );
    expect(config?.projects).toHaveLength(2);
    expect(config?.projects?.[1].profileId).toBe("proj-bar");
  });

  /**
   * 一条坏记录只损失**一个**项目；整个配置作废则损失**全部**（含配对身份）。
   * 这个不对称就是「丢弃单项」这条规则的唯一理由。
   */
  it("drops malformed project entries instead of failing the whole config", () => {
    const config = parseRemoteConfig(
      JSON.stringify({
        url: "wss://r/",
        token: "t",
        projects: [
          { profileId: "ok", session: "s" },
          { nope: 1 },
          { profileId: "", session: "s" },
          { profileId: "x", session: "" },
          "not an object",
          null,
        ],
      }),
    );
    expect(config?.projects).toEqual([{ profileId: "ok", session: "s" }]);
  });

  it("treats a non-array projects field as empty", () => {
    const config = parseRemoteConfig(
      JSON.stringify({ url: "wss://r/", token: "t", projects: "nope" }),
    );
    expect(config?.projects).toEqual([]);
  });
});

describe("saveRemoteConfig / clearRemoteConfig", () => {
  it("round-trips a saved config through storage", () => {
    const s = storages();
    saveRemoteConfig(s, { url: "wss://relay.test/ws", token: "yia_tok" });
    expect(storedRemoteConfig(s)).toEqual({
      url: "wss://relay.test/ws",
      token: "yia_tok",
      projects: [],
    });
  });

  it("clears the binding (unpair)", () => {
    const s = storages({
      [REMOTE_STORAGE_KEY]: '{"url":"wss://x/ws","token":"t"}',
    });
    clearRemoteConfig(s);
    expect(storedRemoteConfig(s)).toBeNull();
  });
});
