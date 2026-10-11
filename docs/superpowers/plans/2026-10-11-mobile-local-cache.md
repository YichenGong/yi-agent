# 手机端本地缓存 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 手机端（iOS 远程客户端）冷启/断线重连时界面不再长时间空白——侧栏会话列表首帧即有、点进会话立刻看到上次内容；连上后由服务端权威数据就地覆盖并清除缓存痕迹；桌面端行为逐字节不变。

**Architecture:** 新增两个模块：`lib/sessionCache.ts`（列表元数据，localStorage，同步，供首帧）+ `lib/contentStore.ts`（对话内容，IndexedDB，异步，LRU 有界）。`App.tsx` 只做接线：初始化 state 时注入缓存、选中会话时预填内容、内容变更后防抖落盘、`thread/resume` 成功后对账删幽灵项。全部读写 gate 在 `isRemoteClient()`，桌面端路径完全不变。

**Tech Stack:** TypeScript, React 19, vitest + jsdom, @testing-library/react。无新依赖（IndexedDB 走原生 API，测试用内存假实现）。

## Global Constraints

- **仅手机端生效**：所有缓存读写 gate 在 `isRemoteClient()`（`desktop/src/lib/platform.ts`）。桌面端行为逐字节不变。
- **服务端权威**：缓存只做首屏占位；`thread/listAll` / `thread/resume` 到达即覆盖。
- **上界**：内容缓存最近 **10** 条；单条 **1 MiB**（超出丢头部保留尾部）；总量 **5 MiB**。
- **不引入新依赖**：不安装 `idb` / `fake-indexeddb`；IndexedDB 适配层经 `AsyncContentStore` 接口注入，测试用内存实现。
- **不缓存**：`local-*` 本地乐观回声、`notice`（命令输出）、`approval`、`unread`。
- **只在全量回放（resume 成功）后删项**；`thread/readItems` 补尾部的情形不删项。
- **无 Rust 改动**：不跑 cargo。提交前无需 `cargo fmt`。
- 计划内所有文件路径相对仓库根的 `desktop/`。

---

### Task 1: `lib/relativeTime.ts` — 相对时间纯函数 + 侧栏复用

**Files:**
- Create: `desktop/src/lib/relativeTime.ts`
- Create: `desktop/src/lib/relativeTime.test.ts`
- Modify: `desktop/src/components/ThreadSidebar.tsx:11-21`（删本地 `relativeTime`，改 import）

**Interfaces:**
- Produces: `formatRelativeTime(ms: number, now?: number): string` — 返回 `"now"` / `"3m"` / `"2h"` / `"5d"`。

- [ ] **Step 1: Write the failing test**

```ts
// desktop/src/lib/relativeTime.test.ts
import { describe, expect, it } from "vitest";
import { formatRelativeTime } from "./relativeTime";

const T = 1_000_000_000_000; // fixed "now"

describe("formatRelativeTime", () => {
  it("returns 'now' under a minute", () => {
    expect(formatRelativeTime(T - 0, T)).toBe("now");
    expect(formatRelativeTime(T - 59_000, T)).toBe("now");
  });
  it("returns minutes under an hour", () => {
    expect(formatRelativeTime(T - 60_000, T)).toBe("1m");
    expect(formatRelativeTime(T - 3 * 60_000, T)).toBe("3m");
    expect(formatRelativeTime(T - 59 * 60_000, T)).toBe("59m");
  });
  it("returns hours under a day", () => {
    expect(formatRelativeTime(T - 60 * 60_000, T)).toBe("1h");
    expect(formatRelativeTime(T - 23 * 60 * 60_000, T)).toBe("23h");
  });
  it("returns days beyond a day", () => {
    expect(formatRelativeTime(T - 24 * 60 * 60_000, T)).toBe("1d");
    expect(formatRelativeTime(T - 5 * 24 * 60 * 60_000, T)).toBe("5d");
  });
  it("clamps a future timestamp to 'now'", () => {
    expect(formatRelativeTime(T + 10_000, T)).toBe("now");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/relativeTime.test.ts`
Expected: FAIL — cannot resolve `./relativeTime`.

- [ ] **Step 3: Write minimal implementation**

```ts
// desktop/src/lib/relativeTime.ts
/**
 * Compact relative time for cache/persisted timestamps, e.g. "now", "3m", "2h",
 * "5d". `now` is injectable so the unit is testable without freezing the clock.
 * A timestamp in the future (clock skew) clamps to "now" rather than rendering a
 * negative unit.
 */
export function formatRelativeTime(ms: number, now: number = Date.now()): string {
  const diff = Math.max(0, now - ms);
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return "now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour}h`;
  const day = Math.floor(hour / 24);
  return `${day}d`;
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/relativeTime.test.ts`
Expected: PASS.

- [ ] **Step 5: Refactor ThreadSidebar to reuse it (delete the private copy)**

In `desktop/src/components/ThreadSidebar.tsx`, delete the local `relativeTime` function
(lines 10-21, including its leading doc comment) and add to the imports:

```ts
import { formatRelativeTime } from "../lib/relativeTime";
```

Then replace the single call site at line 451:

```tsx
              <div className="text-xs text-fg-faint">{formatRelativeTime(t.updated_at)}</div>
```

- [ ] **Step 6: Verify no regression in the sidebar**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add desktop/src/lib/relativeTime.ts desktop/src/lib/relativeTime.test.ts desktop/src/components/ThreadSidebar.tsx
git commit -m "feat: 抽出 formatRelativeTime 纯函数并复用到侧栏"
```

---

### Task 2: `contentStore.ts` 纯逻辑 — 裁剪与 LRU 淘汰

**Files:**
- Create: `desktop/src/lib/contentStore.ts`
- Create: `desktop/src/lib/contentStore.test.ts`

**Interfaces:**
- Produces:
  - `export const MAX_ENTRY_BYTES = 1024 * 1024`
  - `export const CONTENT_LIMITS: { maxCount: number; maxBytes: number }` = `{ maxCount: 10, maxBytes: 5 * 1024 * 1024 }`
  - `export function estimateItemsBytes(items: Item[]): number`
  - `export function applyTrim(items: Item[], maxBytes: number): { items: Item[]; trimmed: boolean }`
  - `export function selectEvictions(metas: EntryMeta[], limits: { maxCount: number; maxBytes: number }): string[]`
  - `export interface EntryMeta { threadId: string; savedAt: number; bytes: number }`

- [ ] **Step 1: Write the failing test**

