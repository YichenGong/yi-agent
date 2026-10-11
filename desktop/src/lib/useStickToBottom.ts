import { useCallback, useEffect, useRef } from "react";
import type { RefObject } from "react";

/**
 * 「是否贴底」的像素容差。
 *
 * 滚动位置与 scrollHeight/clientHeight 都可能落在亚像素上（分数像素、设备像素比），
 * 严格相等会把「就差 1–2px」判成非贴底，于是旋转后不再贴底。取 4px 容忍这点误差。
 */
export const STICK_BOTTOM_THRESHOLD_PX = 4;

/** 判断一个滚动容器当前是否贴在底部。 */
export function isAtBottom(
  el: Pick<HTMLElement, "scrollHeight" | "clientHeight" | "scrollTop">,
  thresholdPx = STICK_BOTTOM_THRESHOLD_PX,
): boolean {
  return el.scrollHeight - el.clientHeight - el.scrollTop <= thresholdPx;
}

export interface StickToBottom {
  ref: RefObject<HTMLDivElement | null>;
  /** 立即贴底（供流式追加时调用）。 */
  scrollToBottom: () => void;
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
 * `ResizeObserver` 缺失时（如 jsdom）退化为「只提供 `scrollToBottom`」，不建立观察，
 * 也不抛错。
 */
export function useStickToBottom(): StickToBottom {
  const ref = useRef<HTMLDivElement>(null);
  // 旋转本身不派发 `scroll`，所以这个 ref 恰好保留了「高度变化之前」是否贴底。
  const atBottomRef = useRef(true);
  const frameRef = useRef<number | null>(null);

  const scrollToBottom = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    const onScroll = () => {
      atBottomRef.current = isAtBottom(el);
    };
    // 初次同步一次真实状态（挂载时元素可能已经有滚动位置）。
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
          if (node) node.scrollTop = node.scrollHeight;
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

  return { ref, scrollToBottom };
}
