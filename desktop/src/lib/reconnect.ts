/**
 * Reconnect timing for the App layer, which is the single owner of reconnection
 * (see `App.tsx`'s connect effect). Kept pure and independent of any transport
 * so the schedule can be tested exactly, and so `wsTransport.ts` stays as-is.
 */

/** First retry delay, in ms. */
export const RECONNECT_BASE_MS = 500;

/** Ceiling for the doubling schedule, in ms. */
export const RECONNECT_MAX_MS = 10000;

const clampAttempt = (attempt: number): number => {
  // A counter that ran away to +Infinity is "very many attempts" → the cap.
  if (attempt === Infinity) return Number.MAX_SAFE_INTEGER;
  // NaN / -Infinity / negative counters are upstream bugs, but a bad value
  // must not become a negative (hot-loop) or NaN timeout. Pin them to base.
  if (!Number.isFinite(attempt) || attempt < 0) return 0;
  return Math.floor(attempt);
};

/**
 * Delay before retry number `attempt` (0 = the first retry after a drop):
 * 500, 1000, 2000, 4000, 8000, then capped at 10000.
 */
export function nextReconnectDelay(attempt: number): number {
  const n = clampAttempt(attempt);
  // Cap the exponent before doubling so a huge `attempt` cannot overflow to
  // Infinity (2 ** 1024 === Infinity, which Math.min would then clamp anyway,
  // but staying finite keeps the intent obvious).
  if (n >= 5) return RECONNECT_MAX_MS;
  return RECONNECT_BASE_MS * 2 ** n;
}
