import { useState, type ReactNode } from "react";
import { compact } from "../lib/compactNumber";
import { connectionStatusLabel } from "../lib/connectionStatus";
import type { SubagentTotals, Usage, UsageTotals } from "../lib/protocol";
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
  subagentTotals,
  subagentCount,
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
   * 本会话所有 subagent 的累计用量。折叠态把它**加进**主会话数字里显示合计，
   * 展开面板则分开显示两组。
   */
  subagentTotals: SubagentTotals;
  /**
   * 本会话的子任务数量，只用于读屏句子（"including subagents, N subagents"）。
   */
  subagentCount: number;
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
  // 「有没有 subagent」看用量而不是看数量：一个跑完但没烧 token 的子任务不该让
  // 折叠态改口径。数量只影响读屏句子里那句 "N subagents"。
  const hasSubagents =
    subagentTotals.input + subagentTotals.output + subagentTotals.cacheRead > 0;
  // 折叠态展示的是合计：主会话 + 本会话所有 subagent。cacheWrite 不参与——
  // subagent 的 cache write 不在用户口径内，主会话的自己那份保持原值。
  const combined: UsageTotals | null = shown
    ? {
        input: shown.input + subagentTotals.input,
        output: shown.output + subagentTotals.output,
        cacheRead: shown.cacheRead + subagentTotals.cacheRead,
        cacheWrite: shown.cacheWrite,
      }
    : null;
  return (
    <div className="app-statusbar relative flex items-center gap-3 border-b border-line bg-panel px-4 py-2 text-xs text-fg-muted">
      <span
        className={`inline-block h-2 w-2 rounded-full ${connected ? "bg-emerald-500" : "bg-red-500"}`}
        title={status}
      />
      {/*
        `title` 留着原始状态：底栏用的是中文说法（`connecting` / `exited` /
        `error: …` 对用户没有意义），排查连接问题时把鼠标放上去仍能看到传输层
        原值，两边各取所需。
      */}
      <span className="text-fg-muted" title={status}>
        {connectionStatusLabel(status)}
      </span>
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
              {/*
               * 读屏句子必须与可见数字表达**同一件事**（都是合计），否则读屏用户
               * 与视觉用户看到的是两个数。没有 subagent 时逐字保持原句。
               */}
              {combined === null
                ? ""
                : !totals
                  ? `${thousands(combined.input)} in / ${thousands(combined.output)} out`
                  : hasSubagents
                    ? `${thousands(combined.input)} cumulative input / ${thousands(combined.cacheRead)} cumulative cache read / ${thousands(combined.output)} cumulative output, including subagents, ${subagentCount} subagents`
                    : `${thousands(combined.input)} cumulative input / ${thousands(combined.cacheRead)} cumulative cache read / ${thousands(combined.output)} cumulative output`}
            </span>
            <span aria-hidden="true" className="inline-flex items-center gap-1">
              <RollingToken value={combined!.input} label="in" usageKey={usageId} />
              <span className="text-fg-muted/60">/</span>
              <RollingToken value={combined!.cacheRead} label="cache" usageKey={usageId} />
              <span className="text-fg-muted/60">/</span>
              <RollingToken value={combined!.output} label="out" usageKey={usageId} />
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
      {shown && combined && showUsage && (
        <UsagePanel
          totals={shown}
          subagentTotals={subagentTotals}
          hasSubagents={hasSubagents}
          isFallback={totals === null}
          model={model}
        />
      )}
    </div>
  );
}
