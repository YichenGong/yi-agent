/**
 * Where the page gets its host capabilities.
 *
 * Everything the plugin can do comes from this one object: `request` for its own
 * declared backend methods, `openThread` and `notify` for host-level actions,
 * `subscribe` for events. There is no RPC client, no Tauri import and no file
 * access here — a plugin page that could reach the host's own transport would be
 * a way around the bridge's declared-method and scope checks.
 */

import { connect, connectOrNull, type PluginHostApi } from "@yi-agent/plugin-sdk";

export type { PluginHostApi };

/**
 * Connect, or fail loudly.
 *
 * Refusing to return a half-working object is deliberate: a page that renders
 * with a missing host looks like an empty board, and "empty" is a valid state
 * the user might act on. Better to show a connection error than a wrong board.
 */
export async function connectHost(): Promise<PluginHostApi> {
  return connect();
}

/**
 * Connect if a host is there, otherwise report absence.
 *
 * Used where a missing host is a legitimate state rather than an error — for
 * example a standalone preview of the bundle outside the desktop app.
 */
export async function connectHostOrNull(): Promise<PluginHostApi | null> {
  return connectOrNull();
}

/** What the page shows before (or instead of) a connected host. */
export const DISCONNECTED_NOTICE =
  "未连接到宿主，无法读取看板。请在桌面端打开本插件；连接恢复后会自动加载。";
