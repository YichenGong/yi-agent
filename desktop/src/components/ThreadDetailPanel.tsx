/**
 * 右侧的会话详情栏：`[子 agent | Git Diff]` 两个 Tab，整列高。
 *
 * 它与**左侧会话侧栏同级**（都是主体行的独立列），而不是聊天区下方的一块内容——
 * Git Diff 是会话级的视图，本就不该跟"聊天流"挤在同一列里。子 agent 的列表与
 * 它的轨迹详情也收在「子 agent」这个 Tab 内，故轨迹不再单独占一处。
 *
 * 宽度可拖拽调整，持久化在 `lib/panelWidth.ts`（与左侧栏同一套约定）。
 * 左缘的拖拽手柄只在桌面端出现：手机端它是抽屉，宽度由媒体查询定。
 *
 * 可见性由 `App` 的 `panelOpen` 管（入口在状态栏那颗图标）。
 */
import { useEffect, useRef, useState } from "react";
import { SubagentTrace } from "./SubagentTrace";
import { GitDiffView } from "./GitDiffView";
import { SubagentRail } from "./SubagentRail";
import type { SubagentRow } from "../lib/subagents";
import {
  clampPanelWidth,
  loadPanelWidth,
  savePanelWidth,
} from "../lib/panelWidth";
import type { ComponentProps } from "react";

type Tab = "subagents" | "diff";

export function ThreadDetailPanel({
  tab,
  onTabChange,
  onClose,
  railRows,
  selectedTaskId,
  onOpenTask,
  onLeaveTask,
  traceProps,
  diffProps,
  isMobile,
}: {
  tab: Tab;
  onTabChange: (t: Tab) => void;
  onClose: () => void;
  /** 本会话的子 agent 卡片列表。 */
  railRows: SubagentRow[];
  /** 正在下钻的任务；为 null 时「子 agent」Tab 显示卡片列表。 */
  selectedTaskId: string | null;
  onOpenTask: (taskId: string) => void;
  /** 从某个任务的轨迹退回卡片列表。 */
  onLeaveTask: () => void;
  /** 选中任务时的轨迹详情 props；`selectedTaskId` 为 null 时该位为 null。 */
  traceProps: ComponentProps<typeof SubagentTrace> | null;
  diffProps: ComponentProps<typeof GitDiffView>;
  /** 手机端不渲染拖拽手柄（那里宽度由媒体查询定）。 */
  isMobile: boolean;
}) {
  const [width, setWidth] = useState(loadPanelWidth);
  const widthRef = useRef(width);
  const cleanupDrag = useRef<(() => void) | null>(null);

  // 手柄在左缘，往左拖是变宽：故位移取反。其余（清理、body 样式、blur 兜底）
  // 与 `ThreadSidebar` 的右缘手柄逐条对齐，避免两处行为漂移。
  const onHandleDown = (e: React.MouseEvent) => {
    e.preventDefault();
    cleanupDrag.current?.();
    const startX = e.clientX;
    const startWidth = widthRef.current;
    const prevCursor = document.body.style.cursor;
    const prevUserSelect = document.body.style.userSelect;

    const onMove = (ev: MouseEvent) => {
      const next = clampPanelWidth(startWidth - (ev.clientX - startX));
      widthRef.current = next;
      setWidth(next);
    };

    const cleanup = () => {
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      window.removeEventListener("blur", onBlur);
      document.body.style.cursor = prevCursor;
      document.body.style.userSelect = prevUserSelect;
      cleanupDrag.current = null;
    };

    const onUp = () => {
      cleanup();
      savePanelWidth(widthRef.current);
    };

    const onBlur = () => {
      cleanup();
      savePanelWidth(widthRef.current);
    };

    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    window.addEventListener("blur", onBlur);
    cleanupDrag.current = cleanup;
  };

  useEffect(() => () => cleanupDrag.current?.(), []);

  const tabClass = (t: Tab) =>
    `rounded px-2 py-0.5 text-xs ${
      tab === t ? "bg-panel text-fg" : "text-fg-muted hover:text-fg"
    }`;

  return (
    <aside
      aria-label="会话详情"
      className="relative flex min-h-0 shrink-0 flex-col border-l border-line bg-surface"
      style={isMobile ? undefined : { width }}
    >
      {/* 拖拽手柄：贴左缘，只在桌面端出现（与左侧栏右缘手柄同一形态）。 */}
      {!isMobile && (
        <div
          role="separator"
          aria-label="调整会话详情栏宽度"
          aria-orientation="vertical"
          onMouseDown={onHandleDown}
          className="absolute inset-y-0 left-0 z-30 w-1.5 cursor-col-resize hover:bg-raised/50"
        />
      )}
      <div className="flex items-center justify-between px-3 py-1.5">
        <div role="tablist" className="flex items-center gap-1">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "subagents"}
            className={tabClass("subagents")}
            onClick={() => onTabChange("subagents")}
          >
            子 agent
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "diff"}
            className={tabClass("diff")}
            onClick={() => onTabChange("diff")}
          >
            Git Diff
          </button>
        </div>
        <button
          type="button"
          aria-label="关闭详情"
          className="text-xs text-fg-subtle hover:text-fg-muted"
          onClick={onClose}
        >
          关闭
        </button>
      </div>

      <div className="flex min-h-0 flex-1 flex-col">
        {tab === "subagents" ? (
          selectedTaskId && traceProps ? (
            <div className="flex min-h-0 flex-1 flex-col">
              {/* 退回列表：卡片列表与轨迹详情同处一个 Tab，需要一条回去的路。 */}
              <button
                type="button"
                className="self-start px-3 py-1 text-xs text-sky-300 hover:text-sky-200"
                onClick={onLeaveTask}
              >
                ← 返回子 agent 列表
              </button>
              <SubagentTrace {...traceProps} />
            </div>
          ) : (
            <SubagentRail
              rows={railRows}
              onOpen={onOpenTask}
              selectedTaskId={selectedTaskId}
              embedded
            />
          )
        ) : (
          <GitDiffView {...diffProps} />
        )}
      </div>
    </aside>
  );
}
