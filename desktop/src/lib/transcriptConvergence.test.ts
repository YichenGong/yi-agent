import { describe, expect, it } from "vitest";
import {
  beginConvergence,
  decideConvergence,
  noteAdvance,
  noteFocus,
  estimateFocusIndex,
  type ConvergenceCycle,
} from "./transcriptConvergence";

const cycle = (over: Partial<ConvergenceCycle> = {}): ConvergenceCycle => ({
  ...beginConvergence("host-a/t/0", 600, 1),
  ...over,
});

describe("beginConvergence", () => {
  it("预算按 ceil(N/page)+2 计，固定条数不会无限补页", () => {
    expect(beginConvergence("o", 600, 1).remaining).toBe(5);
    expect(beginConvergence("o", 3000, 1).remaining).toBe(17);
    expect(beginConvergence("o", 0, 1).remaining).toBe(3);
  });
});

describe("decideConvergence", () => {
  it("未布局一律 paused，且不受覆盖状态影响", () => {
    // 隐藏标签页 / 尺寸为 0：暂停，不能按 0 高度扩窗。
    expect(decideConvergence(cycle(), "sig", false, false)).toBe("paused");
    expect(decideConvergence(cycle(), "sig", true, false)).toBe("paused");
  });

  it("已覆盖就 settled，不再安排帧", () => {
    expect(decideConvergence(cycle(), "sig", true, true)).toBe("settled");
  });

  it("未覆盖且签名是新的推进一页", () => {
    expect(decideConvergence(cycle(), "sig-a", false, true)).toBe("advance");
  });

  it("同一签名再出现说明上一步没有改变几何：先 focus 一次，再 stalled", () => {
    // 关键判据：窗口提交后若视口仍在 spacer 里，几何签名不变。
    // 若允许无限 advance，就变成后台每帧 setState 空转（D09 会红）。
    const first = decideConvergence(cycle({ lastSignature: "sig-a" }), "sig-a", false, true);
    expect(first).toBe("focus");
    const focused = noteFocus(cycle({ lastSignature: "sig-a" }));
    expect(decideConvergence(focused, "sig-a", false, true)).toBe("stalled");
  });

  it("签名变化后重新允许一次 focus", () => {
    const focused = noteFocus(cycle({ lastSignature: "sig-a" }));
    expect(decideConvergence(focused, "sig-b", false, true)).toBe("advance");
    const advanced = noteAdvance(focused, "sig-b");
    expect(decideConvergence(advanced, "sig-b", false, true)).toBe("focus");
  });

  it("预算耗尽后 stalled，即使签名仍在变", () => {
    const spent = cycle({ lastSignature: "sig-a", remaining: 0 });
    expect(decideConvergence(spent, "sig-b", false, true)).toBe("stalled");
  });

  it("noteAdvance 扣一个预算并记录签名", () => {
    const next = noteAdvance(cycle(), "sig-a");
    expect(next.remaining).toBe(4);
    expect(next.lastSignature).toBe("sig-a");
    expect(next.focusedOnce).toBe(false);
  });
});

describe("estimateFocusIndex", () => {
  // 符号约定（与 `measureTranscript` 一致，见其接口注释）：
  //   gapBottomPx = viewportBottom - historyLast.bottom
  //     > 0 -> 视口底在历史区底边**下方**（视口落在历史区之后的 spacer 里）
  //     < 0 -> 视口仍在历史区内部（底边还没到）
  //   gapTopPx = historyFirst.top - viewportTop
  //     > 0 -> 视口顶在历史区顶边**上方**（视口落在历史区之前的 spacer 里）
  //     < 0 -> 视口已进入历史区
  it("视口落在历史区之后时按下方溢出量往后估", () => {
    const index = estimateFocusIndex(
      { viewportTop: 60_000, history: { first: 0, last: 10, gapTopPx: -60_000, gapBottomPx: 50_000 } },
      100,
      3000,
    );
    // 历史区底边之后还有 50000px ≈ 500 条。
    expect(index).toBe(10 + 500 + 1);
  });

  it("视口落在历史区之前时按上方溢出量往前估", () => {
    const index = estimateFocusIndex(
      { viewportTop: 0, history: { first: 900, last: 950, gapTopPx: 40_000, gapBottomPx: -1000 } },
      100,
      3000,
    );
    expect(index).toBe(900 - 400 - 1);
  });

  it("视口里的历史行已经覆盖时不预测（返回历史区首条）", () => {
    const index = estimateFocusIndex(
      { viewportTop: 1000, history: { first: 30, last: 40, gapTopPx: -1000, gapBottomPx: -2000 } },
      100,
      3000,
    );
    expect(index).toBe(30);
  });

  it("没有历史区测量时返回 null，调用方不得乱跳", () => {
    expect(estimateFocusIndex({ viewportTop: 0, history: null }, 100, 3000)).toBeNull();
    expect(estimateFocusIndex(null, 100, 3000)).toBeNull();
  });
});
