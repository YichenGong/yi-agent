import { estimateCost, formatCost } from "../lib/pricing";
import type { Usage } from "../lib/protocol";

/**
 * StatusBar 用量区展开后的明细浮层:四类 token + 估算成本。
 *
 * **向上展开**（`bottom-full`）：状态栏是 `h-screen` 外壳的最后一行，浮层若向下
 * （`top-full`）展开就整个落到视口之外——面板虽不可见，却仍参与文档溢出计算，
 * 于是右侧出现滚动条 / 「黑边」（真实 Chrome 实测 800x600：面板 bottom 超出
 * 视口 169px、文档溢出 169px）。与同一行里的 `ModelPicker` / `ModeChip` 下拉同向。
 */
export function UsagePanel({ usage }: { usage: Usage }) {
  return (
    <div
      id="usage-panel"
      role="dialog"
      className="absolute right-4 bottom-full z-10 mb-1 w-64 rounded-md border border-line-strong bg-panel p-3 text-xs shadow-lg"
    >
      <div className="mb-2 font-semibold text-fg">Usage</div>
      <dl className="grid grid-cols-2 gap-y-1 font-mono">
        <dt className="text-fg-muted">Input</dt>
        <dd className="text-right">{usage.input} tokens</dd>
        <dt className="text-fg-muted">Output</dt>
        <dd className="text-right">{usage.output} tokens</dd>
        <dt className="text-fg-muted">Cache read</dt>
        <dd className="text-right">{usage.cacheRead} tokens</dd>
        <dt className="text-fg-muted">Cache write</dt>
        <dd className="text-right">{usage.cacheWrite} tokens</dd>
        <dt className="text-fg-muted">Model</dt>
        <dd className="truncate text-right" title={usage.model}>
          {usage.model}
        </dd>
        <dt className="text-fg-muted">Est. cost</dt>
        <dd className="text-right">{formatCost(estimateCost(usage))}</dd>
      </dl>
    </div>
  );
}
