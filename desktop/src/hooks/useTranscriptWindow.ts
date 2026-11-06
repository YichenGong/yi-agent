import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  DEFAULT_LIMITS,
  nextWindow,
  type FocusRequest,
  type VisibleRange,
  type WindowLimits,
  type WindowState,
} from "../lib/transcriptWindow";
import type { HeightBook } from "../lib/heightBook";

export interface UseTranscriptWindowArgs {
  itemCount: number;
  /** 测量当前视口。未布局时返回 null（此时保持跟随，不翻页）。 */
  measure: () => VisibleRange | null;
  /** 一次性开窗请求；调用方负责在消费后清空。 */
  focus?: FocusRequest | null;
  limits?: Partial<WindowLimits>;
  book: HeightBook;
}

export interface TranscriptWindowApi {
  window: WindowState;
  requestFocus: (f: FocusRequest) => void;
  clearFocus: () => void;
  /**
   * 告诉 hook「视口可能变了，请重新测量」。
   *
   * **必须由调用方在滚动时调用。** 翻页的触发条件（`gapTopPx` / `gapBottomPx`）
   * 只存在于 `measure()` 的返回值里，而滚动既不改变 `itemCount` 也不改变 `focus`，
   * 没有任何 React 依赖会因此变化。少了这个入口，向上翻页永远不会发生——
   * 窗口会停在冷启动那 60 条上。
   *
   * 每帧最多真正重算一次，滚动事件可以逐帧调用。
   */
  nudge: () => void;
}

/**
 * 维护转录窗口。
 *
 * 它**不含**高度簿记（那是 `HeightBook`）与纯计算（那是 `nextWindow`）：
 * 这里只负责「在合适的时机把测量结果喂给纯函数」，以及把
 * 锚点/搜索跳转翻译成一次性的 `FocusRequest`。
 */
export function useTranscriptWindow({
  itemCount,
  measure,
  focus = null,
  limits,
  book,
}: UseTranscriptWindowArgs): TranscriptWindowApi {
  // **必须按标量字段 memo**，不能依赖 `limits` 对象身份：调用方几乎总是传字面量
  // （`limits={{ pageSize: 200 }}`），对象身份每次渲染都变 -> `merged` 变 ->
  // `recompute` 变 -> 下面依赖 `recompute` 的 effect 每次渲染都跑 -> 每帧一次
  // `setState`，哪怕窗口没变化也是在空转。按值比较才能真正稳定。
  const initialSize = limits?.initialSize ?? DEFAULT_LIMITS.initialSize;
  const pageSize = limits?.pageSize ?? DEFAULT_LIMITS.pageSize;
  const buffer = limits?.buffer ?? DEFAULT_LIMITS.buffer;
  const maxMounted = limits?.maxMounted ?? DEFAULT_LIMITS.maxMounted;
  const merged: WindowLimits = useMemo(
    () => ({
      initialSize,
      pageSize,
      buffer,
      maxMounted,
      // `bufferPx` 在这里只是占位，真正取值在 `recompute` 里按当时的 `book.median()`
      // 现算（见下）。**不能**把它写进 memo：`book` 的身份是稳定的（调用方
      // `useMemo(() => createHeightBook(), [])`），所以 `book.median()` 后来变了
      // memo 也不会重算 —— 中位数会一直停留在冷启动的 fallback 上。反过来，若把
      // 中位数拉进依赖，它每帧都可能变，`merged` 变 -> `recompute` 变 -> effect
      // 每帧重跑，又变成空转。放在 rAF 回调里读，两个坑都避开。
      bufferPx: DEFAULT_LIMITS.bufferPx,
    }),
    [initialSize, pageSize, buffer, maxMounted],
  );

  const [state, setState] = useState<WindowState | null>(null);
  const stateRef = useRef<WindowState | null>(null);
  stateRef.current = state;
  // focus 用 ref 而不是 state：它只影响**一次**窗口计算，不需要触发渲染。
  // `appliedRef` 按对象身份去重——若调用方没及时把 `focus` 清空（清空时机
  // 在调用方，难免滞后），每次重算都会重新跳到同一位置，用户就再也滚不动了。
  const pendingRef = useRef<FocusRequest | null>(null);
  const appliedRef = useRef<FocusRequest | null>(null);

  // 每帧最多重算一次：滚动期间 `measure` 会读 DOM，逐帧多次没有意义。
  const frameRef = useRef<number | null>(null);
  const itemCountRef = useRef(itemCount);
  itemCountRef.current = itemCount;
  const recompute = useCallback(() => {
    if (frameRef.current !== null) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      // 把「挂载区相对序号」翻译成绝对 index：`measure` 只看得见挂载区，
      // 它数不出前面还有多少条。`nextWindow` 的截断判据用的是绝对 index。
      const raw = measure();
      const base = stateRef.current?.start ?? 0;
      const visible = raw ? { ...raw, first: base + raw.first, last: base + raw.last } : null;
      const next = nextWindow({
        current: stateRef.current,
        itemCount: itemCountRef.current,
        // 像素阈值随**当时**的真实中位数走：重尾分布下按条数判断会误触发。
        // 每次重算都重新取，中位数随测量收敛后阈值自动跟上。
        limits: { ...merged, bufferPx: merged.buffer * book.median() },
        visible,
        focus: pendingRef.current,
      });
      // 消费掉这次 focus：它只影响一次窗口计算。
      pendingRef.current = null;
      const prev = stateRef.current;
      if (
        prev !== null &&
        prev.start === next.start &&
        prev.end === next.end &&
        prev.following === next.following
      ) {
        return; // 无变化不触发渲染
      }
      stateRef.current = next;
      setState(next);
    });
  }, [measure, merged, book]);

  // 条数变化（回放、流式增长、clear）都要重算。
  useEffect(() => {
    recompute();
  }, [recompute, itemCount]);

  // 外部传入的 focus 也生效，但同一个对象只应用一次（见 appliedRef 注释）。
  useEffect(() => {
    if (focus === null || focus === appliedRef.current) return;
    appliedRef.current = focus;
    pendingRef.current = focus;
    recompute();
  }, [focus, recompute]);

  useEffect(
    () => () => {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
    },
    [],
  );

  // 同步写入 ref 再调度：不经 React state 往返，跳转少一帧，不会先闪一下旧窗口。
  const requestFocus = useCallback(
    (f: FocusRequest) => {
      appliedRef.current = f;
      pendingRef.current = f;
      recompute();
    },
    [recompute],
  );
  const clearFocus = useCallback(() => {
    appliedRef.current = null;
    pendingRef.current = null;
  }, []);
  const nudge = useCallback(() => {
    recompute();
  }, [recompute]);

  return {
    // 首帧同步给一个合法窗口，避免渲染出空列表再跳。
    window: state ?? {
      start: Math.max(itemCount - DEFAULT_LIMITS.initialSize, 0),
      end: itemCount,
      following: true,
    },
    requestFocus,
    clearFocus,
    nudge,
  };
}
