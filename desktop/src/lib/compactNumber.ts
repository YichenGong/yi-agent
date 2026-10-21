/**
 * 把 token 数压成右下角能放下的短串（`1.2k` / `1.23M`）。
 *
 * 拆成「纯数字位 + 小数位数 + 单位」三段而不是直接给字符串：状态栏用逐位滚动的
 * 动画渲染，它需要每位数字单独成一个节点，小数点与单位则由 CSS 摆放。
 */
export interface CompactParts {
  /** 去掉小数点的全部数字位，例如 `1.23M` → `"123"`。 */
  digits: string;
  /** 末几位是小数。`0` 表示这是整数。 */
  fractionDigits: number;
  /** 空串、`k`、`M`、`G`、`T`。 */
  unit: string;
}

const UNITS = ["", "k", "M", "G", "T"] as const;

/** 每个单位的显示精度：千位留一位小数，百万以上留两位。 */
function precisionOf(index: number): number {
  if (index === 0) return 0;
  return index === 1 ? 1 : 2;
}

export function compact(n: number): CompactParts {
  // token 数不会是负数；真拿到负数（上游算错）也按 0 显示，不往下游传怪值。
  let value = Number.isFinite(n) && n > 0 ? n : 0;
  let index = 0;
  while (value >= 1000 && index < UNITS.length - 1) {
    value /= 1000;
    index += 1;
  }

  for (;;) {
    const fractionDigits = precisionOf(index);
    const scale = 10 ** fractionDigits;
    const scaled = Math.round(value * scale);
    // 取整后整数部分到了 1000（如 999.95k → 1000.0k），就升到下一个单位重算，
    // 否则会出现 `1000.0k` 这种比 `1.00M` 更长也更难读的写法。
    if (scaled >= 1000 * scale && index < UNITS.length - 1) {
      value /= 1000;
      index += 1;
      continue;
    }
    // 补齐前导零：0.1k 的 `scaled` 是 1，得渲染成 `"01"` 才能插出 `0.1`。
    const digits = fractionDigits > 0 ? String(scaled).padStart(fractionDigits + 1, "0") : String(scaled);
    return { digits, fractionDigits, unit: UNITS[index] };
  }
}

/** 一行的短串形式，给非动画场景（如提示文本）用。 */
export function formatCompact(n: number): string {
  const { digits, fractionDigits, unit } = compact(n);
  if (fractionDigits === 0) return digits + unit;
  const cut = digits.length - fractionDigits;
  return `${digits.slice(0, cut)}.${digits.slice(cut)}${unit}`;
}
