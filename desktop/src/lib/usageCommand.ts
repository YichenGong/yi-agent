/**
 * `/usage` 的正文。
 *
 * 纯函数：不碰 React、不碰 RPC，只把一份用量快照与上下文上限排成人读的几行。
 * 这样「有阈值 / 无阈值」「未知模型」这些分叉都能单测，不必拉起整个 App。
 */
import { estimateCost, formatCost } from "./pricing";
import type { Usage } from "./protocol";

/** 千分位。用量是给人看的数字，`12,345` 比 `12345` 容易读。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

export function formatUsage(u: Usage, opts: { compactThreshold: number | null }): string {
  const limit = opts.compactThreshold;
  // 上限未知（老服务端 / 清单没声明 context_length）就不谈占用比例：分母是猜的，
  // 百分比只会误导。0 同理——那会让每一轮都触发压缩，不该被当成真实上限。
  const context =
    limit !== null && limit > 0
      ? `上下文: ${thousands(u.input)} / ${thousands(limit)} (${((u.input / limit) * 100).toFixed(1)}%)`
      : `上下文: ${thousands(u.input)}`;
  return [
    `模型: ${u.model}`,
    `input: ${thousands(u.input)}  output: ${thousands(u.output)}`,
    `cache read: ${thousands(u.cacheRead)}  cache write: ${thousands(u.cacheWrite)}`,
    context,
    `估算成本: ${formatCost(estimateCost(u))}`,
  ].join("\n");
}
