/**
 * 轨迹里的一条事件。
 *
 * 四条展示规则，逐条对应「不编造」这个总原则：
 *
 * 1. **assistant_text** 是唯一走 Markdown 的块——它本来就是 agent 写的正文。
 * 2. **tool_call** 与 **tool_result** 分开画，且**不配对**：客户端并不知道哪次
 *    结果是哪次调用的，硬接成一组就是在猜。结果块只叫「结果摘要」，并且用
 *    **它自己携带的** `name` 打标签。
 * 3. 长结果折叠，展开后明说「这是服务端摘要，不是完整输出」——客户端手里只有摘要。
 * 4. 认不出的事件（新内核、坏 JSON、字段类型不对）保留原文并可展开：宁可让用户
 *    看到一段 JSON，也不能把 agent 真发过的东西渲染成空白。
 *
 * `onPreserve` 存在的唯一目的：展开/收起会改变高度，调用方要在这之前记下阅读锚点。
 * 变化必须包在 `change()` 里执行，否则锚点已经失效。
 */
import { memo, useId, useState } from "react";

import { MarkdownText } from "./MarkdownText";
import { summarizeTrace, type TraceBlock } from "../lib/subagentTraceModel";

/** 在改动 DOM 前捕获锚点，再把改动跑完。 */
export type TracePreserve = (change: () => void) => void;

function run(change: () => void, onPreserve?: TracePreserve): void {
  if (onPreserve) onPreserve(change);
  else change();
}

const EMPTY_SUCCESS = "（无输出）";
const EMPTY_ERROR = "（失败且无输出）";

/** 一条工具调用的名字与参数摘要，分层显示。 */
function ToolCall({ block }: { block: TraceBlock }) {
  return (
    <div className="rounded border border-line bg-panel px-2 py-1.5">
      <div className="flex min-w-0 items-center gap-2">
        <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-trace-info" />
        <span className="min-w-0 truncate font-mono text-xs text-fg">{block.name ?? "工具"}</span>
      </div>
      {block.text.length > 0 ? (
        <div className="mt-1 whitespace-pre-wrap break-words text-xs text-fg-muted">{block.text}</div>
      ) : null}
    </div>
  );
}

/** 工具结果：只叫结果摘要，长内容折叠，展开后可确认来源。 */
function ToolResult({
  block,
  onPreserve,
}: {
  block: TraceBlock;
  onPreserve?: TracePreserve;
}) {
  const [expanded, setExpanded] = useState(false);
  const summary = summarizeTrace(block.text);
  const bodyId = useId();

  // 空内容不是「没什么可显示」，而是一个需要说清楚的事实：成功还是失败。
  const empty = block.text.length === 0;
  const emptyText = block.isError ? EMPTY_ERROR : EMPTY_SUCCESS;

  return (
    <div
      className={`rounded border px-2 py-1.5 ${
        block.isError ? "border-trace-danger/60 bg-panel" : "border-line bg-panel"
      }`}
    >
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <span aria-hidden="true" className={`size-1.5 shrink-0 rounded-full ${block.isError ? "bg-trace-danger" : "bg-trace-success"}`} />
        <span className="text-xs text-fg-muted">结果摘要</span>
        {block.name !== null ? (
          <span className="min-w-0 truncate font-mono text-xs text-fg-subtle">{block.name}</span>
        ) : null}
        {summary.folded ? (
          <button
            type="button"
            aria-expanded={expanded}
            aria-controls={bodyId}
            onClick={() => run(() => setExpanded((value) => !value), onPreserve)}
            className="ml-auto shrink-0 rounded px-1 text-xs text-trace-info hover:underline"
          >
            {expanded ? "收起结果摘要" : "展开结果摘要"}
          </button>
        ) : null}
      </div>

      <div id={bodyId} className="mt-1">
        {empty ? (
          <p className={`text-xs ${block.isError ? "text-trace-danger" : "text-fg-subtle"}`}>{emptyText}</p>
        ) : expanded ? (
          <>
            <pre className="max-h-96 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-fg">
              {block.text}
            </pre>
            <p className="mt-1 text-[11px] text-fg-faint">
              这是服务端提供的摘要，不是完整输出。
            </p>
          </>
        ) : (
          <p className="whitespace-pre-wrap break-words text-xs text-fg-muted">{summary.preview}</p>
        )}
      </div>
    </div>
  );
}

/** 状态说明是日志行，不是 Markdown 文档。 */
function StateNote({ block }: { block: TraceBlock }) {
  return <p className="truncate text-xs text-fg-subtle">{block.text}</p>;
}

/** 无法解析的事件：原文保留，可展开，绝不执行其中的 HTML。 */
function RawPayload({ block, onPreserve }: { block: TraceBlock; onPreserve?: TracePreserve }) {
  const [expanded, setExpanded] = useState(false);
  const bodyId = useId();
  const payload = block.rawPayload ?? block.text;

  return (
    <div className="rounded border border-line-strong border-dashed px-2 py-1.5">
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <span className="text-xs text-fg-muted">无法解析的事件</span>
        <span className="min-w-0 truncate font-mono text-xs text-fg-faint">{block.sourceKind}</span>
        <button
          type="button"
          aria-expanded={expanded}
          aria-controls={bodyId}
          onClick={() => run(() => setExpanded((value) => !value), onPreserve)}
          className="ml-auto shrink-0 rounded px-1 text-xs text-trace-info hover:underline"
        >
          {expanded ? "收起原始载荷" : "查看原始载荷"}
        </button>
      </div>
      {expanded ? (
        <pre id={bodyId} className="mt-1 max-h-80 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-fg-subtle">
          {payload}
        </pre>
      ) : null}
    </div>
  );
}

function TraceEventView({ block, onPreserve }: { block: TraceBlock; onPreserve?: TracePreserve }) {
  switch (block.kind) {
    case "assistant_text":
      return (
        <div className="trace-prose min-w-0">
          <MarkdownText text={block.text} />
        </div>
      );
    case "tool_call":
      return <ToolCall block={block} />;
    case "tool_result":
      return <ToolResult block={block} onPreserve={onPreserve} />;
    case "state_note":
      return <StateNote block={block} />;
    case "raw":
      return <RawPayload block={block} onPreserve={onPreserve} />;
  }
}

/**
 * 按值比较，而不是按对象引用。
 *
 * 上游每来一帧都会新建 `TraceBlock`，引用永远不等；只比引用等于每次都重渲染，
 * 只比 `key` 又会漏掉流式增长的 text。这里比的是渲染真正读到的那几个字段。
 */
function sameBlock(a: TraceBlock, b: TraceBlock): boolean {
  return a.key === b.key
    && a.kind === b.kind
    && a.sourceKind === b.sourceKind
    && a.text === b.text
    && a.name === b.name
    && a.isError === b.isError
    && a.rawPayload === b.rawPayload;
}

export const TraceEvent = memo(TraceEventView, (prev, next) =>
  sameBlock(prev.block, next.block) && prev.onPreserve === next.onPreserve,
);
