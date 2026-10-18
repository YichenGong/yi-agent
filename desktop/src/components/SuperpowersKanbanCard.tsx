import type { BoardCard } from "../lib/superpowersKanbanState";

/** id 中间省略：头尾各留一段，够认出是哪张卡。 */
function middleEllipsis(text: string, head = 14, tail = 10): string {
  if (text.length <= head + tail + 1) return text;
  return `${text.slice(0, head)}…${text.slice(-tail)}`;
}

export function SuperpowersKanbanCard({
  card,
  onOpenThread,
  onPromote,
}: {
  card: BoardCard;
  onOpenThread?: (threadId: string) => void;
  /** 预排队卡专用：补上 spec/plan，把卡提升成排队卡。 */
  onPromote?: (cardId: string) => void;
}) {
  const needsYou = card.state === "needs_you";
  // 一张卡同一时刻只显示一个会话链接：pre-queued/queued 阶段 `threadId` 为空，
  // 显示提出它的**讨论会话**；卡跑起来后 `threadId` 有值，自动切到**执行会话**。
  const sessionId = card.threadId ?? card.originThreadId;
  return (
    // `min-w-0`：卡片要能被列压窄，标题的 `truncate` 才真正生效。
    <li className="min-w-0 rounded border border-line bg-panel p-2 text-sm">
      <div className="flex min-w-0 items-center gap-2">
        <span className="min-w-0 truncate font-medium text-fg" title={card.title ?? card.id}>
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
      {sessionId ? (
        <button
          type="button"
          onClick={() => onOpenThread?.(sessionId)}
          className="mt-1 block max-w-full cursor-pointer truncate text-left font-mono text-xs text-fg-muted hover:text-fg hover:underline"
        >
          {sessionId}
        </button>
      ) : null}
      {card.state === "pre_queued" && onPromote ? (
        <button
          type="button"
          onClick={() => onPromote(card.id)}
          className="mt-1 text-xs text-fg-muted hover:text-fg"
        >
          补 spec/plan 提升
        </button>
      ) : null}
    </li>
  );
}
