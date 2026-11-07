/**
 * 一条轨迹的正文：渐进窗口 + 唯一滚动区。
 *
 * 三条边界：
 *
 * 1. **窗口只向前扩，不向后缩**。首次成功时原子地挂载最近 200 段；之后到达的
 *    事件只是把窗口尾部推长，绝不重算成 `slice(-200)`——那样用户往上滚一点就被
 *    抽走内容。载入更早按 200 一段向前接。
 * 2. **状态由详情层给**。加载中、空、失败、断线、刷新提示只有一处生成（`status`），
 *    这里不再造第二套空态，否则两个地方会给出互相矛盾的说法。有历史时即使是
 *    错误态也照常渲染历史：失败不该把用户正在读的东西抹掉。
 * 3. **滚动只有这一个**。`ol`、子任务区、完整信息都在同一个 `overflow-y-auto`
 *    容器里，页面本身不滚。多个滚动区会争夺滚轮，读长轨迹时最容易迷路。
 */
import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ReactNode } from "react";

import { TraceEvent, type TracePreserve } from "./TraceEvent";
import type { TraceBlock } from "../lib/subagentTraceModel";
import type { TraceSubscriptionSnapshot } from "../lib/subagentTraceSubscription";
import { useTraceScroll } from "../hooks/useTraceScroll";

/** How many blocks the first successful view and each "older" step mounts. */
const WINDOW_STEP = 200;

export interface TraceTimelineTaskInfo {
  taskId: string;
  objective: string | null;
  lastStep: string | null;
}

export interface TraceTimelineProps {
  /** The target key; a new key is a new view and resets the window. */
  identityKey: string;
  phase: TraceSubscriptionSnapshot["phase"];
  blocks: readonly TraceBlock[];
  taskInfo: TraceTimelineTaskInfo;
  /** The one status line: loading / empty / error / disconnected / refresh hint. */
  status: ReactNode;
  onFallbackFocus(): void;
  /** Children area rendered inside the same scroller, after the timeline. */
  after: (preserve: TracePreserve) => ReactNode;
}

export interface TraceTimelineHandle {
  showTaskInfo(entry: HTMLButtonElement): void;
}

const NOT_PROVIDED = "服务端未提供";

/** Where the mounted window starts, by block key. */
interface WindowState {
  identityKey: string;
  /** The key of the first mounted block, or null when the window covers all. */
  startKey: string | null;
  /** Last known first event id, used when the start key was pruned. */
  startEventId: number;
  /** True once a successful view established the window. */
  initialised: boolean;
  /** False until that success; keeps loading/error out of the decision. */
  mounted: number;
}

function initialWindow(identityKey: string): WindowState {
  return { identityKey, startKey: null, startEventId: 0, initialised: false, mounted: 0 };
}

