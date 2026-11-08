import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  DEFAULT_LIMITS,
  nextWindow,
  type FocusRequest,
  type WindowLimits,
  type WindowState,
} from "../lib/transcriptWindow";
import type { WindowMeasurement } from "../lib/transcriptGeometry";
import type { HeightBook } from "../lib/heightBook";
import {
  createTranscriptPositionIndex,
  type TranscriptPositionIndex,
} from "../lib/transcriptPositionIndex";
import {
  beginConvergence,
  decideConvergence,
  estimateFocusIndex,
  geometrySignature,
  noteAdvance,
  noteFocus,
  type ConvergenceCycle,
} from "../lib/transcriptConvergence";

export interface UseTranscriptWindowArgs {
  itemCount: number;
  /**
   * 当前展示的会话 id。窗口状态是「这次访问停在哪」的产物，**不属于组件生命周期**：
   * 切换会话必须回到尾部首屏，不能继承上一条会话的中间窗口。
   */
  threadId?: string | null;
  /**
   * 测量当前视口。未布局时返回 null（此时暂停开窗，但**不**改变跟随意图）。
   * 结果里的 index 已是绝对序号、gap 只算历史区，hook 不再二次换算。
   */
  measure: () => WindowMeasurement | null;
  /** 一次性开窗请求；调用方负责在消费后清空。 */
  focus?: FocusRequest | null;
  limits?: Partial<WindowLimits>;
  book: HeightBook;
  /**
   * 当前条目 id 顺序（只读，长度应等于 `itemCount`）。
   *
   * 用于「视口落在未挂载区间」时把偏移换算成条号。缺省时该能力降级为不做——
   * 调用方没给顺序就无从判断第几条是谁，不能靠猜。
   */
  anchorIds?: readonly string[];
  /**
   * 当前是否处于「跟随尾部」意图（来自 ScrollCoordinator，见 Spec §4.3）。
   *
   * 收敛补页必须**让位**给它：跟随状态下视口位置由贴底写入负责，此刻视口暂时
   * 没盖住窗口是正常的（贴底还没执行）。若收敛也在这里补页，两者会互相抢
   * `scrollTop`，表现为切换会话后首屏被撑到几百条、以及来回抖动。
   */
  following?: () => boolean;
}

export interface TranscriptWindowApi {
  window: WindowState;
  requestFocus: (f: FocusRequest) => void;
  clearFocus: () => void;
  /**
   * 告诉 hook「视口可能变了，请重新测量」。
   *
   * 滚动既不改变 `itemCount` 也不改变 `focus`，没有任何 React 依赖会因此变化，
   * 所以滚动必须显式进入。
   *
   * 但**不能只靠滚动**：`scrollTop` 被设进 spacer 后浏览器已贴顶/贴底，不会再
   * 产生 scroll 事件，而窗口提交本身也不保证视口里有行。那正是「内容显示不全、
   * 切出去切回来才补上」的形状。所以提交后由 hook 自己续测（见下方 cycle）。
   *
   * 每帧最多真正重算一次，滚动事件可以逐帧调用。
   */
  nudge: () => void;
  /**
   * 显示收敛异常（`null` = 正常）。
   *
   * 只在「补页预算用完」或「同一几何反复无进展、focus 也救不回来」时非空：
   * 此时窗口停在视口覆盖不到的位置，需要用户点「重定位阅读位置」。
   */
  displayError: string | null;
  /** 用户手动重定位：开一个新的收敛周期并立即重测。 */
  retry: () => void;
}

/** 收敛异常文案。**不要**把它写进日志当作已完成——它明确表示显示仍有缺口。 */
export const DISPLAY_STALLED_MESSAGE = "阅读位置暂时无法定位，可点击重定位";

/**
 * 维护转录窗口。
 *
 * 它**不含**高度簿记（那是 `HeightBook`）与纯计算（那是 `nextWindow`）：
 * 这里只负责「在合适的时机把测量结果喂给纯函数」，以及把
 * 锚点/搜索跳转翻译成一次性的 `FocusRequest`。
 */
