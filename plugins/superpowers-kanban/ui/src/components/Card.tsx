import { useState } from "react";
import type { BoardCard } from "../lib/cards";

/** Shorten an id in the middle so both ends stay recognisable. */
function middleEllipsis(text: string, head = 14, tail = 10): string {
  if (text.length <= head + tail + 1) return text;
  return `${text.slice(0, head)}…${text.slice(-tail)}`;
}

export function Card({
  card,
  onOpenThread,
  onPromote,
}: {
  card: BoardCard;
  onOpenThread?: (threadId: string) => void;
  /**
   * Pre-queued cards only: supply spec/plan so the card can be promoted.
   *
   * The paths are typed for the same reason the enqueue form asks for them —
   * the host bridge has no file picker, and a WebView file input does not
   * reveal real paths. The backend validates both files before delivering.
   */
  onPromote?: (cardId: string, specPath: string, planPath: string) => Promise<void>;
}) {
  const [promoting, setPromoting] = useState(false);
  const [spec, setSpec] = useState("");
  const [plan, setPlan] = useState("");
  const [promoteError, setPromoteError] = useState<string | null>(null);
  const [promotingBusy, setPromotingBusy] = useState(false);
  const needsYou = card.state === "needs_you";
  // One session link per card, at every stage: before the card runs, the link
  // points at the discussion that proposed it; once it runs, it switches to the
  // execution session. Showing both would be two answers to "where is this".
  const sessionId = card.threadId ?? card.originThreadId;
  return (
    <li className="kanban-card">
      <div className="kanban-card-title-row">
        <span className="kanban-card-title" title={card.title ?? card.id}>
          {card.title ?? card.id}
        </span>
        {needsYou ? (
          <span aria-label="需你处理" className="kanban-badge urgent">
            !
          </span>
        ) : null}
        {card.merging ? (
          <span
            aria-label="正在合并"
            className="kanban-badge info"
            title="这张卡持有本项目的合并名额，正在合并"
          >
            ⇄
          </span>
        ) : null}
        {card.stale ? (
          <span
            aria-label="会话已不在"
            className="kanban-badge warn"
            title="卡片写着 running，但会话已经不在了"
          >
            ⚠
          </span>
        ) : null}
      </div>

      <div className="kanban-card-meta">
        <span className="kanban-mono" title={card.id}>
          {middleEllipsis(card.id)}
        </span>
        <span className="kanban-muted">{card.state}</span>
      </div>

      {card.kind === "merge" && card.detail ? (
        <div className="kanban-card-detail kanban-mono" title={card.detail}>
          {card.detail}
        </div>
      ) : null}

      {card.stale && card.lastAliveAt ? (
        <div className="kanban-card-detail">
          最后活动{" "}
          {new Date(card.lastAliveAt).toLocaleTimeString("zh-CN", {
            hour: "2-digit",
            minute: "2-digit",
          })}
        </div>
      ) : null}

      {sessionId ? (
        <button
          type="button"
          className="kanban-session-link"
          onClick={() => onOpenThread?.(sessionId)}
        >
          {sessionId}
        </button>
      ) : null}

      {card.state === "pre_queued" && onPromote ? (
        promoting ? (
          <div className="kanban-card-form" role="group" aria-label="补 spec/plan 提升">
            <input
              aria-label="spec 路径"
              value={spec}
              onChange={(event) => setSpec(event.currentTarget.value)}
              placeholder="/abs/path/to/change.spec.md"
              className="kanban-input"
            />
            <input
              aria-label="plan 路径"
              value={plan}
              onChange={(event) => setPlan(event.currentTarget.value)}
              className="kanban-input"
            />
            <button
              type="button"
              className="kanban-button"
              disabled={promotingBusy || spec.trim() === "" || plan.trim() === ""}
              onClick={() => {
                setPromoteError(null);
                setPromotingBusy(true);
                void onPromote(card.id, spec.trim(), plan.trim())
                  // A failed promote leaves the form open with its paths, so the
                  // user can fix a typo rather than retype both.
                  .catch((e: unknown) => {
                    setPromoteError(e instanceof Error ? e.message : String(e));
                  })
                  .finally(() => setPromotingBusy(false));
              }}
            >
              提升
            </button>
            <button
              type="button"
              className="kanban-link"
              onClick={() => {
                setPromoting(false);
                setPromoteError(null);
              }}
            >
              取消
            </button>
            {promoteError !== null ? (
              <p className="kanban-error" role="alert">
                {promoteError}
              </p>
            ) : null}
          </div>
        ) : (
          <button type="button" className="kanban-link" onClick={() => setPromoting(true)}>
            补 spec/plan 提升
          </button>
        )
      ) : null}
    </li>
  );
}
