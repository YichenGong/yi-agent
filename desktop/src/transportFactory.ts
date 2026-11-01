import type { Transport } from "./lib/rpc";
import {
  type RemoteConfig,
  type StorageLike,
  storedRemoteConfig,
} from "./lib/remoteConfig";
import { LOCAL_SLOT, type Slot } from "./lib/slots";
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
  /**
   * 桌面端要连的槽位（`local` 或远端 profile id）。只有落到 Tauri 桥接时才
   * 有意义：ws 客户端连的是中继上的一个 session，没有「槽位」概念。
   */
  slot?: Slot;
}

export function transportFactory(
  opts: TransportFactoryOptions = {},
): Transport {
  const slot = opts.slot ?? LOCAL_SLOT;
  const config =
    opts.config ??
    storedRemoteConfig(
      opts.storage ??
        (globalThis.localStorage as StorageLike | undefined) ??
        emptyStorage,
    );
  if (!config) return tauriTransport(slot);

  // A remote client asked for one project's slot: dial that project's own
  // relay session. Falling back to the base URL here would open a second
  // connection to the *default* session — two slots silently sharing one
  // app-server, which is the exact failure multi-connection exists to prevent.
  if (slot !== LOCAL_SLOT) {
    const binding = config.projects?.find((entry) => entry.profileId === slot);
    if (!binding) {
      throw new Error(
        `no relay binding for remote project \`${slot}\` (re-pair to refresh the project list)`,
      );
    }
    return wsTransport(
      relayUrlForSession(config.url, binding.session),
      config.token,
    );
  }
  return wsTransport(config.url, config.token);
}

/**
 * Put `session` in the relay URL's `session` query parameter.
 *
 * The stored `url` may already carry a `?session=` (the desktop's own), so this
 * replaces rather than appends — `?session=a&session=b` leaves the relay to
 * pick one, and which one it picks is not ours to guess.
 */
export function relayUrlForSession(url: string, session: string): string {
  const parsed = new URL(url);
  parsed.searchParams.set("session", session);
  return parsed.toString();
}

const emptyStorage: StorageLike = {
  getItem: () => null,
  setItem: () => {},
  removeItem: () => {},
};