```ts
// desktop/src/lib/contentStore.test.ts
import { describe, expect, it } from "vitest";
import type { Item } from "./protocol";
import {
  applyTrim,
  estimateItemsBytes,
  MAX_ENTRY_BYTES,
  selectEvictions,
} from "./contentStore";

const msg = (id: string, text: string): Item => ({ type: "agentMessage", id, text });

describe("estimateItemsBytes", () => {
  it("grows with content and is > 0 for a non-empty list", () => {
    const one = estimateItemsBytes([msg("a", "hello")]);
    const two = estimateItemsBytes([msg("a", "hello"), msg("b", "world")]);
    expect(one).toBeGreaterThan(0);
    expect(two).toBeGreaterThan(one);
  });
});

describe("applyTrim", () => {
  it("returns items unchanged when under the budget", () => {
    const items = [msg("a", "hello"), msg("b", "world")];
    expect(applyTrim(items, 1_000_000)).toEqual({ items, trimmed: false });
  });

  it("drops from the head (oldest) until it fits, keeping the tail", () => {
    const items = [msg("a", "x".repeat(200)), msg("b", "y".repeat(200)), msg("c", "z".repeat(200))];
    const budget = estimateItemsBytes([items[2]]); // only the last one fits
    const out = applyTrim(items, budget);
    expect(out.trimmed).toBe(true);
    expect(out.items).toEqual([items[2]]); // newest preserved
    expect(estimateItemsBytes(out.items)).toBeLessThanOrEqual(budget);
  });

  it("keeps at least one item even if it alone exceeds the budget", () => {
    const big = msg("a", "x".repeat(5000));
    const out = applyTrim([big], 10);
    expect(out.items).toEqual([big]);
  });

  it("an empty list stays empty and untrimmed", () => {
    expect(applyTrim([], MAX_ENTRY_BYTES)).toEqual({ items: [], trimmed: false });
  });
});

describe("selectEvictions", () => {
  const meta = (threadId: string, savedAt: number, bytes: number) => ({ threadId, savedAt, bytes });

  it("evicts nothing when under both bounds", () => {
    const metas = [meta("a", 1, 100), meta("b", 2, 100)];
    expect(selectEvictions(metas, { maxCount: 10, maxBytes: 1000 })).toEqual([]);
  });

  it("evicts the oldest first to satisfy the count bound", () => {
    const metas = [meta("old", 1, 10), meta("mid", 2, 10), meta("new", 3, 10)];
    expect(selectEvictions(metas, { maxCount: 2, maxBytes: 10_000 })).toEqual(["old"]);
  });

  it("evicts oldest until the byte bound is met too", () => {
    const metas = [meta("old", 1, 600), meta("mid", 2, 600), meta("new", 3, 600)];
    expect(selectEvictions(metas, { maxCount: 10, maxBytes: 1000 })).toEqual(["old", "mid"]);
  });

  it("stops as soon as both bounds hold (no over-eviction)", () => {
    const metas = [meta("old", 1, 10), meta("mid", 2, 10), meta("new", 3, 10)];
    expect(selectEvictions(metas, { maxCount: 2, maxBytes: 1000 })).toHaveLength(1);
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: FAIL — cannot resolve `./contentStore`.

- [ ] **Step 3: Write minimal implementation**

```ts
// desktop/src/lib/contentStore.ts
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/contentStore.ts desktop/src/lib/contentStore.test.ts
git commit -m "feat: 内容缓存纯逻辑（裁剪 + LRU 淘汰）"
```

---

### Task 3: `contentStore.ts` 存储层 — 接口 / 内存实现 / IndexedDB 适配 / 落盘策略

**Files:**
- Modify: `desktop/src/lib/contentStore.ts`（追加）
- Modify: `desktop/src/lib/contentStore.test.ts`（追加）

**Interfaces:**
- Consumes: `applyTrim`, `estimateItemsBytes`, `selectEvictions`, `CONTENT_LIMITS`, `MAX_ENTRY_BYTES`, `EntryMeta`（Task 2）。
- Produces:
  - `export interface ContentEntry { threadId: string; savedAt: number; lastServerItemId: string | null; items: Item[]; trimmed: boolean; bytes: number }`
  - `export interface AsyncContentStore { read(threadId: string): Promise<ContentEntry | null>; write(entry: ContentEntry): Promise<void>; delete(threadId: string): Promise<void>; keys(): Promise<string[]> }`
  - `export class MemoryContentStore implements AsyncContentStore`
  - `export function createContentStore(): AsyncContentStore`
  - `export function createIndexedDbStore(): AsyncContentStore`
  - `export async function evict(store: AsyncContentStore, limits?: { maxCount: number; maxBytes: number }): Promise<void>`
  - `export interface PersistSource { items: (Item | { id: string; type: string })[]; lastServerItemId: string | null }`
  - `export async function persistContent(store: AsyncContentStore, threadId: string, session: PersistSource): Promise<void>`

- [ ] **Step 1: Write the failing test (append)**

```ts
// desktop/src/lib/contentStore.test.ts  (追加)
import { MemoryContentStore, evict, persistContent } from "./contentStore";
import { Session } from "./session";

describe("MemoryContentStore", () => {
  it("round-trips an entry, deletes, and lists keys", async () => {
    const store = new MemoryContentStore();
    const entry = {
      threadId: "t1",
      savedAt: 1,
      lastServerItemId: "i1",
      items: [msg("i1", "hi")],
      trimmed: false,
      bytes: 3,
    };
    await store.write(entry);
    expect(await store.read("t1")).toEqual(entry);
    expect(await store.keys()).toEqual(["t1"]);
    await store.delete("t1");
    expect(await store.read("t1")).toBeNull();
    expect(await store.keys()).toEqual([]);
  });

  it("copies on write so later mutation of the source does not leak in", async () => {
    const store = new MemoryContentStore();
    const items = [msg("i1", "hi")];
    await store.write({ threadId: "t1", savedAt: 1, lastServerItemId: null, items, trimmed: false, bytes: 3 });
    items.push(msg("i2", "later"));
    const back = await store.read("t1");
    expect(back?.items).toHaveLength(1);
  });

  it("reads a missing key as null", async () => {
    expect(await new MemoryContentStore().read("nope")).toBeNull();
  });
});

describe("evict", () => {
  const write = async (store: MemoryContentStore, threadId: string, savedAt: number, bytes: number) =>
    store.write({ threadId, savedAt, lastServerItemId: null, items: [], trimmed: false, bytes });

  it("deletes the least recently written entries past the count bound", async () => {
    const store = new MemoryContentStore();
    await write(store, "old", 1, 10);
    await write(store, "mid", 2, 10);
    await write(store, "new", 3, 10);
    await evict(store, { maxCount: 2, maxBytes: 10_000 });
    expect((await store.keys()).sort()).toEqual(["mid", "new"]);
  });
});

