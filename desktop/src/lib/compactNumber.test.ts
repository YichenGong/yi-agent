import { describe, it, expect } from "vitest";
import { compact, formatCompact } from "./compactNumber";

describe("compact", () => {
  it("keeps small numbers as a single integer part", () => {
    expect(compact(0)).toEqual({ digits: "0", fractionDigits: 0, unit: "" });
    expect(compact(999)).toEqual({ digits: "999", fractionDigits: 0, unit: "" });
  });

  it("switches to k with one decimal", () => {
    expect(compact(1_000)).toEqual({ digits: "10", fractionDigits: 1, unit: "k" });
    expect(compact(1_234)).toEqual({ digits: "12", fractionDigits: 1, unit: "k" });
    expect(compact(999_949)).toEqual({ digits: "9999", fractionDigits: 1, unit: "k" });
  });

  it("switches to M with two decimals", () => {
    expect(compact(1_000_000)).toEqual({ digits: "100", fractionDigits: 2, unit: "M" });
    expect(compact(1_234_567)).toEqual({ digits: "123", fractionDigits: 2, unit: "M" });
  });

  it("switches to G and beyond", () => {
    expect(compact(1_000_000_000)).toEqual({ digits: "100", fractionDigits: 2, unit: "G" });
  });

  it("carries into the next unit instead of printing 1000", () => {
    // 999,950 四舍五入成 1000.0k，那就该进位显示 1.00M。
    // 进位后按目标单位自己的精度重新取整（M 是两位），不沿用 k 的一位。
    expect(compact(999_950)).toEqual({ digits: "100", fractionDigits: 2, unit: "M" });
  });
});

describe("formatCompact", () => {
  it("renders a unit suffix and drops useless decimals", () => {
    expect(formatCompact(1_234)).toBe("1.2k");
    expect(formatCompact(1_234_567)).toBe("1.23M");
    expect(formatCompact(999)).toBe("999");
  });

  it("keeps a trailing zero at the displayed precision", () => {
    // 1.0k 不能塌成 1k：宽度跳动会让右下角数字一直抖。
    expect(formatCompact(1_000)).toBe("1.0k");
  });
});
