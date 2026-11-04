import { describe, expect, it, vi } from "vitest";
import { makeBoardTick } from "./refresh";

describe("makeBoardTick", () => {
  it("fires the refresher", () => {
    const refresh = vi.fn(async () => {});
    makeBoardTick(refresh)();
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it("fires every refresher in one tick", () => {
    const first = vi.fn(async () => {});
    const second = vi.fn(async () => {});
    makeBoardTick(first, second)();
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).toHaveBeenCalledTimes(1);
  });

  it("does not let one failing refresher block the others", () => {
    const doomed = vi.fn(async () => {
      throw new Error("backend restarting");
    });
    const fine = vi.fn(async () => {});
    // The tick itself must not reject: the interval would otherwise die on the
    // first failure and the board would silently stop updating.
    expect(() => makeBoardTick(doomed, fine)()).not.toThrow();
    expect(fine).toHaveBeenCalledTimes(1);
  });
});
