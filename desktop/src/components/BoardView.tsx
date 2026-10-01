import type { SwitchSource } from "../lib/boardSwitch";

export interface BoardCard {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
}

export function BoardView({
  switchOn,
  source,
  cards,
}: {
  switchOn: boolean;
  source: SwitchSource;
  cards: BoardCard[];
}) {
  if (!switchOn) {
    return (
      <div className="p-4 text-sm text-neutral-400">
        Superpowers 看板 is disabled ({source}). Enable it in settings.
      </div>
    );
  }
  if (cards.length === 0) {
    return <div className="p-4 text-sm text-neutral-400">Superpowers 看板 is empty.</div>;
  }
  return (
    <ul className="divide-y divide-neutral-800">
      {cards.map((card) => (
        <li key={card.id} className="flex items-center gap-3 p-3 text-sm">
          <span className="font-mono text-neutral-300">{card.id}</span>
          <span className="text-neutral-400">{card.state}</span>
          {card.progress ? (
            <span className="text-neutral-500">({card.progress})</span>
          ) : null}
          {card.detail ? (
            <span className="truncate text-neutral-500">{card.detail}</span>
          ) : null}
        </li>
      ))}
    </ul>
  );
}
