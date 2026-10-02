/**
 * 子 agent 详情:摘要态起步,展开看完整轨迹;可下钻,可发消息与取消。
 *
 * 轨迹块是"不可变的定稿"——每块的内容由它对应的行决定,不随后续行变化。因此
 * 用 `memo` 把块边界钉在值上:流式追加时只有新增的那个块会渲染,定稿的块不动。
 * 这与主对话里 `ToolCallCard`/`AgentMessage` 的记忆化是同一个理由(见
 * `ChatView.tsx` 的注释):否则每来一行都要重渲染整条轨迹。
 */
import { memo, useEffect, useState } from "react";
import type { AgentCancelPreviewResult, AgentTraceSnapshotResult } from "../lib/protocol";
import { foldTraceRows, type SubagentRow, type TraceBlock } from "../lib/subagents";

/**
 * One trace block, memoized on its **values**, not on the block object.
 *
 * `foldTraceRows` is a fold over the whole row list, so every call builds a new
 * block object: a memo keyed on identity would never hit and every streamed row
 * would re-render the entire trace. Primitives let React's shallow comparison
 * see that an already-settled block is unchanged, so only the tail re-renders —
 * the same reasoning as `ChatView`'s leaf memo boundaries.
 */
const TraceRowView = memo(function TraceRowView({
  kind,
  text,
  isError,
}: {
  kind: TraceBlock["kind"];
  text: string;
  isError: boolean;
}) {
  if (kind === "assistant_text") {
    return <p className="my-1 whitespace-pre-wrap text-sm text-fg">{text}</p>;
  }
  if (kind === "tool_call") {
    return <p className="my-1 font-mono text-xs text-sky-300">{text}</p>;
  }
  if (kind === "tool_result") {
    return (
      <p className={`my-1 font-mono text-xs ${isError ? "text-red-300" : "text-fg-subtle"}`}>
        {text}
      </p>
    );
  }
  return <p className="my-1 text-xs text-fg-subtle">{text}</p>;
});

