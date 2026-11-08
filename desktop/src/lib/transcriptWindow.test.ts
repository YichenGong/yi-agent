import { describe, it, expect } from "vitest";
import {
  nextWindow,
  decideTrimStart,
  cycleBudget,
  DEFAULT_LIMITS,
  type WindowState,
  type VisibleRange,
  type WindowLimits,
} from "./transcriptWindow";
import type { WindowMeasurement } from "./transcriptGeometry";

const L: WindowLimits = DEFAULT_LIMITS;

/* gap 是**有符号**的：`gapTopPx = firstTop - viewTop`，`gapBottomPx =
   viewBottom - lastBottom`。0 表示贴住边界，负值表示视口已越过边界。
   所以「离得很远」在两侧都写成负数，不要再抄旧的无符号距离写法。 */

/** 视口停在挂载区顶部：顶边贴住（0），底边还在视口下方很远（负）。 */
const atWindowTop = (w: WindowState): VisibleRange => ({
  first: w.start,
  last: w.start + 10,
  gapTopPx: 0,
  gapBottomPx: -5000,
});

/** 视口停在挂载区底部：顶边已在视口上方很远（负），底边贴住（0）。 */
const atWindowBottom = (w: WindowState): VisibleRange => ({
  first: w.end - 10,
  last: w.end - 1,
  gapTopPx: -5000,
  gapBottomPx: 0,
});

describe("nextWindow / 冷启动", () => {
  it("只挂 initialSize 条，并处在跟随状态", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    expect(w).toEqual({ start: 2940, end: 3000, following: true });
  });

  it("会话比首屏短时全挂，不越界", () => {
    const w = nextWindow({ current: null, itemCount: 7, limits: L, visible: null });
    expect(w).toEqual({ start: 0, end: 7, following: true });
  });

  it("空会话返回空窗口", () => {
    expect(nextWindow({ current: null, itemCount: 0, limits: L, visible: null })).toEqual({
      start: 0,
      end: 0,
      following: true,
    });
  });

  it("冷启动不翻页：用户在底部，顶边在视口上方（gapTopPx 为负）", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    // 60 条 * 40px = 2400px，视口 800px，所以顶边在视口上方 1600px。
    const grown = nextWindow({
      current: w,
      itemCount: 3000,
      limits: L,
      visible: { first: 2990, last: 2999, gapTopPx: -1600, gapBottomPx: 0 },
    });
    expect(grown).toEqual(w);
  });
});

describe("nextWindow / 向上分页", () => {
  it("视口触到挂载区顶边时追加一页，following 转假", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    const paged = nextWindow({ current: w, itemCount: 3000, limits: L, visible: atWindowTop(w) });
    expect(paged.start).toBe(2740);
    expect(paged.end).toBe(3000);
    expect(paged.following).toBe(false);
    expect(paged.end - paged.start).toBe(260);
  });

  it("翻一页就停：补偿后的 gap 回到阈值外，连续两次调用不再翻", () => {
    const w = nextWindow({ current: null, itemCount: 3000, limits: L, visible: null });
    const paged = nextWindow({ current: w, itemCount: 3000, limits: L, visible: atWindowTop(w) });
    const again = nextWindow({
      current: paged,
      itemCount: 3000,
      limits: L,
      // 补偿后视口仍在原来那几条上：上方新挂载的 200 条在视口上方（负），
      // 下方还有内容在视口下方（负）。
      visible: { first: 2860, last: 2870, gapTopPx: -2500, gapBottomPx: -900 },
    });
    expect(again).toEqual(paged);
  });

  it("已经到 0 就不再减", () => {
    const w: WindowState = { start: 0, end: 400, following: false };
    const paged = nextWindow({ current: w, itemCount: 3000, limits: L, visible: atWindowTop(w) });
    expect(paged.start).toBe(0);
  });
});

