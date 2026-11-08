import { describe, expect, it } from "vitest";
import { createHeightBook } from "./heightBook";
import { createTranscriptPositionIndex } from "./transcriptPositionIndex";

describe("TranscriptPositionIndex", () => {
  const ids = ["a", "b", "c", "d"];
  function book() {
    const b = createHeightBook();
    b.record("a", 100);
    b.record("b", 200);
    b.record("c", 300);
    b.record("d", 400);
    return b;
  }

  it("按累积高度把偏移换成条目序号", () => {
    const index = createTranscriptPositionIndex();
    index.rebuild(ids, book());
    expect(index.totalHeight()).toBe(1000);
    expect(index.size()).toBe(4);

    expect(index.indexAt(0)).toBe(0);
    expect(index.indexAt(99)).toBe(0);
    expect(index.indexAt(100)).toBe(1); // b 的顶边
    expect(index.indexAt(299)).toBe(1);
    expect(index.indexAt(300)).toBe(2); // c 的顶边
    expect(index.indexAt(999)).toBe(3);
  });

  it("超出总高时夹到最后一条，而不是返回越界序号", () => {
    const index = createTranscriptPositionIndex();
    index.rebuild(ids, book());
    expect(index.indexAt(10_000)).toBe(3);
    expect(index.indexAt(-50)).toBe(0);
  });

  it("空资料不返回可用的 focus 目标", () => {
    // Plan A4：空资料不 focus。返回 -1 让调用方明确「没有目标」。
    const index = createTranscriptPositionIndex();
    index.rebuild([], createHeightBook());
    expect(index.indexAt(0)).toBe(-1);
    expect(index.totalHeight()).toBe(0);
  });

  it("高度修订变化后重建，未变化时不重复累加", () => {
    const b = createHeightBook();
    b.record("a", 100);
    b.record("b", 100);
    b.record("c", 100);
    b.record("d", 100);
    const index = createTranscriptPositionIndex();
    index.rebuild(ids, b);
    expect(index.totalHeight()).toBe(400);

    // 同输入重复 rebuild：结果不变（这是滚动帧里的常规调用）。
    index.rebuild(ids, b);
    expect(index.totalHeight()).toBe(400);

    // 真实高度变化 -> revision 变 -> 必须重建。
    b.record("b", 500);
    index.rebuild(ids, b);
    expect(index.totalHeight()).toBe(800);
    expect(index.indexAt(450)).toBe(1);
  });

  it("条目数变化会被识别（不能只看高度修订）", () => {
    const b = createHeightBook();
    for (const id of ids) b.record(id, 100);
    const index = createTranscriptPositionIndex();
    index.rebuild(ids, b);
    expect(index.size()).toBe(4);

    // 追加一条：高度修订没变（新 id 还没测量），但成员变了。
    index.rebuild([...ids, "e"], b);
    expect(index.size()).toBe(5);
    expect(index.totalHeight()).toBe(500);
  });
});
