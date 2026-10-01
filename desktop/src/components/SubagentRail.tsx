/**
 * 主对话旁的子 agent 暂留区。
 *
 * 与主对话并列、可折叠,不是弹窗:用户要能在读主对话的同时瞥见子 agent 在做什么,
 * 弹窗会挡住他正在读的东西。点卡片回调 `onOpen(taskId)` 进入详情。
 */
import type { SubagentRow } from "../lib/subagents";

/** 状态 → 颜色,与 TUI 页签保持同一套语义。 */
function stateColor(state: string): string {
  switch (state) {
    case "completed":
    case "completed_no_changes":
      return "text-emerald-400";
    case "failed":
    case "cancelled":
      return "text-red-400";
    case "running":
    case "waiting_for_children":
      return "text-amber-400";
    default:
      return "text-neutral-400";
  }
}

export function SubagentRail({
  rows,
  onOpen,
  onCollapse,
  selectedTaskId,
}: {
  rows: SubagentRow[];
  onOpen: (taskId: string) => void;
  onCollapse?: () => void;
  /** The card the user has opened, highlighted so the rail shows the choice. */
  selectedTaskId?: string | null;
}) {
  return (
    <aside
      aria-label="子 agent"
      className="flex w-72 min-w-0 shrink-0 flex-col border-l border-neutral-800 bg-neutral-925"
    >
      <div className="flex items-center justify-between px-3 py-2">
        <h2 className="text-xs font-medium uppercase tracking-wide text-neutral-400">
          子 agent{rows.length > 0 ? ` (${rows.length})` : ""}
        </h2>
        {onCollapse && (
          <button
            type="button"
            aria-label="收起子 agent"
            className="text-xs text-neutral-500 hover:text-neutral-300"
            onClick={onCollapse}
          >
            收起
          </button>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {rows.length === 0 ? (
          <p className="px-1 py-2 text-sm text-neutral-500">暂无子 agent</p>
        ) : (
          <ul className="flex flex-col gap-2">
            {rows.map((row) => (
              <li key={row.taskId}>
                <button
                  type="button"
                  aria-label={`查看子 agent ${row.taskId}`}
                  onClick={() => onOpen(row.taskId)}
                  aria-current={selectedTaskId === row.taskId ? "true" : undefined}
                  className={`w-full rounded border px-3 py-2 text-left hover:border-neutral-600 ${
                    selectedTaskId === row.taskId
                      ? "border-sky-600 bg-neutral-900"
                      : "border-neutral-800 bg-neutral-900"
                  }`}
                >
                  <div className="flex items-center gap-2">
                    <span className={`text-xs ${stateColor(row.state)}`}>[{row.state}]</span>
                  </div>
                  <div className="mt-1 truncate text-sm text-neutral-200">
                    {row.objective ?? row.taskId}
                  </div>
                  <div className="mt-1 truncate text-xs text-neutral-500">
                    {row.finished ? "已结束" : (row.lastStep ?? "工作中")}
                  </div>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </aside>
  );
}
