import type { SwitchSource } from "../lib/boardSwitch";

export function SettingsPanel({
  switchOn,
  source,
  onToggle,
}: {
  switchOn: boolean;
  source: SwitchSource;
  onToggle: (next: boolean) => void;
}) {
  return (
    <section className="p-4">
      <label className="flex items-center gap-3 text-sm">
        <input
          type="checkbox"
          checked={switchOn}
          onChange={(event) => onToggle(event.target.checked)}
        />
        <span>Superpowers 看板</span>
        <span className="text-neutral-500">({source})</span>
      </label>
      <p className="mt-2 text-xs text-neutral-500">
        Off by default. Turning it off stops the plugin from advancing the queue; it
        does not cancel sessions already running in the daemon.
      </p>
    </section>
  );
}
