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