describe("persistContent", () => {
  it("stores only server items (drops local echoes and notices)", async () => {
    const store = new MemoryContentStore();
    const session = new Session();
    session.notice("command output");           // local-* id
    session.addUserMessage("hi");               // local-* optimistic echo
    session.apply({ method: "item/completed", params: { item: msg("s1", "server text") } } as never);
    await persistContent(store, "t1", session);
    const entry = await store.read("t1");
    expect(entry?.items.map((i) => i.id)).toEqual(["s1"]);
    expect(entry?.lastServerItemId).toBe("s1");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: FAIL — `MemoryContentStore` / `evict` / `persistContent` not exported.

- [ ] **Step 3: Write minimal implementation (append to contentStore.ts)**

```ts
// desktop/src/lib/contentStore.ts  (追加)

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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/contentStore.ts desktop/src/lib/contentStore.test.ts
git commit -m "feat: 内容缓存存储层（接口 + 内存实现 + IndexedDB 适配 + 落盘策略）"
```

---

### Task 4: `lib/sessionCache.ts` — 列表元数据缓存（localStorage）

**Files:**
- Create: `desktop/src/lib/sessionCache.ts`
- Create: `desktop/src/lib/sessionCache.test.ts`

**Interfaces:**
- Consumes: `StorageLike` from `./remoteConfig`; `WorkspaceGroup`, `ThreadSummary` from `./protocol`.
- Produces:
  - `export const THREAD_LIST_CACHE_KEY = "yi-agent.threadListCache"`
  - `export const THREAD_LIST_CACHE_VERSION = 1`
  - `export interface ThreadListCache { version: number; savedAt: number; selectedId: string | null; pinned: ThreadSummary[]; groups: WorkspaceGroup[] }`
  - `export function readListCache(storage?: StorageLike): ThreadListCache | null`
  - `export function writeListCache(storage: StorageLike | undefined, groups: WorkspaceGroup[], pinned: ThreadSummary[], selectedId: string | null): void`

- [ ] **Step 1: Write the failing test**

```ts
// desktop/src/lib/sessionCache.test.ts
import { describe, expect, it } from "vitest";
import type { StorageLike } from "./remoteConfig";
import type { ThreadSummary, WorkspaceGroup } from "./protocol";
import {
  readListCache,
  THREAD_LIST_CACHE_KEY,
  THREAD_LIST_CACHE_VERSION,
  writeListCache,
} from "./sessionCache";

function memStorage(): StorageLike & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => void map.set(k, v),
    removeItem: (k) => void map.delete(k),
  };
}

const thread = (id: string): ThreadSummary => ({
  thread_id: id, cwd: "/w", model: "m", created_at: 1, updated_at: 2, title: id,
});

describe("sessionCache", () => {
  it("round-trips groups, pinned and selectedId", () => {
    const s = memStorage();
    const groups: WorkspaceGroup[] = [{ workspace: "/w", exists: true, threads: [thread("t1")] }];
    writeListCache(s, groups, [thread("t1")], "t1");
    const back = readListCache(s);
    expect(back?.version).toBe(THREAD_LIST_CACHE_VERSION);
    expect(back?.selectedId).toBe("t1");
    expect(back?.groups).toEqual(groups);
    expect(back?.pinned.map((t) => t.thread_id)).toEqual(["t1"]);
  });

  it("returns null when nothing is stored", () => {
    expect(readListCache(memStorage())).toBeNull();
  });

  it("returns null for corrupt JSON", () => {
    const s = memStorage();
    s.setItem(THREAD_LIST_CACHE_KEY, "{not json");
    expect(readListCache(s)).toBeNull();
  });

  it("returns null for a version mismatch", () => {
    const s = memStorage();
    s.setItem(
      THREAD_LIST_CACHE_KEY,
      JSON.stringify({ version: 99, savedAt: 1, selectedId: null, pinned: [], groups: [] }),
    );
    expect(readListCache(s)).toBeNull();
  });

  it("returns null for a structurally wrong payload", () => {
    const s = memStorage();
    s.setItem(THREAD_LIST_CACHE_KEY, JSON.stringify({ version: 1, groups: "nope" }));
    expect(readListCache(s)).toBeNull();
  });

  it("swallows storage errors on read", () => {
    const s: StorageLike = {
      getItem: () => { throw new Error("blocked"); },
      setItem: () => {},
      removeItem: () => {},
    };
    expect(readListCache(s)).toBeNull();
  });

  it("swallows storage errors on write", () => {
    const s: StorageLike = {
      getItem: () => null,
      setItem: () => { throw new Error("quota"); },
      removeItem: () => {},
    };
    expect(() => writeListCache(s, [], [], null)).not.toThrow();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/sessionCache.test.ts`
Expected: FAIL — cannot resolve `./sessionCache`.

- [ ] **Step 3: Write minimal implementation**

```ts
// desktop/src/lib/sessionCache.ts
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/sessionCache.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/sessionCache.ts desktop/src/lib/sessionCache.test.ts
git commit -m "feat: 会话列表首屏缓存（localStorage）"
```

---

### Task 5: `Session` 回放对账 — `beginReplay` / `endReplay` / `dropStale`

**Files:**
- Modify: `desktop/src/lib/session.ts`
- Modify: `desktop/src/lib/session.test.ts`（追加）

**Interfaces:**
- Produces on `Session`:
  - `beginReplay(): void` — 开始收集回放期到达的服务端 item id。
  - `endReplay(): Set<string> | null` — 停止收集并返回 id 集合（未在收集时返回 `null`）。
  - `dropStale(serverIds: Set<string>): void` — 删除「非本地且不在集合中」的 item，并重算 `lastServerItemId`。

- [ ] **Step 1: Write the failing test (append to session.test.ts)**

```ts
// desktop/src/lib/session.test.ts  (追加)
describe("Session replay reconciliation", () => {
  const serverItem = (id: string, text: string) => ({
    method: "item/completed",
    params: { item: { type: "agentMessage", id, text } },
  });

  it("collects server ids during a replay and drops non-replayed ones", () => {
    const s = new Session();
    // Prime the session with a stale cache: A, B, C plus a local echo + a notice.
    s.upsertItems([
      { type: "agentMessage", id: "A", text: "a" },
      { type: "agentMessage", id: "B", text: "b" },
      { type: "agentMessage", id: "C", text: "c" },
    ]);
    s.notice("cmd");
    s.addUserMessage("hello");
    const notices = s.items.filter((i) => i.type === "notice");
    const localEchoes = s.items.filter((i) => i.id.startsWith("local-") && i.type === "userMessage");

    s.beginReplay();
    s.apply(serverItem("A", "a") as never);
    s.apply(serverItem("C", "c") as never);
    const ids = s.endReplay();
    expect(ids).toEqual(new Set(["A", "C"]));

    s.dropStale(ids!);
    expect(s.items.filter((i) => i.type !== "notice" && !i.id.startsWith("local-")).map((i) => i.id))
      .toEqual(["A", "C"]);
    // Local echoes and notices survive reconciliation.
    expect(s.items.filter((i) => i.type === "notice")).toHaveLength(notices.length);
    expect(s.items.filter((i) => i.id.startsWith("local-") && i.type === "userMessage"))
      .toHaveLength(localEchoes.length);
  });

  it("endReplay is null when no replay was started, and dropStale drops a server item absent from the replay", () => {
    const s = new Session();
    s.upsertItems([{ type: "agentMessage", id: "A", text: "a" }]);
    expect(s.endReplay()).toBeNull();
    s.dropStale(new Set());
    expect(s.items.map((i) => i.id)).toEqual([]);
  });

  it("does not collect ids outside a replay", () => {
    const s = new Session();
    s.apply(serverItem("X", "x") as never);
    expect(s.endReplay()).toBeNull();
  });

  it("recomputes lastServerItemId from the surviving items", () => {
    const s = new Session();
    s.upsertItems([
      { type: "agentMessage", id: "A", text: "a" },
      { type: "agentMessage", id: "B", text: "b" },
    ]);
    s.dropStale(new Set(["A"]));
    expect(s.lastServerItemId).toBe("A");
  });

  it("clears lastServerItemId when nothing server-authored survives", () => {
    const s = new Session();
    s.upsertItems([{ type: "agentMessage", id: "B", text: "b" }]);
    s.dropStale(new Set());
    expect(s.lastServerItemId).toBeNull();
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: FAIL — `beginReplay` / `endReplay` / `dropStale` undefined.

- [ ] **Step 3: Write minimal implementation**

Add the field and a helper near the top of the `Session` class:

```ts
  /**
   * Server item ids seen while a full replay is in flight, or `null` when no
   * replay is being collected. `dropStale` uses the completed set to tell a
   * cached item the server still has from one it no longer has.
   */
  private replayIds: Set<string> | null = null;

  /** Start collecting server item ids (called before `thread/resume`). */
  beginReplay(): void {
    this.replayIds = new Set();
  }

  /**
   * Stop collecting and hand back the ids seen. Returns `null` when no replay
   * was in flight, so a caller cannot accidentally reconcile with an empty set.
   */
  endReplay(): Set<string> | null {
    const ids = this.replayIds;
    this.replayIds = null;
    return ids;
  }

  /**
   * Drop every item the server did not just replay.
   *
   * Called only after a *full* replay (a successful `thread/resume`), where the
   * server re-sent its entire history: any server item we hold that is absent
   * from `serverIds` no longer exists server-side (cleared, compacted, or
   * trimmed away) and must not linger as ghost content. Local echoes and notices
   * are ours, not the server's — they are always kept.
   */
  dropStale(serverIds: Set<string>): void {
    this.items = this.items.filter(
      (i) => Session.isLocalId(i.id) || serverIds.has(i.id),
    );
    // The cursor must point at a surviving server item (or nothing), never at a
    // dropped one: otherwise a later `readItems(afterItemId)` asks for a window
    // anchored past content the transcript no longer holds.
    for (let i = this.items.length - 1; i >= 0; i -= 1) {
      const id = this.items[i].id;
      if (!Session.isLocalId(id)) {
        this.lastServerItemId = id;
        return;
      }
    }
    this.lastServerItemId = null;
  }
```

Then, in `apply`, record ids while collecting. Add at the start of the
`item/started` / `item/completed` branch (right after `const item = …` normalization)
and in the `items/completed` and `item/delta` branches:

```ts
        if (this.replayIds !== null) this.replayIds.add(item.id);
```

```ts
      case "items/completed": {
        if (this.replayIds !== null) {
          for (const it of notification.params.items) this.replayIds.add(it.id);
        }
        this.upsertItems(notification.params.items);
        break;
      }
```

```ts
      case "item/delta": {
        if (this.replayIds !== null) this.replayIds.add(item_id);
        ...
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: PASS (whole file, no regression).

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat: Session 回放对账（beginReplay/endReplay/dropStale）"
```

---

### Task 6: App 接线 — 首屏注入列表、预填内容、落盘、删除时清缓存

**Files:**
- Modify: `desktop/src/App.tsx`
- Modify: `desktop/src/App.test.tsx`（新增 describe 组）

**Interfaces:**
- Consumes: `readListCache` / `writeListCache`（Task 4）；`createContentStore` / `persistContent` / `AsyncContentStore` / `ContentEntry`（Task 3、第 2 节接口）；`Session.upsertItems` / `dropStale` / `beginReplay` / `endReplay`（Task 5）。
- Produces: App 内部 `contentStoreRef`、`prefillFromCache(id)`、`flushContent(id)`；无外部新导出。

- [ ] **Step 1: Add the module mock to App.test.tsx**

At the top of `desktop/src/App.test.tsx`, alongside the existing `vi.mock` calls, add:

```ts
const { contentStore } = vi.hoisted(() => ({
  contentStore: { current: null as null | import("./lib/contentStore").AsyncContentStore },
}));
vi.mock("./lib/contentStore", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./lib/contentStore")>();
  return {
    ...actual,
    // One instance per test: the App grabs it lazily on first remote render.
    createContentStore: () => (contentStore.current ??= new actual.MemoryContentStore()),
  };
});
```

And reset it in `beforeEach`:

```ts
  contentStore.current = null;
```

- [ ] **Step 2: Write the failing tests (new describe group)**

Append to `desktop/src/App.test.tsx`:

```ts
// 手机端本地缓存：冷启首屏即有列表与内容，连上后由服务端覆盖。
describe("App mobile local cache", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  /** Defer `thread/listAll` until the returned resolver is called. */
  const stallList = () => {
    let release!: () => void;
    state.dataSources["thread/listAll"] = () =>
      new Promise((resolve) => {
        release = () =>
          resolve({
            groups: [
              {
                workspace: "/w",
                exists: true,
                threads: [
                  {
                    thread_id: "srv1", cwd: "/w", model: "m", created_at: 0, updated_at: 0,
                    title: "server-only", permission_mode: "normal", status: "idle",
                  },
                ],
              },
            ],
            pinned: [],
          });
      });
    return () => release();
  };

  const seedListCache = (threads: { thread_id: string; title: string }[], selectedId: string | null) =>
    localStorage.setItem(
      "yi-agent.threadListCache",
      JSON.stringify({
        version: 1,
        savedAt: Date.now(),
        selectedId,
        pinned: [],
        groups: [
          {
            workspace: "/w",
            exists: true,
            threads: threads.map((t) => ({
              thread_id: t.thread_id, cwd: "/w", model: "m", created_at: 0, updated_at: 0,
              title: t.title, permission_mode: "normal", status: "idle",
            })),
          },
        ],
      }),
    );

  it("paints the cached list before thread/listAll resolves", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "cached-one" }], null);
    const release = stallList();
    render(<App />);
    expect(screen.getByText("cached-one")).toBeTruthy();
    release();
    await waitFor(() => expect(screen.getByText("server-only")).toBeTruthy());
  });

  it("restores the cached selected thread instead of the first server thread", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "cached-one" }], "t1");
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const resumed = clients[0].requests
      .filter((r) => r.method === "thread/resume")
      .map((r) => (r.params as { threadId: string }).threadId);
    expect(resumed).toEqual(["t1"]);
  });

  it("prefills cached content, then the server replay replaces it in place", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "one" }], "t1");
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "A",
      items: [{ type: "agentMessage", id: "A", text: "cached text" }], trimmed: false, bytes: 11,
    });
    // The server replays a *different* item for the same thread.
    state.dataSources["thread/resume"] = () => ({ thread_id: "t1", cwd: "/w", model: "m", permission_mode: "normal" });
    render(<App />);
    await waitFor(() => expect(screen.getByText("cached text")).toBeTruthy());
    // Deliver the server's authoritative item live.
    act(() => {
      for (const h of state.notifHandlers)
        h({ method: "item/completed", params: { thread_id: "t1", item: { type: "agentMessage", id: "A", text: "server text" } } });
    });
    await waitFor(() => expect(screen.getByText("server text")).toBeTruthy());
    expect(screen.queryByText("cached text")).toBeNull();
  });

  it("drops a cached item the server no longer has after a full replay", async () => {
    asRemote();
    seedListCache([{ thread_id: "t1", title: "one" }], "t1");
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "t1", savedAt: Date.now(), lastServerItemId: "B",
      items: [
        { type: "agentMessage", id: "A", text: "keep me" },
        { type: "agentMessage", id: "B", text: "ghost" },
      ], trimmed: false, bytes: 20,
    });
    // Resume replays only A; B is gone server-side.
    state.dataSources["thread/resume"] = () => ({ thread_id: "t1", cwd: "/w", model: "m", permission_mode: "normal" });
    state.dataSources["thread/readItems"] = () => ({ items: [] });
    render(<App />);
    await waitFor(() => expect(screen.getByText("ghost")).toBeTruthy());
    act(() => {
      for (const h of state.notifHandlers)
        h({ method: "item/completed", params: { thread_id: "t1", item: { type: "agentMessage", id: "A", text: "keep me" } } });
    });
    await waitFor(() => expect(screen.getByText("keep me")).toBeTruthy());
    // After the replay settles, the ghost is gone.
    await waitFor(() => expect(screen.queryByText("ghost")).toBeNull());
  });

  it("never touches the list/content caches on the desktop build", async () => {
    localStorage.setItem("yi-agent.threadListCache", JSON.stringify({
      version: 1, savedAt: 1, selectedId: "t1", pinned: [],
      groups: [{ workspace: "/w", exists: true, threads: [{ thread_id: "t1", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "cached-one", permission_mode: "normal", status: "idle" }] }],
    }));
    contentStore.current = null;
    render(<App />); // no relay binding → desktop
    // The desktop build must not have painted from the cache…
    expect(screen.queryByText("cached-one")).toBeNull();
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy()); // server list
    // …and must not have constructed a content store.
    expect(contentStore.current).toBeNull();
  });
});
```

Add to the test file's imports:

```ts
import { MemoryContentStore } from "./lib/contentStore";
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App mobile local cache"`
Expected: FAIL — cache never read; cached list not painted.

