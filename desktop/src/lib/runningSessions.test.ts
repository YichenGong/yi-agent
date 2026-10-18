import { describe, expect, it } from "vitest";
import { countRunningSessions, runningSessionsLabel } from "./runningSessions";
import type { ThreadStatus } from "./protocol";

/** 按 [id, status] 对构造入参,避免测试里手写一大堆 Map.set。 */
const map = (...entries: [string, ThreadStatus][]) => new Map(entries);

describe("countRunningSessions", () => {
  it("returns 0 for an empty map", () => {
    expect(countRunningSessions(new Map())).toBe(0);
  });

  it("returns 0 when every thread is idle", () => {
    expect(countRunningSessions(map(["a", "idle"], ["b", "idle"]))).toBe(0);
  });

  // 本设计的核心语义:停在审批窗口的会话**不算**在跑(它在等人,没在干活)。
  it("does not count awaiting_approval", () => {
    expect(countRunningSessions(map(["a", "awaiting_approval"], ["b", "awaiting_approval"]))).toBe(0);
  });

  it("counts only running when statuses are mixed", () => {
    const s = map(
      ["a", "idle"],
      ["b", "running"],
      ["c", "awaiting_approval"],
      ["d", "running"],
      ["e", "idle"],
    );
    expect(countRunningSessions(s)).toBe(2);
  });

  it("counts every entry when all are running", () => {
    expect(countRunningSessions(map(["a", "running"], ["b", "running"], ["c", "running"]))).toBe(3);
  });
});

describe("runningSessionsLabel", () => {
  it("renders the exact copy", () => {
    expect(runningSessionsLabel(3)).toBe("3 个会话运行中");
    expect(runningSessionsLabel(1)).toBe("1 个会话运行中");
  });
});
