import type { Transport } from "./lib/rpc";
import { type RemoteConfig, type StorageLike, storedRemoteConfig } from "./lib/remoteConfig";
import { tauriTransport } from "./tauriTransport";
import { wsTransport } from "./wsTransport";

/**
 * Pick the transport for this build.
 *
 * - **desktop**: the Tauri bundling of the app owns the `app-server` sidecar
 *   over stdio, so it uses the Tauri bridge (`tauriTransport`).
 * - **remote (iOS)**: there is no sidecar and no Tauri IPC at all; the app is a
 *   pure WebSocket client of the relay (`wsTransport`).
 *
 * The decision signal is *persisted config*: the iOS app, on first launch,
 * pairs (`pairing.ts`) and writes `{url, token}` under
 * `localStorage["yi-agent.remote"]`. That key is written only by the remote
 * client, so its presence distinguishes the two — cleaner than sniffing
 * `window.__TAURI_INTERNALS__` (which a desktop dev server exposes to the
 * browser too) and it survives reloads. `import.meta.env` was considered as an
 * alternative but build-time env is baked into the bundle, so it cannot express
 * "this install has paired with *that* relay".
 *
 * `opts.storage` and `opts.config` are seams: production passes the real
 * `localStorage`, tests inject a double or a fixed decision.
 */
export interface TransportFactoryOptions {
  storage?: StorageLike;
  /** Explicit config wins over storage (used by tests, and by a future paired-URL deep link). */
  config?: RemoteConfig | null;
}

export function transportFactory(opts: TransportFactoryOptions = {}): Transport {
  const config =
    opts.config ??
    storedRemoteConfig(opts.storage ?? (globalThis.localStorage as StorageLike | undefined) ?? emptyStorage);
  return config ? wsTransport(config.url, config.token) : tauriTransport();
}

const emptyStorage: StorageLike = { getItem: () => null, setItem: () => {}, removeItem: () => {} };
