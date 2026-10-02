/**
 * `PairingScreen` 里纯逻辑部分的单测（不挂 React）。
 *
 * 放在 logic 测试里而不是组件测试里，是因为这些判定是**平台的**（谁需要配对、
 * 这台设备叫什么）——`App` 的 iOS 分支和组件都用它们，回归时最先该被钉住。
 */
import { describe, expect, it } from "vitest";
import { inferDeviceName, needsPairing } from "./PairingScreen";
import { REMOTE_STORAGE_KEY, type StorageLike } from "../lib/remoteConfig";

function storages(entries: Record<string, string> = {}): StorageLike {
  const map = new Map(Object.entries(entries));
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  };
}

const PAIRED = JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" });

describe("needsPairing", () => {
  it("is true on iOS with no persisted config — the dead-end case the screen exists for", () => {
    expect(needsPairing(true, storages())).toBe(true);
  });

  it("is true on iOS when the persisted config is unreadable", () => {
    expect(needsPairing(true, storages({ [REMOTE_STORAGE_KEY]: "{broken" }))).toBe(true);
  });

  it("is false on iOS once a url+token is persisted", () => {
    expect(needsPairing(true, storages({ [REMOTE_STORAGE_KEY]: PAIRED }))).toBe(false);
  });

  it("is false on the desktop regardless of storage — desktop never sees the form", () => {
    expect(needsPairing(false, storages())).toBe(false);
    expect(needsPairing(false, storages({ [REMOTE_STORAGE_KEY]: PAIRED }))).toBe(false);
  });
});

describe("inferDeviceName", () => {
  it("recognises the iOS family and defaults to iPhone when unsure", () => {
    expect(inferDeviceName("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)")).toBe("iPhone");
    expect(inferDeviceName("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)")).toBe("iPad");
    // iPadOS 的桌面版 UA：`Macintosh` 加 `Mobile`（真 Mac 不带 Mobile）。
    expect(inferDeviceName("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Mobile/15E148")).toBe(
      "iPad",
    );
    expect(inferDeviceName("Mozilla/5.0 (X11; Linux x86_64)")).toBe("iPhone");
    expect(inferDeviceName("")).toBe("iPhone");
  });
});
