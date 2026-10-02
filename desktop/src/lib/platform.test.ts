import { describe, expect, it, vi } from "vitest";
import { isRemoteClient, openExternalLink } from "./platform";
import { REMOTE_STORAGE_KEY, type StorageLike } from "./remoteConfig";

function storages(entries: Record<string, string> = {}): StorageLike {
  const map = new Map(Object.entries(entries));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  };
}

describe("isRemoteClient", () => {
  it("is false with no persisted remote config", () => {
    expect(isRemoteClient(storages())).toBe(false);
  });

  it("is true once a remote url+token is persisted", () => {
    const s = storages({
      [REMOTE_STORAGE_KEY]: JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    });
    expect(isRemoteClient(s)).toBe(true);
  });

  it("is false (never throws) on malformed persisted json", () => {
    expect(isRemoteClient(storages({ [REMOTE_STORAGE_KEY]: "{nope" }))).toBe(false);
  });
});

describe("openExternalLink", () => {
  it("delegates to the native opener when not a remote client (desktop)", async () => {
    const native = vi.fn().mockResolvedValue(undefined);
    await openExternalLink("https://example.com", { remote: false, native });
    expect(native).toHaveBeenCalledWith("https://example.com");
  });

  it("uses the webview window.open on the remote (iOS) client, not the native opener", async () => {
    const native = vi.fn().mockResolvedValue(undefined);
    const open = vi.fn();
    await openExternalLink("https://example.com", { remote: true, native, open });
    expect(native).not.toHaveBeenCalled();
    expect(open).toHaveBeenCalledWith("https://example.com", "_blank", "noopener");
  });
});
