# 手机端本地缓存加固 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让手机端首屏缓存不再自我淘汰（空记录不占 LRU 槽位、内容没变不重写），并在换配对/换电脑后不再显示上一台机器的数据。

**Architecture:** 三处独立改动。(1) `persistContent` 增加「空内容不写」守卫与 `PersistOutcome` 返回值，`flushAll` 用归一化后的内容指纹收窄重写范围；(2) 新增 `lib/cacheDb.ts`（纯常量，打断循环依赖）与 `lib/cacheScope.ts`，用 localStorage 标记键记录「这批缓存属于哪个服务端 url」，失配即清；(3) `writeListCache` 加 1000 条上界。所有新行为都 gate 在 `isRemoteClient()` / `storedRemoteConfig() !== null`，桌面端行为逐字节不变。

**Tech Stack:** TypeScript 6 / React 19 / vitest 3 + jsdom 27 / IndexedDB / localStorage

## Global Constraints

- **桌面端行为逐字节不变**：每个新分支都必须在 `isRemoteClient()` 或 `storedRemoteConfig() !== null` 之后才生效。既有前端单测必须全绿。
  **基线以实测为准**：开工前先跑一次 `cd desktop && npx vitest run` 记下 `Test Files` / `Tests` 两个数。本轮起草时实测是 **139 个文件 / 1820 条**（`docs/project-management/desktop.md` 里那个「1470 / 116」已经过期，`App.test.tsx` 现在是 219 条而文档写 183——Task 9 会一并订正）。
- **不新增任何 npm 依赖**。项目里没有 `fake-indexeddb`，本期也不加。
- **不提升任何存储版本号**：`THREAD_LIST_CACHE_VERSION` 与 `DB_VERSION` 都保持 1。
- **不做 IndexedDB 结构迁移、不改 `DB_NAME` / 键名**。
- **`syncCacheScope()` 是模块初始化期副作用，不得放进任何 React 组件函数体**（`App.tsx:209` 的 `useRef(...)` 实参每次 render 都会求值）。
- **`ensureCacheScopeClean()` 必须直接用 `indexedDB.open`**，不得经 `createIndexedDbStore().db()`（否则 `db → ensureClean → clearCache → db` 递归死锁）。
- **`persistedFp` 只能在写成功之后更新**：成功 → `set`，删除成功 → `delete`，抛错 → 两者都不做。
- **提交前跑 `npx tsc --noEmit`**，无 Rust 改动不跑 cargo。
- **commit message 用 conventional commits，禁止写 `Co-Authored-By`。**
- **jsdom 没有 `indexedDB`**：`createContentStore()` 在单测里恒走 `MemoryContentStore`。真实 IndexedDB 路径靠手动验收，不要在单测里假装覆盖。
- **测试环境造「手机端」只需一个条件**：往 `localStorage` 写 `yi-agent.remote`（`isRemoteClient()` 的唯一判据就是 `storedRemoteConfig() !== null`，不看 UA）。`App.test.tsx` 里就是 `asRemote()` 这一行，既有缓存用例全都只调它。
  **不要**顺带设 `state.platformUa = IPHONE_UA`：那会让 `isIos()` 返回 true，`needsPairing(isIos())` 可能把整个 App 换成配对界面，用例会以「找不到侧栏」这种无关症状失败。

## 文件结构

| 文件 | 职责 | 状态 |
| --- | --- | --- |
| `desktop/src/lib/cacheDb.ts` | IndexedDB 的三个常量（`DB_NAME`/`DB_VERSION`/`STORE_NAME`），无任何 import | 新建 |
| `desktop/src/lib/cacheScope.ts` | 服务端 url 标记键的比对、失配清理、IDB 清理收口 | 新建 |
| `desktop/src/lib/contentStore.ts` | 加 `PersistOptions`/`PersistOutcome`/`normalizeForCache`/`contentFingerprintOf`，改 `persistContent`，`MemoryContentStore` 加计数器 | 修改 |
| `desktop/src/lib/sessionCache.ts` | `writeListCache` 加 1000 条上界与 `truncated` 标记 | 修改 |
| `desktop/src/App.tsx` | `persistedFp`、`flushAll` 指纹过滤、`flushContent` 改造、四条删除路径、重配重置 | 修改 |
| `desktop/src/hooks/useSessionState.ts` | 新增 `pairingVersion` prop + 跳过首帧的重置 effect | 修改 |
| `desktop/src/main.tsx` | 模块顶层调用 `syncCacheScope()` | 修改 |
| `desktop/src/lib/cacheScope.test.ts` | 新模块的单测 | 新建 |
| `desktop/src/lib/contentStore.test.ts` | 空内容守卫、权威删除、指纹同源 | 修改 |
| `desktop/src/lib/sessionCache.test.ts` | 截断行为 | 修改 |
| `desktop/src/App.test.tsx` | 端到端：LRU 不误删、指纹、重配隔离 | 修改 |
| `docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md` | 就地订正四处漂移陈述 | 修改 |
| `docs/project-management/desktop.md` | 更新完成度与测试计数 | 修改 |

---

### Task 1: 抽出 `cacheDb.ts` 常量（打断循环依赖）

`cacheScope.ts` 需要 `DB_NAME`/`DB_VERSION`/`STORE_NAME`，而它们在 `contentStore.ts` 里是模块私有 `const`；`contentStore.ts` 反过来又要 import `cacheScope.ts` 的 `ensureCacheScopeClean`（见 Task 5 Step 4）。互相 import 会成环。先建立无依赖的常量模块。

**Files:**
- Create: `desktop/src/lib/cacheDb.ts`
- Modify: `desktop/src/lib/contentStore.ts:120-122`

**Interfaces:**
- Produces: `DB_NAME: string`、`DB_VERSION: number`、`STORE_NAME: string`（三个 `export const`）

- [ ] **Step 1: 建立常量模块**

Create `desktop/src/lib/cacheDb.ts`:

```ts
/**
 * IndexedDB 的身份常量。
 *
 * 单独成文件是为了打断 `contentStore.ts` ↔ `cacheScope.ts` 的循环依赖：
 * 前者需要后者的 `ensureCacheScopeClean`，后者需要前者的库名/表名。
 * 本文件**不得 import 任何东西**。
 */

/** 库名。与 v1 保持一致，本期不迁移、不改名。 */
export const DB_NAME = "yi-agent-cache";

/** 结构版本。本期所有改动都不提升它。 */
export const DB_VERSION = 1;

/** 内容缓存所在的 object store。 */
export const STORE_NAME = "threadContent";
```

- [ ] **Step 2: 让 `contentStore.ts` 改用它**

删除 `desktop/src/lib/contentStore.ts:120-122` 的三行定义，改为文件顶部 import 并重导出（重导出是为了让既有 `import { DB_NAME } from "./contentStore"` 不被打断——先 grep 确认有没有这样的引用）：

```ts
import { DB_NAME, DB_VERSION, STORE_NAME } from "./cacheDb";
```

在原来定义处（`:120` 附近）替换为：

```ts
// 常量已抽到 cacheDb.ts（打断与 cacheScope.ts 的循环依赖）。这里重导出，
// 保持既有 import 路径可用。
export { DB_NAME, DB_VERSION, STORE_NAME };
```

- [ ] **Step 3: 验证没有残留引用被打破**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出（零错误）

- [ ] **Step 4: 跑既有测试确认零回归**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: 全部 PASS（15 条）

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/cacheDb.ts desktop/src/lib/contentStore.ts
git commit -m "refactor(cache): 抽出 IndexedDB 常量到 cacheDb.ts

cacheScope.ts 需要库名/表名，contentStore.ts 需要 cacheScope 的清理入口，
互相 import 会成环。抽到无依赖的常量模块打断环。"
```

---

### Task 2: `persistContent` 的空内容守卫与 `PersistOutcome`

这是本期核心。现在的 `persistContent` 不跳过空 items（`contentStore.ts:205-223`），于是 `flushAll` 会把侧栏里几十个从未打开过的会话都写成「空记录 + 最新 `savedAt`」，把真正有内容的会话按 LRU 挤掉。

改成返回「到底动了盘上什么」：`written`（带指纹）/ `deleted` / `skipped`。调用方据此决定内存里的 `persistedFp` 该 `set` 还是 `delete`——`boolean` 表达不了这个区别。

**Files:**
- Modify: `desktop/src/lib/contentStore.ts`（`PersistSource` 之后新增类型与两个纯函数；`persistContent` 整体重写）
- Test: `desktop/src/lib/contentStore.test.ts`

**Interfaces:**
- Consumes: `applyTrim`、`MAX_ENTRY_BYTES`、`estimateItemsBytes`、`evict`（均已在 `contentStore.ts` 内）
- Produces:
  - `interface PersistOptions { authoritative?: boolean }`
  - `type PersistOutcome = { action: "written"; fingerprint: string; evicted: string[] } | { action: "deleted" } | { action: "skipped" }`
  - `function normalizeForCache(session: PersistSource): { items: Item[]; trimmed: boolean }`
  - `function contentFingerprintOf(items: Item[], lastServerItemId: string | null, contentEpoch: number | null | undefined): string`
  - `function persistContent(store, threadId, session, options?: PersistOptions): Promise<PersistOutcome>`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/contentStore.test.ts` 的 `describe("persistContent", ...)` 块内追加（保留原有的 "stores only server items" 用例）：

