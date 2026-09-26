import type { Usage } from "./protocol";

/** 单价:USD / 1M tokens。 */
export interface Price {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/**
 * 模型 id 前缀 → 单价。按最长前缀匹配。
 *
 * 单价为官方公开定价(USD / 1M tokens),核对日期 2026-09-27。官方调价后需手动更新。
 * OpenAI 的自动 prompt caching 不额外计费写入,故 cacheWrite = 0。
 */
const PRICES: Array<[prefix: string, price: Price]> = [
  ["claude-opus-4", { input: 15, output: 75, cacheRead: 1.5, cacheWrite: 18.75 }],
  ["claude-sonnet-4", { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 }],
  ["claude-3-5-sonnet", { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 }],
  ["claude-3-5-haiku", { input: 0.8, output: 4, cacheRead: 0.08, cacheWrite: 1 }],
  ["gpt-4o-mini", { input: 0.15, output: 0.6, cacheRead: 0.075, cacheWrite: 0 }],
  ["gpt-4o", { input: 2.5, output: 10, cacheRead: 1.25, cacheWrite: 0 }],
  ["gpt-4.1", { input: 2, output: 8, cacheRead: 0.5, cacheWrite: 0 }],
  ["o3", { input: 10, output: 40, cacheRead: 2.5, cacheWrite: 0 }],
  ["o1", { input: 15, output: 60, cacheRead: 7.5, cacheWrite: 0 }],
];

/** 模型对应的单价;未知模型返回 null。最长前缀优先。 */
export function priceFor(model: string): Price | null {
  let best: { prefix: string; price: Price } | null = null;
  for (const [prefix, price] of PRICES) {
    if (model.startsWith(prefix) && (!best || prefix.length > best.prefix.length)) {
      best = { prefix, price };
    }
  }
  return best ? best.price : null;
}

/** 估算成本(USD);未知模型返回 null。 */
export function estimateCost(u: Usage): number | null {
  const p = priceFor(u.model);
  if (!p) return null;
  return (
    (u.input * p.input +
      u.output * p.output +
      u.cacheRead * p.cacheRead +
      u.cacheWrite * p.cacheWrite) /
    1_000_000
  );
}

/** 成本展示:未知 → "—";非零但 < $0.01 → "< $0.01";否则 4 位小数。 */
export function formatCost(cost: number | null): string {
  if (cost === null) return "—";
  if (cost > 0 && cost < 0.01) return "< $0.01";
  return `$${cost.toFixed(4)}`;
}