export function useTranscriptWindow({
  anchorIds,
  itemCount,
  threadId,
  measure,
  focus = null,
  limits,
  book,
  following,
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

  // 窗口状态**连同它属于哪条会话**一起存：只用 `WindowState` 无法区分
  // 「上一条会话的中间窗口」和「本会话的位置」。真实浏览器实测（等长的 A/B
  // 两条 1024 条会话）在 A 里上翻后切到 B，B 直接落在 A 的中间区间。
  const [slot, setSlot] = useState<{ owner: string | null | undefined; window: WindowState | null }>(
    () => ({ owner: threadId, window: null }),
  );
  const slotRef = useRef(slot);
  slotRef.current = slot;
  // rAF 回调可能在新会话已经提交之后才跑，必须读**当时**的会话 id。
  const threadRef = useRef(threadId);
  threadRef.current = threadId;
  // 会话不匹配时同步按冷启动给窗口：不能先渲染旧窗口再等 effect 纠正，
  // 那会闪一帧上一条会话的内容。
  const state = slot.owner === threadId ? slot.window : null;
  // focus 用 ref 而不是 state：它只影响**一次**窗口计算，不需要触发渲染。
  // `appliedRef` 按对象身份去重——若调用方没及时把 `focus` 清空（清空时机
  // 在调用方，难免滞后），每次重算都会重新跳到同一位置，用户就再也滚不动了。
  const pendingRef = useRef<FocusRequest | null>(null);
  const appliedRef = useRef<FocusRequest | null>(null);

  // 每帧最多重算一次：滚动期间 `measure` 会读 DOM，逐帧多次没有意义。
  const frameRef = useRef<number | null>(null);
  const itemCountRef = useRef(itemCount);
  itemCountRef.current = itemCount;
  // 收敛周期（Spec §4.2）。`null` = 当前没有正在进行的自动补页。
  const cycleRef = useRef<ConvergenceCycle | null>(null);
  // 内容代次：items 结构（条数）变化 = 旧周期的预算与签名不再可信。
  const generationRef = useRef(0);
  const [displayError, setDisplayError] = useState<string | null>(null);
  // 意图读取也走 ref：它是外部闭包，变化不应改变 `recompute` 的身份。
  const followingRef = useRef<(() => boolean) | undefined>(following);
  followingRef.current = following;
  // 用 ref 读当前值：如果把它放进 `recompute` 的依赖，`setDisplayError` 会改变
  // `recompute` 的身份 -> 依赖它的 effect 再跑 -> 又一次重算，形成自激。
  const displayErrorRef = useRef(displayError);
  displayErrorRef.current = displayError;

  /**
   * 「当前 scrollTop 对应哪一条」的索引（Spec §4.4 第 6 条）。
   *
   * 只有在 `measure` 完全量不出历史区（视口落在未挂载的 spacer 里）时才用得上：
   * 这时没有任何行可以采信，`estimateFocusIndex` 也无从估算，必须靠高度账本把
   * 偏移换算成条号，否则窗口只能停在原处——用户看到一片空白，不滚动就永远不救。
   */
  const positionIndexRef = useRef<TranscriptPositionIndex>(createTranscriptPositionIndex());
  /**
   * 调用方给的 id 顺序。`itemCount` 不足以判断「第几条是谁」，所以需要真实的
   * id 列表；缺失时退回 `estimateFocusIndex`（旧路径），不假装能定位。
   */


  const recompute = useCallback((reason: "user" | "layout" | "commit" | "items" = "layout") => {
    // 用户手势开启**新**周期：预算与「无进展」判定都要重新计，否则用户滚一下
    // 就继承上一轮的 stalled 标记，永远不再补页。
    if (reason === "user") cycleRef.current = null;
    // 页面不可见时不排帧：后台标签页的 rAF 会被节流甚至完全停发，而一旦切回
    // 可见，积压的帧可能连跑多次、把基于旧布局的结论应用到新布局上。切回可见
    // 时由下面的 visibilitychange 监听补一次（不需要外部 scroll）。
    if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
    if (frameRef.current !== null) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      const owner = threadRef.current;
      const current = slotRef.current.owner === owner ? slotRef.current.window : null;
      // `measure` 给的就是绝对 index；不再加 `current.start`（tail 不在窗口内）。
      const visible = measure();
      const ownerKey = String(owner ?? "");
      const generation = generationRef.current;
      let cycle = cycleRef.current;
      if (cycle === null || cycle.owner !== ownerKey || cycle.generation !== generation) {
        cycle = beginConvergence(ownerKey, itemCountRef.current, generation);
      }
      // 用户的贴底意图必须传给窗口计算：读者把滚动条拖到底之后，视口可能落在
      // **下方 spacer** 里（早先 `focus` 开出来的中段窗口还没被收回来）。只靠
      // 「补页往后追」永远追不上——那些页本身也只是把 spacer 变短，视口仍在
      // 未挂载区间。把 `following` 传进去，`nextWindow` 才能直接把窗口收回到
      // 尾部，视口立刻有内容。
      const followIntent = followingRef.current?.();
      const next = nextWindow({
        current,
        itemCount: itemCountRef.current,
        // 像素阈值随**当时**的真实中位数走：重尾分布下按条数判断会误触发。
        // 每次重算都重新取，中位数随测量收敛后阈值自动跟上。
        limits: { ...merged, bufferPx: merged.buffer * book.median() },
        visible,
        // 一个周期只走一个方向：否则窄视口里「上翻—下卸」会互相抵消。
        direction: cycle.direction,
        focus: pendingRef.current,
        // 只在调用方真的给出意图时传：`nextWindow` 用 `input.following ?? 默认`
        // 决定冷启动要不要贴尾，传一个假的 `false` 会让新会话从头部开始。
        following: followIntent,
      });
      // 消费掉这次 focus：它只影响一次窗口计算。
      pendingRef.current = null;

      const changed =
        current === null ||
        current.start !== next.start ||
        current.end !== next.end ||
        current.following !== next.following;

      // 几何签名描述「这一帧看到的布局」：区间 + 视口内首末行 + tail 是否可见。
      // 只用区间不够——区间没动但视口终于有了行，也是进展。
      const signature = geometrySignature({
        start: current?.start ?? next.start,
        end: current?.end ?? next.end,
        visible: visible?.visible ?? null,
        tailVisible: visible?.tailVisible ?? false,
      });
      // 「已覆盖」= 视口里至少有一行真实消息。这是收敛的目标，也是唯一能停的
      // 条件之一；`measure()` 返回 null 表示尚未布局，此时必须暂停而不是按
      // 0 高度扩窗。
      const covered = visible !== null && visible.visible !== null;
      // 跟随中不补页：贴底写入负责把视口放到尾部，收敛只需退出。
      const decision =
        followIntent === true && !covered
          ? "settled"
          : decideConvergence(cycle, signature, covered, visible !== null);

      if (changed) {
        if (next.start < (current?.start ?? next.start)) cycle.direction = "up";
        else if (next.end > (current?.end ?? next.end)) cycle.direction = "down";
        const nextSlot = { owner, window: next };
        slotRef.current = nextSlot;
        setSlot(nextSlot);
      }

      // **这里就是「切出去再切回来才补齐」的修复点。** 提交只改变挂载区间，
      // 不保证视口里有行；`scrollTop` 已经贴顶/贴底时浏览器不会再产生 scroll
      // 事件。必须自己安排下一次测量，直到覆盖或确认无进展。
      if (decision === "advance") {
        cycleRef.current = noteAdvance(cycle, signature);
        recompute("commit");
        return;
      }
      if (decision === "focus") {
        cycleRef.current = noteFocus(cycle);
        // 先按「视口里真实存在的行」估算；量不出任何历史区时（视口落在未挂载的
        // spacer 里）退回按高度账本换算条号。没有 id 顺序就仍然放弃——不能猜。
        let focused = estimateFocusIndex(visible, book.median(), itemCountRef.current);
        if (focused === null && anchorIds !== undefined) {
          const index = positionIndexRef.current;
          index.rebuild(anchorIds, book);
          const at = index.indexAt(visible?.viewportTop ?? 0);
          focused = at >= 0 ? at : null;
        }
        if (focused !== null) {
          const before = Math.max(1, Math.floor(merged.pageSize / 4));
          appliedRef.current = { index: focused, before, after: merged.pageSize - before };
          pendingRef.current = appliedRef.current;
        }
        recompute("commit");
        return;
      }
      cycleRef.current = cycle;
      if (decision === "stalled") {
        setDisplayError(DISPLAY_STALLED_MESSAGE);
      } else if (decision === "settled" && displayErrorRef.current !== null) {
        setDisplayError(null);
      }
    });
  }, [measure, merged, book, anchorIds]);

  // 切换会话：窗口状态与一次性请求都只属于上一次访问，必须丢弃。
  // 用 layout effect（绘制前）而不是普通 effect，配合上面的渲染期同步回落，
  // 保证这一帧也不会出现上一条会话的内容。
  useLayoutEffect(() => {
    if (slotRef.current.owner === threadId) return;
    pendingRef.current = null;
    appliedRef.current = null;
    recompute();
  }, [threadId, recompute]);

  // 条数变化（回放、流式增长、clear）都要重算，并且换代：上一轮的预算与
  // 「无进展」签名描述的是一份不同的内容，不能继续沿用。
  useEffect(() => {
    generationRef.current += 1;
    cycleRef.current = null;
    recompute("items");
  }, [recompute, itemCount]);

  // 外部传入的 focus 也生效，但同一个对象只应用一次（见 appliedRef 注释）。
  useEffect(() => {
    if (focus === null || focus === appliedRef.current) return;
    appliedRef.current = focus;
    pendingRef.current = focus;
    recompute();
  }, [focus, recompute]);

  // 切回可见：补一次重算。隐藏期间的测量与决策都过期了，而且此时不会有
  // scroll 事件来救——正是「切出去切回来才看到更多」那个形状。
  useEffect(() => {
    if (typeof document === "undefined") return;
    const onVisibility = () => {
      if (document.visibilityState === "hidden") return;
      // 换一轮：隐藏期间积攒的「无进展签名」描述的是旧布局。
      cycleRef.current = null;
      recompute("user");
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, [recompute]);

  useEffect(
    () => () => {
      if (frameRef.current !== null) {
        cancelAnimationFrame(frameRef.current);
        // **必须置空**：`cancelAnimationFrame` 不会清掉帧号。应用根部是
        // `<React.StrictMode>`，开发期每个 effect 都会 挂载→卸载→再挂载；
        // 若这里留着作废帧号，再挂载时 `recompute` 的 `!== null` 守卫会永久
        // 提前返回，翻页/focus/nudge 全部失效。
        frameRef.current = null;
      }
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
    recompute("user");
  }, [recompute]);
  /** 用户手动重定位：重置周期与错误，重新测一次。 */
  const retry = useCallback(() => {
    cycleRef.current = null;
    setDisplayError(null);
    recompute("user");
  }, [recompute]);

  return {
    // 首帧同步给一个合法窗口，避免渲染出空列表再跳。用**调用方**的
    // `initialSize`（已解出），否则传了自定义值时首帧会是 60 条、下一帧再跳成
    // 目标值，产生一次可见闪烁。
    window: state ?? {
      start: Math.max(itemCount - initialSize, 0),
      end: itemCount,
      following: true,
    },
    requestFocus,
    clearFocus,
    nudge,
    displayError,
    retry,
  };
}
