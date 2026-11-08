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
  /**
   * 记录一条真实测量值；返回「这次是否真的改变了账本」。
   *
   * 非法值（`NaN` / 非有限 / `<= 0`）与「与已记值相同」都返回 `false`：零高的
   * 行到处都有（折叠条目、尚未布局），把它们当样本会把中位数拽向 0，spacer
   * 随之塌陷，用户看到整段内容消失。
   */
  record(id: string, height: number): boolean;
  /**
   * 账本修订号：每有一次**真实变化** +1。
   *
   * Spec §4.4 第 2 条要求中位数变化形成**合并后**的 revision，让 spacer、测量
   * 与补偿在同一个布局周期更新。下游按它判断「要不要重算」，而不是每帧重算或
   * 靠比较中位数（中位数可能很晚才动）。
   */
  revision(): number;
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
  let revision = 0;

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
      // `<= 0` 而不是 `< 0`：0 高不是有效测量（折叠/未布局），见接口注释。
      if (!Number.isFinite(height) || height <= 0) return false;
      if (heights.get(id) === height) return false;
      heights.set(id, height);
      sortedCache = null;
      revision += 1;
      return true;
    },
    revision() {
      return revision;
    },
    estimate,
    estimateRange,
    median,
    delta(ids, actualTotal) {
      return actualTotal - estimateRange(ids);
    },
  };
}
