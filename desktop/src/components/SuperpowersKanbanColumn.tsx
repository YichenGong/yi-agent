import type { BoardCard } from "../lib/superpowersKanbanState";
import type { BoardColumnKey } from "../lib/superpowersKanbanBoard";
import { SuperpowersKanbanCard } from "./SuperpowersKanbanCard";

const COLUMN_LABEL: Record<BoardColumnKey, string> = {
  preQueued: "PRE-QUEUED",
  queued: "QUEUED",
  doing: "DOING",
  needDecision: "NEED DECISION",
  done: "DONE",
};

export function SuperpowersKanbanColumn({
  columnKey,
  cards,
  totalCount,
  onOpenThread,
  doneToggle,
}: {
  columnKey: BoardColumnKey;
  /** 本列要画出的卡（done 列的折叠已在调用方完成）。 */
  cards: BoardCard[];
  /** 该列卡片总数；done 列计数含被折叠掉的卡。 */
  totalCount: number;
  onOpenThread?: (threadId: string) => void;
  /** 只有 done 列会传：右侧的「展开全部 / 收起」开关。 */
  doneToggle?: { expanded: boolean; onToggle: () => void; hasMore: boolean };
}) {
  const needsDecision = columnKey === "needDecision";
  return (
    // `min-w-0`：flex 项默认 `min-width:auto` 会取内容最小宽度，卡片标题是
    // `truncate`（nowrap），其 min-content 就是整条标题宽——不解除这一项，列会拒绝
    // 收缩、把整行撑宽（用户报的「左右过宽」）。窄屏堆叠时给个高度下限，让整行走
    // 上下滚动而不是把各列压成等高的可滚区。
    <section
      aria-label={`看板列 ${COLUMN_LABEL[columnKey]}`}
      className={`flex min-h-0 min-w-0 flex-1 flex-col rounded border max-md:min-h-[16rem] ${
        needsDecision ? "border-red-500/40" : "border-line"
      } bg-surface`}
    >
      <header className="flex items-center justify-between px-3 py-2">
        <span
          className={`text-xs font-semibold tracking-wide ${
            needsDecision ? "text-red-400" : "text-fg-muted"
          }`}
        >
          {COLUMN_LABEL[columnKey]} · {totalCount}
        </span>
        {doneToggle && doneToggle.hasMore && (
          <button
            type="button"
            className="cursor-pointer text-xs text-fg-subtle hover:text-fg"
            onClick={doneToggle.onToggle}
          >
            {doneToggle.expanded ? "收起" : "展开全部"}
          </button>
        )}
      </header>
      <ul className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto px-2 pb-2">
        {cards.length === 0 ? (
          <li className="px-1 py-2 text-xs text-fg-subtle">空</li>
        ) : (
          cards.map((card) => (
            <SuperpowersKanbanCard key={card.id} card={card} onOpenThread={onOpenThread} />
          ))
        )}
      </ul>
    </section>
  );
}
