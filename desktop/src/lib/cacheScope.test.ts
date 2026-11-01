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
