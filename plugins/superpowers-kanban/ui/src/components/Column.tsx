import type { BoardCard } from "../lib/cards";
import type { BoardColumnKey } from "../lib/columns";
import { Card } from "./Card";

const LABEL: Record<BoardColumnKey, string> = {
  preQueued: "PRE-QUEUED",
  queued: "QUEUED",
  doing: "DOING",
  needDecision: "NEED DECISION",
  done: "DONE",
};

export function Column({
  columnKey,
  cards,
  totalCount,
  onOpenThread,
  onPromote,
  doneToggle,
}: {
  columnKey: BoardColumnKey;
  /** Cards to draw; the done column's collapse happened in the caller. */
  cards: BoardCard[];
  /** Total in this column, including any collapsed out of `cards`. */
  totalCount: number;
  onOpenThread?: (threadId: string) => void;
  onPromote?: (cardId: string, specPath: string, planPath: string) => Promise<void>;
  /** Only the done column passes this: the expand/collapse control. */
  doneToggle?: { expanded: boolean; onToggle: () => void; hasMore: boolean };
}) {
  const needsDecision = columnKey === "needDecision";
  return (
    <section
      aria-label={`看板列 ${LABEL[columnKey]}`}
      className={needsDecision ? "kanban-column kanban-column-urgent" : "kanban-column"}
    >
      <header className="kanban-column-header">
        <span className={needsDecision ? "kanban-column-title urgent" : "kanban-column-title"}>
          {LABEL[columnKey]} · {totalCount}
        </span>
        {doneToggle && doneToggle.hasMore ? (
          <button type="button" className="kanban-link" onClick={doneToggle.onToggle}>
            {doneToggle.expanded ? "收起" : "展开全部"}
          </button>
        ) : null}
      </header>
      <ul className="kanban-column-list">
        {cards.length === 0 ? (
          <li className="kanban-empty">空</li>
        ) : (
          cards.map((card) => (
            <Card
              key={card.id}
              card={card}
              onOpenThread={onOpenThread}
              onPromote={onPromote}
            />
          ))
        )}
      </ul>
    </section>
  );
}
