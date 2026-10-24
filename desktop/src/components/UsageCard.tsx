/**
 * `/usage` 的输出卡片。
 *
 * 取代此前把 `formatUsage()` 的六行等宽文本塞进 notice 胶囊的做法：那个容器
 * 没有 `whitespace-pre-wrap`，换行被 CSS 折叠，六行压成一行居中灰字。外壳与
 * `ConfigCard` / `HelpCard` 同一套视觉语言，三个多行命令的输出因此长得一样。
 *
 * 分两组：主会话是「主 Agent 自己」，subagent 是它派出去的子任务。只有存在
 * subagent 时才给两个分组加标题 —— 没有对照对象时「主会话」是空指涉。
 */
import type { UsageGroupRow, UsageReport } from "../lib/usageCommand";

/** 千分位。卡片里数字要能一眼扫读，`12,345` 比 `12345` 容易。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

/** 上下文占用一行：上限未知时只给已用值，不编分母。 */
function contextText(c: UsageReport["context"]): string {
  if (c.limit === null) return thousands(c.used);
  return `${thousands(c.used)} / ${thousands(c.limit)} (${((c.used / c.limit) * 100).toFixed(1)}%)`;
}

function Rows({ rows }: { rows: UsageGroupRow[] }) {
  return (
    <>
      {rows.map((row) => (
        <div key={row.label} className="flex gap-3">
          <span className="w-24 shrink-0 text-fg-muted">{row.label}</span>
          <span className="min-w-0 break-all text-fg">{thousands(row.value)}</span>
        </div>
      ))}
    </>
  );
}

/** 一组内的一行成本。标签列与 `Rows` 对齐，值列同样右看齐到同一栏。 */
function CostRow({ value }: { value: string }) {
  return (
    <div className="flex gap-3">
      <span className="w-24 shrink-0 text-fg-muted">成本</span>
      <span className="min-w-0 break-all text-fg">{value}</span>
    </div>
  );
}

export function UsageCard({ report }: { report: UsageReport }) {
  const grouped = report.subagent !== null;
  return (
    <div
      data-testid="usage-card"
      className="my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm"
    >
      <div className="mb-1 font-medium text-fg">{report.title}</div>
      <div className="flex flex-col gap-2">
        <div className="flex flex-col gap-0.5">
          <div className="flex gap-3">
            <span className="w-24 shrink-0 text-fg-muted">模型</span>
            <span className="min-w-0 break-all text-fg">{report.model}</span>
          </div>
          <div className="flex gap-3">
            <span className="w-24 shrink-0 text-fg-muted">上下文</span>
            <span className="min-w-0 break-all text-fg">{contextText(report.context)}</span>
          </div>
        </div>

        <div className="flex flex-col gap-0.5">
          {grouped && <div className="text-xs text-fg-subtle">主会话</div>}
          <Rows rows={report.session.rows} />
          <CostRow value={report.cost} />
        </div>

        {report.subagent !== null && (
          <div className="flex flex-col gap-0.5">
            <div className="text-xs text-fg-subtle">Subagents</div>
            <Rows rows={report.subagent} />
            {/*
             * subagent 的成本恒为 —：它跑在独立 daemon 进程里，上报的只有三个
             * token 数（`AgentChildUsage` 没有模型名），没有价目就无从估价。
             * 用破折号而不是给合计加「（仅主会话）」脚注：成本按组归属时，缺失
             * 本身就是信息，两组结构也对称。
             */}
            <CostRow value="—" />
          </div>
        )}

      </div>
    </div>
  );
}
