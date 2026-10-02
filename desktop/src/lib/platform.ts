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