```ts
  it("writes nothing when there is no server content and it is not authoritative", async () => {
    const store = new MemoryContentStore();
    const session = new Session();
    // 只有本地回显与 notice：服务端尚未回帧。
    session.notice("command output");
    session.addUserMessage("hi");
    const outcome = await persistContent(store, "t1", session);
    expect(outcome).toEqual({ action: "skipped" });
    expect(await store.keys()).toEqual([]);
  });

  it("deletes the entry on an authoritative empty result", async () => {
    const store = new MemoryContentStore();
    await store.write({
      threadId: "t1", savedAt: 1, lastServerItemId: "old",
      items: [{ type: "agentMessage", id: "old", text: "stale" }],
      trimmed: false, bytes: 10,
    });
    const session = new Session();          // 全量 resume 后确认为空
    const outcome = await persistContent(store, "t1", session, { authoritative: true });
    expect(outcome).toEqual({ action: "deleted" });
    expect(await store.keys()).toEqual([]);
  });

  it("does not delete when authoritative but only local echoes exist", async () => {
    const store = new MemoryContentStore();
    await store.write({
      threadId: "t1", savedAt: 1, lastServerItemId: "done",
      items: [{ type: "agentMessage", id: "done", text: "keep" }],
      trimmed: false, bytes: 10,
    });
    const session = new Session();
    session.addUserMessage("just sent, server has not echoed yet");
    const outcome = await persistContent(store, "t1", session, { authoritative: true });
    expect(outcome).toEqual({ action: "skipped" });
    expect(await store.keys()).toEqual(["t1"]);   // 好缓存必须留着
  });

  it("returns a fingerprint computed from the trimmed entry, not raw items", async () => {
    const store = new MemoryContentStore();
    const session = new Session();
    // 一条 notice + 三条服务端项：归一化前 4 条，归一化后 3 条。
    session.notice("notice");
    for (const id of ["a", "b", "c"]) {
      session.apply({ method: "item/completed", params: { item: msg(id, `text-${id}`) } } as never);
    }
    const outcome = await persistContent(store, "t1", session);
    expect(outcome.action).toBe("written");
    if (outcome.action !== "written") throw new Error("unreachable");
    const entry = await store.read("t1");
    expect(entry).not.toBeNull();
    // 关键：指纹必须与「从盘上条目反算」逐字相等，否则冷启后永远判「变了」。
    expect(outcome.fingerprint).toBe(
      contentFingerprintOf(entry!.items, entry!.lastServerItemId, entry!.contentEpoch),
    );
    // 且不能等于用原始 items 算出来的那个（那会把 notice 也数进去）。
    expect(outcome.fingerprint).not.toBe(
      contentFingerprintOf(session.items as never, session.lastServerItemId, session.contentEpoch),
    );
  });

  it("reports which threads the write evicted from the LRU", async () => {
    const store = new MemoryContentStore();
    // 先放满上限（CONTENT_LIMITS.maxCount = 10），再写第 11 条。
    // `savedAt` 递增：old0 最旧，必是第一个被淘汰的。
    for (let i = 0; i < 10; i++) {
      await store.write({
        threadId: `old${i}`, savedAt: i + 1, lastServerItemId: null,
        items: [msg(`i${i}`, "x")], trimmed: false, bytes: 1,
      });
    }
    const session = new Session();
    session.apply({ method: "item/completed", params: { item: msg("fresh", "y") } } as never);
    const outcome = await persistContent(store, "fresh", session);
    expect(outcome.action).toBe("written");
    if (outcome.action !== "written") throw new Error("unreachable");
    // 写第 11 条后共 11 条 > 10，必须淘汰 1 条：`old0`（savedAt 最小）。
    // 它要在返回值里报出来——调用方靠它清 persistedFp，否则该会话既已不在
    // 盘上、又会被 `flushAll` 判「没变」而永不重写。
    expect(outcome.evicted).toEqual(["old0"]);
    expect(await store.keys()).not.toContain("old0");
    expect(await store.keys()).toContain("fresh");
  });
```

同时在文件顶部的 import 里加上新符号：

```ts
import {
  applyTrim,
  contentFingerprintOf,
  estimateItemsBytes,
  evict,
  MAX_ENTRY_BYTES,
  MemoryContentStore,
  persistContent,
  selectEvictions,
} from "./contentStore";
```

**不要**把 `normalizeForCache` 一起导入：本文件的用例不直接调用它（指纹同源那条
比较的是 `contentFingerprintOf`），而仓库开了 `noUnusedLocals`，多导一个会让
`tsc --noEmit` 报 `TS6133`。

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: FAIL — `contentFingerprintOf is not a function`（或 `normalizeForCache` 未导出）

- [ ] **Step 3: 实现类型与两个纯函数**

在 `desktop/src/lib/contentStore.ts` 的 `PersistSource` 接口之后插入：

```ts
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
```

- [ ] **Step 4: 让 `evict` 回报它删掉了谁**

`evict`（`contentStore.ts:177-187`）现在返回 `void`，但 `persistContent` 需要知道
「这次写入把谁挤出了 LRU」，才能让调用方同步清掉内存指纹。改它：

```ts
/** Read every entry's meta and delete the ids the LRU policy returns.
 *
 *  返回被删掉的 id：调用方（`persistContent` → `flushContent`）要把它们从
 *  `persistedFp` 里删掉，否则内存会以为那些会话「仍与盘上一致」而永不重写。
 */
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
  return gone;
}
```

既有用例 `evict` 的 "deletes the least recently written entries" 只断言 `keys()`，
不读返回值，故不受影响。

- [ ] **Step 5: 重写 `persistContent`**

把 `desktop/src/lib/contentStore.ts` 的 `persistContent` 整体替换为：

```ts
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
```

- [ ] **Step 6: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: 全部 PASS（20 条 = 原 15 + 新 5）

- [ ] **Step 7: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出。**注意**：此时 `App.tsx` 里 `flushContent` 仍是 `persistContent(...).catch(() => {})`，返回值类型变化尚未被消费，不会报错。

- [ ] **Step 8: Commit**

```bash
git add desktop/src/lib/contentStore.ts desktop/src/lib/contentStore.test.ts
git commit -m "feat(cache): persistContent 空内容不落盘，返回 PersistOutcome

原先不跳空 items，flushAll 会把侧栏几十个从未打开的会话写成空记录，
按 savedAt 把有内容的会话挤出 LRU。新增 authoritative 选项，默认空内容
直接跳过；返回 written/deleted/skipped 三态，供调用方更新 persistedFp。

指纹复用与写入完全相同的归一化流程，保证冷启反算时逐字相等。"
```

---

### Task 3: `MemoryContentStore` 加写/删计数器

Task 5 与 Task 6 要断言「某会话只落盘一次」，需要一个可观测的写次数。计数器**只能加在 `MemoryContentStore` 类上**——`ContentEntry` 被 `toEqual` 逐字段比对，加字段会让既有用例全挂。

**Files:**
- Modify: `desktop/src/lib/contentStore.ts:102-117`（`MemoryContentStore` 类）

**Interfaces:**
- Produces: `MemoryContentStore.writes: number`、`MemoryContentStore.deletes: number`（只读计数，每次 `write`/`delete` 自增）

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/contentStore.test.ts` 的 `describe("MemoryContentStore", ...)` 块内追加：

```ts
  it("counts writes and deletes without changing the stored shape", async () => {
    const store = new MemoryContentStore();
    expect(store.writes).toBe(0);
    expect(store.deletes).toBe(0);
    await store.write({
      threadId: "t1", savedAt: 1, lastServerItemId: null,
      items: [], trimmed: false, bytes: 0,
    });
    expect(store.writes).toBe(1);
    // 计数不写进条目本身：读回来逐字段比对仍与写入值一致。
    expect(await store.read("t1")).toEqual({
      threadId: "t1", savedAt: 1, lastServerItemId: null,
      items: [], trimmed: false, bytes: 0,
    });
    await store.delete("t1");
    expect(store.deletes).toBe(1);
  });
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts -t "counts writes"`
Expected: FAIL — `store.writes` 是 `undefined`

- [ ] **Step 3: 加计数器**

`desktop/src/lib/contentStore.ts` 的 `MemoryContentStore` 类改为：

```ts
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
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts`
Expected: 全部 PASS（21 条 = 原 15 + Task 2 的 5 + 本步 1）

- [ ] **Step 5: Commit**

```bash
git add desktop/src/lib/contentStore.ts desktop/src/lib/contentStore.test.ts
git commit -m "test(cache): MemoryContentStore 加写/删计数器

后续要断言「内容没变时只落盘一次」，需要一个可观测的写次数。计数只加在
测试替身类上，不进 ContentEntry（它被 toEqual 逐字段比对）。"
```

---

### Task 4: `writeListCache` 的 1000 条上界

列表快照目前无任何条数上限。实测 2000 条约 700 KB，1.45 万条才撞 5 MB——所以这不是紧急修复，是「不让载荷取决于服务端返回多少」的防御性上界。

**Files:**
- Modify: `desktop/src/lib/sessionCache.ts:83-102`（`writeListCache`）
- Test: `desktop/src/lib/sessionCache.test.ts`

**Interfaces:**
- Produces: `LIST_CACHE_MAX_THREADS: number`（= 1000）
- 变更：`writeListCache(storage, groups, pinned, selectedId)` 签名不变，内部截断

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/sessionCache.test.ts` 的 `describe("sessionCache", ...)` 内追加：

