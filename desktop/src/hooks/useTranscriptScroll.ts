import { useCallback, useEffect, useRef, useState } from "react";
import { TranscriptScrollCoordinator, type PositionPlan, type ScrollLayout } from "../lib/transcriptScroll";
import type { ViewAnchor } from "../lib/viewAnchor";

/**
 * 转录滚动的唯一入口（Spec §4.4 第 5 条）。
 *
 * 旧实现有五个独立的 `scrollTop` 写入者（贴底、整体高度差补偿、锚点恢复、锚点
 * 漂移校正、窗口 focus），真实浏览器里互相覆盖。这个 hook 把「写哪里」全部交给
 * `TranscriptScrollCoordinator`，组件层只负责：读几何、执行**一个**写入、把用户
 * 滚动与意图转达给协调器。
 *
 * 对外形状刻意与 `useStickToBottom` 对齐（`scrollToBottom` / `followIfAtBottom` /
 * `releaseFollow` / `isFollowing`），这样 `ChatView` 的接线改动最小，其它消费者
 * 仍可用旧 hook。
 */

export interface UseTranscriptScrollArgs {
  /**
   * 布局归属：`slot/host` + threadId + 内容代次 + 排版签名。
   * 归属变化必须重建协调器——上一条会话的位置不属于这一条。
   */
  ownerKey: string;
  /** 用户真实滚动时通知外层（翻页重算、采集阅读位置）。 */
  onUserScroll?: () => void;
  /** 视口/挂载行尺寸变化时通知外层（重新测量）。 */
  onLayoutChange?: () => void;
}

export interface TranscriptScrollApi {
  /** 交给滚动容器的 ref。 */
  ref: (el: HTMLDivElement | null) => void;
  scrollToBottom: () => void;
  followIfAtBottom: () => void;
  releaseFollow: () => void;
  isFollowing: () => boolean;
  /** 请求把某个锚点恢复到它当初的视口偏移。 */
  requestRestore: (anchor: ViewAnchor) => void;
  /** 用户滚动后重新采集到的阅读锚点。 */
  holdAnchor: (anchor: ViewAnchor) => void;
  /**
   * 执行一次布局决策；返回实际写入的位置（`null` = 本次不写）。
   *
   * 一次布局最多一个 `scrollTop` setter，调用方不要绕过它另写位置。
   */
  planLayout: () => number | null;
  /** 当前滚动容器的 `scrollTop`（读不到时返回 0）。 */
  currentTop: () => number;
  /** 是否还有未完成的锚点恢复（`false` = 最近一次 `requestRestore` 已落地）。 */
  hasPendingRestore: () => boolean;
}

/**
 * 视口顶部那条已挂载消息 + 它在内容坐标里的位置，顺带本次所有挂载行的坐标。
 *
 * 坐标必须**全量**带回：纠正要比较「同一条消息在这次与上次布局之间的位移」，
 * 而上一次的参照行不一定是这一次的视口顶那条（窗口移动后它可能落在挂载区中间）。
 * 只带参照 id 的坐标会让上一份几何永远缺那一行，位移恒算不出来。
 * 挂载区本身是有界的（一页 + buffer），全量记录不会随会话长度增长。
 */
function readAnchor(el: HTMLElement): {
  anchor: ViewAnchor | null;
  anchorTop: number | null;
  positions: Record<string, number>;
} {
  const base = el.getBoundingClientRect().top - el.scrollTop;
  const rows = el.querySelectorAll<HTMLElement>("[data-item-id]");
  const positions: Record<string, number> = {};
  let best: { id: string; top: number } | null = null;
  let first: { id: string; top: number } | null = null;
  for (const node of rows) {
    const id = node.dataset.itemId;
    if (!id) continue;
    // **不过滤零高度行**：折叠条目确实可能为 0，而 `clientHeight > 0` 已经足以
    // 说明容器完成布局。按高度过滤会把「内容盒暂时为 0」的合法行整段丢掉，
    // 结果是没有可视锚点、恢复永远对不上（jsdom 里所有行都是 0 高度）。
    const box = node.getBoundingClientRect();
    const top = box.top - base;
    positions[id] = top;
    if (first === null || top < first.top) first = { id, top };
    // 顶边已在视口顶之上（含相等，容忍亚像素）的最后一条 = 视口顶部那条。
    if (top <= el.scrollTop + 1) {
      if (best === null || top >= best.top) best = { id, top };
    }
  }
  const chosen = best ?? first;
  if (chosen === null) return { anchor: null, anchorTop: null, positions };
  return {
    anchor: { itemId: chosen.id, offset: chosen.top - el.scrollTop },
    anchorTop: chosen.top,
    positions,
  };
}

