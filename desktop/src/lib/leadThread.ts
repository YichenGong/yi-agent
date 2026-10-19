import type { ThreadSummary } from "./protocol";

/**
 * Whether `cwd` is a kanban-plugin worktree: a card worktree
 * (`<project>/.worktrees/kanban/<slug>`) or the merge round's base worktree
 * (`<project>/.worktrees/kanban-merge/<slug>`, used when the base branch is not
 * checked out anywhere).
 *
 * Both are directories the plugin carved out for one card, so neither is a
 * project and neither has a lead. Mirrors the server's `ThreadStore::is_kanban_worktree`
 * — the two must agree, or the sidebar would show a lead row the server no longer
 * considers one. A user's own `.worktrees/<branch>` is not in that namespace.
 */
export function isKanbanWorktree(cwd: string): boolean {
  const parts = cwd.split("/").filter((p) => p !== "");
  let sawWorktrees = false;
  for (const part of parts) {
    if (sawWorktrees && (part === "kanban" || part.startsWith("kanban-"))) return true;
    sawWorktrees = part === ".worktrees";
  }
  return false;
}

/**
 * Pick this project's lead thread out of a flat thread list.
 *
 * Mirrors the server rule (`ThreadStore::find_lead` + `is_lead_meta`): the lead is
 * the `role === "lead"` thread with the smallest `created_at` that does **not** live
 * in a kanban worktree, so a transient duplicate converges on the same answer
 * here and on the server. An unknown `role` value is ignored rather than treated as
 * a lead — a future role must not silently take over the sidebar's lead slot.
 */
export function findLeadThread(threads: ThreadSummary[]): ThreadSummary | null {
  let best: ThreadSummary | null = null;
  for (const thread of threads) {
    if (thread.role !== "lead") continue;
    if (isKanbanWorktree(thread.cwd)) continue;
    if (best === null || thread.created_at < best.created_at) best = thread;
  }
  return best;
}
