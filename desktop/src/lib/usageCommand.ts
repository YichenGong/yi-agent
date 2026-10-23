/**
 * `/usage` 的正文。
 *
 * 纯函数：不碰 React、不碰 RPC，只把用量数字与上下文上限排成人读的几行。
 * 这样「有阈值 / 无阈值」「未知模型」「有没有累计值」这些分叉都能单测，
 * 不必拉起整个 App。
 *
 * 口径分两套，**不能混用**：
 * - `totals`（累计）：本会话所有 provider 调用之和，是 `/usage` 的主体；
 * - `snapshot`（快照）：最后一次调用的用量，只用来算上下文占用。
 *
 * subagent 用量单独成段：它与主会话是两笔账，混在一行里既看不出谁花得多，也没法
 * 判断子任务值不值。没有 subagent 时保持原来的单段文案——「主会话」这个限定只有在
 * 同时显示 subagent 时才有区分意义，否则只是无谓地改动用户熟悉的输出。
 */
import { estimateCost, formatCost } from "./pricing";
import type { SubagentTotals, Usage, UsageTotals } from "./protocol";

/** 千分位。用量是给人看的数字，`12,345` 比 `12345` 容易读。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

export function formatUsage(
  snapshot: Usage,
  totals: UsageTotals | null,
  subagent: SubagentTotals | null,
  opts: { compactThreshold: number | null },
): string {
  const limit = opts.compactThreshold;
  // 全零与 `null` 同义：都没有 subagent 用量可报。
  const hasSubagents =
    subagent !== null && subagent.input + subagent.output + subagent.cacheRead > 0;
  // 上限未知（老服务端 / 清单没声明 context_length）就不谈占用比例：分母是猜的，
  // 百分比只会误导。0 同理——那会让每一轮都触发压缩，不该被当成真实上限。
  // 这里的分子必须是**快照**：累计值代表整个会话的输入量，与「当前上下文还剩多少」
  // 无关，拿它算百分比会得出远超 100% 的荒谬数字。
  const context =
    limit !== null && limit > 0
      ? `上下文: ${thousands(snapshot.input)} / ${thousands(limit)} (${((snapshot.input / limit) * 100).toFixed(1)}%)`
      : `上下文: ${thousands(snapshot.input)}`;

  const lines = [`模型: ${snapshot.model}`];
  if (totals) {
    // 累计口径是主体。cache write 是"写进缓存"的一次性开销，累计后通常远小于
    // cache read；为零时不留空行。
    //
    // 有 subagent 时前缀由「累计」改成「主会话」：那种语境下「累计」会被读成
    // 「含 subagent 的合计」，与下面那段正好冲突。
    const prefix = hasSubagents ? "主会话" : "累计";
    lines.push(
      `${prefix} input: ${thousands(totals.input)}  output: ${thousands(totals.output)}`,
    );
    lines.push(
      totals.cacheWrite > 0
        ? `${prefix} cache read: ${thousands(totals.cacheRead)}  cache write: ${thousands(totals.cacheWrite)}`
        : `${prefix} cache read: ${thousands(totals.cacheRead)}`,
    );
    if (hasSubagents) {
      // 只报三项：cache write 是 subagent 侧不采集的口径，不要凭空补一个 0 行。
      lines.push(
        `subagent input: ${thousands(subagent.input)}  output: ${thousands(subagent.output)}`,
      );
      lines.push(`subagent cache read: ${thousands(subagent.cacheRead)}`);
    }
  } else {
    // 老服务端没有累计值：退回快照，不显示「累计」字样，免得把一轮当一辈子。
    lines.push(`input: ${thousands(snapshot.input)}  output: ${thousands(snapshot.output)}`);
    lines.push(
      `cache read: ${thousands(snapshot.cacheRead)}  cache write: ${thousands(snapshot.cacheWrite)}`,
    );
  }
  lines.push(context);

  // 成本按累计口径估算：快照只是一次调用的钱，用户关心的是这一整段对话花了多少。
  const billable: Usage = totals
    ? { ...snapshot, ...totals }
    : snapshot;
  // 有 subagent 时必须声明这笔钱只算了主会话：subagent 跑在独立进程里，模型名没随
  // 用量上报，按同一个价目算出来的数会偏；不标注就会被读成合计。
  const costNote = hasSubagents ? "（主会话，不含 subagent）" : "";
  lines.push(`估算成本: ${formatCost(estimateCost(billable))}${costNote}`);
  return lines.join("\n");
}
