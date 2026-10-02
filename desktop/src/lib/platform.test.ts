import { describe, expect, it, vi } from "vitest";
import { isIos, isRemoteClient, openExternalLink } from "./platform";
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

describe("isIos", () => {
  it("is true for the iPhone/iPad/iPod user agents the Tauri iOS build reports", () => {
    expect(isIos("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)")).toBe(true);
    expect(isIos("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)")).toBe(true);
    expect(isIos("Mozilla/5.0 (iPod touch; CPU iPhone OS 15_0 like Mac OS X)")).toBe(true);
    // iPadOS 桌面版 UA：漏认会被当成普通 Mac，首启就不弹配对表单。
    expect(
      isIos(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Mobile/15E148 Safari/604.1",
      ),
    ).toBe(true);
  });

  it("is false on the desktop webviews the same bundle runs in", () => {
    expect(isIos("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15")).toBe(false);
    expect(isIos("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")).toBe(false);
    expect(isIos("")).toBe(false);
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
