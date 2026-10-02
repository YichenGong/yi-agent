import type { SwitchSource } from "../lib/superpowersKanbanSwitch";

export interface BoardCard {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
  /** 该卡关联的会话；可为空（插件还没起会话）。 */
  threadId?: string | null;
}

export function SuperpowersKanbanView({
  switchOn,
  source,
  cards,
  pluginMissing = false,
  onOpenThread,
}: {
  switchOn: boolean;
  source: SwitchSource;
  cards: BoardCard[];
  /** The plugin never answered, so there is no board to render at all. */
  pluginMissing?: boolean;
  /** 打开某张卡关联的会话；上层接到既有的 thread/resume 入口。 */
  onOpenThread?: (threadId: string) => void;
}) {
  if (pluginMissing) {
    return (
      <div className="p-4 text-sm text-fg-muted">
        Superpowers 看板插件未安装。插件负责回答看板的所有问题，装上它这里才会显示卡片。
      </div>
    );
  }
  if (!switchOn) {
    return (
      <div className="p-4 text-sm text-fg-muted">
        Superpowers 看板 is disabled ({source}). Enable it in settings.
      </div>
    );
  }
  if (cards.length === 0) {
    return <div className="p-4 text-sm text-fg-muted">Superpowers 看板 is empty.</div>;
  }
  return (
    <ul className="divide-y divide-line">
      {cards.map((card) => (
        <li key={card.id} className="flex items-center gap-3 p-3 text-sm">
          <span className="font-mono text-fg-muted">{card.id}</span>
          <span className="text-fg-muted">{card.state}</span>
          {card.progress ? (
            <span className="text-fg-subtle">({card.progress})</span>
          ) : null}
          {card.detail ? (
            <span className="truncate text-fg-subtle">{card.detail}</span>
          ) : null}
          {card.threadId ? (
            <button
              type="button"
              onClick={() => onOpenThread?.(card.threadId as string)}
              className="ml-auto shrink-0 cursor-pointer font-mono text-fg-muted hover:text-fg hover:underline"
            >
              {card.threadId}
            </button>
          ) : null}
        </li>
      ))}
    </ul>
  );
}