export function useTranscriptScroll({
  ownerKey,
  onUserScroll,
  onLayoutChange,
}: UseTranscriptScrollArgs): TranscriptScrollApi {
  const elRef = useRef<HTMLDivElement | null>(null);
  /**
   * 元素挂载状态。
   *
   * `ref` 是 **callback ref**：`ChatView` 在首帧渲染时把它挂到节点上，那一刻
   * 组件已经渲染完、`useEffect` 也已经跑过一遍（此时 `elRef.current` 还是 null，
   * effect 直接 return）。只依赖 `ownerKey` 的话，监听与 `ResizeObserver` 就永远
   * 不会注册——滚动事件拿不到，跟随意图停在初始值 true，用户上翻也不生效。
   * 用状态把「元素到位」变成一次真正的依赖变化，effect 才会重跑。
   */
  const [mounted, setMounted] = useState(false);
  const coordRef = useRef<TranscriptScrollCoordinator | null>(null);
  if (coordRef.current === null || coordRef.current.owner !== ownerKey) {
    coordRef.current = new TranscriptScrollCoordinator(ownerKey);
  }
  /**
   * 最近一次程序化写入的「回声窗口」：位置 + 还允许吞掉几个事件。
   *
   * 一次写入可能被派发成**多个** `scroll` 事件（浏览器按帧合并滚动步骤，末尾值
   * 可能重复派发），只吞第一个会让第二个被读成用户接管，阅读锚点当场被清掉。
   * 反过来也不能无限吞：用户真滚到同一个坐标时必须被识别。用有限次数（2）覆盖
   * 实测的重复派发，同时保留「用户随后滚到别处再回来」的识别能力。
   */
  const echoRef = useRef<{ top: number; budget: number } | null>(null);
  /** 每次 `planLayout` 递增，作为「这一次布局」的代次。 */
  const layoutGenerationRef = useRef(0);
  const userScrollRef = useRef(onUserScroll);
  userScrollRef.current = onUserScroll;
  const layoutChangeRef = useRef(onLayoutChange);
  layoutChangeRef.current = onLayoutChange;

  const write = useCallback((plan: PositionPlan): number | null => {
    const el = elRef.current;
    if (el === null || plan.kind === "none") return null;
    // 贴底一律写 `scrollHeight`：浏览器会把它 clamp 到真正的上限，而直接写
    // `maxTop` 在「布局尚未落定」时会比实际可达位置小一截（内容还在长高）。
    // 写超量再依赖 clamp 是既有契约，别改成精确值。
    const before = el.scrollTop;
    el.scrollTop = plan.following ? el.scrollHeight : plan.top;
    // 浏览器会 clamp；把**实际值**回流给协调器，后续比较才有正确基线。
    const actual = el.scrollTop;
    // 位置真的变了才开回声窗口：没变化时浏览器不会派发事件，窗口会一直留着
    // 并吞掉用户下一次真实滚动。
    echoRef.current = actual !== before ? { top: actual, budget: 2 } : echoRef.current;
    coordRef.current?.acknowledgeWrite(actual, el.scrollHeight - el.clientHeight);
    return actual;
  }, []);

  const scrollToBottom = useCallback(() => {
    const el = elRef.current;
    coordRef.current?.requestLatest();
    if (!el) return;
    write({ kind: "write", top: el.scrollHeight - el.clientHeight, following: true });
  }, [write]);

  const followIfAtBottom = useCallback(() => {
    const el = elRef.current;
    if (!el) return;
    if (!coordRef.current?.isFollowing()) return;
    write({ kind: "write", top: el.scrollHeight - el.clientHeight, following: true });
  }, [write]);

  const releaseFollow = useCallback(() => {
    coordRef.current?.releaseFollow();
  }, []);

  const isFollowing = useCallback(() => coordRef.current?.isFollowing() ?? true, []);

  const requestRestore = useCallback((anchor: ViewAnchor) => {
    coordRef.current?.requestRestore(anchor);
  }, []);

  const holdAnchor = useCallback((anchor: ViewAnchor) => {
    coordRef.current?.holdAnchor(anchor);
  }, []);

  const planLayout = useCallback((): number | null => {
    const el = elRef.current;
    const coord = coordRef.current;
    if (!el || !coord) return null;
    layoutGenerationRef.current += 1;
    const { anchor, anchorTop, positions } = readAnchor(el);
    const pendingId = coord.pendingRestoreId();
    const readingId = coord.readingAnchorId();
    const target =
      pendingId !== null && positions[pendingId] !== undefined
        ? { itemId: pendingId, top: positions[pendingId] }
        : null;
    const readingAnchorTop = readingId !== null ? (positions[readingId] ?? null) : null;
    const layout: ScrollLayout = {
      owner: coord.owner,
      generation: layoutGenerationRef.current,
      maxTop: el.scrollHeight - el.clientHeight,
      scrollTop: el.scrollTop,
      anchor,
      anchorTop,
      readingAnchorTop,
      positions,
      target,
    };
    // **先决策、后记账**：`acceptLayout` 记录的是「上一份几何」，决策要用它之前
    // 的值。反过来（先 accept 再 plan）等于把上一份当场覆盖成这一份，任何
    // 「与上一条消息比位移」的纠正都恒等于 0（实测视口停在 5000 不动）。
    const plan = coord.planLayout(layout);
    coord.acceptLayout(layout);
    return write(plan);
  }, [write]);

  const observerRef = useRef<ResizeObserver | null>(null);
  const currentTop = useCallback(() => elRef.current?.scrollTop ?? 0, []);
  const hasPendingRestore = useCallback(() => coordRef.current?.hasPendingRestore() ?? false, []);

  const ref = useCallback((el: HTMLDivElement | null) => {
    elRef.current = el;
    setMounted(el !== null);
  }, []);

  useEffect(() => {
    const el = elRef.current;
    if (!el) return;

    const onScroll = () => {
      const coord = coordRef.current;
      const top = el.scrollTop;
      const max = el.scrollHeight - el.clientHeight;
      // 落点正是自己刚写的值 ⇒ 这是程序化写入的回声，不是用户意图。
      // 只吞有限次（见 `echoRef`），用户随后真实滚动照常识别。
      const echo = echoRef.current;
      if (echo !== null && Math.abs(top - echo.top) < 1) {
        echo.budget -= 1;
        if (echo.budget <= 0) echoRef.current = null;
        return;
      }
      echoRef.current = null;
      coord?.acceptUserScroll(top, max);
      userScrollRef.current?.();
    };
    el.addEventListener("scroll", onScroll, { passive: true });

    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      let frame: number | null = null;
      observer = new ResizeObserver(() => {
        // 尺寸变化（图片迟到、工具卡展开、窗口旋转）同样要重新决策；
        // 合并到一帧，避免逐条回调各写一次位置。
        if (frame !== null) return;
        frame = requestAnimationFrame(() => {
          frame = null;
          layoutChangeRef.current?.();
        });
      });
      observerRef.current = observer;
      observer.observe(el);
    }

    return () => {
      el.removeEventListener("scroll", onScroll);
      observer?.disconnect();
      observerRef.current = null;
    };
  }, [ownerKey, mounted]);

  /**
   * 观察集合必须跟着**当前**挂载区走。
   *
   * 旧实现只在挂载时观察一次，翻页/切窗口后新挂载的行没人看：图片迟到把锚点行
   * 推下去时不会有人重新决策，视口就停在旧位置（实测恢复 1312 却停在 1012）。
   * spacer 也要看——它是「未挂载区间」高度变化的唯一信号。
   */
  useEffect(() => {
    const el = elRef.current;
    const observer = observerRef.current;
    if (!el || !observer) return;
    observer.disconnect();
    observer.observe(el);
    for (const node of el.querySelectorAll("[data-item-id], [data-spacer]")) observer.observe(node);
  });

  return {
    ref,
    scrollToBottom,
    followIfAtBottom,
    releaseFollow,
    isFollowing,
    requestRestore,
    holdAnchor,
    planLayout,
    currentTop,
    hasPendingRestore,
  };
}
