import { estimateCost, formatCost } from "../lib/pricing";
import type { SubagentTotals, UsageTotals } from "../lib/protocol";

/** 千分位。面板是明细表，用精确数字比紧凑写法更有用。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

/**
 * StatusBar 用量区展开后的明细浮层：会话累计的四类 token + 估算成本。
 *
 * **向上展开**（`bottom-full`）：状态栏是 `h-screen` 外壳的最后一行，浮层若向下
 * （`top-full`）展开就整个落到视口之外——面板虽不可见，却仍参与文档溢出计算，
 * 于是右侧出现滚动条 / 「黑边」（真实 Chrome 实测 800x600：面板 bottom 超出
 * 视口 169px、文档溢出 169px）。与同一行里的 `ModelPicker` / `ModeChip` 下拉同向。
 */
export function UsagePanel({
  totals,
  subagentTotals,
  hasSubagents,
  isFallback,
  model,
}: {
  totals: UsageTotals;
  /** 主会话累计。面板把它与 subagent 拆成两组分别显示。 */
  subagentTotals: SubagentTotals;
  /** subagent 是否有非零用量。为假时不渲染 Subagents 段（不留死行）。 */
  hasSubagents: boolean;
  /** 没有累计值、正在显示最后一轮快照时为 `true`：标题必须说清楚，别让人误会。 */
  isFallback: boolean;
  model: string | null;
}) {
  // 快照兜底（老服务端）时没有 subagent 数据可言：那种情况下 subagentTotals 恒为
  // 零，hasSubagents 自然为假，两处无需再特判。
  const sessionCost = model
    ? formatCost(
        estimateCost({
          model,
          input: totals.input,
          output: totals.output,
          cacheRead: totals.cacheRead,
          cacheWrite: totals.cacheWrite,
        }),
      )
    : "—";
  /*
   * subagent 的成本恒为 `—`：它跑在独立 daemon 进程里，上报的只有三个 token
   * 数（`AgentChildUsage` 没有模型名），没有价目就无从估价。
   *
   * 用破折号而不是把「不含 subagent」写成合计的脚注：成本按组归属时，两组
   * 各有自己的 Cost 行，缺失本身就是信息；脚注则要求读者把那句话与被限定的
   * 数字对上，还容易与浮层标题 "Usage (this session)" 里的 session 混淆。
   */
  const SUBAGENT_COST = "—";
  return (
    <div
      id="usage-panel"
      role="dialog"
      className="absolute right-4 bottom-full z-10 mb-1 w-64 rounded-md border border-line-strong bg-panel p-3 text-xs shadow-lg"
    >
      <div className="mb-2 font-semibold text-fg">
        {isFallback ? "Usage (last call)" : "Usage (this session)"}
      </div>
      <dl className="grid grid-cols-2 gap-y-1 font-mono">
        {/*
         * 两组分开：主会话是「主 Agent 自己」，subagent 是它派出去的子任务。
         * 有 subagent 时给主会话加一行组标题，否则保持原样（不无谓改动）。
         */}
        {hasSubagents && (
          <dt className="col-span-2 mt-1 text-fg">Session</dt>
        )}
        <dt className="text-fg-muted">Input</dt>
        <dd className="text-right">{thousands(totals.input)} tokens</dd>
        <dt className="text-fg-muted">Output</dt>
        <dd className="text-right">{thousands(totals.output)} tokens</dd>
        <dt className="text-fg-muted">Cache read</dt>
        <dd className="text-right">{thousands(totals.cacheRead)} tokens</dd>
        {/* cache write 累计后通常远小于 cache read，为零时不留空行。 */}
        {totals.cacheWrite > 0 && (
          <>
            <dt className="text-fg-muted">Cache write</dt>
            <dd className="text-right">{thousands(totals.cacheWrite)} tokens</dd>
          </>
        )}
        <dt className="text-fg-muted">Cost</dt>
        <dd className="text-right">{sessionCost}</dd>
        {hasSubagents && (
          <>
            <dt className="col-span-2 mt-2 text-fg">Subagents</dt>
            <dt className="text-fg-muted">Input</dt>
            <dd className="text-right">{thousands(subagentTotals.input)} tokens</dd>
            <dt className="text-fg-muted">Output</dt>
            <dd className="text-right">{thousands(subagentTotals.output)} tokens</dd>
            <dt className="text-fg-muted">Cache read</dt>
            <dd className="text-right">{thousands(subagentTotals.cacheRead)} tokens</dd>
            <dt className="text-fg-muted">Cost</dt>
            <dd className="text-right">{SUBAGENT_COST}</dd>
          </>
        )}
        {model && (
          <>
            <dt className="text-fg-muted">Model</dt>
            <dd className="truncate text-right" title={model}>
              {model}
            </dd>
          </>
        )}

      </dl>
    </div>
  );
}
