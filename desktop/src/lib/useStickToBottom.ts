import { useCallback, useEffect, useRef } from "react";
import type { RefObject } from "react";

/**
 * 「是否贴底」的像素容差。
 *
 * 滚动位置与 scrollHeight/clientHeight 都可能落在亚像素上（分数像素、设备像素比），
 * 严格相等会把「就差 1–2px」判成非贴底，于是旋转后不再贴底。取 4px 容忍这点误差。
 */
export const STICK_BOTTOM_THRESHOLD_PX = 4;

/**
 * 判定「用户把视口往上挪了」的最小位移。
 *
 * `scrollTop` 会因设备像素比/缩放落在分数上，布局抖动也可能让它差个零点几像素；
 * 这类噪声不是上翻意图。真实的上翻（滚轮/拖动/PageUp）每个事件至少几个像素，
 * 且是相对**上次确认的贴底位置**累计比较的，所以 1px 的下限不会漏掉慢速上滑。
 */
export const SCROLL_UP_EPSILON_PX = 1;

/** 判断一个滚动容器当前是否贴在底部。 */
export function isAtBottom(
  el: Pick<HTMLElement, "scrollHeight" | "clientHeight" | "scrollTop">,
  thresholdPx = STICK_BOTTOM_THRESHOLD_PX,
): boolean {
  return el.scrollHeight - el.clientHeight - el.scrollTop <= thresholdPx;
}

export interface StickToBottom {
  ref: RefObject<HTMLDivElement | null>;
  /**
   * 强制贴底，并重新表明「贴底意图」。
   *
   * 用于**用户明确要看到最新内容**的动作：发出消息、切换会话、点「回到底部」。
   * 即使用户此刻正在上翻历史，也照做——这是用户主动要求的。
   */
  scrollToBottom: () => void;
  /**
   * 内容增长时的可选跟随：用户仍在底部才贴底，已上翻历史则原地不动。
   *
   * 与 `scrollToBottom` 的区别只有一点——是否尊重用户的上翻意图。流式生成期间
   * 必须用这个：每个 delta 都强制贴底会让用户永远读不到上面的内容。
   */
  followIfAtBottom: () => void;
  /**
   * 放弃跟随，但**不动视口**。
   *
   * 用于"恢复阅读位置"：调用方刚把 `scrollTop` 设到锚点，此刻若仍保持贴底意图，
   * 下一个 delta 就会把人拽回底部。与 `scrollToBottom` 的区别正是它不移动元素。
   */
  releaseFollow: () => void;
  /**
   * 当前是否仍保持「贴底意图」。
   *
   * 给需要判断"用户是否还在看最新内容"的调用方使用（`ChatView` 采集阅读位置）。
   * 它返回的是由 `scroll` 事件按**相对位移**维护的意图，而不是当下的绝对 gap——
   * 后者在内容增长之后必然判成非贴底，正是贴底/跟随失效的根源。
   */
  isFollowing: () => boolean;
}

