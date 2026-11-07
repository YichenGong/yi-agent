/**
 * 主对话旁的子 agent 列。
 *
 * 两种形态，共用同一份卡片渲染：
 * - **嵌进右栏**（`embedded`）：`ThreadDetailPanel` 的「子 agent」Tab 内容，外框
 *   与页眉（Tab 条）由面板提供，这里只出列表主体；
 * - **独立一列**：自带 `aside` 外框与标题栏。此形态保留给仍需要独立列的场景。
 *
 * 无论哪种，点卡片都回调 `onOpen(taskId)` 进入轨迹详情。
 *
 * 两条克制：
 * - **状态只从共享表读**（`subagentState`），不在这里再写一份 switch：本地 switch
 *   一旦落后，侧栏就会给 `blocked` 配灰色、给 `cancelled` 配红色，和详情页对不上；
 * - **没有元数据就显示 ID**。生产上 `objective`/`lastStep` 常为 null，编一个「目标」
 *   或拿旧步骤充数，都是用户无法验证的谎。
 */
import { type Ref } from "react";
import { subagentState, type SubagentRow } from "../lib/subagents";

/** 状态点用哪个强调色；文字本身始终读 `fg`，颜色不是唯一信号。 */
const TONE_CLASS: Record<string, string> = {
  neutral: "bg-fg-faint",
  success: "bg-trace-success",
  warning: "bg-trace-warning",
  danger: "bg-trace-danger",
};

export function SubagentRail({
  rows,
  onOpen,
  onCollapse,
  selectedTaskId,
  embedded = false,
  headingRef,
  rowRef,
}: {
  rows: SubagentRow[];
  onOpen: (taskId: string) => void;
  onCollapse?: () => void;
  /** The card the user has opened, highlighted so the rail shows the choice. */
  selectedTaskId?: string | null;
  /** 嵌入右栏时不再自带 `aside` 外框与标题栏（面板已提供）。 */
  embedded?: boolean;
  /** 面板的标题节点，返回焦点时用。 */
  headingRef?: Ref<HTMLHeadingElement>;
  /** 每张卡片自己的节点，离开详情时把焦点还给点开它的那一行。 */
  rowRef?: (taskId: string, node: HTMLButtonElement | null) => void;
}) {
  const activeCount = rows.filter((row) => subagentState(row.state).active).length;
  const heading =
    rows.length === 0
      ? "子 agent"
      : activeCount > 0
        ? `子 agent (${activeCount} 进行中 / ${rows.length})`
        : `子 agent (${rows.length})`;

  const list = (
    <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3" {...(embedded ? { "data-embedded": "" } : {})}>
      {rows.length === 0 ? (
        <p className="px-1 py-2 text-sm text-fg-subtle">暂无子 agent</p>
      ) : (
        <ul className="flex flex-col gap-1 divide-y divide-line rounded border border-line bg-panel">
          {rows.map((row) => {
            const state = subagentState(row.state);
            const selected = selectedTaskId === row.taskId;
            // 终态和暂停都藏着旧步骤：一个是旅程结束，一个是停下来，
            // 两者继续显示「正在跑测试」都会让人以为它还在动。
            const showStep = !row.finished && row.state !== "paused" && row.lastStep !== null;
            return (
              <li key={row.taskId} className="min-w-0">
                <button
                  type="button"
                  ref={(node) => rowRef?.(row.taskId, node)}
                  aria-label={`查看子 agent ${row.taskId}`}
                  onClick={() => onOpen(row.taskId)}
                  aria-current={selected ? "true" : undefined}
                  className={`w-full min-w-0 px-3 py-2 text-left transition-colors hover:bg-raised ${
                    selected ? "bg-raised" : ""
                  }`}
                >
                  <div className="flex min-w-0 items-center gap-2">
                    <span aria-hidden="true" className={`size-1.5 shrink-0 rounded-full ${TONE_CLASS[state.tone]}`} />
                    <span className="shrink-0 text-xs text-fg-muted">{state.label}</span>
                  </div>
                  <div className="mt-1 line-clamp-2 break-words text-sm text-fg">
                    {row.objective ?? row.taskId}
                  </div>
                  {showStep ? (
                    <div className="mt-1 truncate text-xs text-fg-subtle">{row.lastStep}</div>
                  ) : null}
                </button>
              </li>
            );
          })}
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
      <div className="flex items-center justify-between gap-2 px-3 py-2">
        <h2 ref={headingRef} tabIndex={-1} className="truncate text-xs font-medium tracking-wide text-fg-muted">
          {heading}
        </h2>
        {onCollapse && (
          <button
            type="button"
            aria-label="收起子 agent"
            className="shrink-0 text-xs text-fg-subtle hover:text-fg-muted"
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
