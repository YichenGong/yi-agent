/**
 * 时间未知时的占位文案。
 *
 * 相对时间只能由**真实时间戳**算出来。0 / 负数 / NaN / Infinity / 缺值都不是
 * 时间戳（本模块的生产者取值一律是 `epoch 毫秒`，来自 `Date.now()` 或服务端
 * 的毫秒字段），它们唯一的含义是「不知道」。旧实现把 0 当作 1970 年算下去，
 * 于是 UI 上出现「更新于 20758d」这种谁也对不上的天数（用户报的「20xxxd」）。
 */
export const RELATIVE_TIME_UNKNOWN = "未知";

/**
 * 超过这个天数就判「不是人类会话的时间戳」。
 *
 * 取 10 年（3650 天）：真实会话积攒数年仍属正常，「5 年前」用户看得懂；而
 * 2 万天那种数只可能来自单位错误（秒当毫秒，差值约 20000 天）或零值兜底，
 * 正是本条要挡住的形状。
 */
const MAX_DAYS = 3_650;

/** 一个可用的 epoch 毫秒时间戳：有限且为正数。 */
function isTimestamp(ms: unknown): ms is number {
  return typeof ms === "number" && Number.isFinite(ms) && ms > 0;
}

/**
 * Compact relative time for cache/persisted timestamps, e.g. "now", "3m", "2h",
 * "5d". `now` is injectable so the unit is testable without freezing the clock.
 *
 * 两种「算不出来」的情形都返回 {@link RELATIVE_TIME_UNKNOWN}，而不是编一个数字：
 * - 入参不是时间戳（0 / 负数 / NaN / 缺值）——它不指向任何时刻；
 * - 差值超过 `MAX_DAYS` 天——那说明入参不是毫秒（漏乘 1000 的秒、单位错误的
 *   字段都会落在这条），继续按天显示只会得到一个无意义的大数。
 *
 * 未来时间（时钟偏差）仍夹到 "now" 而不是负数：那是**已知**的时刻，只是偏了。
 */
export function formatRelativeTime(ms: number, now: number = Date.now()): string {
  if (!isTimestamp(ms)) return RELATIVE_TIME_UNKNOWN;
  const diff = Math.max(0, now - ms);
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return "now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour}h`;
  const day = Math.floor(hour / 24);
  if (day > MAX_DAYS) return RELATIVE_TIME_UNKNOWN;
  return `${day}d`;
}
