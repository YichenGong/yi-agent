/**
 * 「当前 scrollTop 对应哪一条消息」的索引（Spec §4.4 第 6 条）。
 *
 * 用途只有两个：窗口 stalled 时按当前位置发**一次** focus；以及判断滚动目标
 * 落在未挂载区间的哪一侧。两者都在滚动帧里发生，所以不能用「从头累加每条高度」
 * 的线性扫描——3000 条 × 每帧一次就是一场性能事故。
 *
 * 累积高度只在**结构或高度修订变化**时重建；查询走二分。高度不是真值而是
 * `HeightBook` 的估计（有测量用测量，没有用中位数），所以这里只承诺「落在哪一
 * 条附近」，不承诺像素精确——精确的那一步由测量层在挂载后完成。
 */
import type { HeightBook } from "./heightBook";

export interface TranscriptPositionIndex {
  /** 用当前的 id 顺序与高度账本重建累积和。重复调用同样输入是廉价的空操作。 */
  rebuild(ids: readonly string[], book: HeightBook): void;
  /**
   * 内容坐标 `offsetPx` 落在哪一条上。
   *
   * 空资料返回 `-1`（调用方不得据此 focus）。超出总高时夹到最后一条。
   */
  indexAt(offsetPx: number): number;
  /** 全部条目的估算总高（spacer 的上下合计据此核对）。 */
  totalHeight(): number;
  /** 条目数（诊断与断言用）。 */
  size(): number;
}

export function createTranscriptPositionIndex(): TranscriptPositionIndex {
  /** `offsets[i]` = 第 i 条的顶边；长度 = size。 */
  let offsets: number[] = [];
  let total = 0;
  /** 上一次重建的输入指纹，避免同一输入反复排序/累加。 */
  let signature: string | null = null;

  const build = (ids: readonly string[], book: HeightBook): void => {
    const next: number[] = [];
    let acc = 0;
    for (const id of ids) {
      next.push(acc);
      acc += Math.max(0, book.estimate(id));
    }
    offsets = next;
    total = acc;
  };

  return {
    rebuild(ids, book) {
      // 签名同时覆盖「顺序/成员」与「高度修订」：只要账本真的变了就重建，
      // 没变就跳过（滚动帧里会被频繁调用）。
      const next = `${book.revision()}|${ids.length}|${ids[0] ?? ""}|${ids[ids.length - 1] ?? ""}`;
      if (next === signature) return;
      signature = next;
      build(ids, book);
    },
    indexAt(offsetPx) {
      if (offsets.length === 0) return -1;
      if (!Number.isFinite(offsetPx) || offsetPx <= 0) return 0;
      if (offsetPx >= total) return offsets.length - 1;
      // 二分找「最后一个顶边 <= offset」的条目。
      let lo = 0;
      let hi = offsets.length - 1;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (offsets[mid] <= offsetPx) lo = mid;
        else hi = mid - 1;
      }
      // 顶边正好落在某条上时就是它；落在条目内部也是它（二分已经停在「最后一个
      // 顶边 <= offset」的条目，不需要再拿高度判断）。
      return lo;
    },
    totalHeight() {
      return total;
    },
    size() {
      return offsets.length;
    },
  };
}