describe("nextWindow / 卸载", () => {
  it("视口贴着顶边且已超限：截 end，不卸用户正在读的 start 侧", () => {
    // 视口贴着挂载区顶边（gapTopPx=0）说明用户正在往上读。此时若超限，
    // 必须截 end：截 start 会把刚翻出来、正在看的历史立刻卸掉。
    // 不断言具体数字（规则 5 会先再翻一页，再截断），只断言不变量：
    //   宽度恰好 maxMounted，且 end 被裁到 3000 以下。
    const before: WindowState = { start: 2540, end: 3000, following: true };
    const w = nextWindow({ current: before, itemCount: 3000, limits: L, visible: atWindowTop(before) });
    expect(w.end - w.start).toBe(400);
    expect(w.end).toBeLessThan(3000);
    expect(w.start).toBeLessThan(2540);
  });

  it("decideTrimStart: following 为真 -> 截 start", () => {
    expect(
      decideTrimStart({
        following: true,
        visible: { first: 100, last: 110, gapTopPx: -5000, gapBottomPx: -5000 },
        start: 0,
        end: 500,
        limits: L,
      }),
    ).toBe(true);
  });

  it("decideTrimStart: 未测量 -> 保守截 start", () => {
    expect(decideTrimStart({ following: false, visible: null, start: 0, end: 500, limits: L })).toBe(
      true,
    );
  });

  it("decideTrimStart: 刚向下分页（底边贴住）-> 截 start", () => {
    // 覆盖 `gapBottomPx` 分支：底边贴住视口底，说明用户刚往下翻，
    // 上方那批旧内容才是该让位的。
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 2500, last: 2510, gapTopPx: -5000, gapBottomPx: 0 },
        start: 0,
        end: 3000,
        limits: L,
      }),
    ).toBe(true);
  });

  it("decideTrimStart: 两端都远离 -> 截掉离视口更远的一侧", () => {
    // 覆盖距离比较分支（gapTopPx / gapBottomPx 都超阈值的路径）。
    // 视口在 2500：上方 2500 条、下方 490 条 -> 上方更远 -> 截 start。
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 2500, last: 2510, gapTopPx: -5000, gapBottomPx: -5000 },
        start: 0,
        end: 3000,
        limits: L,
      }),
    ).toBe(true);
  });

  it("decideTrimStart: 视口靠近 start -> 截 end（方向必须反过来）", () => {
    // 同一分支的反向用例。缺了这条，比较符号写反也测不出来。
    // 视口在 200：上方 200 条、下方 2790 条 -> 下方更远 -> 截 end。
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 200, last: 210, gapTopPx: -5000, gapBottomPx: -5000 },
        start: 0,
        end: 3000,
        limits: L,
      }),
    ).toBe(false);
  });

  it("decideTrimStart: 刚向上分页 -> 截 end（即使视口离 start 更近）", () => {
    expect(
      decideTrimStart({
        following: false,
        visible: { first: 5, last: 15, gapTopPx: 0, gapBottomPx: -9000 },
        start: 0,
        end: 500,
        limits: L,
      }),
    ).toBe(false);
  });
});

describe("nextWindow / 底部跟随稳定性（回归）", () => {
  it("用户不滚动时连续多次测量必须稳定，不得自己上翻", () => {
    // 回归用例：gap 曾被夹到 ≥0，导致「顶边在视口上方」被读成「翻到顶了」，
    // 窗口逐帧上翻再跳回，形成永不收敛的抖动。
    const ROW = 40;
    const VIEWPORT = 800;
    let w = nextWindow({ current: null, itemCount: 3251, limits: L, visible: null });
    const first = { ...w };
    let scrollTop = (w.end - w.start) * ROW - VIEWPORT; // 贴底

    for (let i = 0; i < 6; i += 1) {
      const mounted = w.end - w.start;
      const visible = {
        first: w.start,
        last: w.end - 1,
        gapTopPx: 0 - scrollTop, // 顶边相对视口顶（负 = 已在视口上方）
        gapBottomPx: scrollTop + VIEWPORT - mounted * ROW,
      };
      const next = nextWindow({ current: w, itemCount: 3251, limits: L, visible });
      expect(next).toEqual(w); // 每一次都必须原地不动
      w = next;
    }
    expect(w).toEqual(first);
    expect(w.following).toBe(true);
  });
});

