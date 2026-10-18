/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { createPersistedWidth } from "./persistedWidth";

afterEach(() => localStorage.clear());

describe("createPersistedWidth", () => {
  it("clamps into the configured range", () => {
    const w = createPersistedWidth({ storageKey: "t", defaultPx: 300, minPx: 200, maxPx: 400 });
    expect(w.clamp(100)).toBe(200);
    expect(w.clamp(500)).toBe(400);
    expect(w.clamp(350)).toBe(350);
  });

  it("falls back to default on a non-finite value", () => {
    const w = createPersistedWidth({ storageKey: "t", defaultPx: 300, minPx: 200, maxPx: 400 });
    expect(w.clamp(Number.NaN)).toBe(300);
  });

  it("loads the default when storage is empty", () => {
    const w = createPersistedWidth({ storageKey: "missing-key", defaultPx: 300, minPx: 200, maxPx: 400 });
    expect(w.load()).toBe(300);
  });

  it("round-trips through save/load", () => {
    const w = createPersistedWidth({ storageKey: "rt", defaultPx: 300, minPx: 200, maxPx: 400 });
    w.save(333);
    expect(w.load()).toBe(333);
    localStorage.removeItem("rt");
  });
});
