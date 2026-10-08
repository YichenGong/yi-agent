import { describe, expect, it, vi } from "vitest";
import { makeBoardTick } from "./boardRefresh";

describe("makeBoardTick", () => {
  it("一拍同时刷新选中看板与侧栏摘要", () => {
    // 两套数据必须同拍：摘要只在握手时读过一次，不并入轮询就会长期停在旧值，
    // 侧栏因此一直显示过期的「0 排队」。
    const selected = vi.fn(async () => {});
    const boards = vi.fn(async () => {});
    makeBoardTick(selected, boards)();
    expect(selected).toHaveBeenCalledTimes(1);
    expect(boards).toHaveBeenCalledTimes(1);
  });
});
