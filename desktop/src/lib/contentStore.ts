import type { Item } from "./protocol";
import { DB_NAME, DB_VERSION, STORE_NAME } from "./cacheDb";
import { ensureCacheScopeClean } from "./cacheScope";

/** Per-entry cap: how large a single thread's cached transcript may get. */
export const MAX_ENTRY_BYTES = 1024 * 1024;

/** Global caps over the whole content cache: entry count and total bytes. */
export const CONTENT_LIMITS = { maxCount: 10, maxBytes: 5 * 1024 * 1024 };

/** Byte size of a serialized item list. UTF-8 length is close enough to what
 *  the storage engine holds, and it is what the budget is defined against. */
export function estimateItemsBytes(items: Item[]): number {
  if (items.length === 0) return 0;
  return new TextEncoder().encode(JSON.stringify(items)).length;
}

/** The slice of a cache entry the eviction policy needs. */
export interface EntryMeta {
  threadId: string;
  savedAt: number;
  bytes: number;
}

/**
 * Drop the oldest items from the head until the serialized list fits `maxBytes`.
 *
 * Trimming from the head keeps the *tail* — the most recent exchange, which is
 * what a user opening a thread cares about. At least one item survives even if it
 * alone busts the budget: an empty transcript is worse than one oversized item,
 * and a single item is already bounded by the frame size the server produces.
 */
export function applyTrim(
  items: Item[],
  maxBytes: number,
): { items: Item[]; trimmed: boolean } {
  // Measure only the suffix that can survive. Repeatedly serializing/slicing
  // the shrinking full list is quadratic and blocks desktop's main thread for
  // seconds on multi-MiB histories. UTF-8 bytes are additive across JSON array
  // elements, with two brackets and one comma between adjacent elements.
  const encoder = new TextEncoder();
  let start = items.length;
  let bytes = 2;
  while (start > 0) {
    const itemBytes = encoder.encode(JSON.stringify(items[start - 1])).length;
    const nextBytes = bytes + itemBytes + (start < items.length ? 1 : 0);
    if (start < items.length && nextBytes > maxBytes) break;
    bytes = nextBytes;
    start -= 1;
  }
  return { items: start === 0 ? items : items.slice(start), trimmed: start > 0 };
}

/**
 * Ids to evict, oldest first, until both the count and byte bounds hold.
 *
 * `savedAt` is refreshed on every write, so ordering by it is LRU: the least
 * recently touched thread is evicted first. Pure + total, so the policy is unit
 * tested without a database.
 */
export function selectEvictions(
  metas: EntryMeta[],
  limits: { maxCount: number; maxBytes: number },
): string[] {
  const sorted = [...metas].sort((a, b) => a.savedAt - b.savedAt);
  let count = sorted.length;
  let total = sorted.reduce((sum, m) => sum + m.bytes, 0);
  const evicted: string[] = [];
  for (const m of sorted) {
    if (count <= limits.maxCount && total <= limits.maxBytes) break;
    evicted.push(m.threadId);
    count -= 1;
    total -= m.bytes;
  }
  return evicted;
}

/** One thread's cached transcript. JSON-serializable (IndexedDB + clone). */
export interface ContentEntry {
  threadId: string;
  /** Epoch ms of the last write — the LRU key. */
  savedAt: number;
  /** Resume/read-items cursor at the time of the write. */
  lastServerItemId: string | null;
  /** Server-authored items only (no `local-*`, no notices). */
  items: Item[];
  /** True when head items were dropped to fit `MAX_ENTRY_BYTES`. */
  trimmed: boolean;
  /** Estimated serialized size, kept so eviction need not re-serialize. */
  bytes: number;
  /**
   * The server content generation this transcript belongs to, or `null`/absent
   * when unknown (a legacy entry). Absent must be read as "unknown", never as
   * "matches" — the caller then does a conservative full resume (spec §4.3).
   */
  contentEpoch?: number | null;
}

/**
 * The seam the App talks to. The real implementation is IndexedDB; tests (and
 * any environment without IndexedDB) use the in-memory one. Nothing above this
 * line knows which.
 */
export interface AsyncContentStore {
  read(threadId: string): Promise<ContentEntry | null>;
  write(entry: ContentEntry): Promise<void>;
  delete(threadId: string): Promise<void>;
  keys(): Promise<string[]>;
}

/** In-memory implementation: the test double and the no-IndexedDB fallback. */
export class MemoryContentStore implements AsyncContentStore {
  private entries = new Map<string, ContentEntry>();
  /**
   * 调用次数。**仅供测试观测**，不参与任何存储语义，也绝不写进 `ContentEntry`
   * （那是被 `toEqual` 逐字段比对的对象，加字段会让既有断言全挂）。
   *
   * 注意 `deletes` 会混入 `evict` 的淘汰删除——`persistContent` 写完就
   * `await evict(store)`，后者也调 `delete`。要区分「业务删除」请改用
   * `keys()` 断言。
   */
  writes = 0;
  deletes = 0;