/**
 * 让一个滚动容器在**视口高度变化**（手机旋转、`dvh` 变化、键盘顶起、桌面拖窗口）
 * 之后保持用户的「贴底意图」：原本贴底的重新贴底；原本在上翻历史的则不动。
 *
 * 为什么需要它：这类高度变化**不派发 `scroll`**，浏览器保留的是**绝对像素** `scrollTop`，
 * 而 Safari/WKWebView **不实现 scroll anchoring**（Chrome/Firefox 会替你粘住位置），
 * 于是手机「竖→横→竖」后内容整体被推离底部（实测差 4346px）。
 *
 * 为什么用容器自身的 `ResizeObserver`：旋转/键盘/dvh 变化的共同效果就是容器盒高变了，
 * 以 `clientHeight` 为唯一事实来源最准；`window.resize` 时序偏抖且不覆盖父容器变化。
 *
 * 注意 `clientHeight`（含 padding）与 `entry.contentRect.height`（内容盒）相差一个常量；
 * 后者只用作**变化信号**，贴底判定一律直接读元素自身的三个度量，避免两套坐标混淆。
 *
 * `ResizeObserver` 缺失时（如 jsdom）退化为「只提供两个贴底命令」，不建立观察，
 * 也不抛错。
 *
 * **贴底意图**（`atBottomRef`）是这里唯一的用户状态：`scroll` 事件写入它，
 * `followIfAtBottom` 读取它。流式增长必须走 `followIfAtBottom`，否则用户上翻
 * 历史后每个 delta 都会被拽回底部；`scrollToBottom` 是显式意图（发消息 / 切会话），
 * 会重置该状态。
 *
 * 判定「用户上翻」**不能只看当下的绝对 gap**：`scroll` 事件是异步派发的——我们
 * 写 `scrollTop = scrollHeight` 之后浏览器要到后续的 scroll steps 才发事件，而流式
 * 文本在同一 throttle 窗口里已经把内容又长高了（真实 Chrome 实测：写入时 gap 为 0，
 * 事件送达时 gap 已是 126px）。用当下 gap 重算会把「我们自己刚贴的底」判成
 * 「用户上翻」，跟随就此永久关闭，于是生成到某处停住、后面内容继续增长却不贴底。
 * 因此改为比较**视口是否相对上次确认的位置向上移动**：只有 `scrollTop` 变小到
 * 超出噪声下限才算上翻；`scrollTop` 不动或变大（内容变长后我们重新贴底）都不改变意图。
 */
