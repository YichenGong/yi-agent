import type { ApprovalRequest } from "../lib/protocol";

/**
 * 顶部非模态横幅：提示"某个后台 thread 正在等确认"。只提示、不抢焦点；
 * 关闭后由调用方记住，直到**新的**审批到达才再弹。
 *
 * 每个 Jump 按钮显示工具名（`Jump · <tool>`）以便视力用户区分；`aria-label`
 * 标注目标 thread，既服务读屏器又能消歧同名工具。
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
    <div
      role="status"
      aria-live="polite"
      className="flex flex-wrap items-center gap-3 border-b border-amber-900/60 bg-amber-950/60 px-3 py-2 text-xs text-amber-100"
    >
      <span className="min-w-0 truncate">
        {items.length === 1
          ? "1 thread needs approval"
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
          {`Jump · ${r.params.tool_name}`}
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