- [ ] **Step 4: Wire the list cache into initial state**

In `desktop/src/App.tsx`, add imports:

```ts
import { readListCache, writeListCache } from "./lib/sessionCache";
import { createContentStore, persistContent, type AsyncContentStore } from "./lib/contentStore";
```

（`CacheNotice` 的 import 在 Task 7 接线提示条时再加——本任务还不渲染它。）

Inside `App`, just before the `groups`/`pinned` state (around line 179), add:

```ts
  // 手机端首屏缓存：权威列表在服务端，这里只放一份同步快照，让侧栏在
  // `thread/listAll` 回来之前就能画出来。桌面端不读（`null`），初值保持 []，
  // 行为与从前逐字节一致。
  const initialListCache = useRef(isRemoteClient() ? readListCache() : null).current;
  const [groups, setGroups] = useState<WorkspaceGroup[]>(initialListCache?.groups ?? []);
  const [pinned, setPinned] = useState<ThreadSummary[]>(initialListCache?.pinned ?? []);
```

Replace the existing two lines:

```ts
  const [groups, setGroups] = useState<WorkspaceGroup[]>([]);
  ...
  const [pinned, setPinned] = useState<ThreadSummary[]>([]);
```

Add the content store ref and helpers right after `const isMobile = useIsMobile();`:

