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
 */
import { estimateCost, formatCost } from "./pricing";
import type { Usage, UsageTotals } from "./protocol";

/** 千分位。用量是给人看的数字，`12,345` 比 `12345` 容易读。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

export function formatUsage(
  snapshot: Usage,
  totals: UsageTotals | null,
  opts: { compactThreshold: number | null },
): string {
  const limit = opts.compactThreshold;
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
    lines.push(
      `累计 input: ${thousands(totals.input)}  output: ${thousands(totals.output)}`,
    );
    lines.push(
      totals.cacheWrite > 0
        ? `累计 cache read: ${thousands(totals.cacheRead)}  cache write: ${thousands(totals.cacheWrite)}`
        : `累计 cache read: ${thousands(totals.cacheRead)}`,
    );
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
  lines.push(`估算成本: ${formatCost(estimateCost(billable))}`);
  return lines.join("\n");
}
