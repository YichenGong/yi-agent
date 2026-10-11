import type { Item } from "./protocol";

/** Per-entry cap: how large a single thread's cached transcript may get. */
export const MAX_ENTRY_BYTES = 1024 * 1024;

/** Global caps over the whole content cache: entry count and total bytes. */
export const CONTENT_LIMITS = { maxCount: 10, maxBytes: 5 * 1024 * 1024 };

/** Byte size of a serialized item list. UTF-8 length is close enough to what
 *  the storage engine holds, and it is what the budget is defined against. */
export function estimateItemsBytes(items: Item[]): number {
  if (items.length === 0) return 0;
  return new TextEncoder().encode(JSON.stringify(items)).length;
}

/** The slice of a cache entry the eviction policy needs. */
export interface EntryMeta {
  threadId: string;
  savedAt: number;
  bytes: number;
}

/**
 * Drop the oldest items from the head until the serialized list fits `maxBytes`.
 *
 * Trimming from the head keeps the *tail* — the most recent exchange, which is
 * what a user opening a thread cares about. At least one item survives even if it
 * alone busts the budget: an empty transcript is worse than one oversized item,
 * and a single item is already bounded by the frame size the server produces.
 */
export function applyTrim(
  items: Item[],
  maxBytes: number,
): { items: Item[]; trimmed: boolean } {
  let working = items;
  let trimmed = false;
  while (working.length > 1 && estimateItemsBytes(working) > maxBytes) {
    working = working.slice(1);
    trimmed = true;
  }
  return { items: working, trimmed };
}

/**
 * Ids to evict, oldest first, until both the count and byte bounds hold.
 *
 * `savedAt` is refreshed on every write, so ordering by it is LRU: the least
 * recently touched thread is evicted first. Pure + total, so the policy is unit
 * tested without a database.
 */
export function selectEvictions(
  metas: EntryMeta[],
  limits: { maxCount: number; maxBytes: number },
): string[] {
  const sorted = [...metas].sort((a, b) => a.savedAt - b.savedAt);
  let count = sorted.length;
  let total = sorted.reduce((sum, m) => sum + m.bytes, 0);
  const evicted: string[] = [];
  for (const m of sorted) {
    if (count <= limits.maxCount && total <= limits.maxBytes) break;
    evicted.push(m.threadId);
    count -= 1;
    total -= m.bytes;
  }
  return evicted;
}
