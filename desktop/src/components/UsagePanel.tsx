import { estimateCost, formatCost } from "../lib/pricing";
import type { Usage } from "../lib/protocol";

/** StatusBar 用量区展开后的明细浮层:四类 token + 估算成本。 */
export function UsagePanel({ usage }: { usage: Usage }) {
  return (
    <div
      id="usage-panel"
      role="dialog"
      className="absolute right-4 top-full z-10 mt-1 w-64 rounded-md border border-line-strong bg-panel p-3 text-xs shadow-lg"
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