```ts
  it("caps non-pinned threads at 1000 and marks the payload truncated", () => {
    const s = memStorage();
    const threads = Array.from({ length: 1001 }, (_, i) => ({
      ...thread(`t${i}`),
      updated_at: i,          // 越靠后越新
    }));
    writeListCache(s, [{ workspace: "/w", exists: true, threads }], [], null);
    const raw = JSON.parse(s.map.get(THREAD_LIST_CACHE_KEY)!);
    const kept = raw.groups.flatMap((g: WorkspaceGroup) => g.threads);
    expect(kept).toHaveLength(1000);
    expect(kept.map((t: ThreadSummary) => t.thread_id)).not.toContain("t0"); // 最旧的被砍
    expect(raw.truncated).toBe(true);
  });

  it("omits the truncated flag when under the cap", () => {
    const s = memStorage();
    writeListCache(s, [{ workspace: "/w", exists: true, threads: [thread("t1")] }], [], null);
    const raw = JSON.parse(s.map.get(THREAD_LIST_CACHE_KEY)!);
    expect(raw.truncated).toBeUndefined();
  });

  it("keeps every pinned thread and does not count them against the cap", () => {
    const s = memStorage();
    // 置顶项留在各自分组里（带 pinned:true），同时出现在顶层 pinned 数组。
    const pinnedThreads = Array.from({ length: 5 }, (_, i) => ({
      ...thread(`p${i}`), pinned: true, updated_at: -i,
    }));
    const normalThreads = Array.from({ length: 1000 }, (_, i) => ({
      ...thread(`n${i}`), updated_at: i,
    }));
    writeListCache(
      s,
      [{ workspace: "/w", exists: true, threads: [...pinnedThreads, ...normalThreads] }],
      pinnedThreads,
      null,
    );
    const raw = JSON.parse(s.map.get(THREAD_LIST_CACHE_KEY)!);
    const ids = raw.groups.flatMap((g: WorkspaceGroup) => g.threads).map((t: ThreadSummary) => t.thread_id);
    // 1000 条非置顶全部保住（限额只数非置顶）+ 5 条置顶
    expect(ids).toHaveLength(1005);
    for (const p of pinnedThreads) expect(ids).toContain(p.thread_id);
  });

  it("drops groups whose members were all truncated away", () => {
    const s = memStorage();
    // A 组全是新的，B 组全是老的：B 组整组被截空，应被丢弃而不是留个空表头。
    const A = Array.from({ length: 1000 }, (_, i) => ({ ...thread(`a${i}`), updated_at: 10000 + i }));
    const B = [{ ...thread("b0"), updated_at: 1 }];
    writeListCache(
      s,
      [
        { workspace: "/a", exists: true, threads: A },
        { workspace: "/b", exists: true, threads: B },
      ],
      [],
      null,
    );
    const raw = JSON.parse(s.map.get(THREAD_LIST_CACHE_KEY)!);
    expect(raw.groups.map((g: WorkspaceGroup) => g.workspace)).toEqual(["/a"]);
  });

  it("keeps the newest threads across groups, not the first group's", () => {
    const s = memStorage();
    const A = Array.from({ length: 600 }, (_, i) => ({ ...thread(`a${i}`), updated_at: 20000 + i }));
    const B = Array.from({ length: 600 }, (_, i) => ({ ...thread(`b${i}`), updated_at: 10000 + i }));
    writeListCache(
      s,
      [
        { workspace: "/a", exists: true, threads: A },
        { workspace: "/b", exists: true, threads: B },
      ],
      [],
      null,
    );
    const raw = JSON.parse(s.map.get(THREAD_LIST_CACHE_KEY)!);
    const byWs = (ws: string) =>
      (raw.groups.find((g: WorkspaceGroup) => g.workspace === ws)?.threads ?? []).length;
    expect(byWs("/a")).toBe(600);   // 新的全留
    expect(byWs("/b")).toBe(400);   // 老的只留 400
  });
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/sessionCache.test.ts`
Expected: FAIL — 1001 条时 `kept` 长度为 1001，`raw.truncated` 为 `undefined`

- [ ] **Step 3: 实现截断**

在 `desktop/src/lib/sessionCache.ts` 的 `THREAD_LIST_CACHE_VERSION` 之后加常量：

```ts
/**
 * 非置顶会话最多保留多少条。
 *
 * 实测（`node` + 真实 `JSON.stringify`）：1000 条约 248–352 KB，取决于
 * `cwd` 与 `title` 长度；最坏形状要约 1.45 万条才撞 5 MB 配额，故选 1000
 * 有两位数余量。这是**防御性上界**，不是配额问题的修复。
 */
export const LIST_CACHE_MAX_THREADS = 1000;
```

把 `writeListCache` 替换为：

```ts
export function writeListCache(
  storage: StorageLike | undefined,
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  selectedId: string | null,
): void {
  if (!storage) return;
  const { groups: cappedGroups, truncated } = capListCache(groups, pinned);
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
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/lib/sessionCache.test.ts`
Expected: 全部 PASS（14 条 = 原 9 + 新 5）

- [ ] **Step 5: 验证读路径不受影响**

`parseListCache` 显式重建返回对象，所以 `truncated` 不会出现在读取结果里——这是刻意的（它只是诊断标记）。跑一遍确认：

Run: `cd desktop && npx vitest run src/lib/sessionCache.test.ts`
Expected: 既有「round-trips groups, pinned and selectedId」等用例仍 PASS

- [ ] **Step 6: Commit**

```bash
git add desktop/src/lib/sessionCache.ts desktop/src/lib/sessionCache.test.ts
git commit -m "feat(cache): 列表快照加 1000 条上界

原先无条数上限，载荷大小取决于服务端返回多少会话。按全局 updated_at
降序保留前 1000 条非置顶会话（不能按 groups 遍历顺序砍，否则排在后面的
workspace 会整块消失），装回原骨架并丢弃被截空的组。置顶不占限额，但其
id 计入保留集合，否则「组内全是置顶」的分组会凭空消失。

版本号不变：只多了一个可选的 truncated 字段，v1 读取路径不受影响。"
```

---

### Task 5: `cacheScope.ts` 的核心逻辑

新增模块，用 localStorage 标记键记住「这批缓存属于哪个服务端 url」，失配即清。

**Files:**
- Create: `desktop/src/lib/cacheScope.ts`
- Test: `desktop/src/lib/cacheScope.test.ts`

**Interfaces:**
- Consumes: `DB_NAME`/`DB_VERSION`/`STORE_NAME`（Task 1）、`StorageLike`/`storedRemoteConfig`（`remoteConfig.ts`）、`THREAD_LIST_CACHE_KEY`（`sessionCache.ts`）、`SELECTED_THREAD_KEY`（`selectedThread.ts`）
- Produces:
  - `CACHE_SCOPE_KEY: string`
  - `syncCacheScope(storage?: StorageLike): void`
  - `resetCacheScope(storage?: StorageLike): Promise<void>`
  - `clearContentCache(): Promise<void>`
  - `ensureCacheScopeClean(): Promise<void>`
  - 测试注入点：`__setClearThreadContent(fn: () => Promise<void>): void`

- [ ] **Step 1: 写失败的测试**

Create `desktop/src/lib/cacheScope.test.ts`:

