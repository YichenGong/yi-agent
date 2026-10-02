/**
 * Platform branches for behavior that differs between the desktop build (Tauri
 * IPC, native dialogs) and the remote iOS build (a plain webview, no sidecar).
 *
 * The remote client is identified by the same persisted signal
 * `transportFactory` uses (`lib/remoteConfig`), so both agree on what "remote"
 * means.
 */

import { type StorageLike, storedRemoteConfig } from "./remoteConfig";

const emptyStorage: StorageLike = { getItem: () => null, setItem: () => {}, removeItem: () => {} };

/** Storage to consult when the caller does not inject one. */
function defaultStorage(): StorageLike {
  return (globalThis.localStorage as StorageLike | undefined) ?? emptyStorage;
}

/** True when this build is a paired remote client (the iOS app), not the desktop. */
export function isRemoteClient(storage: StorageLike = defaultStorage()): boolean {
  return storedRemoteConfig(storage) !== null;
}

/**
 * True when the host is iOS (iPhone/iPad/iPod).
 *
 * The Tauri iOS build runs the same React bundle as the desktop, so the platform
 * cannot be told apart by the persisted config alone: on first launch there is
 * none. This sniffs the webview's user agent; iPadOS 13+ reports `Macintosh` with
 * a `Mobile` token (desktop Safari never sends `Mobile`), so that pair counts as
 * iPad too. `navigator` is only touched here, at the edge, so pure logic stays
 * testable by passing a UA in.
 */
/** iPadOS 13+ 的桌面版 UA：`Macintosh` 加 `Mobile`（真 Mac 浏览器不发 Mobile）。 */
function looksLikeIpadDesktopMode(ua: string): boolean {
  return /\bMacintosh\b/i.test(ua) && /\bMobile\b/i.test(ua);
}

export function isIos(userAgent: string = globalThis.navigator?.userAgent ?? ""): boolean {
  if (/\biPhone\b|\biPad\b|\biPod\b/i.test(userAgent)) return true;
  // 认这段的理由同 `inferDeviceName`：漏掉它，iPad 在桌面版 UA 下会被当成
  // 普通 Mac，于是首启不弹配对表单——正好是这条分支要防的死路。
  return looksLikeIpadDesktopMode(userAgent);
}

export interface OpenExternalLinkDeps {
  remote: boolean;
  /** Desktop: `openUrl` from `@tauri-apps/plugin-opener`. */
  native: (url: string) => Promise<void>;
  /** Remote: the webview's `window.open`. Defaults to the global. */
  open?: (url: string, target?: string, features?: string) => unknown;
}

/**
 * Open an external link.
 *
 * Desktop routes through the native opener so the webview is not navigated away.
 * On the iOS remote build there is no Tauri opener, so it falls back to
 * `window.open` — the platform branch that keeps the bundle from invoking a
 * plugin that does not exist.
 */
export async function openExternalLink(url: string, deps: OpenExternalLinkDeps): Promise<void> {
  if (deps.remote) {
    const open = deps.open ?? globalThis.open;
    open(url, "_blank", "noopener");
    return;
  }
  await deps.native(url);
}