```ts
  // 内容缓存（IndexedDB）：仅手机端。懒建，桌面端永不构造（`null`）。
  const contentStoreRef = useRef<AsyncContentStore | null>(null);
  const getContentStore = (): AsyncContentStore => (contentStoreRef.current ??= createContentStore());
  // 防抖落盘定时器，按 thread 隔离：一个会话的写入不能被另一个的盖掉。
  const persistTimers = useRef(new Map<string, number>());
  // 内容来自缓存、尚未被服务端覆盖的会话 → 其 `savedAt`（供离线提示）。
  const cachedAt = useRef(new Map<string, number>());
```

- [ ] **Step 5: Seed the list cache into the store and seed content on mount**

Add an effect (placed near the other initial effects, e.g. after the `current` derivation):

```ts
  // 首屏：把缓存里的会话播种进 store（标题/cwd/model/状态徽标），并预填选中会话
  // 的内容。预填**不**置 warm：那会让连上后的 `thread/resume` 被跳过，服务端历史
  // 永远合不进来。
  const prefilledOnce = useRef(false);
  useEffect(() => {
    const c = initialListCache;
    if (!c || prefilledOnce.current) return;
    prefilledOnce.current = true;
    store.seed([...c.pinned, ...c.groups.flatMap((g) => g.threads)]);
    if (c.selectedId && c.groups.some((g) => g.threads.some((t) => t.thread_id === c.selectedId))) {
      store.select(c.selectedId);
      setCurrentId(c.selectedId);
      void prefillFromCache(c.selectedId);
    }
    force((v) => v + 1);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
```

Add `prefillFromCache` above that effect:

