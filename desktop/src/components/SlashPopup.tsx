import type { SlashCommandSpec } from "../lib/slash";

/**
 * The command list shown above the input while the user types `/`.
 *
 * Presentational only: the parent owns filtering and the selected index. A
 * plain list of divs (not `role="listbox"`) because the textarea keeps DOM
 * focus — the popup is a visual hint driven by keyboard events on the textarea.
 */
export function SlashPopup({
  commands,
  selected,
}: {
  commands: SlashCommandSpec[];
  selected: number;
}) {
  if (commands.length === 0) return null;
  return (
    <div
      data-testid="slash-popup"
      className="absolute bottom-full left-0 mb-1 max-h-64 w-full overflow-y-auto rounded-md border border-line-strong bg-panel py-1 shadow-lg"
    >
      {commands.map((c, i) => (
        <div
          key={c.name}
          data-testid="slash-option"
          data-selected={i === selected ? "true" : "false"}
          className={
            i === selected
              ? "flex items-baseline gap-2 bg-raised px-3 py-1 text-sm"
              : "flex items-baseline gap-2 px-3 py-1 text-sm"
          }
        >
          <span className="font-mono text-fg">
            /{c.name}
            {c.usage ? ` ${c.usage}` : ""}
          </span>
          <span className="truncate text-xs text-fg-muted">{c.description}</span>
        </div>
      ))}
    </div>
  );
}
