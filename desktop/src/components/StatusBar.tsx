import { useState, type ReactNode } from "react";
import { compact } from "../lib/compactNumber";
import type { Usage, UsageTotals } from "../lib/protocol";
import { RollingNumber } from "./RollingNumber";
import { UsagePanel } from "./UsagePanel";

/** 千分位。读屏句子用精确数字，不用紧凑写法。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

/** 一组「数字 + 标签」的折叠态显示。 */
function RollingToken({
  value,
  label,
  usageKey,
}: {
  value: number;
  label: string;
  usageKey?: string | null;
}) {
  const { digits, fractionDigits, unit } = compact(value);
  return (
    <>
      <RollingNumber
        value={Number(digits)}
        usageKey={usageKey}
        fractionDigits={fractionDigits}
        unit={unit}
      />
      <span>{label}</span>
    </>
  );
}

export function StatusBar({
  cwd,
  status,
  totals,
  fallbackUsage,
  usageId,
  actions,
  onRepair,
}: {
  cwd: string | null;
  status: string;
  /**
   * 会话累计用量（本会话所有 provider 调用之和）。这是折叠态与面板的主体。
   */
  totals: UsageTotals | null;
  /**
   * 最后一次调用的快照。仅在 `totals === null`（老服务端）时用来兜底显示，
   * 它代表的是「一轮」而非整个会话，不能冒充累计值。
   */
  fallbackUsage: Usage | null;
  /**
   * 当前会话标识。变化时用量数字直接跳到终值——那是换成另一份数据，
   * 不该从上一个会话的数字滚动过来。
   */
  usageId?: string | null;
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
  // 有累计值就只显示累计值；没有才退回快照（老服务端）。
  const shown: UsageTotals | null =
    totals ??
    (fallbackUsage
      ? {
          input: fallbackUsage.input,
          output: fallbackUsage.output,
          cacheRead: fallbackUsage.cacheRead,
          cacheWrite: fallbackUsage.cacheWrite,
        }
      : null);
  const model = fallbackUsage?.model ?? null;
  return (
    <div className="app-statusbar relative flex items-center gap-3 border-b border-line bg-panel px-4 py-2 text-xs text-fg-muted">
      <span
        className={`inline-block h-2 w-2 rounded-full ${connected ? "bg-emerald-500" : "bg-red-500"}`}
        title={status}
      />
      <span className="text-fg-muted">{status}</span>
      {cwd && <span className="truncate font-mono">{cwd}</span>}
      <div className="ml-auto flex items-center gap-3">
        {shown && (
          <button
            type="button"
            className="inline-flex cursor-pointer items-center gap-1 font-mono hover:text-fg"
            aria-expanded={showUsage}
            aria-controls="usage-panel"
            aria-label="Token usage details"
            onClick={() => setShowUsage((v) => !v)}
          >
            {/*
             * 机械数字对读屏没有意义（每一位都是整条 0-9），故整块 `aria-hidden`
             * 并另给一句完整文本：读屏拿到的仍是精确数字。
             */}
            <span className="sr-only">
              {totals
                ? `${thousands(totals.input)} cumulative input / ${thousands(totals.cacheRead)} cumulative cache read / ${thousands(totals.output)} cumulative output`
                : `${thousands(shown.input)} in / ${thousands(shown.output)} out`}
            </span>
            <span aria-hidden="true" className="inline-flex items-center gap-1">
              <RollingToken value={shown.input} label="in" usageKey={usageId} />
              <span className="text-fg-muted/60">/</span>
              <RollingToken value={shown.cacheRead} label="cache" usageKey={usageId} />
              <span className="text-fg-muted/60">/</span>
              <RollingToken value={shown.output} label="out" usageKey={usageId} />
            </span>
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
      {shown && showUsage && (
        <UsagePanel totals={shown} isFallback={totals === null} model={model} />
      )}
    </div>
  );
}
