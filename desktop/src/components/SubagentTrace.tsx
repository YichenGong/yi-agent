/**
 * 子 agent 详情：头部 + 时间线 + 子任务 + 常驻消息区。
 *
 * 这个组件只负责**组合与布局**，不推断任务状态：
 *
 * * 头部三行标题、两行步骤、短 ID、「查看完整任务信息」入口。完整目标 / 完整 ID /
 *   完整步骤都在主滚动区里展开——header 不放 `title` 悬停提示，因为触屏和键盘都
 *   拿不到它。
 * * 时间线是唯一主纵向滚动区。子任务区也在这个滚动区里（`after` 槽），不额外造
 *   第二个滚动区：两个滚动区会争夺滚轮，读长轨迹时最容易迷路。
 * * 「等待新的执行进展」只在**已知活动状态**下出现。paused 说已暂停、终态说终态、
 *   row 缺失说状态未知——否则会给用户一个永远不动的假象。
 * * loading / empty / error / disconnected 文案只在这里生成一次（作为
 *   `TraceTimeline` 的 `status`），Timeline 自己不再造第二套空态。
 */
import { useCallback, useId, useLayoutEffect, useMemo, useRef, useState } from "react";

import { TraceComposer } from "./TraceComposer";
import { TraceTimeline, type TraceTimelineHandle } from "./TraceTimeline";
import type { TraceActionSnapshot } from "../lib/subagentTraceActions";
import { foldTraceRows } from "../lib/subagentTraceModel";
import type { TraceSubscriptionSnapshot } from "../lib/subagentTraceSubscription";
import { subagentState, type SubagentRow } from "../lib/subagents";

export interface SubagentTraceProps {
  /** The target key; a new key is a new view (window, disclosure state). */
  identityKey: string;
  taskId: string;
  row: SubagentRow | null;
  parent: SubagentRow | null;
  children: SubagentRow[];
  subscription: TraceSubscriptionSnapshot;
  actions: TraceActionSnapshot;
  onDrill(taskId: string): void;
  onBack(): void;
  onRetry(): void;
  onDraftChange(text: string): void;
  onSend(): Promise<void>;
  onPreview(): Promise<void>;
  onConfirm(): Promise<void>;
  onDismiss(): void;
}

const NOT_PROVIDED = "服务端未提供";
const NO_TRACE = "该任务暂无轨迹";
const LOADING = "正在加载轨迹…";
const DISCONNECTED = "连接已断开";
const WAITING = "等待新的执行进展…";

/** Enough of the id to recognise it; the full value lives in the info region. */
const SHORT_ID_CHARS = 8;

function shortId(taskId: string): string {
  return taskId.length <= SHORT_ID_CHARS ? taskId : taskId.slice(0, SHORT_ID_CHARS);
}

const TONE: Record<string, string> = {
  neutral: "text-fg-muted",
  success: "text-success",
  warning: "text-warning",
  danger: "text-danger",
};