```ts
import { beforeEach, describe, expect, it } from "vitest";
import type { StorageLike } from "./remoteConfig";
import { THREAD_LIST_CACHE_KEY } from "./sessionCache";
import { SELECTED_THREAD_KEY } from "./selectedThread";
import {
  CACHE_SCOPE_KEY,
  __setClearThreadContent,
  ensureCacheScopeClean,
  resetCacheScope,
  syncCacheScope,
} from "./cacheScope";

function memStorage(): StorageLike & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k) => map.get(k) ?? null,
    setItem: (k, v) => void map.set(k, v),
    removeItem: (k) => void map.delete(k),
  };
}

const bind = (s: StorageLike, url: string) =>
  s.setItem("yi-agent.remote", JSON.stringify({ url, token: "yia_tok" }));

describe("cacheScope", () => {
  let clears: number;

  beforeEach(() => {
    clears = 0;
    __setClearThreadContent(async () => void (clears += 1));
  });

  it("does nothing on the desktop build (no relay binding)", async () => {
    const s = memStorage();
    s.setItem(THREAD_LIST_CACHE_KEY, "keep-me");
    s.setItem(SELECTED_THREAD_KEY, "t1");
    syncCacheScope(s);
    expect(s.getItem(THREAD_LIST_CACHE_KEY)).toBe("keep-me");
    expect(s.getItem(SELECTED_THREAD_KEY)).toBe("t1");
    expect(s.getItem(CACHE_SCOPE_KEY)).toBeNull();
    await ensureCacheScopeClean();
    expect(clears).toBe(0);
  });

  it("leaves caches alone when the scope matches", async () => {
    const s = memStorage();
    bind(s, "wss://relay.test/ws");
    // 首次（无 scope 键）按「失配」处理：置脏标记。必须先把这次清理走完，
    // 否则脏标记会留到下一次 ensureCacheScopeClean 才消费，本用例会被
    // 它数出 1 次而误判。
    syncCacheScope(s);
    await ensureCacheScopeClean();

    s.setItem(THREAD_LIST_CACHE_KEY, "list");
    clears = 0;
    syncCacheScope(s);                       // 第二次：相等，什么都不做
    expect(s.getItem(THREAD_LIST_CACHE_KEY)).toBe("list");
    await ensureCacheScopeClean();
    expect(clears).toBe(0);                  // 无脏标记 → 一次都不清
  });

  it("clears both localStorage keys and marks IDB dirty when the url changes", async () => {
    const s = memStorage();
    bind(s, "wss://relay-a.test/ws");
    syncCacheScope(s);
    s.setItem(THREAD_LIST_CACHE_KEY, "list");
    s.setItem(SELECTED_THREAD_KEY, "t1");

    bind(s, "wss://relay-b.test/ws");
    syncCacheScope(s);

    expect(s.getItem(THREAD_LIST_CACHE_KEY)).toBeNull();
    expect(s.getItem(SELECTED_THREAD_KEY)).toBeNull();
    expect(JSON.parse(s.getItem(CACHE_SCOPE_KEY)!)).toBe("wss://relay-b.test/ws");

    await ensureCacheScopeClean();
    expect(clears).toBe(1);
  });

  it("clears on first upgrade (no scope key yet)", () => {
    const s = memStorage();
    bind(s, "wss://relay.test/ws");
    s.setItem(THREAD_LIST_CACHE_KEY, "list");
    syncCacheScope(s);
    expect(s.getItem(THREAD_LIST_CACHE_KEY)).toBeNull();
  });

  it("treats a throwing getItem as 'not a remote client'", () => {
    const s: StorageLike = {
      getItem: () => { throw new Error("blocked"); },
      setItem: () => {},
      removeItem: () => {},
    };
    expect(() => syncCacheScope(s)).not.toThrow();
  });

  it("runs the IDB clear at most once across repeated calls", async () => {
    const s = memStorage();
    bind(s, "wss://relay.test/ws");
    syncCacheScope(s);                       // 标记待清
    await ensureCacheScopeClean();
    await ensureCacheScopeClean();
    await ensureCacheScopeClean();
    expect(clears).toBe(1);
  });

  it("does not clear on re-pair to the same url", async () => {
    const s = memStorage();
    bind(s, "wss://relay.test/ws");
    syncCacheScope(s);
    s.setItem(THREAD_LIST_CACHE_KEY, "list");
    clears = 0;
    await resetCacheScope(s);
    expect(s.getItem(THREAD_LIST_CACHE_KEY)).toBe("list");
    expect(clears).toBe(0);
  });

  it("clears both stores on re-pair to a different url", async () => {
    const s = memStorage();
    bind(s, "wss://relay-a.test/ws");
    syncCacheScope(s);
    s.setItem(THREAD_LIST_CACHE_KEY, "list");
    s.setItem(SELECTED_THREAD_KEY, "t1");
    bind(s, "wss://relay-b.test/ws");

    await resetCacheScope(s);

    expect(s.getItem(THREAD_LIST_CACHE_KEY)).toBeNull();
    expect(s.getItem(SELECTED_THREAD_KEY)).toBeNull();
    expect(clears).toBe(1);
    expect(JSON.parse(s.getItem(CACHE_SCOPE_KEY)!)).toBe("wss://relay-b.test/ws");
  });

  it("clears again when the url changes a second time in the same session", async () => {
    const s = memStorage();
    bind(s, "wss://relay-a.test/ws");
    syncCacheScope(s);
    await ensureCacheScopeClean();
    expect(clears).toBe(1);

    bind(s, "wss://relay-b.test/ws");
    syncCacheScope(s);                       // 又一次失配
    await ensureCacheScopeClean();
    // `scopeClean` 是一次性记忆：若 `syncCacheScope` 置脏时不作废它，第二次
    // 失配就完全不清理，而且不报错——「同一会话里连换两台电脑」会串数据。
    expect(clears).toBe(2);
  });

  it("rolls the scope key back to absent when the IDB clear fails", async () => {
    const s = memStorage();
    bind(s, "wss://relay-a.test/ws");
    syncCacheScope(s);
    bind(s, "wss://relay-b.test/ws");
    __setClearThreadContent(async () => { throw new Error("quota"); });

    await expect(resetCacheScope(s)).rejects.toThrow("quota");
    // scope 键必须仍是旧的，让下次启动重新走失配清理。
    expect(JSON.parse(s.getItem(CACHE_SCOPE_KEY)!)).toBe("wss://relay-a.test/ws");
  });
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/cacheScope.test.ts`
Expected: FAIL — `Cannot find module './cacheScope'`

- [ ] **Step 3: 实现 `cacheScope.ts`**

Create `desktop/src/lib/cacheScope.ts`:

```ts
/**
 * 手机端缓存的「服务端身份」隔离。
 *
 * 三个存储键（`yi-agent.threadListCache` / `yi-agent-cache` /
 * `yi-agent.selectedThread`）都是全局固定名，没有服务端标识。手机从 A 电脑
 * 改连 B 电脑后，首帧会画出 A 的列表与对话。
 *
 * 修法不是把 url 编进键名（那要运行期动态化库名、迁移旧库、处理孤儿库），
 * 而是一个**标记键**：记住这批缓存属于哪个 url，启动早期比对，失配即清。
 */
import { DB_NAME, DB_VERSION, STORE_NAME } from "./cacheDb";
import { storedRemoteConfig, type StorageLike } from "./remoteConfig";
import { SELECTED_THREAD_KEY } from "./selectedThread";
import { THREAD_LIST_CACHE_KEY } from "./sessionCache";

/** 标记键：值是 JSON 字符串形式的服务端 url。 */
export const CACHE_SCOPE_KEY = "yi-agent.cacheScope";

/** 「IDB 待清」标记。模块级，跨 store 实例共享。 */
let pendingContentClear = false;

/** 清理 Promise 的一次性记忆，跨 store 实例共享。 */
let scopeClean: Promise<void> | null = null;

/**
 * 真正的 IndexedDB 清理。**必须直接用 `indexedDB.open`**，不能经
 * `createIndexedDbStore().db()`——后者会 await `ensureCacheScopeClean()`，
 * 形成 `db → ensureClean → clearCache → db` 的自我等待（永久挂起，表现为
 * 「内容缓存彻底不工作且不报错」）。
 *
 * 测试通过 `__setClearThreadContent` 替换它：jsdom 没有 indexedDB。
 */
let clearThreadContent: () => Promise<void> = async () => {
  if (typeof indexedDB === "undefined") return;
  await new Promise<void>((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE_NAME)) {
        db.createObjectStore(STORE_NAME, { keyPath: "threadId" });
      }
    };
    req.onerror = () => reject(req.error);
    req.onsuccess = () => {
      const db = req.result;
      try {
        const tx = db.transaction(STORE_NAME, "readwrite");
        tx.objectStore(STORE_NAME).clear();
        tx.oncomplete = () => { db.close(); resolve(); };
        tx.onerror = () => { db.close(); reject(tx.error); };
      } catch (err) {
        db.close();
        reject(err);
      }
    };
  });
};

/** 测试注入点：替换真实清理副作用。同时重置模块级状态，保证用例间隔离。 */
export function __setClearThreadContent(fn: () => Promise<void>): void {
  clearThreadContent = fn;
  pendingContentClear = false;
  scopeClean = null;
}

function defaultStorage(): StorageLike | undefined {
  return globalThis.localStorage as StorageLike | undefined;
}

/** 读标记键；读不到或读抛错都返回 null（= 未知）。 */
function readScope(storage: StorageLike): string | null {
  let raw: string | null;
  try {
    raw = storage.getItem(CACHE_SCOPE_KEY);
  } catch {
    return null;
  }
  if (!raw) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    return typeof parsed === "string" && parsed.length > 0 ? parsed : null;
  } catch {
    return null;
  }
}

/**
 * 同步段：比对 scope，失配就清 localStorage 的两个键并设「IDB 待清」。
 *
 * **必须在 `main.tsx` 的 `createRoot().render()` 之前调用一次。**
 */
export function syncCacheScope(storage: StorageLike | undefined = defaultStorage()): void {
  if (!storage) return;
  const config = storedRemoteConfig(storage);   // 内部吞掉 getItem 抛错
  if (!config) return;                          // 桌面端：不读不写不清

  const current = config.url;
  if (readScope(storage) === current) return;   // 同一个服务端，什么都不做

  // 先设标记、再写 scope 键。两者都在同一个同步块里，但顺序仍要对：
  // 否则「写了键却漏了标记」。localStorage 侧的串数据在这一步就闭合了。
  pendingContentClear = true;
  // **必须同时作废已缓存的清理 Promise**：`scopeClean` 是一次性记忆，上一次
  // 失配清完后它是已 resolve 的值。不置空的话，本次新置的脏标记会一直没人消费，
  // 「同一会话里第二次换电脑」就完全不清理，且不报错。
  scopeClean = null;
  try {
    storage.removeItem(THREAD_LIST_CACHE_KEY);
    storage.removeItem(SELECTED_THREAD_KEY);
    storage.setItem(CACHE_SCOPE_KEY, JSON.stringify(current));
  } catch {
    // 私密模式等：清不掉就只能退化。下次启动会重试（scope 键没写成）。
    return;
  }
}

/**
 * 一次性清理收口。由 `createIndexedDbStore` 的 `db()` 在每个事务前 await，
 * 保证清理先于任何读写。
 *
 * **失败时必须把 scope 键回滚为「未知」**：启动路径是先写 scope 键、IDB 稍后
 * 清；若清理失败而键已写入新 url，下次启动会判「未失配」而保留旧服务器的内容
 * ——串数据重新出现且再也不会被自动纠正。
 */
export function ensureCacheScopeClean(storage: StorageLike | undefined = defaultStorage()): Promise<void> {
  return (scopeClean ??= (async () => {
    if (!pendingContentClear) return;    // 无脏标记：不碰 IDB
    pendingContentClear = false;
    try {
      await clearThreadContent();
    } catch (err) {
      if (storage) {
        try {
          storage.removeItem(CACHE_SCOPE_KEY);
        } catch {
          /* 连回滚都失败：只能寄望下次启动 */
        }
      }
      throw err;
    }
  })());
}

/**
 * 清空内容缓存。**幂等，但不吞错**——失败照常 reject，由调用方决定是否上报。
 * 不走 `ensureCacheScopeClean` 的标记，因为它需要「本会话立即清」而不是
 * 「下次 db() 时清」。
 */
export async function clearContentCache(): Promise<void> {
  await clearThreadContent();
}

/**
 * 配对成功后调用：重新比对一次 scope，**仅在 url 变化时才清**（重配到同一台
 * 电脑不该丢缓存）。
 *
 * 顺序不可颠倒：**先清 IDB，成功后**才写 scope 键。反过来的话，一旦清理抛错，
 * scope 键已是新 url，下次启动判「未失配」而保留旧内容——串数据永不自愈。
 *
 * 调用方（`onPaired` 是同步回调）必须自己接住 rejection，见 `App.tsx`。
 */
export async function resetCacheScope(
  storage: StorageLike | undefined = defaultStorage(),
): Promise<void> {
  if (!storage) return;
  const config = storedRemoteConfig(storage);
  if (!config) return;

  const current = config.url;
  if (readScope(storage) === current) return;   // 同一 url：保留缓存

  // 同步段：先清 localStorage，让本次会话的首帧读不到旧快照。
  const previous = readScope(storage);
  try {
    storage.removeItem(THREAD_LIST_CACHE_KEY);
    storage.removeItem(SELECTED_THREAD_KEY);
  } catch {
    return;
  }

  // 异步段：清 IDB，成功后才写 scope 键。
  try {
    await clearThreadContent();
  } catch (err) {
    // 回滚到「未知」，让下次启动重新走失配清理。
    try {
      if (previous) storage.setItem(CACHE_SCOPE_KEY, JSON.stringify(previous));
      else storage.removeItem(CACHE_SCOPE_KEY);
    } catch {
      /* 回滚失败只能寄望下次启动 */
    }
    throw err;
  }

  try {
    storage.setItem(CACHE_SCOPE_KEY, JSON.stringify(current));
  } catch {
    /* 写不进去：下次启动会判失配并再清一次，幂等 */
  }
  // 允许后续 db() 继续用（清理已完成）。
  pendingContentClear = false;
  scopeClean = Promise.resolve();
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/lib/cacheScope.test.ts`
Expected: 全部 PASS（10 条）

