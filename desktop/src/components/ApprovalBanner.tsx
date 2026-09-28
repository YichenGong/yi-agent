import type { ApprovalRequest } from "../lib/protocol";

/**
 * 顶部非模态横幅：提示"某个后台 thread 正在等确认"。只提示、不抢焦点；
 * 关闭后由调用方记住，直到**新的**审批到达才再弹。
 *
 * 工具名只出现在摘要行里（避免 `getByText(/<tool>/)` 命中多个节点），
 * 每个 Jump 按钮用 aria-label 标注目标 thread。
 */
export function ApprovalBanner({
  items,
  onJump,
  onDismiss,
}: {
  items: ApprovalRequest[];
  onJump: (threadId: string) => void;
  onDismiss: () => void;
}) {
  if (items.length === 0) return null;
  return (
    <div className="flex items-center gap-3 border-b border-amber-900/60 bg-amber-950/60 px-3 py-2 text-xs text-amber-100">
      <span>
        {items.length === 1
          ? `Thread needs approval: ${items[0].params.tool_name}`
          : `${items.length} threads need approval`}
      </span>
      {items.map((r) => (
        <button
          key={r.id}
          type="button"
          onClick={() => onJump(r.params.thread_id)}
          aria-label={`Jump to ${r.params.thread_id}`}
          className="rounded bg-amber-800/70 px-2 py-0.5 hover:bg-amber-700/70"
        >
          Jump
        </button>
      ))}
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss"
        className="ml-auto rounded px-1 text-amber-300 hover:text-amber-100"
      >
        ✕
      </button>
    </div>
  );
}