export const TraceTimeline = forwardRef<TraceTimelineHandle, TraceTimelineProps>(
  function TraceTimeline({ identityKey, phase, blocks, taskInfo, status, onFallbackFocus, after }, ref) {
    const [window, setWindow] = useState<WindowState>(() => initialWindow(identityKey));
    const [infoOpen, setInfoOpen] = useState(false);
    const scroll = useTraceScroll(identityKey, phase === "ready", onFallbackFocus);
    const listRef = useRef<HTMLOListElement | null>(null);
    /** 载早入口点了之后是否该把焦点移交给新的首段。 */
    const handoffRef = useRef(false);

    // A different task is a different timeline: start over.
    if (window.identityKey !== identityKey) {
      setWindow(initialWindow(identityKey));
    }

    /**
     * The mounted slice.
     *
     * `startKey` wins over the index: the snapshot may prune rows while the window
     * still points at one, and clamping by key keeps the user near the same place
     * instead of falling back to "mount everything".
     */
    const visible = useMemo(() => {
      if (blocks.length === 0) return [];
      if (!window.initialised) return [];
      if (window.startKey === null) return blocks as TraceBlock[];
      const index = blocks.findIndex((block) => block.key === window.startKey);
      if (index >= 0) return (blocks as TraceBlock[]).slice(index);
      // The start was pruned: resume at the first block at or after the old id.
      const resume = blocks.findIndex((block) => block.firstEventId >= window.startEventId);
      const from = resume < 0 ? 0 : resume;
      return (blocks as TraceBlock[]).slice(from);
    }, [blocks, window.initialised, window.startEventId, window.startKey]);

    // Establish the window the first time there is anything to show.
    //
    // `ready` is the canonical trigger, but an error after a successful load (or a
    // snapshot that arrives while the panel is rebinding) still has real history,
    // and rendering an empty list would look like the agent did nothing. An empty
    // `blocks` array is left alone: that is the loading case, not a history.
    useLayoutEffect(() => {
      if (window.initialised) return;
      if (blocks.length === 0) return;
      if (phase !== "ready" && phase !== "error") return;
      const start = blocks.length > WINDOW_STEP ? blocks.length - WINDOW_STEP : 0;
      setWindow({
        identityKey,
        startKey: start === 0 ? null : blocks[start].key,
        startEventId: start === 0 ? (blocks[0]?.firstEventId ?? 0) : blocks[start].firstEventId,
        initialised: true,
        mounted: blocks.length - start,
      });
    }, [blocks, identityKey, phase, window.initialised]);

    // Appends push the tail; they never move the start.
    useEffect(() => {
      if (!window.initialised) return;
      setWindow((previous) => {
        if (previous.identityKey !== identityKey) return previous;
        if (previous.startKey === null) return previous;
        if (visible.length === previous.mounted) return previous;
        return { ...previous, mounted: visible.length };
      });
    }, [identityKey, visible.length, window.initialised, window.startKey]);

    // Follow or hold the reading position whenever the visible content changes.
    useLayoutEffect(() => {
      scroll.followIfAtBottom();
    }, [scroll, visible.length]);

    const loadEarlier = useCallback((entry: HTMLButtonElement) => {
      scroll.preserve(() => {
        setWindow((previous) => {
          const current = visible;
          if (current.length === 0) return previous;
          const firstIndex = blocks.findIndex((block) => block.key === current[0].key);
          const from = Math.max(0, (firstIndex < 0 ? 0 : firstIndex) - WINDOW_STEP);
          return {
            ...previous,
            startKey: from === 0 ? null : blocks[from].key,
            startEventId: blocks[from].firstEventId,
            mounted: blocks.length - from,
          };
        });
      });
      // 焦点不能留在一个即将消失的按钮上。判据是「点击的入口是否持焦」而不是
      // 「当前焦点是否还连着」：节点被移除时浏览器把 `activeElement` 退回 `body`，
      // 而 `body` 是 connected 的，用后者判断永远不成立。只有在入口本来就持焦时
      // 才移交，避免抢走用户正在编辑的 textarea 焦点。
      handoffRef.current = document.activeElement === entry;
    }, [blocks, scroll, visible]);

    useImperativeHandle(ref, () => ({
      showTaskInfo(entry: HTMLButtonElement) {
        scroll.showTaskInfo(() => setInfoOpen(true), entry);
      },
    }), [scroll]);

    // 移交发生在 DOM 落定之后：此刻才知道入口是否真的消失了。
    useLayoutEffect(() => {
      if (!handoffRef.current) return;
      handoffRef.current = false;
      if (document.activeElement instanceof HTMLElement && document.activeElement.isConnected
        && listRef.current?.contains(document.activeElement)) {
        return;
      }
      listRef.current?.querySelector<HTMLElement>("[data-trace-key]")?.focus();
    });

    const hasEarlier = window.initialised && blocks.length > visible.length;

    return (
      // 外层只负责定位浮钮：浮钮必须悬浮在视口之上，不能占正文流的空间，
      // 否则它自己就把最后一条事件顶出视野。
      <div className="relative flex min-h-0 flex-1 flex-col">
        <div
          ref={scroll.ref}
          data-trace-viewport=""
          role="log"
          aria-label="子 agent 轨迹"
          className="subagent-trace min-h-0 flex-1 overflow-y-auto px-3 py-2"
          style={{ overflowAnchor: scroll.following ? undefined : "none" }}
        >
          <div ref={scroll.contentRef} className="flex min-w-0 flex-col gap-2 pb-8">
            {status}

            <div
              ref={scroll.taskInfoRef}
              hidden={!infoOpen}
              className="rounded border border-line bg-panel px-3 py-2"
            >
              <div className="flex items-center justify-between gap-2">
                <h3
                  ref={scroll.taskInfoHeadingRef}
                  tabIndex={-1}
                  className="text-xs font-medium text-fg"
                >
                  完整任务信息
                </h3>
                <button
                  type="button"
                  onClick={() => {
                    setInfoOpen(false);
                    scroll.restoreTaskInfo(() => {});
                  }}
                  className="shrink-0 text-xs text-fg-subtle hover:text-fg-muted"
                >
                  收起完整信息
                </button>
              </div>
              <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs">
                <dt className="text-fg-subtle">任务 ID</dt>
                <dd className="min-w-0 break-all font-mono text-fg">{taskInfo.taskId}</dd>
                <dt className="text-fg-subtle">目标</dt>
                <dd className="min-w-0 break-words text-fg">
                  {taskInfo.objective ?? <span className="text-fg-subtle">{NOT_PROVIDED}</span>}
                </dd>
                <dt className="text-fg-subtle">当前步骤</dt>
                <dd className="min-w-0 break-words text-fg">
                  {taskInfo.lastStep ?? <span className="text-fg-subtle">{NOT_PROVIDED}</span>}
                </dd>
              </dl>
            </div>

            {hasEarlier ? (
              <button
                type="button"
                onClick={(event) => loadEarlier(event.currentTarget)}
                className="self-center rounded border border-line px-2 py-1 text-xs text-fg-muted hover:border-line-strong hover:text-fg"
              >
                载入更早 200 段
              </button>
            ) : null}

            <ol ref={listRef} className="flex min-w-0 flex-col gap-2">
              {visible.map((block) => (
                <li key={block.key} data-trace-key={block.key} tabIndex={-1} className="min-w-0">
                  <TraceEvent block={block} onPreserve={scroll.preserve} />
                </li>
              ))}
            </ol>

            {after(scroll.preserve)}
          </div>
        </div>

        {!scroll.following ? (
          <button
            type="button"
            onClick={scroll.scrollToBottom}
            className="absolute bottom-2 right-3 rounded-full border border-line bg-raised px-3 py-1 text-xs text-fg-muted shadow hover:text-fg"
          >
            回到底部
          </button>
        ) : null}
      </div>
    );
  },
);