- [ ] **Step 5: 把 `ensureCacheScopeClean()` 接进 `createIndexedDbStore` 的 `db()`**

**这一步不能省。** 少了它，`syncCacheScope()` 只是在模块里置一个 `pendingContentClear`
标志，没有任何人消费它——IndexedDB 里的旧对话**永远不会被清**，「换电脑后不串数据」
的核心场景直接失效，而且不报错、单测也发现不了（jsdom 没有 indexedDB）。

改 `desktop/src/lib/contentStore.ts:141`（`const db = (): Promise<IDBDatabase> => (dbPromise ??= open());`）：

```ts
  // 打开库之前先等「失配清理」完成：所有读写都经 run() → db()，故这是唯一
  // 收口点。用 `??=` 一次写完，避免「清理完成」与「开始建库」之间的空档被
  // 另一个调用插进来，各自建一个库句柄。
  //
  // 不能写成 `const d = await ensureCacheScopeClean(); dbPromise ??= open()`：
  // 那样两个并发调用都会走到 open()。
  const db = (): Promise<IDBDatabase> => (dbPromise ??= ensureCacheScopeClean().then(open));
```

文件顶部补 import：

```ts
import { ensureCacheScopeClean } from "./cacheScope";
```

**循环依赖的检查点**：此时 `contentStore.ts` → `cacheScope.ts` → `cacheDb.ts`，
而 `cacheScope.ts` **不** import `contentStore.ts`（它直接用 `indexedDB.open`）。
Task 1 抽常量就是为了这一点，不要在这一步退回 `contentStore` 里取常量。

- [ ] **Step 6: 验证接线没把既有内容缓存测试打挂**

Run: `cd desktop && npx vitest run src/lib/contentStore.test.ts src/lib/cacheScope.test.ts`
Expected: 全部 PASS。`contentStore.test.ts` 走 `MemoryContentStore`，根本不调
`createIndexedDbStore`，所以不会真的碰 IndexedDB；`cacheScope.test.ts` 已被
`__setClearThreadContent` 摘掉真实副作用。这条只验证「新增 import 没造成环」。

- [ ] **Step 7: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出

- [ ] **Step 8: Commit**

```bash
git add desktop/src/lib/cacheScope.ts desktop/src/lib/cacheScope.test.ts desktop/src/lib/contentStore.ts
git commit -m "feat(cache): 新增 cacheScope 做服务端身份隔离

三个存储键都是全局固定名，换配对后会显示上一台电脑的列表与对话。
用一个标记键记住「这批缓存属于哪个 url」，启动期失配即清；配对成功后
再比对一次，本次会话内即生效（重配到同一 url 不丢缓存）。

ensureCacheScopeClean 直接用 indexedDB.open 拿库，不经 db()——否则
db → ensureClean → clearCache → db 会自我等待并永久挂起。

清理失败时回滚 scope 键，否则下次启动会信任错误的键而永久串数据。

ensureCacheScopeClean 接进 createIndexedDbStore 的 db()：
(dbPromise ??= ensureCacheScopeClean().then(open))。这是唯一收口点，漏掉
这一步则 pendingContentClear 无人消费，IndexedDB 旧对话永远不会被清。"
```

---

### Task 6: `App.tsx` 接入 `persistedFp` 与指纹收窄

把 Task 2 的新返回值和 Task 5 的 scope 接进 `App.tsx`。

**Files:**
- Modify: `desktop/src/App.tsx`（`flushContent`、`flushAll`、冷启播种、四条删除路径）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `persistContent` → `PersistOutcome`（Task 2）、`normalizeForCache`/`contentFingerprintOf`（Task 2）、`MemoryContentStore.writes`（Task 3）
- Produces: 无对外接口，纯内部接线

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/App.test.tsx` 的 `describe("App mobile local cache", ...)` 块内追加。

先在该 `describe` 里加一个**会自己复位**的 helper（`visibilityState` 是 `document` 上的
全局属性，改了不复位会污染本文件后面所有用例——`App.tsx:646` 的 `flushAll` 与
`:1376` 的前台重连都读它）：

```ts
  /**
   * 派发一次「切到后台」。**必须在函数内复位**：jsdom 的 `document` 是整个文件
   * 共用的，`visibilityState` 被改成 `hidden` 后若不还原，后面的用例（前台重连、
   * 补齐尾部）会按「仍在后台」走，失败原因与缓存毫无关系，极难归因。
   */
  const hidePage = () => {
    Object.defineProperty(document, "visibilityState", { value: "hidden", configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));
    Object.defineProperty(document, "visibilityState", { value: "visible", configurable: true });
  };

  it("keeps a real thread's content when many empty threads are flushed", async () => {
    asRemote();
    // 12 个空会话 + 1 个有内容的会话，塞满 store。
    const ids = Array.from({ length: 12 }, (_, i) => `empty${i}`);
    state.threads = [
      ...ids.map((id) => ({ thread_id: id, title: id, permission_mode: "normal" as const })),
      { thread_id: "real", title: "real", permission_mode: "normal" as const },
    ];
    seedListCache(state.threads.map((t) => ({ thread_id: t.thread_id, title: t.title })), null);
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "real", savedAt: 1, lastServerItemId: "A",
      items: [{ type: "agentMessage", id: "A", text: "precious" }],
      trimmed: false, bytes: 10,
    });
    state.dataSources["thread/listAll"] = () => ({
      groups: [{
        workspace: "/w", exists: true,
        threads: state.threads.map((t) => ({
          thread_id: t.thread_id, cwd: "/w", model: "m", created_at: 0, updated_at: 0,
          title: t.title, permission_mode: "normal", status: "idle",
        })),
      }],
      pinned: [],
    });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/listAll")).toBe(true),
    );
    // 触发页面隐藏 → flushAll
    act(() => hidePage());
    await waitFor(async () => {
      const keys = await contentStore.current!.keys();
      expect(keys).toContain("real");
      // 空会话一个都不该落盘（无论 12 个都写还是只写部分，只要不挤掉 real）
      expect(keys.length).toBe(1);
    });
  });

  it("writes a session with unchanged content only once across two flushes", async () => {
    asRemote();
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" as const }];
    seedListCache([{ thread_id: "t1", title: "one" }], "t1");
    contentStore.current = new MemoryContentStore();
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());

    // **必须先有真实内容**：空会话的 `persistContent` 走 `skipped`（Task 2），
    // 一次 `write()` 都不会发生，`writes` 恒为 0，这条用例会以「一次都没写」假绿。
    // 用一条 `item/completed` 通知把内容放进会话。
    act(() => {
      for (const h of state.notifHandlers) {
        h({
          method: "item/completed",
          params: { thread_id: "t1", item: { type: "agentMessage", id: "s1", text: "hello" } },
        });
      }
    });
    await waitFor(() => expect(screen.getByText("hello")).toBeTruthy());

    const before = contentStore.current!.writes;
    act(() => hidePage());
    await waitFor(() => expect(contentStore.current!.writes).toBeGreaterThan(before));
    const afterFirst = contentStore.current!.writes;
    act(() => hidePage());   // 内容没变：不应再写
    await new Promise((r) => setTimeout(r, 20));
    expect(contentStore.current!.writes).toBe(afterFirst);
  });

  it("flushes a cold thread whose content arrived only via resume replay", async () => {
    asRemote();
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" as const }];
    seedListCache([{ thread_id: "t1", title: "one" }], null);
    contentStore.current = new MemoryContentStore();
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    // 冷会话走 `thread/resume`（plan 缺失 ⇒ full）。测试替身的 resume **不投递
    // 任何回放帧**（见本文件既有用例的注释），所以内容必须由测试自己派发一条
    // `item/completed` 送进来。resume 成功后的 `flushContent`（App.tsx:913）是
    // 直接调用的，**不经** `schedulePersist` 的防抖——本用例钉的就是这条路径。
    act(() => {
      for (const h of state.notifHandlers) {
        h({
          method: "item/completed",
          params: { thread_id: "t1", item: { type: "agentMessage", id: "A", text: "server text" } },
        });
      }
    });
    await waitFor(() => expect(screen.getByText("server text")).toBeTruthy());
    act(() => hidePage());
    await waitFor(async () => {
      const entry = await contentStore.current!.read("t1");
      expect(entry?.items.map((i) => i.id)).toEqual(["A"]);
    });
  });
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "keeps a real thread"`
Expected: FAIL — 空会话把 `real` 挤掉（`keys` 不含 `real`）

- [ ] **Step 3: 加 `persistedFp` ref 与改造 `flushContent`**

在 `desktop/src/App.tsx` 里 `cachedAt` 声明附近（`cacheMetaRef` 之后）加：

```ts
  /**
   * 「盘上是什么」的内存镜像：threadId → 内容指纹。
   *
   * 与 `cacheMetaRef`（盘上条目的元数据，供 hints 用）职责不同，不要合并。
   * **只能在写成功之后更新**：先记指纹再写，一次 IndexedDB 失败就会把该会话
   * 永久标记为「已落盘」，此后 flushAll 都判「没变」而不再重试——静默永久丢缓存。
   */
  const persistedFp = useRef(new Map<string, string>());