```ts
  /**
   * 从内容缓存预填某会话的对话（手机端）。服务端 item 到达后按 id 就地替换，
   * 所以这里的旧内容只会短暂停留；若服务端已先填好了内容则整段跳过。
   */
  const prefillFromCache = (id: string) => {
    if (!isRemoteClient()) return;
    const store0 = getContentStore();
    void store0
      .read(id)
      .then((entry) => {
        if (!entry || entry.items.length === 0) return;
        const v = store.peek(id);
        if (!v || v.session.items.length > 0) return; // 服务端已经填过
        v.session.upsertItems(entry.items);
        v.session.lastServerItemId = v.session.lastServerItemId ?? entry.lastServerItemId;
        cachedAt.current.set(id, entry.savedAt);
        force((n) => n + 1);
      })
      .catch(() => {
        // 预填是附加能力：读失败保留空态，由 resume 兜底。
      });
  };
```

- [ ] **Step 6: Prefill on every cold selection, and persist after content changes**

In `selectThread`, at the very top (right after `store.select(id); setCurrentId(id);`), add:

```ts
    // 冷会话先拿缓存内容垫上（有则显示、无则空），随后 resume/readItems 覆盖。
    if (!warm.current.has(id)) prefillFromCache(id);
```

In the notification handler inside `wireClient`, right after `store.applyNotification(n); force((v) => v + 1);`, add:

```ts
        schedulePersist(n.params.thread_id);
```

Add `schedulePersist` / `flushContent` helpers (near `prefillFromCache`):

```ts
  /** 节流：内容变更后延迟落盘，避免逐字 delta 每次都写 IndexedDB。 */
  const CONTENT_PERSIST_DEBOUNCE_MS = 800;
  const schedulePersist = (id: string) => {
    if (!isRemoteClient()) return;
    const timers = persistTimers.current;
    const existing = timers.get(id);
    if (existing !== undefined) window.clearTimeout(existing);
    const timer = window.setTimeout(() => {
      timers.delete(id);
      void flushContent(id);
    }, CONTENT_PERSIST_DEBOUNCE_MS);
    timers.set(id, timer);
  };
  /** 立即落盘某会话（切走 / 页面隐藏 / 卸载时用）。 */
  const flushContent = (id: string) => {
    if (!isRemoteClient()) return Promise.resolve();
    const v = store.peek(id);
    if (!v) return Promise.resolve();
    return persistContent(getContentStore(), id, v.session).catch(() => {
      // 缓存写失败不影响任何用户可见行为。
    });
  };
```

Add lifecycle flush effects:

```ts
  // 页面隐藏 / 卸载时把在途的会话内容立刻写盘：手机切后台后可能很久不再执行
  // 定时器，等着防抖会把最后一段内容丢掉。
  useEffect(() => {
    if (!isRemoteClient()) return;
    const flushAll = () => {
      for (const id of persistTimers.current.keys()) {
        window.clearTimeout(persistTimers.current.get(id));
      }
      const ids = new Set([...persistTimers.current.keys(), ...store.ids()]);
      persistTimers.current.clear();
      for (const id of ids) void flushContent(id);
    };
    const onHide = () => {
      if (document.visibilityState === "hidden") flushAll();
    };
    document.addEventListener("visibilitychange", onHide);
    window.addEventListener("pagehide", flushAll);
    return () => {
      document.removeEventListener("visibilitychange", onHide);
      window.removeEventListener("pagehide", flushAll);
    };
  }, []);
```

Also flush when switching away from a thread: in `selectThread`, capture the previous id before `store.select(id)` and flush it:

```ts
    const prevId = store.currentId;
    store.select(id);
    ...
    if (prevId && prevId !== id) void flushContent(prevId);
```

- [ ] **Step 7: Write the list cache when authoritative lists arrive**

In `refreshThreads`, after `store.seed(...)`, add:

```ts
      if (isRemoteClient()) writeListCache(localStorage, r.groups, r.pinned ?? [], store.currentId);
```

In `handshake`, after the `thread/listAll` block (`store.seed(...)`) add the same call:

```ts
        if (isRemoteClient()) writeListCache(localStorage, list.groups, list.pinned ?? [], store.currentId);
```

- [ ] **Step 8: Delete cached content when a thread is deleted or cleared**

**Reconcile after a full replay.** In `selectThread`, wrap the `thread/resume` request so
the replay's item ids are collected and the ghosts are dropped once it succeeds. Replace the
existing resume block:

```ts
    inFlightResume.current.add(id);
    try {
      const resumed = await c.request<{ permission_mode?: ThreadMode }>("thread/resume", {
        threadId: id,
      });
```

with:

```ts
    inFlightResume.current.add(id);
    // 全量回放期间收集服务端 item id；成功后据此删掉缓存里服务端已经没有的项
    // （clear / compact / 裁剪造成的幽灵内容）。readItems 只补尾部，不走这条路径。
    const replaySession = store.view(id).session;
    replaySession.beginReplay();
    try {
      const resumed = await c.request<{ permission_mode?: ThreadMode }>("thread/resume", {
        threadId: id,
      });
      const replayed = replaySession.endReplay();
      if (replayed !== null) replaySession.dropStale(replayed);
```

and in the `catch` branch, also stop collecting (so a failed resume leaves no half-open
replay):

```ts
    } catch (e) {
      replaySession.endReplay();
      // 同样地,失败路径只在视图仍存在时写错误,避免 re-create 一个已被
      // drop 的 ThreadView(无界泄漏 + 残留 warm 条目)。
```

- [ ] **Step 8b: Persist right after a successful resume (so the cache is warm immediately)**

In `selectThread`, right after `warm.current.add(id);` add:

```ts
      void flushContent(id);
```

- [ ] **Step 8c: Delete cached content when a thread is deleted or cleared**

In `deleteThread`, after `store.drop(id);`:

```ts
    void getContentStore().delete(id).catch(() => {});
    persistTimers.current.delete(id);
    cachedAt.current.delete(id);
```

In the `/clear` success path (where `session.reset()` is called, `App.tsx:1307`), add:

```ts
          void getContentStore().delete(id).catch(() => {});
          cachedAt.current.delete(id);
```

(Use the thread id in scope at that call site; it is the current thread.)

- [ ] **Step 9: Run tests to verify they pass**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS (new group green; existing suites unaffected).

- [ ] **Step 10: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat: App 接线手机端本地缓存（列表首屏 + 内容预填 + 落盘 + 对账）"
```

---

### Task 7: 离线态 UI — 提示条、徽标过期、发送挂起

**Files:**
- Create: `desktop/src/components/CacheNotice.tsx`
- Create: `desktop/src/components/CacheNotice.test.tsx`
- Modify: `desktop/src/components/ThreadSidebar.tsx`（`StatusBadge` 增 `stale`）
- Modify: `desktop/src/App.tsx`（渲染提示条、传 `stale`、`send` 离线守卫）
- Modify: `desktop/src/App.test.tsx`（新增用例）

**Interfaces:**
- Consumes: `formatRelativeTime`（Task 1）；`cachedAt` ref（Task 6）。
- Produces:
  - `components/CacheNotice.tsx`：`CacheNotice({ savedAt }: { savedAt: number })`
  - `ThreadSidebar` 新 prop `statusStale?: boolean`

- [ ] **Step 1: Write the failing component test**

```tsx
// desktop/src/components/CacheNotice.test.tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { CacheNotice } from "./CacheNotice";

