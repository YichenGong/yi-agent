import { useState } from "react";
import type { Usage } from "../lib/protocol";
import { UsagePanel } from "./UsagePanel";

export function StatusBar({
  cwd,
  model,
  status,
  usage,
}: {
  cwd: string | null;
  model: string | null;
  status: string;
  usage: Usage | null;
}) {
  const connected = status === "connected";
  const [showUsage, setShowUsage] = useState(false);
  return (
    <div className="relative flex items-center gap-3 border-b border-neutral-800 bg-neutral-900 px-4 py-2 text-xs text-neutral-400">
      <span
        className={`inline-block h-2 w-2 rounded-full ${connected ? "bg-emerald-500" : "bg-red-500"}`}
        title={status}
      />
      <span className="text-neutral-300">{status}</span>
      {cwd && <span className="truncate font-mono">{cwd}</span>}
      {model && <span className="truncate font-mono">{model}</span>}
      {usage && (
        <button
          type="button"
          className="ml-auto cursor-pointer font-mono hover:text-neutral-200"
          aria-expanded={showUsage}
          aria-controls="usage-panel"
          aria-label="Token usage details"
          onClick={() => setShowUsage((v) => !v)}
        >
          {usage.input} in / {usage.output} out
        </button>
      )}
      {usage && showUsage && <UsagePanel usage={usage} />}
    </div>
  );
}
