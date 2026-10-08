import { useState, type ReactNode } from "react";
import type { Usage } from "../lib/protocol";
import { UsagePanel } from "./UsagePanel";

export function StatusBar({
  cwd,
  status,
  usage,
  actions,
}: {
  cwd: string | null;
  status: string;
  usage: Usage | null;
  /**
   * 追加在右端的控件（用量之后）。给「重新打开某个可收起面板」这类常驻入口用：
   * 入口若跟着面板一起收起，就再也没有东西能把它打开。
   */
  actions?: ReactNode;
}) {
  const connected = status === "connected";
  const [showUsage, setShowUsage] = useState(false);
  return (
    <div className="relative flex items-center gap-3 border-b border-line bg-panel px-4 py-2 text-xs text-fg-muted">
      <span
        className={`inline-block h-2 w-2 rounded-full ${connected ? "bg-emerald-500" : "bg-red-500"}`}
        title={status}
      />
      <span className="text-fg-muted">{status}</span>
      {cwd && <span className="truncate font-mono">{cwd}</span>}
      <div className="ml-auto flex items-center gap-3">
        {usage && (
          <button
            type="button"
            className="cursor-pointer font-mono hover:text-fg"
            aria-expanded={showUsage}
            aria-controls="usage-panel"
            aria-label="Token usage details"
            onClick={() => setShowUsage((v) => !v)}
          >
            {usage.input} in / {usage.output} out
          </button>
        )}
        {actions}
      </div>
      {usage && showUsage && <UsagePanel usage={usage} />}
    </div>
  );
}
