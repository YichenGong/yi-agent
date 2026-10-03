import type { BoardCard } from "../lib/superpowersKanbanState";
import {
  DONE_COLLAPSED_LIMIT,
  boardColumns,
  collapseDone,
  type BoardColumnKey,
} from "../lib/superpowersKanbanBoard";
import { SuperpowersKanbanColumn } from "./SuperpowersKanbanColumn";

export function SuperpowersKanbanBoard({
  cards,
  expandedDone = false,
  onToggleDone,
  onOpenThread,
}: {
  cards: BoardCard[];
  expandedDone?: boolean;
  onToggleDone?: () => void;
  onOpenThread?: (threadId: string) => void;
}) {
  const columns = boardColumns(cards);
  const visibleDone = collapseDone(columns.done, expandedDone);
  const order: BoardColumnKey[] = ["queued", "doing", "needDecision", "done"];

  return (
    <div className="flex min-h-0 flex-1 gap-3 overflow-x-auto p-3">
      {order.map((key) => {
        const all = columns[key];
        const shown = key === "done" ? visibleDone : all;
        return (
          <SuperpowersKanbanColumn
            key={key}
            columnKey={key}
            cards={shown}
            totalCount={all.length}
            onOpenThread={onOpenThread}
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
