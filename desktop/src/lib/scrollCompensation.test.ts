import { describe, it, expect } from "vitest";
import { planCompensation } from "./scrollCompensation";

const base = {
  prevScrollHeight: 10_000,
  nextScrollHeight: 10_000,
  prevViewportHeight: 800,
  scrollTop: 5_000,
  wasFollowing: false,
};

describe("planCompensation", () => {
  it("上方挂载更多内容：scrollTop 增加同样的高度差", () => {
    // 在视口上方插入 4000px。用户的阅读位置必须跟着下移 4000px，
    // 否则同一段文字会被推到视口下方。
    const plan = planCompensation({ ...base, nextScrollHeight: 14_000 });
    expect(plan.scrollTop).toBe(9_000);
    expect(plan.follow).toBe("release");
  });

  it("上方卸载内容（高度变小）：scrollTop 相应减少，但不为负", () => {
    // 卸载 2000px 后原本 1000 的 scrollTop 会算成 -1000，必须夹到 0。
    const plan = planCompensation({ ...base, scrollTop: 1_000, nextScrollHeight: 8_000 });
    expect(plan.scrollTop).toBe(0);
  });

  it("原本贴底：补偿后重新贴底并保持跟随", () => {
    // 贴底时**不按差值补**，直接把视口放回新的底部。
    //
    // 这里的 `scrollTop: 9_000` 是刻意的：旧底部是 `10_000 - 800 = 9_200`，
    // 用户被标记为跟随、但 scrollTop 还差 200px 没跟上（内容刚长高、滚动事件
    // 尚未落地时的真实瞬态）。此时两种公式**不等**：
    //   差值法 → 9_000 + 1_000 = 10_000（停在底部上方 200px）
    //   直接贴底 → 11_000 - 800 = 10_200
    // 若把 scrollTop 设成正好 9_200，两式代数上恒等，这条断言就退化成了
    // 空断言——注入「按差值补」照样通过。用 9_000 才真正钉住「直接贴底」。
    const nextScrollHeight = 11_000;
    const plan = planCompensation({
      ...base,
      wasFollowing: true,
      scrollTop: 9_000,
      nextScrollHeight,
    });
    expect(plan.scrollTop).toBe(nextScrollHeight - base.prevViewportHeight);
    expect(plan.scrollTop).toBe(10_200);
    expect(plan.follow).toBe("to-bottom");
  });

  it("高度没变：scrollTop 不动，跟随意图不变", () => {
    // 即使 delta 为 0 也要 release：它把滚动基线对齐到新布局，
    // 下一次 onScroll 的增量比较才有干净基准。
    const plan = planCompensation(base);
    expect(plan.scrollTop).toBe(5_000);
    expect(plan.follow).toBe("release");
  });

  it("新内容比视口还短：scrollTop 夹到 0", () => {
    // 可滚动上限为 max(0, 300 - 800) = 0，任何正值都不合法。
    const plan = planCompensation({
      ...base,
      prevScrollHeight: 400,
      nextScrollHeight: 300,
      scrollTop: 10,
    });
    expect(plan.scrollTop).toBe(0);
  });

  it("结果不超过新的可滚动上限", () => {
    // 差值法可能把 scrollTop 推到新布局的下方之外，必须按新上限夹取。
    const plan = planCompensation({
      ...base,
      scrollTop: 9_950,
      prevScrollHeight: 10_000,
      nextScrollHeight: 12_000,
    });
    expect(plan.scrollTop).toBe(12_000 - 800);
    expect(plan.scrollTop).toBeLessThanOrEqual(12_000 - 800);
  });
});
