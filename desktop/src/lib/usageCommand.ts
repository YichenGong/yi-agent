/**
 * `/usage` 卡片的数据来源。
 *
 * 纯函数：不碰 React、不碰 RPC。它只把用量数字排成**结构**（分组 → 键值行），
 * 千分位与百分比留给渲染层 —— `/config`、`/help` 也是这么分工的。
 *
 * 口径分两套，**不能混用**：
 * - `totals`（累计）：本会话所有 provider 调用之和，是主体；
 * - `snapshot`（快照）：最后一次调用的用量，只用来算上下文占用。
 *
 * 无累计值时（老服务端）整份报告退回快照，并把标题改成「本轮用量」——卡片有标题
 * 位置，正好用来主动说清这是一轮还是一辈子；文本版没标题，只能靠「不写累计二字」
 * 这种消极方式表达。
 */
import { estimateCost, formatCost } from "./pricing";
import type { SubagentTotals, Usage, UsageTotals } from "./protocol";

/** 一组「标签 + token 数」。`value` 保持原始数字，格式化属于呈现。 */
export interface UsageGroupRow {
  label: string;
  value: number;
}

export interface UsageReport {
  /** `Token 用量`（有累计值）或 `本轮用量`（只有快照）。 */
  title: string;
  model: string;
  /** 上下文占用。`limit` 为 null 时不显示分母与百分比。 */
  context: { used: number; limit: number | null };
  /** 主会话用量。`isSnapshot` 为真表示这份数字是最后一轮而非整个会话。 */
  session: { rows: UsageGroupRow[]; isSnapshot: boolean };
  /** subagent 三项。`null` 表示没有 subagent 用量，卡片不渲染该组。 */
  subagent: UsageGroupRow[] | null;
  /** 估算成本，只含钱数（`"$1.23"` / `"< $0.01"` / `"—"`）。 */
  cost: string;
}

export function buildUsageReport(
  snapshot: Usage,
  totals: UsageTotals | null,
  subagent: SubagentTotals | null,
  opts: { compactThreshold: number | null },
): UsageReport {
  const limit = opts.compactThreshold;
  // 全零与 `null` 同义：都没有 subagent 用量可报。
  const hasSubagents =
    subagent !== null && subagent.input + subagent.output + subagent.cacheRead > 0;

  // 上限未知（老服务端 / 清单没声明 context_length）就不谈占用比例：分母是猜的，
  // 百分比只会误导。0 同理 —— 那会让每一轮都触发压缩，不该被当成真实上限。
  // 分子必须是**快照**：累计值代表整个会话的输入量，与「当前上下文还剩多少」无关。
  const contextLimit = limit !== null && limit > 0 ? limit : null;

  // 主会话四行。cache write 是「写进缓存」的一次性开销，累计后通常远小于
  // cache read；为零时不留死行。
  const sessionRowsFrom = (src: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
  }): UsageGroupRow[] => {
    const rows: UsageGroupRow[] = [
      { label: "Input", value: src.input },
      { label: "Output", value: src.output },
      { label: "Cache read", value: src.cacheRead },
    ];
    if (src.cacheWrite > 0) rows.push({ label: "Cache write", value: src.cacheWrite });
    return rows;
  };

  const session = totals
    ? { rows: sessionRowsFrom(totals), isSnapshot: false }
    : { rows: sessionRowsFrom(snapshot), isSnapshot: true };

  // subagent 只报三项：cache write 在子 agent 侧不采集，不要凭空补一个 0 行。
  const subagentRows: UsageGroupRow[] | null =
    subagent !== null && hasSubagents
      ? [
          { label: "Input", value: subagent.input },
          { label: "Output", value: subagent.output },
          { label: "Cache read", value: subagent.cacheRead },
        ]
      : null;

  // 成本按累计口径估：快照只是一次调用的钱，用户关心的是这一整段对话花了多少。
  // 有 subagent 时这笔钱只算了主会话（子任务跑在独立进程里，模型名没随用量上报），
  // 限定语由渲染层按 `subagent !== null` 补 —— 数据层不替渲染层拼句子。
  const billable: Usage = totals ? { ...snapshot, ...totals } : snapshot;

  return {
    title: totals ? "Token 用量" : "本轮用量",
    model: snapshot.model,
    context: { used: snapshot.input, limit: contextLimit },
    session,
    subagent: subagentRows,
    cost: formatCost(estimateCost(billable)),
  };
}
