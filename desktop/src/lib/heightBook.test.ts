import { describe, it, expect } from "vitest";
import { createHeightBook } from "./heightBook";

describe("HeightBook", () => {
  it("没测量过时用 fallback", () => {
    const book = createHeightBook(40);
    expect(book.estimate("unknown")).toBe(40);
  });

  it("测量过就用测量值", () => {
    const book = createHeightBook(40);
    book.record("a", 120);
    expect(book.estimate("a")).toBe(120);
  });

  it("中位数对重尾分布比平均值稳", () => {
    const book = createHeightBook();
    // 绝大多数是两三行，少数是超长工具卡
    for (let i = 0; i < 9; i += 1) book.record(`m${i}`, 40);
    book.record("huge", 5000);
    expect(book.median()).toBe(40);
  });

  it("完全测量后 estimateRange 等于精确总和（硬不变量）", () => {
    const book = createHeightBook();
    const ids = ["a", "b", "c"];
    const heights = [30, 55, 900];
    ids.forEach((id, i) => book.record(id, heights[i]));
    expect(book.estimateRange(ids)).toBe(985);
  });

  it("未测量的项用**中位数**补齐（有测量时中位数优先于 fallback）", () => {
    const book = createHeightBook(50);
    book.record("a", 100);
    // 只有一条测量时中位数就是 100，所以 b/c 也估成 100：100*3 = 300。
    // fallback(50) 只在**一条都没测量**时才有机会出场。
    expect(book.estimateRange(["a", "b", "c"])).toBe(300);
  });

  it("一条都没测量时才用 fallback", () => {
    const book = createHeightBook(50);
    expect(book.estimateRange(["a", "b", "c"])).toBe(150);
  });

  it("delta 是「真实总高 − 估算总高」", () => {
    const book = createHeightBook(50);
    book.record("a", 100);
    // 估算 = 100(a) + 100(b，中位数) = 200，真实是 260 -> 差 60
    expect(book.delta(["a", "b"], 260)).toBe(60);
  });

  it("追加同分布样本时中位数误差不增大（断言 <=）", () => {
    const book = createHeightBook();
    const truth = [40, 40, 42, 38, 40, 41, 39, 40, 40, 40];
    // 前 5 个的估计
    truth.slice(0, 5).forEach((h, i) => book.record(`a${i}`, h));
    const errBefore = Math.abs(book.median() - 40);
    truth.slice(5).forEach((h, i) => book.record(`b${i}`, h));
    const errAfter = Math.abs(book.median() - 40);
    expect(errAfter).toBeLessThanOrEqual(errBefore);
  });

  it("同一 id 重复测量以最后一次为准，且不重复计数", () => {
    const book = createHeightBook(10);
    book.record("a", 100);
    book.record("a", 200);
    expect(book.estimate("a")).toBe(200);
    expect(book.estimateRange(["a", "a"])).toBe(200);
    expect(book.median()).toBe(200);
  });

  it("非法高度（NaN / 负数）被忽略", () => {
    const book = createHeightBook(40);
    book.record("bad", Number.NaN);
    book.record("neg", -5);
    expect(book.estimate("bad")).toBe(40);
    expect(book.estimate("neg")).toBe(40);
    expect(book.median()).toBe(40);
  });
});
