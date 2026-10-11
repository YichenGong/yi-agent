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
