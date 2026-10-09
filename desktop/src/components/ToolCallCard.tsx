import { useId, useState } from "react";
import type { Item, ToolStatus } from "../lib/protocol";
import { toolCallSummary } from "../lib/toolSummary";
import { useImageData, type ImageReadCall } from "../lib/useImageData";

type ToolCallItem = Extract<Item, { type: "toolCall" }>;

const statusStyles: Record<ToolStatus, string> = {
  running: "bg-amber-500/15 text-amber-300 border-amber-500/40",
  completed: "bg-emerald-500/15 text-emerald-300 border-emerald-500/40",
  failed: "bg-red-500/15 text-red-300 border-red-500/40",
};

/** 一张工具产出的图片：`path` 经 `useImageData` 分片取回，画成有界大小的 `<img>`。 */
function ToolImage({
  path,
  threadId,
  call,
}: {
  path: string;
  threadId: string | null;
  call: ImageReadCall;
}) {
  const { url, error } = useImageData(path, { threadId, call });
  if (error) {
    return (
      <div className="rounded border border-red-500/40 bg-red-500/10 px-2 py-1 text-xs text-red-300">
        图片读取失败：{error}
      </div>
    );
  }
  // 对象 URL 就绪前不占位：空 <img> 在窄栏里会闪一下 0×0 的边框。
  return url ? (
    <img
      src={url}
      alt="tool image"
      data-testid="tool-image"
      className="max-h-64 max-w-full rounded border border-line"
    />
  ) : null;
}

/**
 * 一张工具调用卡片。
 *
 * `threadId` / `call` 只为读取工具产出的图片（`item.images`），是宿主注入的**稳定**
 * 引用（`App` 里的 `imageCall`，空依赖 `useCallback`）。调用方（`ChatView` 的
 * `ToolCallRow`）把它们原样透传，**不要**在这里或那里现造内联箭头/对象，否则 memo
 * 的浅比较每次都不等，settled 的卡片会随每个流式 delta 重渲染。
 */
export function ToolCallCard({
  item,
  threadId = null,
  call,
}: {
  item: ToolCallItem;
  threadId?: string | null;
  call?: ImageReadCall;
}) {
  const [open, setOpen] = useState(false);
  const regionId = useId();
  const summary = toolCallSummary(item.name, item.input);

  return (
    <div className="my-2 rounded-md border border-line bg-panel/60">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-controls={regionId}
        className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm hover:bg-raised/50"
      >
        <span className="text-fg-subtle">{open ? "▾" : "▸"}</span>
        <span className="font-mono font-medium text-fg">
          {item.name || "(unknown tool)"}
        </span>
        {summary && (
          <span className="min-w-0 truncate font-mono text-fg-muted" title={summary}>
            {summary}
          </span>
        )}
        <span
          className={`ml-auto rounded-full border px-2 py-0.5 text-xs ${statusStyles[item.status]}`}
        >
          {item.status}
        </span>
      </button>
      <div id={regionId} hidden={!open} className="space-y-2 border-t border-line px-3 py-2">
        <div>
          <div className="mb-1 text-xs uppercase tracking-wide text-fg-subtle">Input</div>
          <pre className="overflow-x-auto rounded bg-surface p-2 font-mono text-xs whitespace-pre-wrap text-fg-muted">
            {JSON.stringify(item.input, null, 2)}
          </pre>
        </div>
        {item.images && item.images.length > 0 && call && (
          <div className="space-y-2">
            {item.images.map((img) => (
              <ToolImage key={img.path} path={img.path} threadId={threadId} call={call} />
            ))}
          </div>
        )}
        {item.result !== undefined && (
          <div>
            <div className="mb-1 text-xs uppercase tracking-wide text-fg-subtle">Result</div>
            <pre className="max-h-64 overflow-y-auto rounded bg-surface p-2 font-mono text-xs whitespace-pre-wrap text-fg-muted">
              {item.result}
            </pre>
          </div>
        )}
      </div>
    </div>
  );
}