describe("nextWindow / 增长与收缩", () => {
  it("following 为真时 itemCount 增长，end 跟上、start 不动", () => {
    const before: WindowState = { start: 2940, end: 3000, following: true };
    const w = nextWindow({ current: before, itemCount: 3050, limits: L, visible: null });
    expect(w.start).toBe(2940);
    expect(w.end).toBe(3050);
    expect(w.following).toBe(true);
  });

  it("following 为假时 itemCount 增长，end 不动", () => {
    const before: WindowState = { start: 500, end: 560, following: false };
    const w = nextWindow({ current: before, itemCount: 3000, limits: L, visible: null });
    expect(w).toEqual(before);
  });

  it("收缩到窗口起点之前时重置为尾部小窗口，不留空窗口", () => {
    const before: WindowState = { start: 2940, end: 3000, following: true };
    const w = nextWindow({ current: before, itemCount: 30, limits: L, visible: null });
    expect(w).toEqual({ start: 0, end: 30, following: true });
  });
});

describe("nextWindow / focus 定点开窗", () => {
  it("focus 把目标包进窗口，且不吸到尾部", () => {
    const w = nextWindow({
      current: null,
      itemCount: 3000,
      limits: L,
      visible: null,
      focus: { index: 500, before: 20, after: 60 },
    });
    expect(w.start).toBe(480);
    expect(w.end).toBe(561);
    expect(w.following).toBe(false);
  });

  it("focus 后正常调用不会一步跳到尾部", () => {
    const focused = nextWindow({
      current: null,
      itemCount: 3000,
      limits: L,
      visible: null,
      focus: { index: 500, before: 20, after: 60 },
    });
    const next = nextWindow({
      current: focused,
      itemCount: 3000,
      limits: L,
      visible: { first: 490, last: 520, gapTopPx: -5000, gapBottomPx: -3000 },
    });
    expect(next).toEqual(focused);
  });

  it("向下分页扩到尾部后**不**自动恢复跟随（意图只由用户显式给出）", () => {
    let w = nextWindow({
      current: null,
      itemCount: 800,
      limits: L,
      visible: null,
      focus: { index: 100, before: 20, after: 60 },
    });
    expect(w.following).toBe(false);
    for (let i = 0; i < 10; i += 1) {
      w = nextWindow({
        current: w,
        itemCount: 800,
        limits: L,
        visible: atWindowBottom(w),
      });
    }
    expect(w.end).toBe(800);
    // 读到底部 ≠ 用户要贴底：自动转真会让「翻历史」在中段被新输出拽走。
    expect(w.following).toBe(false);
  });

  it("显式 following:true 时扩到尾部并持续跟住新增", () => {
    let w: WindowState = { start: 700, end: 800, following: true };
    w = nextWindow({ current: w, itemCount: 860, limits: L, visible: null, following: true });
    expect(w).toEqual({ start: 700, end: 860, following: true });
  });
});

