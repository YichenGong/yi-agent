/**
 * First-paint cache of the thread list, for the remote (phone) client.
 *
 * The authoritative list lives on the server (`thread/listAll`); this is only a
 * synchronous snapshot so the sidebar can paint before the relay round-trip
 * completes. Same contract as the theme cache (`theme.ts`): storage is a cache,
 * the server is the truth. Anything malformed degrades to `null` — never throw,
 * never block boot.
 */
import type { WorkspaceGroup, ThreadSummary } from "./protocol";
import type { StorageLike } from "./remoteConfig";

export const THREAD_LIST_CACHE_KEY = "yi-agent.threadListCache";
export const THREAD_LIST_CACHE_VERSION = 1;

export interface ThreadListCache {
  version: number;
  savedAt: number;
  /** The thread selected when the snapshot was taken, to restore on cold start. */
  selectedId: string | null;
  pinned: ThreadSummary[];
  groups: WorkspaceGroup[];
}

function defaultStorage(): StorageLike | undefined {
  return globalThis.localStorage as StorageLike | undefined;
}

/** Parse a persisted payload; `null` for anything that is not a v1 snapshot. */
export function parseListCache(raw: string | null): ThreadListCache | null {
  if (!raw) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const c = parsed as Partial<ThreadListCache>;
  if (c.version !== THREAD_LIST_CACHE_VERSION) return null;
  if (!Array.isArray(c.groups) || !Array.isArray(c.pinned)) return null;
  // 元素形状也要挡：groups 里若藏着非分组对象（threads 缺失/非数组），首屏
  // `flatMap((g) => g.threads)` 会在渲染期 effect 里抛错，React 接不住，正好
  // 变成本功能要消灭的白屏。宁可当作没有缓存。pinned 同理，至少要能拿到 id。
  for (const g of c.groups) {
    if (typeof g !== "object" || g === null || !Array.isArray((g as WorkspaceGroup).threads)) {
      return null;
    }
    // 分组内的每个会话也要挡：`threads: [null]` 这类脏值不会被上面的
    // `Array.isArray` 拦下，但侧栏渲染 `g.threads.filter((t) => !t.pinned …)`
    // 会在 `null` 上抛错 → 同样白屏。至少要能取到字符串 thread_id。
    for (const t of (g as WorkspaceGroup).threads) {
      if (typeof t !== "object" || t === null || typeof (t as ThreadSummary).thread_id !== "string") {
        return null;
      }
    }
  }
  for (const t of c.pinned) {
    if (typeof t !== "object" || t === null || typeof (t as ThreadSummary).thread_id !== "string") {
      return null;
    }
  }
  return {
    version: THREAD_LIST_CACHE_VERSION,
    savedAt: typeof c.savedAt === "number" ? c.savedAt : 0,
    selectedId: typeof c.selectedId === "string" ? c.selectedId : null,
    pinned: c.pinned,
    groups: c.groups,
  };
}

/** Read the snapshot, or `null` when absent/corrupt/unavailable. */
export function readListCache(storage: StorageLike | undefined = defaultStorage()): ThreadListCache | null {
  if (!storage) return null;
  try {
    return parseListCache(storage.getItem(THREAD_LIST_CACHE_KEY));
  } catch {
    return null;
  }
}

/** Persist a snapshot. Storage failures are non-fatal (it is only a cache). */
export function writeListCache(
  storage: StorageLike | undefined,
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  selectedId: string | null,
): void {
  if (!storage) return;
  const payload: ThreadListCache = {
    version: THREAD_LIST_CACHE_VERSION,
    savedAt: Date.now(),
    selectedId,
    pinned,
    groups,
  };
  try {
    storage.setItem(THREAD_LIST_CACHE_KEY, JSON.stringify(payload));
  } catch {
    // Quota / private mode: losing the cache is not worth breaking the app.
  }
}