export function SubagentTrace({
  identityKey,
  taskId,
  row,
  parent,
  children,
  subscription,
  actions,
  onDrill,
  onBack,
  onRetry,
  onDraftChange,
  onSend,
  onPreview,
  onConfirm,
  onDismiss,
}: SubagentTraceProps) {
  const timelineRef = useRef<TraceTimelineHandle | null>(null);
  const headingRef = useRef<HTMLHeadingElement | null>(null);
  const childrenToggleRef = useRef<HTMLButtonElement | null>(null);
  const childrenId = useId();
  const [childrenOpen, setChildrenOpen] = useState(true);

  // 折叠状态属于上一条轨迹：切任务要回到默认展开。
  const [seenIdentity, setSeenIdentity] = useState(identityKey);
  if (seenIdentity !== identityKey) {
    setSeenIdentity(identityKey);
    setChildrenOpen(true);
  }

  // 进入即把焦点交给标题：键盘用户需要落在面板里，而不是留在列表按钮上。
  useLayoutEffect(() => {
    headingRef.current?.focus({ preventScroll: true });
  }, [identityKey]);

  const blocks = useMemo(() => foldTraceRows(subscription.rows), [subscription.rows]);

  const presentation = row === null ? null : subagentState(row.state);
  const finished = row?.finished === true;
  // 只按共享状态表判断「还在等进展」：paused 与未知状态都不算。
  const waiting = presentation?.active === true;

  const status = (() => {
    if (subscription.phase === "loading" && blocks.length === 0) {
      return <p className="text-xs text-fg-subtle">{LOADING}</p>;
    }
    if (subscription.disconnected) {
      return (
        <div className="flex items-center gap-2 text-xs text-warning">
          <span>{DISCONNECTED}</span>
          <button type="button" onClick={onRetry} className="underline hover:text-fg">
            重新订阅
          </button>
        </div>
      );
    }
    if (subscription.phase === "error") {
      return (
        <div className="flex items-center gap-2 text-xs text-danger">
          <span>{subscription.error ?? DISCONNECTED}</span>
          <button type="button" onClick={onRetry} className="underline hover:text-fg">
            重新订阅
          </button>
        </div>
      );
    }
    if (subscription.phase === "ready" && blocks.length === 0) {
      return <p className="text-xs text-fg-subtle">{NO_TRACE}</p>;
    }
    return null;
  })();

  const focusHeading = useCallback(() => {
    headingRef.current?.focus({ preventScroll: true });
  }, []);

  const parentIsOther = parent !== null && parent.taskId !== taskId;
  const visibleChildren = children.filter((child) => child.taskId !== taskId);

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <header className="shrink-0 border-b border-line px-3 py-2">
        <div className="flex min-w-0 items-start justify-between gap-2">
          <div className="flex min-w-0 flex-col gap-1">
            {parentIsOther ? (
              <button
                type="button"
                onClick={() => onDrill(parent.taskId)}
                aria-label={`返回父任务 ${parent.objective ?? parent.taskId}`}
                className="self-start text-xs text-fg-subtle hover:text-fg"
              >
                ‹ {parent.objective ?? parent.taskId}
              </button>
            ) : null}

            <h2
              ref={headingRef}
              tabIndex={-1}
              className="line-clamp-3 min-w-0 break-words text-sm font-medium text-fg"
            >
              {row?.objective ?? NOT_PROVIDED}
            </h2>

            <p className="line-clamp-2 min-w-0 break-words text-xs text-fg-muted">
              {row?.lastStep ?? NOT_PROVIDED}
            </p>

            <div className="flex min-w-0 items-center gap-2 text-xs text-fg-subtle">
              <span className={presentation ? TONE[presentation.tone] : TONE.neutral}>
                {presentation?.label ?? "状态未知"}
              </span>
              <span aria-hidden="true">·</span>
              <span className="font-mono">{shortId(taskId)}</span>
              <button
                type="button"
                onClick={(event) => timelineRef.current?.showTaskInfo(event.currentTarget)}
                className="text-fg-muted underline hover:text-fg"
              >
                查看完整任务信息
              </button>
            </div>
          </div>

          <div className="flex shrink-0 items-center gap-2">
            {subscription.phase === "ready" ? (
              <button
                type="button"
                onClick={onRetry}
                className="text-xs text-fg-subtle hover:text-fg"
              >
                刷新轨迹
              </button>
            ) : null}
            <button
              type="button"
              onClick={onBack}
              className="text-xs text-fg-muted hover:text-fg"
            >
              返回任务列表
            </button>
          </div>
        </div>
      </header>

      <TraceTimeline
        ref={timelineRef}
        identityKey={identityKey}
        phase={subscription.phase}
        blocks={blocks}
        taskInfo={{ taskId, objective: row?.objective ?? null, lastStep: row?.lastStep ?? null }}
        status={status}
        onFallbackFocus={focusHeading}
        after={(preserve) => (
          <>
            {visibleChildren.length > 0 ? (
              <section className="border-t border-line pt-2">
                <button
                  type="button"
                  ref={childrenToggleRef}
                  aria-expanded={childrenOpen}
                  aria-controls={childrenId}
                  onClick={() => preserve(() => setChildrenOpen((open) => !open))}
                  className="text-xs text-fg-muted hover:text-fg"
                >
                  子任务 {visibleChildren.length}
                  <span aria-hidden="true">{childrenOpen ? " 收起" : " 展开"}</span>
                </button>
                <ul id={childrenId} hidden={!childrenOpen} className="mt-1 flex flex-col gap-1">
                  {visibleChildren.map((child) => (
                    <li key={child.taskId}>
                      <button
                        type="button"
                        aria-label={`进入子任务 ${child.objective ?? child.taskId}`}
                        onClick={() => onDrill(child.taskId)}
                        className="flex w-full min-w-0 items-center gap-2 rounded px-1 py-1 text-left text-xs hover:bg-panel"
                      >
                        <span className={TONE[subagentState(child.state).tone]}>
                          {subagentState(child.state).label}
                        </span>
                        <span className="min-w-0 flex-1 truncate text-fg">
                          {child.objective ?? child.taskId}
                        </span>
                        <span aria-hidden="true" className="text-fg-faint">
                          ›
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              </section>
            ) : null}

            {waiting ? (
              <p className="px-1 text-xs text-fg-faint">{WAITING}</p>
            ) : null}
          </>
        )}
      />

      <TraceComposer
        value={actions}
        finished={finished}
        onDraftChange={onDraftChange}
        onSend={onSend}
        onPreview={onPreview}
        onConfirm={onConfirm}
        onDismiss={onDismiss}
        onFallbackFocus={focusHeading}
      />
    </div>
  );
}
