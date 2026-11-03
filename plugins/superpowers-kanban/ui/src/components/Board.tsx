import type { BoardCard } from "../lib/cards";
import { DONE_COLLAPSED_LIMIT, boardColumns, collapseDone, type BoardColumnKey } from "../lib/columns";
import { Column } from "./Column";

const ORDER: BoardColumnKey[] = ["preQueued", "queued", "doing", "needDecision", "done"];

/**
 * Five columns, laid out side by side on wide screens and stacked when narrow.
 *
 * The row container deliberately does *not* scroll horizontally: a card's title
 * is single-line with ellipsis, so a horizontally scrolling row would put
 * content off screen with no visible affordance. Stacking keeps everything
 * reachable by scrolling in one direction.
 */
export function Board({
  cards,
  expandedDone = false,
  onToggleDone,
  onOpenThread,
  onPromote,
}: {
  cards: BoardCard[];
  expandedDone?: boolean;
  onToggleDone?: () => void;
  onOpenThread?: (threadId: string) => void;
  onPromote?: (cardId: string, specPath: string, planPath: string) => Promise<void>;
}) {
  const columns = boardColumns(cards);
  const visibleDone = collapseDone(columns.done, expandedDone);

  return (
    <div className="kanban-board">
      {ORDER.map((key) => {
        const all = columns[key];
        const shown = key === "done" ? visibleDone : all;
        return (
          <Column
            key={key}
            columnKey={key}
            cards={shown}
            totalCount={all.length}
            onOpenThread={onOpenThread}
            onPromote={onPromote}
            doneToggle={
              key === "done"
                ? {
                    expanded: expandedDone,
                    onToggle: () => onToggleDone?.(),
                    hasMore: all.length > DONE_COLLAPSED_LIMIT,
                  }
                : undefined
            }
          />
        );
      })}
    </div>
  );
}
