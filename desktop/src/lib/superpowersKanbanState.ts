import type { BoardCard } from "../components/SuperpowersKanbanView";

export function boardJsonPath(stateDir: string): string {
  return `${stateDir}/board.json`;
}

/**
 * Mirrors the Rust `yi_agent_board_ui::state::load_cards` mapping line for line:
 * a non-object root or non-array `cards` yields no cards, each card is validated
 * individually (a card missing id / plan_path / state is skipped, the rest are
 * kept), `state` is lowercased, `detail` falls back from workdir to the plan
 * path (including when workdir is an empty string), and cards are sorted by
 * `order` ascending with ties keeping file order. Corrupt or unexpected input
 * yields no cards rather than throwing.
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
    const order = typeof record.order === "number" ? record.order : 0;
    if (id === "" || planPath === "" || state === "") continue;
    mapped.push({
      card: {
        id,
        state: state.toLowerCase(),
        progress: null,
        detail: workdir !== "" ? workdir : planPath,
      },
      order,
    });
  }

  return mapped.sort((left, right) => left.order - right.order).map((entry) => entry.card);
}
