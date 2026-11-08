import { describe, expect, it } from "vitest";
import {
  beginConvergence,
  decideConvergence,
  noteAdvance,
  noteFocus,
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