export function useStickToBottom(): StickToBottom {
  const ref = useRef<HTMLDivElement>(null);
  // 旋转本身不派发 `scroll`，所以这个 ref 恰好保留了「高度变化之前」是否贴底。
  const atBottomRef = useRef(true);
  // 上次观察到的位置：上翻判定是**相对**它的，而不是相对当前内容高度。
  const lastTopRef = useRef<number | null>(null);
  const lastMaxRef = useRef<number | null>(null);
  const frameRef = useRef<number | null>(null);

  /** 贴底并把基线对齐到写入后的真实位置（浏览器会把 `scrollTop` 夹到最大值）。 */
  const pin = useCallback((el: HTMLElement) => {
    el.scrollTop = el.scrollHeight;
    lastTopRef.current = el.scrollTop;
    lastMaxRef.current = el.scrollHeight - el.clientHeight;
  }, []);

  const scrollToBottom = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    // 显式贴底同时重置「贴底意图」：用户刚发了消息 / 刚切会话 / 刚点了回到底部，
    // 接下来流式增长应当继续跟随。
    atBottomRef.current = true;
    pin(el);
  }, [pin]);

  /**
   * 内容增长时的跟随。**只依赖记住的意图**，不在调用时重算 `isAtBottom`：
   * 新内容已经让 `scrollHeight` 变大，此刻再算必然判成「不贴底」，于是永远不会跟随。
   * 意图由 `scroll` 事件维护——用户上翻的瞬间就会写入 `false`。
   */
  const followIfAtBottom = useCallback(() => {
    const el = ref.current;
    if (!el || !atBottomRef.current) return;
    pin(el);
  }, [pin]);

  const releaseFollow = useCallback(() => {
    atBottomRef.current = false;
    // 基线跟着更新：调用方刚把视口放到锚点，之后的事件不应把它读成"再次上翻"或"贴底"。
    const el = ref.current;
    if (el) {
      lastTopRef.current = el.scrollTop;
      lastMaxRef.current = el.scrollHeight - el.clientHeight;
    }
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    /**
     * 维护「贴底意图」。**不以当下的绝对 gap 为准**：
     * `scroll` 事件是异步派发的，我们贴底之后内容若又长高，事件送达时 gap 天然
     * 大于阈值（真实 Chrome 实测 126px），按 gap 判定会把"我们自己贴的底"误读成
     * "用户上翻"，跟随从此永久关闭——现场表现就是生成到某处停住、视口不再贴底。
     *
     * 因此只在**视口相对上次观察真的向上移动**时才认定上翻；内容增长（`scrollTop`
     * 不变、`scrollHeight` 变大）不改写意图。内容/视口收缩时浏览器会把 `scrollTop`
     * 往下夹，这种"变小"不是用户意图，用 `min(上次 top, 上次 max)` 作下限排除掉。
     */
    const onScroll = () => {
      const top = el.scrollTop;
      const max = el.scrollHeight - el.clientHeight;
      const prevTop = lastTopRef.current;
      const prevMax = lastMaxRef.current;
      lastMaxRef.current = max;

      // 首次观察，或基线是在容器还不可滚动时（`max <= 0`，内容尚未渲染/挂载瞬间）
      // 记下的——那种基线不携带任何用户意图信息，只能看绝对位置。
      if (prevTop === null || prevMax === null || prevMax <= 0) {
        lastTopRef.current = top;
        atBottomRef.current = max - top <= STICK_BOTTOM_THRESHOLD_PX;
        return;
      }
      // 布局收缩会把 scrollTop 往下夹，这种「变小」不是用户意图：以
      // `min(上次 top, 上次 max)` 为下限，低于它才算真的往上挪。
      const lowestPossible = Math.min(prevTop, Math.max(0, prevMax));
      // 认定上翻需要**两个信号同时成立**：视口相对上次确实向上移动，且已经离开底部
      // 区域。只看位移会把亚像素抖动/上方内容重排当成上翻；只看 gap 会被内容增长骗到。
      if (top < lowestPossible - SCROLL_UP_EPSILON_PX && max - top > STICK_BOTTOM_THRESHOLD_PX) {
        atBottomRef.current = false;
        lastTopRef.current = top;
        return;
      }
      // 基线更新：向下移动或未被夹取时跟随实际位置；幅度小于噪声的向上移动保留原基线，
      // 让连续的上滑（每步不足 1px）也能累积到越过噪声下限。
      lastTopRef.current = top >= lowestPossible ? top : lowestPossible;
      // 真的到了底部（容差内）就恢复跟随：用户自己滑回最新内容也是意图。
      if (max - top <= STICK_BOTTOM_THRESHOLD_PX) atBottomRef.current = true;
      // 其余情况（内容增长让 gap 变大、scrollTop 没动）保持现有意图不变。
    };
    onScroll();
    el.addEventListener("scroll", onScroll, { passive: true });

    let observer: ResizeObserver | null = null;
    if (typeof ResizeObserver !== "undefined") {
      let previousHeight: number | null = null;
      observer = new ResizeObserver((entries) => {
        const entry = entries[0];
        if (!entry) return;
        const height = entry.contentRect.height;
        // 首次回调只用来建立基准高度，不算「高度变化」。
        if (previousHeight === null) {
          previousHeight = height;
          return;
        }
        if (height === previousHeight) return;
        previousHeight = height;
        if (!atBottomRef.current) return;
        // 延后一帧：iOS 上 `dvh` 的高度变化可能晚于这次回调，且要等布局落定再赋值。
        // 同一帧内多次尺寸抖动只保留一次贴底。
        if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
        frameRef.current = requestAnimationFrame(() => {
          frameRef.current = null;
          const node = ref.current;
          // 在**执行时**重读「是否贴底」：这次回调可能比「用户在这一帧上翻」晚。
          // 若把判断冻结在 resize 回调那一刻，键盘收起 / `dvh` 动画期间
          // 「高度变化 + 用户上滚」同帧发生时，延后的贴底仍会把正在翻历史的用户拽回底部。
          if (node && atBottomRef.current) pin(node);
        });
      });
      observer.observe(el);
    }

    return () => {
      el.removeEventListener("scroll", onScroll);
      observer?.disconnect();
      if (frameRef.current !== null) {
        cancelAnimationFrame(frameRef.current);
        frameRef.current = null;
      }
    };
  }, []);

  const isFollowing = useCallback(() => atBottomRef.current, []);

  return { ref, scrollToBottom, followIfAtBottom, releaseFollow, isFollowing };
}
