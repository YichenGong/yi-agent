import type { SwitchSource } from "../lib/superpowersKanbanSwitch";

export function SuperpowersKanbanSettings({
  switchOn,
  source,
  onToggle,
  onCollapse,
}: {
  switchOn: boolean;
  source: SwitchSource;
  onToggle: (next: boolean) => void;
  /** Supplied by a host that can collapse the panel; omitted, no button shows. */
  onCollapse?: () => void;
}) {
  return (
    <section className="p-4">
      <div className="flex items-center justify-between">
        <label className="flex items-center gap-3 text-sm">
          <input
            type="checkbox"
            checked={switchOn}
            onChange={(event) => onToggle(event.target.checked)}
          />
          <span>Superpowers 看板</span>
          <span className="text-fg-subtle">({source})</span>
        </label>
        {onCollapse && (
          <button
            type="button"
            aria-label="收起看板"
            className="text-xs text-fg-subtle hover:text-fg-muted"
            onClick={onCollapse}
          >
            收起
          </button>
        )}
      </div>
      <p className="mt-2 text-xs text-fg-subtle">
        Off by default. Turning it off stops the plugin from advancing the queue; it
        does not cancel sessions already running in the daemon.
      </p>
    </section>
  );
}