```

把 `flushContent`（`App.tsx:349-356` 附近）替换为：

```ts
  /**
   * 落盘一条会话，并维护 `persistedFp`。
   *
   * 返回「动了盘上什么」，而不是 boolean：`written` 与 `deleted` 都算「没失败」，
   * 但 `persistedFp` 要做相反的操作（set / delete）。`skipped` 表示盘上没动过。
   * 抛错或前置条件不满足 → 返回 null，表示什么都没记，下次自然重试。
   */
  const flushContent = async (
    id: string,
    options?: PersistOptions,
  ): Promise<PersistOutcome | null> => {
    if (!isRemoteClient()) return null;       // 桌面端：本就没有缓存
    const v = store.peek(id);
    if (!v) return null;                      // 会话已不存在（如刚被 drop）
    try {
      const outcome = await persistContent(getContentStore(), id, v.session, options);
      // 只有 persistContent 成功返回才更新「盘上是什么」；顺序不可颠倒。
      if (outcome.action === "deleted") {
        persistedFp.current.delete(id);
      } else if (outcome.action === "written") {
        persistedFp.current.set(id, outcome.fingerprint);
        // LRU 被淘汰的会话已从盘上消失，指纹必须跟着删。留着的话，`flushAll`
        // 会判「它与盘上一致、没变过」而跳过，它再也写不回来。
        for (const gone of outcome.evicted) persistedFp.current.delete(gone);
      }
      // skipped：盘上没动过，persistedFp 保持原值。
      return outcome;
    } catch {
      return null;                            // 写失败：不抛给调用方，也不记指纹
    }
  };
```

在 `App.tsx` 顶部 import 补上：

```ts
import {
  contentFingerprintOf,
  createContentStore,
  normalizeForCache,
  persistContent,
  type PersistOptions,
  type PersistOutcome,
} from "./lib/contentStore";
```

（保留原有的 `createContentStore`、`persistContent` 等，只补新的。若原 import 是分开的多行，合并进同一个 `import { ... } from "./lib/contentStore"`。）

- [ ] **Step 4: 改造 `flushAll` 用指纹收窄**

把 `flushAll`（`App.tsx:637-644`）内的候选集替换：

```ts
    const flushAll = () => {
      for (const id of persistTimers.current.keys()) {
        window.clearTimeout(persistTimers.current.get(id));
      }
      // 候选 = 脏集 ∪ 指纹变化的会话。
      //
      // 不能只刷脏集：内容进入会话有三条路径，只有 schedulePersist 会注册防抖。
      // thread/readItems 的两个分支（App.tsx:817 / :865）直接 upsertItems，
      // prefillFromCache（:276）同理——只看 persistTimers 会让这些会话丢掉
      // 首次落盘机会。
      //
      // 也不能用「有没有内容」当判据：它不区分「内容变了」与「内容一直在」，
      // 每次切后台都会把所有有内容的会话重写一遍、savedAt 一起抬到当下，
      // 写序被抹平成迭代顺序，LRU 仍然失效。
      const dirty = persistTimers.current.keys();
      const changed = store.ids().filter((id) => {
        const sess = store.peek(id)?.session;
        if (!sess) return false;
        const { items } = normalizeForCache(sess);
        const fp = contentFingerprintOf(items, sess.lastServerItemId, sess.contentEpoch);
        return fp !== persistedFp.current.get(id);
      });
      const ids = new Set([...dirty, ...changed]);
      persistTimers.current.clear();
      for (const id of ids) void flushContent(id);
    };