afterEach(cleanup);

describe("CacheNotice", () => {
  it("labels the content as offline and shows when it was cached", () => {
    render(<CacheNotice savedAt={Date.now() - 3 * 60_000} />);
    expect(screen.getByRole("status").textContent).toContain("离线内容");
    expect(screen.getByRole("status").textContent).toContain("3m");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/components/CacheNotice.test.tsx`
Expected: FAIL — cannot resolve `./CacheNotice`.

- [ ] **Step 3: Write the component**

```tsx
// desktop/src/components/CacheNotice.tsx
import { formatRelativeTime } from "../lib/relativeTime";

/**
 * The "this is cached, not live" strip shown above the transcript while the
 * phone client is offline. Its only job is to stop a stale transcript (and its
 * stale `running` badge) from being read as the current truth.
 */
export function CacheNotice({ savedAt }: { savedAt: number }) {
  return (
    <div
      role="status"
      className="border-b border-line bg-raised px-4 py-1 text-xs text-fg-muted"
    >
      离线内容 · 更新于 {formatRelativeTime(savedAt)}
    </div>
  );
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/components/CacheNotice.test.tsx`
Expected: PASS.

- [ ] **Step 5: Add the `stale` prop to `StatusBadge`**

In `desktop/src/components/ThreadSidebar.tsx`, change `StatusBadge` to accept `stale`:

```tsx
export const StatusBadge = memo(
  function StatusBadge({ status, stale = false }: { status: ThreadStatus; stale?: boolean }) {
    if (status === "idle") return null;
    const label = status === "running" ? "Running" : "Awaiting approval";
    return (
      <span
        role="img"
        aria-label="Thread status"
        data-status={status}
        data-stale={stale ? "true" : undefined}
        title={stale ? `${label}（可能过期）` : label}
        className={`shrink-0 animate-spin rounded-full ${stale ? "opacity-40" : ""} ${
          status === "running"
            ? "size-3 border-2 border-line-strong border-t-fg"
            : "size-2 bg-amber-400"
        }`}
      />
    );
  },
  (prev, next) => prev.status === next.status && prev.stale === next.stale,
);
```

Add `statusStale?: boolean` to `ThreadSidebar`'s props (default `false`), thread it into
the memoized `ThreadRow`, and pass it at the `<StatusBadge … />` usage (line ~356):

```tsx
    const st = statuses.get(t.thread_id) ?? "idle";
    ...
    <StatusBadge status={st} stale={statusStale} />
```

- [ ] **Step 6: Write the failing App tests**

Append to `desktop/src/App.test.tsx`:

```ts
describe("App mobile offline UX", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  const seed = (listStatus: "idle" | "running", selectedId: string | null) =>
    localStorage.setItem("yi-agent.threadListCache", JSON.stringify({
      version: 1, savedAt: Date.now(), selectedId, pinned: [],
      groups: [{ workspace: "/w", exists: true, threads: [
        { thread_id: "t1", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "one", permission_mode: "normal", status: listStatus },
      ] }],
    }));

  it("marks a cached running badge as possibly stale while offline", async () => {
    asRemote();
    seed("running", "t1");
    // Never resolve the list: stay in `connecting`.
    state.dataSources["thread/listAll"] = () => new Promise(() => {});
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("Thread status")).toBeTruthy());
    expect(screen.getByLabelText("Thread status").getAttribute("data-stale")).toBe("true");
  });

  it("refuses to send while offline and keeps the draft", async () => {
    asRemote();
    seed("idle", "t1");
    state.dataSources["thread/listAll"] = () => new Promise(() => {});
    render(<App />);
    const box = await screen.findByPlaceholderText(/Type a message/);
    fireEvent.change(box, { target: { value: "hello offline" } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));
    // No turn/start went out, and the text is still there.
    await waitFor(() => expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false));
    expect((box as HTMLTextAreaElement).value).toBe("hello offline");
  });
});
```

- [ ] **Step 7: Run tests to verify they fail**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App mobile offline UX"`
Expected: FAIL.

- [ ] **Step 8: Wire the offline UI in App.tsx**

Derive the flag near `const isMobile = useIsMobile();`:

```ts
  // 手机端离线 = 尚未握手成功。桌面端恒为 false（不显示缓存相关 UI）。
  const offline = isRemoteClient() && status !== "connected";
```

Guard `send` (top of the function, after the `if (!id || !c) return false;` line):

```ts
    // 离线时不发：缓存内容可以看，但不能对着它发消息（服务端根本收不到）。
    // 只提示、保留草稿，交给用户连上后再发——不排队、不自动重发。
    if (offline) {
      store.view(id).session.lastError = "正在连接，请稍候";
      force((v) => v + 1);
      return false;
    }
```

Render the notice above `ChatView` (inside the `<>{...}</>` that holds `ChatView`/`MessageInput`):

```tsx
              {offline && currentId !== null && cachedAt.current.has(currentId) && (
                <CacheNotice savedAt={cachedAt.current.get(currentId)!} />
              )}
              <ChatView
                ...
```

Pass `statusStale` to `ThreadSidebar`:

```tsx
          statusStale={offline}
```

Clear the cached-at marks once connected (add an effect):

```ts
  // 连上即清"内容来自缓存"的标记：此刻起服务端是唯一来源，提示条退场。
  useEffect(() => {
    if (status === "connected") cachedAt.current.clear();
  }, [status]);
```

- [ ] **Step 9: Run tests to verify they pass**

Run: `cd desktop && npx vitest run src/App.test.tsx src/components/ThreadSidebar.test.tsx src/components/CacheNotice.test.tsx`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add desktop/src/components/CacheNotice.tsx desktop/src/components/CacheNotice.test.tsx desktop/src/components/ThreadSidebar.tsx desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat: 离线态 UI（缓存提示条、徽标过期标注、发送挂起）"
```

---

### Task 8: 恢复上次选中 — 手机端握手不再跳回首个会话

**Files:**
- Modify: `desktop/src/App.tsx`（`handshake` 的选中逻辑）
- Modify: `desktop/src/App.test.tsx`（新增用例）

**Interfaces:**
- Consumes: App 的 `currentIdRef`、`isRemoteClient()`；Task 6 的缓存恢复。

- [ ] **Step 1: Write the failing tests**

Append to `desktop/src/App.test.tsx`:

```ts
describe("App mobile reopen keeps the selection", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  it("keeps the current thread across a foreground reconnect (remote)", async () => {
    asRemote();
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests.filter((r) => r.method === "thread/resume").map((r) => (r.params as { threadId: string }).threadId))
        .toContain("t2"),
    );
    clients[0].requests.length = 0;
    // Foreground the app: the App rebuilds the client and replays the handshake.
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    await waitFor(() =>
      expect(clients[clients.length - 1].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    // The reconnect must NOT have jumped back to the first thread.
    const resumed = clients[clients.length - 1].requests
      .filter((r) => r.method === "thread/resume")
      .map((r) => (r.params as { threadId: string }).threadId);
    expect(resumed).not.toContain("t1");
  });

  it("still selects the first thread on the desktop reconnect (unchanged)", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    fireEvent.click(screen.getByText("two"));
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    clients[0].requests.length = 0;
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    await waitFor(() => expect(clients.length).toBeGreaterThan(1));
    await waitFor(() =>
      expect(clients[clients.length - 1].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    // Desktop replay keeps its historical behaviour: it re-selects the first thread.
    await waitFor(() =>
      expect(clients[clients.length - 1].requests
        .filter((r) => r.method === "thread/resume")
        .map((r) => (r.params as { threadId: string }).threadId)).toContain("t1"),
    );
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App mobile reopen keeps the selection"`
Expected: FAIL — remote reconnect currently jumps back to `t1`.

- [ ] **Step 3: Guard the handshake selection**

In `desktop/src/App.tsx`, in `handshake`, replace:

```ts
        const first = (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
        if (first) await selectThread(first.thread_id, { leaveBoard: false });
```

with:

```ts
        // 手机端：已有当前会话就保持不变——重连（含切回前台）只是重放握手，
        // 不是「用户换了页面」，把选中的会话踢回第一条会让人丢掉正在看的东西。
        // 桌面端保留原行为（每次重放都选第一条），故只在远端加这道门。
        const restored = isRemoteClient() && currentIdRef.current !== null;
        if (!restored) {
          const first = (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
          if (first) await selectThread(first.thread_id, { leaveBoard: false });
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS (both new cases; existing handshake tests unaffected).

- [ ] **Step 5: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "fix: 手机端重连不再跳回首个会话（桌面行为不变）"
```

---

### Task 9: 文档同步 + 全量验证

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md`（无需改；仅确认路径）

- [ ] **Step 1: Add the feature line to the module file**

In `docs/project-management/desktop.md`, add under the appropriate feature list
(following the existing `- [x] … 判据：…` format):

```markdown
- [x] 手机端本地缓存（首屏不空、连上即刷）— 远程客户端冷启/重连时侧栏列表首帧即有（
  `desktop/src/lib/sessionCache.ts`：localStorage + 首帧注入 `App.tsx` 的 `groups`/`pinned` 初值），
  点进会话先显示上次内容（`desktop/src/lib/contentStore.ts`：IndexedDB，LRU 最近 10 条 / 单条 1 MiB / 总量 5 MiB，
  `App.tsx` 的 `prefillFromCache`），连上后由 `thread/listAll` / `thread/resume` 覆盖，
  全量回放成功即删幽灵项（`desktop/src/lib/session.ts` 的 `dropStale`）；
  断网时显示「离线内容」提示条（`desktop/src/components/CacheNotice.tsx`）、状态徽标标过期、发送挂起；
  全部 gate 在 `isRemoteClient()`，桌面端行为不变。判据：`cd desktop && npx vitest run src/lib/sessionCache.test.ts src/lib/contentStore.test.ts src/lib/session.test.ts src/App.test.tsx`
```

- [ ] **Step 2: Update the README module index counts if they changed**

Run: `cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && grep -n "desktop" README.md | head`

If `README.md` tracks a 完成/总计 count for the desktop module, bump the 完成 number by
1 to match the line added in Step 1. If no count is tracked, skip.

- [ ] **Step 3: Run the full desktop suite**

Run: `cd desktop && npx vitest run`
Expected: PASS — all suites green.

- [ ] **Step 4: Build check (types + bundle)**

Run: `cd desktop && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add docs/project-management/desktop.md README.md
git commit -m "docs: 登记手机端本地缓存并同步模块计数"
```

---

## Self-Review

**Spec coverage**

- §3 决策 1（范围：列表/内容/徽标）→ Tasks 4/6（列表）、3/6（内容）、7（徽标）。
- 决策 2（仅手机端）→ 所有任务 gate `isRemoteClient()`；Task 6/8 桌面回归用例。
- 决策 3（localStorage + IndexedDB）→ Tasks 4、3。
- 决策 4/5（LRU 10 / 1 MiB / 5 MiB）→ Task 2（`selectEvictions`、`applyTrim`）、Task 3（`evict`、`CONTENT_LIMITS`）。
- 决策 6（服务端权威）→ Task 6 Step 7 覆盖；Task 5 + 6 对账。
- 决策 7（离线提示 + 徽标降级）→ Task 7。未读点离线已天然为空（无通知），Task 7 未新增代码，属已验证的自然结论。
- 决策 8（可打字、发送挂起、不自动重发）→ Task 7 Step 8 的 `send` 守卫。
- 决策 9（恢复上次选中 + 不跳首个）→ Task 6 Step 5（冷启恢复）+ Task 8（重连保持）。
- §4.3 「预填不置 warm」→ Task 6 注释与实现（`prefillFromCache` 不 add 到 `warm`）。
- §5.C/D（只有全量回放才删项）→ Task 5 只在成功 resume 后 `dropStale`；`readItems` 路径不调用（Task 6 Step 6 只在 `thread/resume` 成功后对账）。
- §8 测试项 → 各任务的测试步骤 + Task 9 全量。

**Placeholder scan** — 无 TBD/TODO；每个代码步骤给出可编译代码。

**Type consistency** — `ContentEntry` / `AsyncContentStore` / `EntryMeta` / `CONTENT_LIMITS` / `MAX_ENTRY_BYTES` / `selectEvictions` / `applyTrim` / `persistContent` / `evict` / `MemoryContentStore` / `createContentStore` 在 Task 2/3 定义、Task 6 消费，名称一致。`Session` 的 `beginReplay`/`endReplay`/`dropStale` 在 Task 5 定义、Task 6 消费。`formatRelativeTime` 在 Task 1 定义、Task 7 消费。`statusStale` 在 Task 7 定义并消费。

**已知需在执行时核对的点**（非阻塞）：Task 6 Step 8 的 `/clear` 成功路径行号（`App.tsx:1307` 附近）以实际 `session.reset()` 调用点为准；Task 7 的 Send 按钮无障碍名以现有 `MessageInput` 实现为准（`name: /send/i`）。执行时若选择器不匹配，读取该文件后按实际文案调整选择器即可，不改变断言意图。
