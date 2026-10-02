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
    expect(parseRemoteConfig('{"url":"wss://relay.test/ws","token":"yia_tok"}')).toEqual({
      url: "wss://relay.test/ws",
      token: "yia_tok",
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
    const s = storages({ [REMOTE_STORAGE_KEY]: '{"url":"wss://x/ws","token":"t"}' });
    expect(storedRemoteConfig(s)).toEqual({ url: "wss://x/ws", token: "t" });
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

describe("saveRemoteConfig / clearRemoteConfig", () => {
  it("round-trips a saved config through storage", () => {
    const s = storages();
    saveRemoteConfig(s, { url: "wss://relay.test/ws", token: "yia_tok" });
    expect(storedRemoteConfig(s)).toEqual({ url: "wss://relay.test/ws", token: "yia_tok" });
  });

  it("clears the binding (unpair)", () => {
    const s = storages({ [REMOTE_STORAGE_KEY]: '{"url":"wss://x/ws","token":"t"}' });
    clearRemoteConfig(s);
    expect(storedRemoteConfig(s)).toBeNull();
  });
});
