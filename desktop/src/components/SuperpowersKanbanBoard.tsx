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
  const order: BoardColumnKey[] = ["preQueued", "queued", "doing", "needDecision", "done"];

  return (
    // 宽度自适应：行容器**不再**声明 `overflow-x-auto`（那就是左右滚动本身）。
    // 宽屏（`md` 起）五列并排（最左常驻 PRE-QUEUED）、各自 `flex-1 min-w-0` 均分宽度，长标题在卡内截断；
    // 窄屏纵向堆叠、整行上下滚动——宁可上下滑，不要左右滑。
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3 md:flex-row">
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
