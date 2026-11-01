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

  it("returns null when a groups element has no threads array", () => {
    const s = memStorage();
    // threads 缺失的分组元素：若照单全收，App 首屏 `flatMap((g) => g.threads)`
    // 会在渲染期 effect 里抛错（React 接不住 → 白屏），正是本缓存要预防的故障。
    s.setItem(
      THREAD_LIST_CACHE_KEY,
      JSON.stringify({ version: 1, savedAt: 1, selectedId: null, pinned: [], groups: [{ workspace: "/w" }] }),
    );
    expect(readListCache(s)).toBeNull();
  });

  it("returns null when a thread inside a group is not an object", () => {
    const s = memStorage();
    // 分组元素本身合法（threads 是数组），但数组里藏着 null：侧栏渲染
    // `g.threads.filter((t) => !t.pinned …)` 会在 null 上抛错 → 同样白屏。
    s.setItem(
      THREAD_LIST_CACHE_KEY,
      JSON.stringify({
        version: 1, savedAt: 1, selectedId: null, pinned: [],
        groups: [{ workspace: "/w", exists: true, threads: [null] }],
      }),
    );
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
});
