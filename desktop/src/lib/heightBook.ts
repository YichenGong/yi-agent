/**
 * 未挂载区间的高度估算。
 *
 * 用**中位数**而不是平均值：工具卡 / 长代码块是重尾分布，平均值会被少数
 * 超长项带偏，中位数对「绝大多数消息是两三行」这一真实形状更稳。
 *
 * 零 DOM：这里只做算术，测量由组件层做完传进来。所以
 * 「真实总高 − 估算总高」这个差值也能单测。
 */
export interface HeightBook {
  /** 记录一条真实测量值。非法值（NaN / 负数）被忽略。 */
  record(id: string, height: number): void;
  /** 一条 item 的高度估计：有测量用测量，否则用中位数兜底。 */
  estimate(id: string): number;
  /** 一段区间的总高度估计。同一个 id 只计一次。 */
  estimateRange(ids: readonly string[]): number;
  /** 当前中位数（用于诊断与测试）。 */
  median(): number;
  /** 「真实总高 − 估算总高」，用于反向补偿 `scrollTop`。 */
  delta(ids: readonly string[], actualTotal: number): number;
}

export function createHeightBook(fallback = 40): HeightBook {
  const heights = new Map<string, number>();
  let sortedCache: number[] | null = null;

  const median = (): number => {
    if (heights.size === 0) return fallback;
    if (sortedCache === null) sortedCache = [...heights.values()].sort((a, b) => a - b);
    const mid = Math.floor(sortedCache.length / 2);
    return sortedCache.length % 2 === 1
      ? sortedCache[mid]
      : (sortedCache[mid - 1] + sortedCache[mid]) / 2;
  };

  const estimate = (id: string): number => heights.get(id) ?? median();

  // 闭包内直接调用，不用 `this.estimateRange`：对象字面量方法一旦被解构
  // （`const { delta } = book`）`this` 就是 undefined，会静默抛错。
  const estimateRange = (ids: readonly string[]): number => {
    let total = 0;
    const seen = new Set<string>();
    for (const id of ids) {
      if (seen.has(id)) continue;
      seen.add(id);
      total += estimate(id);
    }
    return total;
  };

  return {
    record(id, height) {
      if (!Number.isFinite(height) || height < 0) return;
      if (heights.get(id) === height) return;
      heights.set(id, height);
      sortedCache = null;
    },
    estimate,
    estimateRange,
    median,
    delta(ids, actualTotal) {
      return actualTotal - estimateRange(ids);
    },
  };
}
