import { useState } from "react";
import type { Item, ToolStatus } from "../lib/protocol";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;

const statusStyles: Record<ToolStatus, string> = {
  running: "bg-amber-500/15 text-amber-300 border-amber-500/40",
  completed: "bg-emerald-500/15 text-emerald-300 border-emerald-500/40",
  failed: "bg-red-500/15 text-red-300 border-red-500/40",
};

export function ToolCallCard({ item }: { item: ToolCallItem }) {
  const [open, setOpen] = useState(false);

  return (
    <div className="my-2 rounded-md border border-neutral-800 bg-neutral-900/60">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-neutral-800/50"
      >
        <span className="text-neutral-500">{open ? "▾" : "▸"}</span>
        <span className="font-mono font-medium text-neutral-200">{item.name}</span>
        <span
          className={`ml-auto rounded-full border px-2 py-0.5 text-xs ${statusStyles[item.status]}`}
        >
          {item.status}
        </span>
      </button>
      {open && (
        <div className="space-y-2 border-t border-neutral-800 px-3 py-2">
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-neutral-500">Input</div>
            <pre className="overflow-x-auto rounded bg-neutral-950 p-2 font-mono text-xs whitespace-pre-wrap text-neutral-300">
              {JSON.stringify(item.input, null, 2)}
            </pre>
          </div>
          {item.result !== undefined && (
            <div>
              <div className="mb-1 text-xs uppercase tracking-wide text-neutral-500">Result</div>
              <pre className="max-h-64 overflow-y-auto rounded bg-neutral-950 p-2 font-mono text-xs whitespace-pre-wrap text-neutral-300">
                {item.result}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
