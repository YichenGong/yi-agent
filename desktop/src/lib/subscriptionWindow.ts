/**
 * The remote client's *warm* subscription window (S2, IM-style delivery).
 *
 * Delivery is split in three layers: List (`thread/started`,
 * `thread/status/updated`) and Global frames always reach every client, while
 * Content frames (`item/*`, `turn/*` …) are only delivered for threads the
 * client has explicitly subscribed to. A client therefore no longer needs to
 * subscribe to *every* thread — only the handful the user is actively
 * looking at — and the read-mostly long tail stays cold until opened.
 *
 * This class is that handful: an LRU of the most recently selected thread ids.
 * Touching a thread makes it the most-recent; once more than {@link
 * SUBSCRIPTION_WINDOW_SIZE} are tracked the least-recent is dropped. The
 * server also caps a single `thread/subscribe` at 16 ids, so the window must
 * stay at or below that.
 *
 * Pure and synchronous on purpose: no I/O, no globals, so it unit-tests
 * without a client. `App.tsx` owns the wiring.
 */

/** How many threads stay warm (subscribed) at once. Must be ≤ the server cap. */
export const SUBSCRIPTION_WINDOW_SIZE = 8;

/** An LRU window of thread ids kept warm via `thread/subscribe`. */
export class SubscriptionWindow {
  private ids: string[] = [];

  constructor(private readonly max: number = SUBSCRIPTION_WINDOW_SIZE) {}

  /**
   * Mark `id` as most-recently-used and return the resulting window (a fresh
   * array, most-recent first). Callers send this straight to
   * `thread/subscribe`.
   */
  touch(id: string): string[] {
    this.ids = [id, ...this.ids.filter((x) => x !== id)].slice(0, this.max);
    return [...this.ids];
  }

  /** Whether `id` is currently warm (i.e. already subscribed). */
  has(id: string): boolean {
    return this.ids.includes(id);
  }

  /** A snapshot copy of the current window, most-recent first. */
  current(): string[] {
    return [...this.ids];
  }
}
