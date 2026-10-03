import type { BoardCard } from "../components/SuperpowersKanbanView";

export function boardJsonPath(stateDir: string): string {
  return `${stateDir}/board.json`;
}

/**
 * Maps the plugin's `list` result — the `board.json` cards the runner owns under
 * `<project>/.yi-agent/superpowers-kanban/` — onto the card rows the board panel
 * renders: a non-object root or non-array `cards` yields no cards, each card is
 * validated individually (only a card missing id / state is skipped, the rest are
 * kept — a missing `plan_path` is no longer a reason to drop a card, since merge
 * cards have none), `state` is lowercased, `detail` falls back from workdir to the
 * plan path (including when workdir is an empty string) and, for a merge card, to
 * its `source → base` refs, and cards are sorted by `order` ascending with ties
 * keeping file order. Corrupt or unexpected input yields no cards rather than
 * throwing.
 *
 * The runner's own `dispatch` builds this shape for `list` and deliberately does
 * not forward `order` (the queue's order is the file's), so the sort below is a
 * no-op in practice — it stays as the guard for a caller that does send one.
 */
export function parseBoard(json: string): BoardCard[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return [];
  }
  if (typeof parsed !== "object" || parsed === null) return [];
  const cards = (parsed as { cards?: unknown }).cards;
  if (!Array.isArray(cards)) return [];

  const mapped: Array<{ card: BoardCard; order: number }> = [];
  for (const raw of cards) {
    if (typeof raw !== "object" || raw === null) continue;
    const record = raw as Record<string, unknown>;
    const id = typeof record.id === "string" ? record.id : "";
    const planPath = typeof record.plan_path === "string" ? record.plan_path : "";
    const state = typeof record.state === "string" ? record.state : "";
    const workdir = typeof record.workdir === "string" ? record.workdir : "";
    const kind = typeof record.kind === "string" ? record.kind : "implementation";
    const source = typeof record.source === "string" ? record.source : "";
    const base = typeof record.base === "string" ? record.base : "";
    const order = typeof record.order === "number" ? record.order : 0;
    const threadId =
      typeof record.thread_id === "string" && record.thread_id !== ""
        ? record.thread_id
        : null;
    // 合并卡没有 plan_path，不能再按 plan_path 丢弃；只按 id / state 校验。
    if (id === "" || state === "") continue;
    const detail =
      workdir !== ""
        ? workdir
        : kind === "merge"
          ? `${source} → ${base}`
          : planPath;
    mapped.push({
      card: {
        id,
        state: state.toLowerCase(),
        progress: null,
        detail,
        threadId,
      },
      order,
    });
  }

  return mapped.sort((left, right) => left.order - right.order).map((entry) => entry.card);
}
