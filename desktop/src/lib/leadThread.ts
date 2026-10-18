import type { ThreadSummary } from "./protocol";

/**
 * Pick this project's lead thread out of a flat thread list.
 *
 * Mirrors the server rule (`ThreadStore::find_lead`): the lead is the
 * `role === "lead"` thread with the smallest `created_at`, so a transient
 * duplicate converges on the same answer here and on the server. An unknown
 * `role` value is ignored rather than treated as a lead — a future role must
 * not silently take over the sidebar's lead slot.
 */
export function findLeadThread(threads: ThreadSummary[]): ThreadSummary | null {
  let best: ThreadSummary | null = null;
  for (const thread of threads) {
    if (thread.role !== "lead") continue;
    if (best === null || thread.created_at < best.created_at) best = thread;
  }
  return best;
}