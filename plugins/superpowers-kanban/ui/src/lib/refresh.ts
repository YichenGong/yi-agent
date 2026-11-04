/**
 * One poll tick.
 *
 * The plugin page has a single source of truth to refresh (its own board), but
 * the tick stays a list rather than a single callback: adding a second read
 * later must not require changing every mount site, and a tick must never be
 * held up by one refresher that fails — each is fired and forgotten, and each
 * swallows its own errors.
 */
export function makeBoardTick(...refreshers: Array<() => Promise<unknown>>): () => void {
  return () => {
    for (const refresh of refreshers) void refresh();
  };
}
