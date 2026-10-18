import type { ThreadSummary } from "./protocol";

/**
 * Whether `cwd` is a kanban card worktree (`<project>/.worktrees/kanban/<slug>`).
 *
 * A card session's cwd is such a directory, and it is **not** a project: it has no
 * lead. Mirrors the server's `ThreadStore::is_card_worktree` — the two must agree,
 * or the sidebar would show a lead row the server no longer considers one.
 */
export function isCardWorktree(cwd: string): boolean {
  const parts = cwd.split("/").filter((p) => p !== "");
  let sawWorktrees = false;
  for (const part of parts) {
    if (part === "kanban" && sawWorktrees) return true;
    sawWorktrees = part === ".worktrees";
  }
  return false;
}

/**
 * Pick this project's lead thread out of a flat thread list.
 *
 * Mirrors the server rule (`ThreadStore::find_lead` + `is_lead_meta`): the lead is
 * the `role === "lead"` thread with the smallest `created_at` that does **not** live
 * in a kanban card worktree, so a transient duplicate converges on the same answer
 * here and on the server. An unknown `role` value is ignored rather than treated as
 * a lead — a future role must not silently take over the sidebar's lead slot.
 */
export function findLeadThread(threads: ThreadSummary[]): ThreadSummary | null {
  let best: ThreadSummary | null = null;
  for (const thread of threads) {
    if (thread.role !== "lead") continue;
    if (isCardWorktree(thread.cwd)) continue;
    if (best === null || thread.created_at < best.created_at) best = thread;
  }
  return best;
}
