/**
 * Persisted relay binding for a *remote* client (the iOS app).
 *
 * On first launch the iOS app pairs with the desktop (`pairing.ts`), stores the
 * minted device token under {@link REMOTE_STORAGE_KEY}, and thereafter
 * `transportFactory` reads it here to decide remote-vs-desktop. The desktop
 * build never writes this key, so its presence is the signal that we are the
 * remote client.
 *
 * This module is deliberately storage-shape-agnostic: callers pass any object
 * with a `getItem` (i.e. `localStorage` or a test double), so nothing here
 * reaches for a global and every branch stays unit-testable.
 */

import type { Scope } from "./generated/Scope";

/** The `localStorage` key a paired remote client persists its config under. */
export const REMOTE_STORAGE_KEY = "yi-agent.remote";

/**
 * One project the phone may open a second connection to.
 *
 * `session` is the relay session that project's remote app-server answers on;
 * it comes from `remote/relayTargets` and is a **credential** (a read-only
 * device never receives one). Persisting it is what lets the phone show a
 * project list before it has a connection to ask over.
 */
export interface RemoteProjectBinding {
  profileId: string;
  session: string;
}

export interface RemoteConfig {
  /** Relay endpoint, e.g. `wss://relay.example.com/ws`. */
  url: string;
  /** Device token minted at pairing (`yia_…`). */
  token: string;
  /**
   * Per-project bindings.
   *
   * Readers always get an array — a legacy `{url, token}` config parses to
   * `[]`, never `null`, so consumers iterate without a branch. Writers may
   * omit it: the pairing flow has nothing to fill in yet, and forcing every
   * caller to write `projects: []` invites them to get it wrong.
   */
  projects?: RemoteProjectBinding[];
  /**
   * 配对时服务端授予的权限层。
   *
   * 为什么落盘：旁支必须知道本设备是 `Observe`（只能看）还是 `Control`（能
   * 开/发/停/关）。这个值在配对那一刻定下来，之后不再变化；只在每次 `initialize`
   * 后靠猜（例如「远端 = Observe」）会把一台 `Control` 手机误降成只读。缺省视为
   * `control`，与配对协议里服务端缺字段时的默认一致。
   */
  scope?: Scope;
}

/** The slice of `localStorage` this module needs (injectable for tests). */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/**
 * Parse a persisted config. Returns `null` for anything that is not a
 * `{url, token}` pair of non-empty strings — malformed JSON, a partial write, or
 * an unrelated value. Never throws: a corrupt entry must degrade to "not a
 * remote client", not crash the app on boot.
 */
export function parseRemoteConfig(raw: string | null): RemoteConfig | null {
  if (!raw) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const { url, token, projects, scope } = parsed as {
    url?: unknown;
    token?: unknown;
    projects?: unknown;
    scope?: unknown;
  };
  if (typeof url !== "string" || url.length === 0) return null;
  if (typeof token !== "string" || token.length === 0) return null;
  const parsedScope = parseScope(scope);
  return {
    url,
    token,
    projects: parseProjectBindings(projects),
    // 缺失时不写这个键：老版本写下的配置反序列化后再序列化必须逐字不变，
    // 否则「保存一次」就会给一份从没有过 scope 的配置伪造一个值。
    ...(parsedScope === undefined ? {} : { scope: parsedScope }),
  };
}

/**
 * 权限层只认协议里的三个字面量，其余（含缺失）一律 `control`。
 *
 * 不要 fail-closed 成 `observe`：那只会在用户已经能发指令时把 UI 灰掉，
 * 而服务端仍然按 token 的真实 scope 执行——界面上「允许」与「实际执行」就
 * 分叉了。真正的权限校验在服务端，前端的值只用于显示与提前禁用。
 */
function parseScope(raw: unknown): Scope | undefined {
  if (raw === "observe" || raw === "control" || raw === "admin") return raw;
  return undefined;
}

/**
 * Parse the `projects` field, dropping malformed entries rather than failing
 * the whole config.
 *
 * The asymmetry is deliberate: a bad entry costs the phone one project, while
 * rejecting the config would unpair the device and cost it *everything*. A
 * missing field means "written by an older version" and is not an error.
 */
function parseProjectBindings(raw: unknown): RemoteProjectBinding[] {
  if (!Array.isArray(raw)) return [];
  return raw.flatMap((entry) => {
    if (typeof entry !== "object" || entry === null) return [];
    const { profileId, session } = entry as {
      profileId?: unknown;
      session?: unknown;
    };
    if (typeof profileId !== "string" || profileId.length === 0) return [];
    if (typeof session !== "string" || session.length === 0) return [];
    return [{ profileId, session }];
  });
}

/** Read {@link REMOTE_STORAGE_KEY} from `storage` and parse it. */
export function storedRemoteConfig(storage: StorageLike): RemoteConfig | null {
  let raw: string | null;
  try {
    raw = storage.getItem(REMOTE_STORAGE_KEY);
  } catch {
    // Private-mode webviews can throw on storage access; treat as "no binding".
    return null;
  }
  return parseRemoteConfig(raw);
}

/**
 * Persist the relay binding. Called by a remote client after a successful
 * pairing; `transportFactory` then picks the ws transport on the next boot.
 */
export function saveRemoteConfig(
  storage: StorageLike,
  config: RemoteConfig,
): void {
  storage.setItem(REMOTE_STORAGE_KEY, JSON.stringify(config));
}

/** Forget the relay binding (unpair). */
export function clearRemoteConfig(storage: StorageLike): void {
  storage.removeItem(REMOTE_STORAGE_KEY);
}