export function SubagentTrace({
  taskId,
  row,
  children,
  rows,
  onClose,
  onDrill,
  onMessage,
  onCancel,
  onConfirmCancel,
}: {
  taskId: string;
  /** The rail row this task came from, for the summary level. */
  row: SubagentRow | null;
  /** This task's direct children, which can be drilled into. */
  children: SubagentRow[];
  /** The task's trace rows, backfilled then appended live by the caller. */
  rows: AgentTraceSnapshotResult["rows"];
  onClose: () => void;
  onDrill: (taskId: string) => void;
  onMessage: (taskId: string, message: string) => Promise<void>;
  /** Two steps by contract: preview returns a token, confirm carries it. */
  onCancel: (taskId: string) => Promise<AgentCancelPreviewResult>;
  onConfirmCancel: (taskId: string, token: string) => Promise<void>;
}) {
  const [expanded, setExpanded] = useState(false);
  const [composing, setComposing] = useState(false);
  const [draft, setDraft] = useState("");
  const [pendingCancel, setPendingCancel] = useState<string | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  // A different task is a different view: the expanded/composing state belongs
  // to the task it was set on, not to the panel.
  useEffect(() => {
    setExpanded(false);
    setComposing(false);
    setDraft("");
    setPendingCancel(null);
    setStatus(null);
  }, [taskId]);

  const blocks = foldTraceRows(rows);

  return (
    <section
      aria-label={`子 agent 详情 ${taskId}`}
      className="flex min-h-0 flex-1 flex-col border-t border-line bg-surface"
    >
      <div className="flex items-center justify-between px-3 py-2">
        <div className="flex items-center gap-2">
          <span className="text-xs text-fg-muted">[{row?.state ?? "unknown"}]</span>
          <span className="truncate text-sm text-fg">
            {row?.objective ?? taskId}
          </span>
          <span className="text-xs text-fg-faint">{taskId}</span>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="text-xs text-fg-muted hover:text-fg"
            aria-expanded={expanded}
            onClick={() => setExpanded((v) => !v)}
          >
            {expanded ? "收起轨迹" : "展开轨迹"}
          </button>
          <button
            type="button"
            aria-label="关闭详情"
            className="text-xs text-fg-subtle hover:text-fg-muted"
            onClick={onClose}
          >
            关闭
          </button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-2">
        {!expanded ? (
          <div className="text-sm text-fg-muted">
            <p>最近步骤：{row?.finished ? "已结束" : (row?.lastStep ?? "工作中")}</p>
            <p className="mt-1 text-xs text-fg-faint">
              轨迹 {rows.length} 行（展开查看完整轨迹）
            </p>
          </div>
        ) : blocks.length === 0 ? (
          <p className="text-sm text-fg-subtle">该任务暂无轨迹</p>
        ) : (
          blocks.map((block) => (
            <TraceRowView key={block.key} kind={block.kind} text={block.text} isError={block.isError} />
          ))
        )}
      </div>

      {children.length > 0 && (
        <div className="border-t border-line px-3 py-2">
          <p className="text-xs text-fg-subtle">子任务</p>
          <ul className="mt-1 flex flex-wrap gap-2">
            {children.map((child) => (
              <li key={child.taskId}>
                <button
                  type="button"
                  aria-label={`进入子任务 ${child.taskId}`}
                  className="rounded border border-line px-2 py-1 text-xs text-fg-muted hover:border-line-strong"
                  onClick={() => onDrill(child.taskId)}
                >
                  {child.objective ?? child.taskId}
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="border-t border-line px-3 py-2">
        {status && <p className="mb-1 text-xs text-amber-300">{status}</p>}
        {pendingCancel ? (
          <div className="flex items-center gap-2">
            <span className="text-xs text-red-300">确认取消该子 agent？</span>
            <button
              type="button"
              className="text-xs text-red-300 hover:text-red-200"
              onClick={async () => {
                try {
                  await onConfirmCancel(taskId, pendingCancel);
                  setStatus("已取消");
                } catch (e) {
                  setStatus(e instanceof Error ? e.message : String(e));
                } finally {
                  setPendingCancel(null);
                }
              }}
            >
              确认取消
            </button>
            <button
              type="button"
              className="text-xs text-fg-muted hover:text-fg"
              onClick={() => setPendingCancel(null)}
            >
              放弃
            </button>
          </div>
        ) : composing ? (
          <form
            className="flex items-center gap-2"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!draft.trim()) return;
              try {
                await onMessage(taskId, draft);
                setStatus("已发送");
              } catch (err) {
                setStatus(err instanceof Error ? err.message : String(err));
              } finally {
                setDraft("");
                setComposing(false);
              }
            }}
          >
            <input
              aria-label="发给子 agent 的消息"
              className="min-w-0 flex-1 rounded border border-line bg-panel px-2 py-1 text-sm text-fg"
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
            />
            <button type="submit" className="text-xs text-sky-300 hover:text-sky-200">
              发送
            </button>
            <button
              type="button"
              className="text-xs text-fg-muted hover:text-fg"
              onClick={() => {
                setDraft("");
                setComposing(false);
              }}
            >
              取消
            </button>
          </form>
        ) : (
          <div className="flex items-center gap-3">
            <button
              type="button"
              className="text-xs text-sky-300 hover:text-sky-200"
              onClick={() => setComposing(true)}
            >
              发消息
            </button>
            <button
              type="button"
              className="text-xs text-red-300 hover:text-red-200"
              onClick={async () => {
                try {
                  const preview = await onCancel(taskId);
                  setPendingCancel(preview.confirmationToken);
                } catch (e) {
                  setStatus(e instanceof Error ? e.message : String(e));
                }
              }}
            >
              取消该任务
            </button>
          </div>
        )}
      </div>
    </section>
  );
}
