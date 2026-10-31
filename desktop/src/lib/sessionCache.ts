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

/**
 * 非置顶会话最多保留多少条。
 *
 * 实测（`node` + 真实 `JSON.stringify`）：1000 条约 248–352 KB，取决于
 * `cwd` 与 `title` 长度；最坏形状要约 1.45 万条才撞 5 MB 配额，故选 1000
 * 有两位数余量。这是**防御性上界**，不是配额问题的修复。
 */
export const LIST_CACHE_MAX_THREADS = 1000;

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
  const { groups: cappedGroups, truncated } = capListCache(groups, pinned);
  // `truncated` 只在真截断时出现：它会让既有的 v1 载荷多一个键，
  // `parseListCache` 显式重建返回对象，所以读取侧看不到它（诊断标记）。
  const payload: ThreadListCache & { truncated?: boolean } = {
    version: THREAD_LIST_CACHE_VERSION,
    savedAt: Date.now(),
    selectedId,
    pinned,
    groups: cappedGroups,
    ...(truncated ? { truncated: true } : {}),
  };
  try {
    storage.setItem(THREAD_LIST_CACHE_KEY, JSON.stringify(payload));
  } catch {
    // Quota / private mode: losing the cache is not worth breaking the app.
  }
}

/**
 * 按**全局 `updated_at` 降序**保留前 `LIST_CACHE_MAX_THREADS` 条非置顶会话。
 *
 * 为什么不能按 `groups` 的遍历顺序截断：服务端 `list()` 产出的是全局降序
 * （`thread_store.rs:459-463`），但装进 `groups` 后变成「按 workspace 分组、
 * 组内各自降序」，组与组之间没有可比顺序。按遍历顺序砍会让排在后面的 workspace
 * 整块消失，哪怕它们刚更新过。
 *
 * `pinned` 不占限额，但它们的 id 必须计入保留集合：置顶项按约定仍留在各自分组
 * 的 `threads` 里，若不计入，某个「组内成员全是置顶」的分组会被判成空组而丢弃，
 * 冷启首屏就少一块目录。
 */
function capListCache(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
): { groups: WorkspaceGroup[]; truncated: boolean } {
  const keep = new Set(pinned.map((t) => t.thread_id));
  const candidates = groups
    .flatMap((g) => g.threads)
    .filter((t) => !t.pinned && !keep.has(t.thread_id))
    .sort((a, b) => b.updated_at - a.updated_at);

  let truncated = false;
  for (const t of candidates.slice(0, LIST_CACHE_MAX_THREADS)) keep.add(t.thread_id);
  if (candidates.length > LIST_CACHE_MAX_THREADS) truncated = true;

  // 装回原骨架：只决定保留谁，不改动渲染顺序。
  const capped = groups
    .map((g) => ({ ...g, threads: g.threads.filter((t) => keep.has(t.thread_id)) }))
    .filter((g) => g.threads.length > 0);

  return { groups: capped, truncated };
}
