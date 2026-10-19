import { describe, expect, it } from "vitest";
import {
  BASE_MS,
  MAX_MS,
  MAX_STAGGER_MS,
  PER_STEP_MS,
  STAGGER_MS,
  digitDelayMs,
  digitDurationMs,
  digitMove,
  splitDigits,
  stripY,
} from "./rollingDigits";

describe("splitDigits", () => {
  it("splits a number into digits, most significant first", () => {
    expect(splitDigits(1234)).toEqual([1, 2, 3, 4]);
  });

  it("renders zero as a single digit", () => {
    expect(splitDigits(0)).toEqual([0]);
  });

  it("ignores sign and fraction", () => {
    expect(splitDigits(-42)).toEqual([4, 2]);
    expect(splitDigits(12.9)).toEqual([1, 2]);
  });
});

describe("digitMove", () => {
  it("moves one step when a digit rolls over upward", () => {
    expect(digitMove(9, 0, 1)).toEqual({ steps: 1, direction: 1 });
  });

  it("walks the long way upward from 0 to 9", () => {
    expect(digitMove(0, 9, 1)).toEqual({ steps: 9, direction: 1 });
  });

  it("mirrors the walk when the value decreases", () => {
    expect(digitMove(0, 9, -1)).toEqual({ steps: 1, direction: -1 });
    expect(digitMove(9, 0, -1)).toEqual({ steps: 9, direction: -1 });
  });

  it("does not move a digit whose face is unchanged", () => {
    expect(digitMove(4, 4, 1)).toEqual({ steps: 0, direction: 0 });
  });

  it("does not move anything when the value is unchanged", () => {
    expect(digitMove(4, 7, 0)).toEqual({ steps: 0, direction: 0 });
  });
});

describe("digitDurationMs", () => {
  it("is zero for no movement", () => {
    expect(digitDurationMs(0)).toBe(0);
  });

  it("grows with the number of steps", () => {
    expect(digitDurationMs(1)).toBe(BASE_MS);
    expect(digitDurationMs(3)).toBe(BASE_MS + 2 * PER_STEP_MS);
    expect(digitDurationMs(3)).toBe(410);
  });

  it("caps at MAX_MS", () => {
    expect(digitDurationMs(99)).toBe(MAX_MS);
  });
});

describe("digitDelayMs", () => {
  it("starts the least significant digit immediately", () => {
    expect(digitDelayMs(0)).toBe(0);
    expect(digitDelayMs(1)).toBe(STAGGER_MS);
  });

  it("caps the stagger", () => {
    expect(digitDelayMs(20)).toBe(MAX_STAGGER_MS);
  });
});

describe("stripY", () => {
  it("translates by one line per position", () => {
    expect(stripY(10)).toBe("translateY(-10em)");
  });
});