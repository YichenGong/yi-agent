import { describe, expect, it } from "vitest";
import { formatRelativeTime, RELATIVE_TIME_UNKNOWN } from "./relativeTime";

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

  /**
   * 0 / 负数 / NaN / Infinity / 缺值都不是时间戳，只能表示「不知道」。
   *
   * 修复前 0 会一路算到 `Math.floor(diff / 86_400_000)` 得 20758 —— 手机上离线
   * 提示条因此显示「更新于 20758d」，即用户报的「20xxxd 用户不友好」。
   */
  it("reports an unknown timestamp instead of a huge day count", () => {
    expect(formatRelativeTime(0, T)).toBe(RELATIVE_TIME_UNKNOWN);
    expect(formatRelativeTime(-1, T)).toBe(RELATIVE_TIME_UNKNOWN);
    expect(formatRelativeTime(Number.NaN, T)).toBe(RELATIVE_TIME_UNKNOWN);
    expect(formatRelativeTime(Number.POSITIVE_INFINITY, T)).toBe(RELATIVE_TIME_UNKNOWN);
    expect(formatRelativeTime(undefined as unknown as number, T)).toBe(RELATIVE_TIME_UNKNOWN);
    expect(formatRelativeTime(null as unknown as number, T)).toBe(RELATIVE_TIME_UNKNOWN);
  });
});