describe("nextWindow / tail 名额与单方向", () => {
  it("最大挂载数含 LiveTail：tail 在窗口外时历史只占 399", () => {
    const before: WindowState = { start: 0, end: 400, following: false };
    const w = nextWindow({
      current: before,
      itemCount: 5000,
      limits: L,
      visible: { first: 100, last: 110, gapTopPx: -5000, gapBottomPx: -5000 },
      following: false,
    });
    // 400 历史行 + 恒挂 tail 会把实际 DOM 顶到 401；必须给 tail 留 1 个名额。
    expect(w.end - w.start).toBe(399);
    expect(w.end).toBeLessThan(5000);
  });

  it("读到底（end===itemCount）时 tail 就在窗口内，历史可用满 400", () => {
    const before: WindowState = { start: 0, end: 400, following: false };
    const w = nextWindow({
      current: before,
      itemCount: 400,
      limits: L,
      visible: { first: 100, last: 110, gapTopPx: -5000, gapBottomPx: -5000 },
      following: false,
    });
    expect(w.end - w.start).toBe(400);
  });

  it("direction=up 时即使底边也贴住，本周期也不向下开窗", () => {
    const before: WindowState = { start: 1000, end: 1200, following: false };
    const both = { first: 1000, last: 1199, gapTopPx: 0, gapBottomPx: 0 };
    const up = nextWindow({ current: before, itemCount: 5000, limits: L, visible: both, following: false, direction: "up" });
    // 向上翻一页（start 减 200）；随后按含 tail 的实际 DOM 上限裁到 399。
    // 视口贴着历史顶边，所以裁掉的是下方（end）——不卸用户正在读的一侧。
    expect(up.start).toBe(800);
    expect(up.end).toBe(1199);
    expect(up.end - up.start).toBe(399);
    const down = nextWindow({ current: before, itemCount: 5000, limits: L, visible: both, following: false, direction: "down" });
    // 向下开一页；顶边仍贴住视口顶，按上限裁掉远离视口的 end 侧。
    expect(down.start).toBe(1000);
    expect(down.end).toBe(1399);
    expect(down.end - down.start).toBe(399);
  });

  it("cycleBudget 是 ceil(N/page)+2，不是无限滚", () => {
    expect(cycleBudget(600, 200)).toBe(5);
    expect(cycleBudget(3000, 200)).toBe(17);
    expect(cycleBudget(0, 200)).toBe(3);
  });

  it("history 边界被远处的 tail 掩盖时必须继续向下开窗（回归）", () => {
    // 旧实现取首末挂载行 => 历史底边 + tail 底边；tail 在视口内让
    // gapBottomPx 看起来非负，窗口就停住不动，只能靠切走再切回触发。
    const before: WindowState = { start: 100, end: 299, following: false };
    const m: WindowMeasurement = {
      visible: { first: 999, last: 999 },
      history: { first: 100, last: 298, gapTopPx: -800, gapBottomPx: 2300 },
      tailVisible: true,
      viewportTop: 5000,
      viewportBottom: 5800,
    };
    const w = nextWindow({ current: before, itemCount: 600, limits: L, visible: m, following: false });
    expect(w.end).toBe(499);
    expect(w.start).toBe(100);
  });
});

describe("nextWindow / 不变量", () => {
  it("连续上翻 12 页：挂载数恒 ≤ maxMounted、start 单调递减、无回翻", () => {
    let w = nextWindow({ current: null, itemCount: 5000, limits: L, visible: null });
    w = nextWindow({ current: w, itemCount: 5000, limits: L, visible: atWindowTop(w) });
    let prevStart = w.start;
    for (let i = 0; i < 12; i += 1) {
      const next = nextWindow({
        current: w,
        itemCount: 5000,
        limits: L,
        visible: atWindowTop(w),
      });
      expect(next.end - next.start).toBeLessThanOrEqual(L.maxMounted);
      expect(next.start).toBeLessThan(prevStart);
      expect(next.end).toBeLessThanOrEqual(w.end);
      prevStart = next.start;
      w = next;
    }
  });

  it("任何输入下区间都合法", () => {
    const cases: Array<WindowState | null> = [
      null,
      { start: 0, end: 0, following: true },
      { start: 5, end: 5, following: false },
      { start: 2999, end: 3000, following: true },
      { start: 0, end: 3000, following: false },
    ];
    for (const current of cases) {
      for (const itemCount of [0, 1, 30, 3000]) {
        const w = nextWindow({ current, itemCount, limits: L, visible: null });
        expect(w.start).toBeGreaterThanOrEqual(0);
        expect(w.end).toBeLessThanOrEqual(itemCount);
        expect(w.start).toBeLessThanOrEqual(w.end);
      }
    }
  });
});
