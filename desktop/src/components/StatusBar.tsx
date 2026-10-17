import { useState, type ReactNode } from "react";
import type { Usage } from "../lib/protocol";
import { UsagePanel } from "./UsagePanel";

export function StatusBar({
  cwd,
  status,
  usage,
  actions,
  onRepair,
}: {
  cwd: string | null;
  status: string;
  usage: Usage | null;
  /**
   * 追加在右端的控件（用量之后）。给「重新打开某个可收起面板」这类常驻入口用：
   * 入口若跟着面板一起收起，就再也没有东西能把它打开。
   */
  actions?: ReactNode;
  /**
   * 手机端「重新配对」入口；**只在传入时渲染**。
   *
   * 由宿主按连接状态决定是否传入（远程客户端且未连接）。做成状态栏自己的语义
   * 而不是塞进通用的 `actions`：它的可见性由连接状态决定，与状态栏的职责同源
   * ——状态显示在哪，从该状态脱困的入口就在哪。
   */
  onRepair?: () => void;
}) {
  const connected = status === "connected";
  const [showUsage, setShowUsage] = useState(false);
  return (
    <div className="app-statusbar relative flex items-center gap-3 border-b border-line bg-panel px-4 py-2 text-xs text-fg-muted">
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
        {onRepair && (
          <button
            type="button"
            onClick={onRepair}
            className="rounded border border-line-strong px-2 py-0.5 text-fg-muted hover:text-fg"
          >
            重新配对
          </button>
        )}
        {actions}
      </div>
      {usage && showUsage && <UsagePanel usage={usage} />}
    </div>
  );
}
