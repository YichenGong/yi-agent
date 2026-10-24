import { describe, it, expect } from "vitest";
import { readSelectedThread, writeSelectedThread, SELECTED_THREAD_KEY } from "./selectedThread";

const memory = () => {
  const data = new Map<string, string>();
  return {
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
};

describe("selectedThread", () => {
  it("round-trips an id", () => {
    const s = memory();
    writeSelectedThread(s, "thread-1");
    expect(readSelectedThread(s)).toBe("thread-1");
  });

  it("writes the key the cache is documented under", () => {
    const s = memory();
    writeSelectedThread(s, "thread-1");
    expect(s.getItem(SELECTED_THREAD_KEY)).toBe("thread-1");
  });

  it("treats null as absent rather than the string 'null'", () => {
    const s = memory();
    writeSelectedThread(s, "thread-1");
    writeSelectedThread(s, null);
    expect(readSelectedThread(s)).toBeNull();
  });

  it("degrades to null for garbage storage", () => {
    const s = memory();
    s.setItem(SELECTED_THREAD_KEY, "");
    expect(readSelectedThread(s)).toBeNull();
  });

  it("never throws when storage is unavailable", () => {
    expect(readSelectedThread(undefined)).toBeNull();
    expect(() => writeSelectedThread(undefined, "x")).not.toThrow();
  });
});
