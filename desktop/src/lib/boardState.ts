import type { BoardCard } from "../components/BoardView";

export function boardJsonPath(stateDir: string): string {
  return `${stateDir}/board.json`;
}

/**
 * Mirrors the Rust `yi_agent_board_ui::state::load_cards` mapping: queue order,
 * lowercased state, and `detail` falling back from workdir to the plan path.
 * Corrupt or unexpected input yields no cards rather than throwing.
 */
export function parseBoard(json: string): BoardCard[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return [];
  }
  const cards = (parsed as { cards?: unknown }).cards;
  if (!Array.isArray(cards)) return [];

  return cards
    .filter((card): card is Record<string, unknown> => typeof card === "object" && card !== null)
    .map((card) => ({
      id: String(card.id ?? ""),
      state: String(card.state ?? "").toLowerCase(),
      progress: null,
      detail: String(card.workdir ?? card.plan_path ?? ""),
    }))
    .filter((card) => card.id !== "")
    .sort((left, right) => {
      const leftOrder = orderOf(cards, left.id);
      const rightOrder = orderOf(cards, right.id);
      return leftOrder - rightOrder;
    });
}

function orderOf(cards: unknown[], id: string): number {
  for (const card of cards) {
    if (typeof card === "object" && card !== null) {
      const record = card as Record<string, unknown>;
      if (String(record.id ?? "") === id) {
        return typeof record.order === "number" ? record.order : 0;
      }
    }
  }
  return 0;
}
