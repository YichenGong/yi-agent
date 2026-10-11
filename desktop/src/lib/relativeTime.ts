/**
 * Compact relative time for cache/persisted timestamps, e.g. "now", "3m", "2h",
 * "5d". `now` is injectable so the unit is testable without freezing the clock.
 * A timestamp in the future (clock skew) clamps to "now" rather than rendering a
 * negative unit.
 */
export function formatRelativeTime(ms: number, now: number = Date.now()): string {
  const diff = Math.max(0, now - ms);
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return "now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour}h`;
  const day = Math.floor(hour / 24);
  return `${day}d`;
}
