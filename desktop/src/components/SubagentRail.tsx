/**
 * 主对话旁的子 agent 列。
 *
 * 两种形态，共用同一份卡片渲染：
 * - **嵌进右栏**（`embedded`）：`ThreadDetailPanel` 的「子 agent」Tab 内容，外框
 *   与页眉（Tab 条）由面板提供，这里只出列表主体；
 * - **独立一列**：自带 `aside` 外框与标题栏。此形态保留给仍需要独立列的场景。
 *
 * 无论哪种，点卡片都回调 `onOpen(taskId)` 进入轨迹详情。
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
      return "text-fg-muted";
  }
}

export function SubagentRail({
  rows,
  onOpen,
  onCollapse,
  selectedTaskId,
  embedded = false,
}: {
  rows: SubagentRow[];
  onOpen: (taskId: string) => void;
  onCollapse?: () => void;
  /** The card the user has opened, highlighted so the rail shows the choice. */
  selectedTaskId?: string | null;
  /** 嵌入右栏时不再自带 `aside` 外框与标题栏（面板已提供）。 */
  embedded?: boolean;
}) {
  const list = (
    <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3" {...(embedded ? { "data-embedded": "" } : {})}>
      {rows.length === 0 ? (
        <p className="px-1 py-2 text-sm text-fg-subtle">暂无子 agent</p>
      ) : (
        <ul className="flex flex-col gap-2">
          {rows.map((row) => (
            <li key={row.taskId}>
              <button
                type="button"
                aria-label={`查看子 agent ${row.taskId}`}
                onClick={() => onOpen(row.taskId)}
                aria-current={selectedTaskId === row.taskId ? "true" : undefined}
                className={`w-full rounded border px-3 py-2 text-left hover:border-line-strong ${
                  selectedTaskId === row.taskId
                    ? "border-sky-600 bg-panel"
                    : "border-line bg-panel"
                }`}
              >
                <div className="flex items-center gap-2">
                  <span className={`text-xs ${stateColor(row.state)}`}>[{row.state}]</span>
                </div>
                <div className="mt-1 truncate text-sm text-fg">
                  {row.objective ?? row.taskId}
                </div>
                <div className="mt-1 truncate text-xs text-fg-subtle">
                  {row.finished ? "已结束" : (row.lastStep ?? "工作中")}
                </div>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );

  if (embedded) return list;

  return (
    <aside
      aria-label="子 agent"
      className="flex w-72 min-w-0 shrink-0 flex-col border-l border-line bg-raised"
    >
      <div className="flex items-center justify-between px-3 py-2">
        <h2 className="text-xs font-medium uppercase tracking-wide text-fg-muted">
          子 agent{rows.length > 0 ? ` (${rows.length})` : ""}
        </h2>
        {onCollapse && (
          <button
            type="button"
            aria-label="收起子 agent"
            className="text-xs text-fg-subtle hover:text-fg-muted"
            onClick={onCollapse}
          >
            收起
          </button>
        )}
      </div>
      {list}
    </aside>
  );
}
