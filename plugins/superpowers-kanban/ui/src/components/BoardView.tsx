import type { BoardCard } from "../lib/cards";
import type { SwitchSource } from "../lib/board";
import { Board } from "./Board";
import { Enqueue } from "./Enqueue";

/**
 * The main view: everything the board page shows, and the one place that
 * decides what "nothing to show" means.
 *
 * The three empty states are deliberately distinct. "Not installed" and
 * "switched off" both render an empty list if you only look at the cards, and
 * telling them apart is exactly what the user needs in order to act.
 */
export function BoardView({
  switchOn,
  source,
  cards,
  pluginMissing = false,
  expandedDone = false,
  busy = false,
  error = null,
  onToggleDone,
  onOpenThread,
  onPromote,
  onEnqueue,
  onPrequeue,
  onToggleSwitch,
}: {
  switchOn: boolean;
  source: SwitchSource;
  cards: BoardCard[];
  /** The backend never answered, so there is no board to render at all. */
  pluginMissing?: boolean;
  expandedDone?: boolean;
  busy?: boolean;
  error?: string | null;
  onToggleDone?: () => void;
  /** Open a card's session through the host's own session navigation. */
  onOpenThread?: (threadId: string) => void;
  /** Give a pre-queued card its spec and plan. */
  onPromote?: (cardId: string, specPath: string, planPath: string) => Promise<void>;
  onEnqueue?: (specPath: string, planPath: string) => Promise<void>;
  onPrequeue?: (title: string) => Promise<void>;
  onToggleSwitch?: (on: boolean) => void;
}) {
  if (pluginMissing) {
    return (
      <p className="kanban-notice" role="status">
        看板后端未就绪。插件负责回答看板的所有问题，启用后这里才会显示卡片。
      </p>
    );
  }
  return (
    <section className="kanban-view">
      <header className="kanban-view-header">
        <span className="kanban-view-title">Superpowers 看板</span>
        <span
          className={switchOn ? "kanban-dot kanban-dot-on" : "kanban-dot"}
          aria-hidden="true"
        />
        <span className="kanban-muted">{switchOn ? "运行中" : "已停用"}</span>
        <span className="kanban-subtle">({source})</span>
        <button
          type="button"
          className="kanban-switch"
          aria-label={switchOn ? "停用看板推进" : "启用看板推进"}
          aria-pressed={switchOn}
          disabled={busy}
          onClick={() => onToggleSwitch?.(!switchOn)}
        >
          {switchOn ? "停用" : "启用"}
        </button>
        {onEnqueue && onPrequeue ? (
          <Enqueue enqueue={onEnqueue} prequeue={onPrequeue} busy={busy} />
        ) : null}
      </header>

      {error ? (
        <p className="kanban-error" role="alert">
          {error}
        </p>
      ) : null}

      {!switchOn ? (
        <p className="kanban-notice" role="status">
          看板推进已停用（{source}）。卡片会保留，但队列不会被推进。
        </p>
      ) : cards.length === 0 ? (
        <p className="kanban-notice" role="status">
          看板是空的。用右上角的 ＋ 投递 spec/plan，或先记一个只有标题的想法。
        </p>
      ) : (
        <Board
          cards={cards}
          expandedDone={expandedDone}
          onToggleDone={onToggleDone}
          onOpenThread={onOpenThread}
          onPromote={onPromote}
        />
      )}
    </section>
  );
}
