import type { Item } from "./protocol";

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
  let working = items;
  let trimmed = false;
  while (working.length > 1 && estimateItemsBytes(working) > maxBytes) {
    working = working.slice(1);
    trimmed = true;
  }
  return { items: working, trimmed };
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
  async read(threadId: string): Promise<ContentEntry | null> {
    return this.entries.get(threadId) ?? null;
  }
  async write(entry: ContentEntry): Promise<void> {
    // Deep-copy so a caller mutating its arrays afterwards cannot alter what was
    // "stored" — the real store serializes, so this mirrors that boundary.
    this.entries.set(entry.threadId, JSON.parse(JSON.stringify(entry)) as ContentEntry);
  }
  async delete(threadId: string): Promise<void> {
    this.entries.delete(threadId);
  }
  async keys(): Promise<string[]> {
    return [...this.entries.keys()];
  }
}

const DB_NAME = "yi-agent-cache";
const DB_VERSION = 1;
const STORE_NAME = "threadContent";

/** IndexedDB-backed store. Opens lazily on first use. */
export function createIndexedDbStore(): AsyncContentStore {
  let dbPromise: Promise<IDBDatabase> | null = null;

  const open = (): Promise<IDBDatabase> =>
    new Promise((resolve, reject) => {
      const req = indexedDB.open(DB_NAME, DB_VERSION);
      req.onupgradeneeded = () => {
        const db = req.result;
        if (!db.objectStoreNames.contains(STORE_NAME)) {
          db.createObjectStore(STORE_NAME, { keyPath: "threadId" });
        }
      };
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });

  const db = (): Promise<IDBDatabase> => (dbPromise ??= open());

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
export function createContentStore(): AsyncContentStore {
  try {
    if (typeof indexedDB !== "undefined") return createIndexedDbStore();
  } catch {
    // Accessing `indexedDB` can throw in locked-down webviews.
  }
  return new MemoryContentStore();
}

/** Read every entry's meta and delete the ids the LRU policy returns. */
export async function evict(
  store: AsyncContentStore,
  limits: { maxCount: number; maxBytes: number } = CONTENT_LIMITS,
): Promise<void> {
  const keys = await store.keys();
  const metas = (await Promise.all(keys.map((k) => store.read(k))))
    .filter((e): e is ContentEntry => e !== null)
    .map((e) => ({ threadId: e.threadId, savedAt: e.savedAt, bytes: e.bytes }));
  for (const id of selectEvictions(metas, limits)) await store.delete(id);
}

/**
 * The slice of a `Session` this module persists. Structural (not the `Session`
 * class) so the module has no dependency on the app's session shape beyond
 * these two members.
 */
export interface PersistSource {
  items: (Item | { id: string; type: string })[];
  lastServerItemId: string | null;
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
): Promise<void> {
  const serverItems = session.items.filter(
    (i): i is Item => i.type !== "notice" && !i.id.startsWith("local-"),
  );
  const { items, trimmed } = applyTrim(serverItems, MAX_ENTRY_BYTES);
  await store.write({
    threadId,
    savedAt: Date.now(),
    lastServerItemId: session.lastServerItemId,
    items,
    trimmed,
    bytes: estimateItemsBytes(items),
  });
  await evict(store);
}