  async read(threadId: string): Promise<ContentEntry | null> {
    return this.entries.get(threadId) ?? null;
  }
  async write(entry: ContentEntry): Promise<void> {
    this.writes += 1;
    // Deep-copy so a caller mutating its arrays afterwards cannot alter what was
    // "stored" — the real store serializes, so this mirrors that boundary.
    this.entries.set(entry.threadId, JSON.parse(JSON.stringify(entry)) as ContentEntry);
  }
  async delete(threadId: string): Promise<void> {
    this.deletes += 1;
    this.entries.delete(threadId);
  }
  async keys(): Promise<string[]> {
    return [...this.entries.keys()];
  }
}

// 常量已抽到 cacheDb.ts（打断与 cacheScope.ts 的循环依赖）。这里重导出，
// 保持既有 import 路径可用。
export { DB_NAME, DB_VERSION, STORE_NAME };

/** IndexedDB-backed store. Opens lazily on first use. */
export function createIndexedDbStore(options?: { desktop?: boolean }): AsyncContentStore {
  // Separate DB: remote pairing cleanup must not erase local desktop snapshots.
  const desktop = options?.desktop === true;
  const dbName = desktop ? "yi-agent-desktop-cache" : DB_NAME;
  let dbPromise: Promise<IDBDatabase> | null = null;

  const open = (): Promise<IDBDatabase> =>
    new Promise((resolve, reject) => {
      const req = indexedDB.open(dbName, DB_VERSION);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(STORE_NAME)) {
          db.createObjectStore(STORE_NAME, { keyPath: "threadId" });
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });

  // 打开库之前先等「失配清理」完成：所有读写都经 run() → db()，故这是唯一
  // 收口点。用 `??=` 一次写完，避免「清理完成」与「开始建库」之间的空档被
  // 另一个调用插进来，各自建一个库句柄。
  //
  // 不能写成 `const d = await ensureCacheScopeClean(); dbPromise ??= open()`：
  // 那样两个并发调用都会走到 open()。
  const db = (): Promise<IDBDatabase> =>
    (dbPromise ??= (desktop ? Promise.resolve() : ensureCacheScopeClean()).then(open));

  const run = async <T>(
    mode: IDBTransactionMode,
    fn: (store: IDBObjectStore) => IDBRequest,
  ): Promise<T> => {
    const d = await db();
    return new Promise<T>((resolve, reject) => {
      const tx = d.transaction(STORE_NAME, mode);
      const req = fn(tx.objectStore(STORE_NAME));
      req.onsuccess = () => resolve(req.result as T);
      req.onerror = () => reject(req.error);
    });
  };

  return {
    read: (threadId) =>
      run<ContentEntry | undefined>("readonly", (s) => s.get(threadId)).then((e) => e ?? null),
    write: (entry) => run<void>("readwrite", (s) => s.put(entry)).then(() => undefined),
    delete: (threadId) => run<void>("readwrite", (s) => s.delete(threadId)).then(() => undefined),
    keys: () =>
      run<IDBValidKey[]>("readonly", (s) => s.getAllKeys()).then((keys) => keys.map(String)),
  };
}

/** The store for this environment: IndexedDB when present, else in-memory. */
export function createContentStore(options?: { desktop?: boolean }): AsyncContentStore {
  try {
    if (typeof indexedDB !== "undefined") return createIndexedDbStore(options);
  } catch {
    // Accessing `indexedDB` can throw in locked-down webviews.
  }
  return new MemoryContentStore();
}

/** Read every entry's meta and delete the ids the LRU policy returns. */
export async function evict(
  store: AsyncContentStore,
  limits: { maxCount: number; maxBytes: number } = CONTENT_LIMITS,
): Promise<string[]> {
  const keys = await store.keys();
  const metas = (await Promise.all(keys.map((k) => store.read(k))))
    .filter((e): e is ContentEntry => e !== null)
    .map((e) => ({ threadId: e.threadId, savedAt: e.savedAt, bytes: e.bytes }));
  const gone = selectEvictions(metas, limits);
  for (const id of gone) await store.delete(id);
  // 返回被删掉的 id：调用方（`persistContent` → `flushContent`）要把它们从
  // `persistedFp` 里删掉，否则内存会以为那些会话「仍与盘上一致」而永不重写。
  return gone;
}

/**
 * The slice of a `Session` this module persists. Structural (not the `Session`
 * class) so the module has no dependency on the app's session shape beyond
 * these two members.
 */
export interface PersistSource {
  items: (Item | { id: string; type: string })[];
  lastServerItemId: string | null;
  /** 该会话已知的服务端内容世代；`undefined`/`null` = 未知（保守全量）。(spec §4.3) */
  contentEpoch?: number | null;
}

