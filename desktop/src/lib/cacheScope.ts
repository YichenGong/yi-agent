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
