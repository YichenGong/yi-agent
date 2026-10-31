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
  const { url, token, projects } = parsed as {
    url?: unknown;
    token?: unknown;
    projects?: unknown;
  };
  if (typeof url !== "string" || url.length === 0) return null;
  if (typeof token !== "string" || token.length === 0) return null;
  return { url, token, projects: parseProjectBindings(projects) };
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
