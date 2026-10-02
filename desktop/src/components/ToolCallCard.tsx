import { useId, useState } from "react";
import type { Item, ToolStatus } from "../lib/protocol";
import { toolCallSummary } from "../lib/toolSummary";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;

const statusStyles: Record<ToolStatus, string> = {
  running: "bg-amber-500/15 text-amber-300 border-amber-500/40",
  completed: "bg-emerald-500/15 text-emerald-300 border-emerald-500/40",
  failed: "bg-red-500/15 text-red-300 border-red-500/40",
};

export function ToolCallCard({ item }: { item: ToolCallItem }) {
  const [open, setOpen] = useState(false);
  const regionId = useId();
  const summary = toolCallSummary(item.name, item.input);

  return (
    <div className="my-2 rounded-md border border-line bg-panel/60">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-controls={regionId}
        className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-raised/50"
      >
        <span className="text-fg-subtle">{open ? "▾" : "▸"}</span>
        <span className="font-mono font-medium text-fg">{item.name}</span>
        {summary && (
          <span className="min-w-0 truncate font-mono text-fg-muted" title={summary}>
            {summary}
          </span>
        )}
        <span
          className={`ml-auto rounded-full border px-2 py-0.5 text-xs ${statusStyles[item.status]}`}
        >
          {item.status}
        </span>
      </button>
      <div id={regionId} hidden={!open} className="space-y-2 border-t border-line px-3 py-2">
        <div>
          <div className="mb-1 text-xs uppercase tracking-wide text-fg-subtle">Input</div>
          <pre className="overflow-x-auto rounded bg-surface p-2 font-mono text-xs whitespace-pre-wrap text-fg-muted">
            {JSON.stringify(item.input, null, 2)}
          </pre>
        </div>
        {item.result !== undefined && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-fg-subtle">Result</div>
            <pre className="max-h-64 overflow-y-auto rounded bg-surface p-2 font-mono text-xs whitespace-pre-wrap text-fg-muted">
              {item.result}
            </pre>
          </div>
        )}
      </div>
    </div>
  );
}