```

- [ ] **Step 5: 冷启用盘上条目播种 `persistedFp`**

把冷启 `keys()` 扫描（`App.tsx:305-318`）改为：

```ts
  useEffect(() => {
    if (!isRemoteClient()) return;
    const cs = getContentStore();
    void (async () => {
      try {
        for (const id of await cs.keys()) {
          const entry = await cs.read(id);
          if (entry) {
            cacheMetaRef.current.set(entry.threadId, entry);
            // 用盘上条目反算指纹，而不是留空。留空的话冷启后第一次 flushAll
            // 会把每个有缓存的会话都重写一遍、savedAt 全部抬到当下，又一次
            // 抹平 LRU。entry.items 已是归一化后的写入结果，与写入时记下的
            // 指纹天然同源。
            persistedFp.current.set(
              entry.threadId,
              contentFingerprintOf(entry.items, entry.lastServerItemId, entry.contentEpoch),
            );
          }
        }
      } catch {
        /* hints 是优化：读失败就按"没有缓存"声明 */
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
```

- [ ] **Step 6: 两处不经 `flushContent` 的删除路径补 `persistedFp.delete`**

**路径一**：`deleteCachedContent`（`App.tsx:358` 附近）：

```ts
  const deleteCachedContent = (id: string) => {
    if (!isRemoteClient()) return;
    cacheMetaRef.current.delete(id);
    cachedAt.current.delete(id);
    persistedFp.current.delete(id);      // 新增：否则指纹会永久偏旧
    void getContentStore().delete(id).catch(() => {});
  };
```

**路径二**：`listAll` 报 `drop` 的分支（`App.tsx:744-750`）：

```ts
      if (action === "drop") {
        store.drop(id);
        cacheMetaRef.current.delete(id);
        void getContentStore().delete(id);
        cachedAt.current.delete(id);
        persistedFp.current.delete(id);   // 新增：store.peek 已为 undefined，
                                          // flushContent 会提前返回，不会自己清
      }
```

- [ ] **Step 7: `:913` 传 authoritative（复用既有的 dropStale 守卫条件）**

在 `selectThread` 的全量 resume 成功分支（`App.tsx:904-913`）改：

```ts
      // 只有在真的收到过回放帧并做过 dropStale 时才「权威」：replayed 为空意味着
      // 这趟 resume 压根没投递帧，此时 items 仍可能是刚预填的缓存内容，
      // 不能据此判定「服务端确认无内容」而删盘上条目。
      const authoritative = replayed !== null && replayed.size > 0;
      if (authoritative) replaySession.dropStale(replayed);
      // ……（保持既有的 peek / warm / syncState / cachedAt 处理不变）……
      void flushContent(id, { authoritative });
```

**不要**把 `flushContent` 挪进 `if (authoritative)` 里：`authoritative: false` 时仍然要落盘（那是预填内容），只是不授权删除。

- [ ] **Step 8: 运行测试**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App mobile local cache"`
Expected: 新 3 条 + 该 `describe` 既有 20 条全 PASS（共 23 条）

- [ ] **Step 9: 跑全量前端测试确认零回归**

Run: `cd desktop && npx vitest run`
Expected: 全部 PASS（基线见「Global Constraints」的实测数 + 本轮新增；起草时基线为 1820）

- [ ] **Step 10: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出

- [ ] **Step 11: Commit**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(cache): flushAll 用内容指纹收窄重写范围

原先 flushAll 遍历 store.ids() 全写，每次切后台都把所有有内容的会话
重写一遍、savedAt 抬到当下，LRU 顺序被抹平成迭代顺序。

改为「脏集 ∪ 指纹变化」：内容没变的会话不再重写。指纹在冷启时用盘上条目
播种，避免首次切后台全量重写。persistedFp 只在 persistContent 成功后更新，
写失败不记，下次自然重试。

四条删除路径全部成对清理指纹（deleteCachedContent 与 drop 分支不经
flushContent，必须显式清）。"
```

---

### Task 7: 重配时重置内存状态

清掉 IndexedDB 还不够：重配是**同一个 App 实例续跑**，`store` / `warm` / hook 里的 `useState` 全都还在，串数据在内存层原封不动。

**Files:**
- Modify: `desktop/src/lib/threadStore.ts`（新增 `reset()`）
- Modify: `desktop/src/App.tsx`（两处 `onPaired`、重置清单、上移 `prefs` 声明）
- Modify: `desktop/src/hooks/useSessionState.ts`（新增 `pairingVersion` prop + effect）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `resetCacheScope`（Task 5）
- Produces: `useSessionState({ ..., pairingVersion: number })`

- [ ] **Step 1: 写失败的测试**

这条用例必须走**真实的重新配对流程**，不能自己造按钮名。本文件里已有的
`describe("App mobile repair path", ...)`（`App.test.tsx:2987` 起）已经把它走过一遍，
照它的方式写：

```ts
  it("drops A's sessions from memory after re-pairing to B", async () => {
    // 本用例需要假定时器 + WebSocket 替身，与 "App mobile repair path" 同款。
    vi.useFakeTimers();
    class FakeSocket {
      onopen: (() => void) | null = null;
      onmessage: ((e: { data: unknown }) => void) | null = null;
      onclose: ((e: { code: number }) => void) | null = null;
      onerror: unknown = null;
      constructor(readonly url: string) {
        queueMicrotask(() => this.onopen?.());
      }
      send(raw: string) {
        const frame = JSON.parse(raw) as { id?: number };
        // `redeemPairCodeViaRelay`（pairing.ts:186）的握手是两帧：
        //   1) 连上后发 id=1 `initialize`，收到回包才发 id=2 `pair/redeem`。
        // 少回 id=1 的话兑换会一直等到 15s 超时，用例以「配对超时」失败。
        if (frame.id === 1) {
          queueMicrotask(() =>
            this.onmessage?.({ data: JSON.stringify({ jsonrpc: "2.0", id: 1, result: {} }) }),
          );
        } else if (frame.id === 2) {
          queueMicrotask(() =>
            this.onmessage?.({
              data: JSON.stringify({
                jsonrpc: "2.0",
                id: 2,
                result: { device_id: "dev-2", token: "yia_new", scope: "control" },
              }),
            }),
          );
        }
      }
      close() {}
    }
    const settle = async () => {
      for (let i = 0; i < 3; i++) {
        await act(async () => {
          await vi.advanceTimersByTimeAsync(0);
        });
      }
    };

    // A：已配对的手机 + 一条有内容的会话。
    asRemote();
    state.threads = [{ thread_id: "a1", title: "from-A", permission_mode: "normal" as const }];
    seedListCache([{ thread_id: "a1", title: "from-A" }], "a1");
    contentStore.current = new MemoryContentStore();
    await contentStore.current.write({
      threadId: "a1", savedAt: 1, lastServerItemId: "x",
      items: [{ type: "agentMessage", id: "x", text: "A content" }],
      trimmed: false, bytes: 9,
    });

    vi.stubGlobal("WebSocket", FakeSocket);
    try {
      render(<App />);
      await settle();
      expect(screen.getByText("from-A")).toBeTruthy();

      // B：换一个 url、换一组会话。配对成功后 transport 会按新配置重建连接。
      state.threads = [{ thread_id: "b1", title: "from-B", permission_mode: "normal" as const }];

      // 打开重配界面 → 填配对码 → 提交。选择器与文案照抄既有的 repair 用例。
      fireEvent.click(screen.getByRole("button", { name: "重新配对" }));
      fireEvent.change(screen.getByLabelText("配对码"), { target: { value: "ABCD-EFGH" } });
      fireEvent.click(screen.getByRole("button", { name: "配对" }));
      await settle();

      // A 的会话标题必须离场，B 的必须出现。
      expect(screen.queryByText("from-A")).toBeNull();
      await settle();
      expect(screen.getByText("from-B")).toBeTruthy();
      // 且 B 的会话必须真的发 resume：`warm` 若没清，`selectThread` 的早退分支
      // 会把它吞掉，用户打开就是一个空白对话。
      const latest = clients.at(-1)!;
      expect(
        latest.requests.some(
          (r) => r.method === "thread/resume" && (r.params as { threadId: string }).threadId === "b1",
        ),
      ).toBe(true);
      // 盘上属于 A 的内容也必须没了（resetCacheScope 的同步段清 localStorage，
      // 异步段清 IndexedDB——这里断言的是后者）。
      expect(await contentStore.current!.keys()).not.toContain("a1");
    } finally {
      vi.unstubAllGlobals();
      vi.useRealTimers();
    }
  });
```

`asRemote()` / `seedListCache()` / `MemoryContentStore` / `contentStore` / `state` / `clients`
都是本 `describe` 与文件顶层已有的设施，直接用。`vi.useRealTimers()` 必须放在
`finally` 里——否则一条失败用例会把后面所有用例的定时器都留在假的状态。

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npm test -- src/App.test.tsx -t "drops A's sessions"`
Expected: FAIL — `from-A` 仍在屏上，或 `b1` 没发 `resume`

- [ ] **Step 3: 给 `useSessionState` 加 `pairingVersion` prop 与重置 effect**

在 `desktop/src/hooks/useSessionState.ts` 的参数类型里加：

```ts
  /** 配对版本号；每次成功配对/重配自增。用于丢弃上一个服务端的会话列表。 */
  pairingVersion: number;
```

在解构处加 `pairingVersion,`，并把下面这段 effect 插在 **`viewAnchors` 声明之后**
（`useSessionState.ts:112` 附近）——它引用了 `detailStack`/`traceRows`/`panelOpen`/
`viewAnchors`，插在 `workspaces` 之后会踩 TDZ（`const` 尚未初始化就进 effect 闭包，
运行时 `ReferenceError: Cannot access 'viewAnchors' before initialization`）。

```ts
  // 重配后必须丢掉上一个服务端的列表：这是同一个 App 实例续跑，不是冷启动，
  // `store` 换成新的了，但这些 state 属于本 hook，跟着一起清。
  //
  // 判据是「版本号变了」，不是「首帧已过」：
  //   - groups/pinned 的初值来自 initialListCache（首屏画缓存的来源），首帧
  //     执行本 effect 时**不能**清掉它；
  //   - `main.tsx` 用 `<React.StrictMode>` 包着 `<App/>`，开发模式下 effect 会
  //     跑两遍（挂载 → 清理 → 再挂载）。若用 `useRef(false)` 记「首帧已过」，
  //     第二遍就会把首屏缓存清空——手机上表现为「开发构建首屏永远是空的」，
  //     而生产构建不复现，极难排查。
  //   - 版本号比对对两种情形都安全：挂载时相等 → 直接返回；自增时不等 → 清一次
  //     并把新值记下来，重复跑不再清。
  //
  // 也不能判断 `pairingVersion === 0`：那把语义绑在「0 是特殊值」上，将来若
  // 持久化该值，初值不再是 0，就会静默失效且无任何报错。
  const lastPairingVersion = useRef(pairingVersion);
  useEffect(() => {
    if (lastPairingVersion.current === pairingVersion) return;
    lastPairingVersion.current = pairingVersion;
    // 列表与选中项。`workspaces` 是最近目录，同样属于上一台机器。
    setCurrentId(null);
    setGroups([]);
    setPinned([]);
    setWorkspaces([]);
    // 详情栈与轨迹行里存的是上一台机器的 subagent id：不清就会点进一个
    // 在 B 上不存在的任务，一路报 -32602。
    setDetailStack([]);
    setTraceRows([]);
    setPanelOpen(false);
    // 阅读位置按 thread_id 记，A 的 id 在 B 上没有意义，留着只是占内存。
    viewAnchors.current.clear();
  }, [pairingVersion]);
```

- [ ] **Step 4: 给 `ThreadStore` 加 `reset()`**

重配要丢掉上一台机器的**所有**会话视图，而 `store` 实例本身必须留住（Step 6 的
`onPaired` 会调它）。在 `desktop/src/lib/threadStore.ts` 的 `drop()` 之后加：

```ts
  /**
   * 清空全部会话视图，回到刚构造的状态。
   *
   * 重配到另一台电脑时用：`sess`/`proc` 持有的是**这个实例**的引用
   * （`store` 在 `App.tsx:162` 被解构成常量后传下去），换实例不会让它们换，
   * 只能就地清。`statusSource` 必须一起清——留着的 `"live"` 标记会让下一份
   * `listAll` 的快照被忽略，新机器的会话被当成「仍在跑，别 resume」。
   */
  reset(): void {
    this.views.clear();
    this.statusSource.clear();
    this.currentId = null;
  }
```

**验证字段名**：`currentId` 在 `threadStore.ts:67` 声明为
`currentId: string | null = null;`（**类字段、不是 `private`**，`select()`/`drop()` 都在改它）。
`views`/`statusSource` 都是 `private`，同在这个类里，直接点得到。

- [ ] **Step 5: `App.tsx` 传入 `pairingVersion`**

`useSessionState` 在 `App.tsx:376` 调用，而 `prefs`（`useSettingsState`）在 `:400`
才声明——直接把 `prefs.pairingVersion` 传进去会踩 TDZ。**必须先移动 `prefs` 的声明**：

把 `const prefs = useSettingsState({ rpc, force, setCurrentError });` 整段（连同它上面
那段注释）上移到 `const sess = useSessionState({...})` **之前**，然后在
`useSessionState` 的参数里加：

```ts
    pairingVersion: prefs.pairingVersion,
```

为什么这一次移动是安全的：`useSettingsState` 只依赖 `rpc`/`force`/`setCurrentError`，
三者都在 `:178`/`:238`/`:364` 就已经可用，**不依赖** `sess`/`proc`/`diff`/`board`。
反过来，`useSessionState` 也不依赖 `prefs` 的其它成员，只取这一个数字。两者没有
循环，只是原先的书写顺序恰好相反。

**不要**改用「把 `pairingVersion` 也放进 `sess` 之外的另一个 ref」之类绕法：那会让
`useSessionState` 拿不到「变了」这个信号，重置 effect 永远不会触发。

- [ ] **Step 6: 两处 `onPaired` 接入 `resetCacheScope` + 内存重置**

首次配对（`App.tsx:2292`）：

```tsx
        onPaired={() => {
          void resetCacheScope().catch(() => {
            /* 清理失败：scope 键保持旧值，下次启动会重试。
               不能让配对本身失败——否则用户卡在配对界面连不上。 */
          });
          prefs.setPairingVersion((v) => v + 1);
        }}
```

重新配对（`App.tsx:2316`）：

```tsx
        onPaired={() => {
          // 已配对过时 `inited.current` 仍是 true，单靠版本号会被守卫挡掉；
          // 必须先放开守卫，让旧 effect 走 cleanup、新 effect 真正建连。
          inited.current = false;
          // 内存层的串数据必须一起清：resetCacheScope 只清存储，不清 store/warm。
          //
          // 不能写 `storeRef.current = new ThreadStore()`：`store` 是
          // `const store = storeRef.current`（App.tsx:162）的一次性快照，已经作为
          // 参数传给了 useSessionState / useProcessState，换 ref 不会让它们换实例——
          // 旧会话仍在 `sess`/`proc` 手里。必须就地清空同一个实例。
          store.reset();
          warm.current.clear();
          inFlightResume.current.clear();
          cacheMetaRef.current.clear();
          cachedAt.current.clear();
          syncPlanRef.current.clear();
          persistTimers.current.clear();
          toppedUp.current.clear();
          persistedFp.current.clear();
          // SubscriptionWindow 没有 clear()，整体换新（与 App.tsx:1579 的重连路径一致）。
          subWindow.current = new SubscriptionWindow();
          void resetCacheScope().catch(() => {});
          prefs.setRepairing(false);
          prefs.setRepairNotice(null);
          prefs.setPairingVersion((v) => v + 1);
        }}
```

> 上面这些名字都已在 `App.tsx` 核对过（`storeRef:161`、`warm:185`、`inFlightResume:188`、
> `subWindow:193`、`toppedUp:199`、`syncPlanRef:253`）。它们全是 `useRef`，`Set`/`Map`
> 用 `.current.clear()`；`subWindow` 是自建类实例，没有 `clear()`，必须整体换新。
> `inFlightResume` 也要清——否则上一台机器在途的 resume 会把 `selectThread` 的早退分支
> 走掉（`App.tsx:878`），B 的会话永远不 resume。

- [ ] **Step 7: `main.tsx` 加模块顶层调用**

```tsx
import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { syncCacheScope } from "./lib/cacheScope";
import "./index.css";

// 必须在任何组件渲染之前跑一次：它是模块初始化期副作用，不是渲染期副作用。
// 放进 App.tsx 的 render body（如 initialListCache 那行旁边）会导致每次重渲染
// 都比对一遍并可能重写 localStorage，StrictMode 的双次渲染会放大这个问题。
syncCacheScope();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
```

- [ ] **Step 8: 运行测试**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "App mobile local cache"`
Expected: 全部 PASS

- [ ] **Step 9: 跑全量确认零回归**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: 全 PASS，类型零错误

- [ ] **Step 10: Commit**

```bash
git add desktop/src/App.tsx desktop/src/hooks/useSessionState.ts desktop/src/lib/threadStore.ts desktop/src/main.tsx desktop/src/App.test.tsx
git commit -m "feat(cache): 重配时清内存状态，并接入 cacheScope

重配不是冷启动而是同一 App 实例续跑，清掉 IndexedDB 后 store/warm/hook 的
useState 仍在，串数据在内存层原封不动。两处 onPaired 都接入 resetCacheScope
并在重配分支重置内存；useSessionState 新增 pairingVersion prop 与跳过首帧的
重置 effect（首帧清空会废掉首屏画缓存）。

syncCacheScope 放在 main.tsx 模块顶层：它是初始化期副作用，写进 App 的
render body 会每次重渲染都执行。"
```

---

### Task 8: 订正旧设计文档

`2026-10-11-mobile-local-cache-design.md` 声称的 `serializeCache` / `parseCache` / `pickCacheable` 三个符号**都不存在**，写入时机也写错了。就地订正并加一行指回新 spec。

**Files:**
- Modify: `docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md`（§4.1、§8、§9）

- [ ] **Step 1: 定位四处待改段落**

Run: `grep -n "serializeCache\|parseCache\|pickCacheable" docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md`
Expected: 命中 §4.1（符号名 + 写入时机）、§8（测试清单）、§9（超限兜底）

- [ ] **Step 2: 在文档顶部加订正说明**

在标题下方插入：

```markdown
> **部分内容已由 `2026-11-01-mobile-cache-hardening-design.md` 订正**：
> 本文 §4.1 的函数名与写入时机、§8 的测试清单、§9 的超限兜底
> 均与实现不符，请以新文档的「对旧文档的修正表」为准。（2026-11-01）
```

- [ ] **Step 3: 订正 §4.1 的函数名**

把「纯函数：`serializeCache` / `parseCache` / `pickCacheable`」改为：

```markdown
纯函数：`parseListCache` / `readListCache` / `writeListCache`（`sessionCache.ts`）。
```

- [ ] **Step 4: 订正 §4.1 的写入时机**

改为：

```markdown
写入时机：`thread/listAll` 成功后，两处——握手（`App.tsx:1676`）与
`refreshThreads`（`useSessionState.ts:127`）。快照里带 `selectedId`（写入时取
当时的 `store.currentId`，不随 `selectThread` 即时更新）；「最后看的会话」另有
独立键 `yi-agent.selectedThread`，两者用途不同。
```

- [ ] **Step 5: 订正 §8 与 §9**

§8 的测试清单改为：

```markdown
`parseListCache` 的坏值/版本/结构校验；`writeListCache` 的截断上界
（见新文档 §4.3）。
```

§9 改为：

```markdown
localStorage 超限：`writeListCache` 有 1000 条上界（见新文档 §4.3）+ try/catch 兜底。
```

- [ ] **Step 6: 确认没有遗留错误符号**

Run: `grep -c "serializeCache\|pickCacheable" docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md`
Expected: `0`

- [ ] **Step 7: Commit**

```bash
git add docs/superpowers/specs/2026-10-11-mobile-local-cache-design.md
git commit -m "docs: 订正 2026-10-11 本地缓存设计的三处漂移

声称的 serializeCache / parseCache / pickCacheable 三个符号都不存在；
写入时机漏了握手路径且误称 selectThread 会更新 selectedId；§9 的
pickCacheable 兜底描述也与实现不符。逐处订正并指向新 spec。"
```

---

### Task 9: 更新项目进度文档

按 CLAUDE.md 要求，功能完成后同步 `docs/project-management/`。

**Files:**
- Modify: `docs/project-management/desktop.md`

- [ ] **Step 1: 记录本轮改动**

在 `desktop.md` 的手机端缓存相关小节追加（若无该小节则新建）：

```markdown
### 手机端本地缓存加固（2026-11-01）

- [x] 空记录不落盘：`persistContent` 的 `authoritative` 选项与 `PersistOutcome`
      （`contentStore.ts` 的 `persistContent`）。判据：`contentStore.test.ts`
      的 "writes nothing when there is no server content" 用例
- [x] `flushAll` 用归一化内容指纹收窄重写范围（`App.tsx` 的 `flushAll`）。
      判据：`App.test.tsx` 的 "writes a session with unchanged content only once"
- [x] 服务端身份隔离：`lib/cacheScope.ts` 的 `syncCacheScope` / `resetCacheScope`。
      判据：`cacheScope.test.ts` 的 "clears both stores on re-pair to a different url"
- [x] 列表快照 1000 条上界（`sessionCache.ts` 的 `LIST_CACHE_MAX_THREADS`）。
      判据：`sessionCache.test.ts` 的 "caps non-pinned threads at 1000"
- [x] 旧文档 `2026-10-11` 的四处漂移订正
- [-] 不做：`savedAt` 拆两字段、清除缓存 UI、IndexedDB open 失败重试、
      多 WebView 协调、任何存储版本号提升、真正的访问序 LRU
```

- [ ] **Step 2: 更新测试基线计数**

Run: `cd desktop && npx vitest run 2>&1 | tail -5`
把输出里的文件数与用例数填进 `desktop.md` 里原「1470 个前端单测 / 116 个测试文件」的位置，
并同步那一长串逐文件计数（它已经漂了：`App.test.tsx` 实测 219 条、文档写 183）。
**以 `npx vitest run` 的输出为准**，不要沿用旧数字。

- [ ] **Step 3: 同步 README 索引计数**

若 `docs/project-management/README.md` 的模块索引表有「完成 / 总计」计数，且本轮完成项改变了它，同步更新该数字。

- [ ] **Step 4: Commit**

```bash
git add docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs: 更新手机端缓存加固的完成状态与测试基线"
```

---

## 验收：手动验证清单

jsdom 没有 `indexedDB`，以下三件事**单测覆盖不到**，必须在真机（或 `npx tauri dev` 配 iPhone UA + localStorage 的桌面浏览器）上确认：

1. **首次升级清一次缓存**：装新版本后首开，确认不白屏；随后再开一次，首屏应直接有列表（说明 scope 键已写入、不再清）。
2. **内容缓存真的能用**：打开一个有历史的会话 → 切后台 → 杀进程 → 重开 → 点该会话，应先看到上次内容，随后被服务端回放覆盖。
3. **换配对不串数据**：连电脑 A 打开几个会话 → 重新配对到电脑 B → 确认看不到 A 的任何会话标题或对话内容。

若第 2 项失败（切后台回来内容没了），先查 `ensureCacheScopeClean` 的层级是否搞反——表现为所有 `db()` 都 pending，且不报任何错。