/** `persistContent` 的选项：把「没内容」的处置权交给调用点。 */
export interface PersistOptions {
  /**
   * 该会话的内容已与服务端对账完成（全量 resume 成功）。
   * true 时 items 为空 = 服务端确认此会话无内容 ⇒ 删除条目；
   * false（默认）时 items 为空 = 我们还没有内容可写 ⇒ 不写也不删。
   *
   * **注意**：`session.items` 非空（本地回显 / notice）时，即使 authoritative
   * 也只跳过、不删除——「服务端确认无内容」在有在途回显时不成立。
   */
  authoritative?: boolean;
}

/**
 * `persistContent` 到底动了盘上什么。调用方据此决定 `persistedFp` 的 set/delete。
 *
 * `evicted` 是**写入成功后 LRU 淘汰掉的其它会话 id**。调用方必须把它们从
 * `persistedFp` 里删掉：否则那些会话在盘上已经没了，而内存还以为「已落盘且内容
 * 没变」，`flushAll` 会一直判「没变」而跳过，它们从此再也写不回去——表现为
 * 「用着用着，十几个会话里只有最近几条还有缓存」。这是内存镜像与盘上状态脱钩，
 * 只有 `persistContent` 自己知道发生了什么，必须由它回报。
 */
export type PersistOutcome =
  | { action: "written"; fingerprint: string; evicted: string[] }
  | { action: "deleted" }
  | { action: "skipped" };

/**
 * 归一化：与 `persistContent` 内部完全同一套（先剔 notice / `local-*`，
 * 再按 `MAX_ENTRY_BYTES` 从头部裁剪）。
 *
 * 指纹必须基于归一化结果，不能基于 `session.items`——否则含 notice 或有本地
 * 回显的会话两侧条目数永远不等，指纹永远判「变了」，等于没有优化。
 */
export function normalizeForCache(session: PersistSource): { items: Item[]; trimmed: boolean } {
  const serverItems = session.items.filter(
    (i): i is Item => i.type !== "notice" && !i.id.startsWith("local-"),
  );
  return applyTrim(serverItems, MAX_ENTRY_BYTES);
}

/**
 * 指纹：盘上条目的身份。入参必须是**归一化后**的条目列表。
 *
 * 已知盲区：同 id 就地替换且长度/游标/世代都不变时判「没变」。这种情形是
 * 服务端权威值的幂等重放，与已落盘内容等价。**它不是内容哈希**。
 */
export function contentFingerprintOf(
  items: Item[],
  lastServerItemId: string | null,
  contentEpoch: number | null | undefined,
): string {
  return `${items.length}:${lastServerItemId ?? ""}:${contentEpoch ?? ""}`;
}

/**
 * Serialize a session and write it under the per-entry budget, then evict to
 * the global budget. Errors propagate so the caller can swallow them: a failed
 * cache write must never surface as an app error (it is only a cache).
 */
export async function persistContent(
  store: AsyncContentStore,
  threadId: string,
  session: PersistSource,
  options: PersistOptions = {},
): Promise<PersistOutcome> {
  // 复用与 normalizeForCache 同一套过滤：判据只能有一套，两套迟早漂移。
  const serverItems = session.items.filter(
    (i): i is Item => i.type !== "notice" && !i.id.startsWith("local-"),
  );

  // 空内容守卫。位置在 applyTrim 之前：对空数组做裁剪只会得到空数组。
  if (serverItems.length === 0) {
    // 还有本地回显 / notice 时**绝不删**：那说明用户刚发了一条、服务端尚未回帧，
    // 内容正在流动，此刻「服务端确认无内容」这个结论不成立。删了会把盘上已有的
    // 好历史抹掉，而 resume 只补锚点之后的项，补不回来。
    if (!options.authoritative || session.items.length > 0) return { action: "skipped" };
    await store.delete(threadId);
    return { action: "deleted" };
  }

  const { items, trimmed } = applyTrim(serverItems, MAX_ENTRY_BYTES);
  const entry: ContentEntry = {
    threadId,
    savedAt: Date.now(),
    lastServerItemId: session.lastServerItemId,
    items,
    trimmed,
    bytes: estimateItemsBytes(items),
    contentEpoch: session.contentEpoch,
  };
  await store.write(entry);
  // `evict` 现在返回它删掉的 id：调用方必须同步清掉这些指纹（见 PersistOutcome）。
  const evicted = await evict(store);
  // 指纹取自写入的 entry（已 trim），与冷启时 contentFingerprintOf(entry.items, …)
  // 是同一个函数作用于同一份数据，天然相等；用 session.items 会因裁剪而永不相等。
  return {
    action: "written",
    fingerprint: contentFingerprintOf(entry.items, entry.lastServerItemId, entry.contentEpoch),
    evicted,
  };
}
