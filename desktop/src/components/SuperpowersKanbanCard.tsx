import type { BoardCard } from "../lib/superpowersKanbanState";

/** id 中间省略：头尾各留一段，够认出是哪张卡。 */
function middleEllipsis(text: string, head = 14, tail = 10): string {
  if (text.length <= head + tail + 1) return text;
  return `${text.slice(0, head)}…${text.slice(-tail)}`;
}

export function SuperpowersKanbanCard({
  card,
  onOpenThread,
}: {
  card: BoardCard;
  onOpenThread?: (threadId: string) => void;
}) {
  const needsYou = card.state === "needs_you";
  return (
    <li className="rounded border border-line bg-panel p-2 text-sm">
      <div className="flex items-center gap-2">
        <span className="truncate font-medium text-fg" title={card.title ?? card.id}>
          {card.title ?? card.id}
        </span>
        {needsYou && (
          <span aria-label="需你处理" className="shrink-0 text-red-400">
            !
          </span>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs text-fg-subtle">
        <span className="font-mono" title={card.id}>
          {middleEllipsis(card.id)}
        </span>
        <span className="text-fg-muted">{card.state}</span>
      </div>
      {card.kind === "merge" && card.detail ? (
        <div className="mt-1 truncate font-mono text-xs text-fg-subtle" title={card.detail}>
          {card.detail}
        </div>
      ) : null}
      {card.threadId ? (
        <button
          type="button"
          onClick={() => onOpenThread?.(card.threadId as string)}
          className="mt-1 cursor-pointer font-mono text-xs text-fg-muted hover:text-fg hover:underline"
        >
          {card.threadId}
        </button>
      ) : null}
    </li>
  );
}
