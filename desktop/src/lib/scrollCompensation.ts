/**
 * 窗口变化后的 `scrollTop` 补偿。
 *
 * 背景（本项目实测过）：WKWebView / Safari **不实现 scroll anchoring**，
 * 所以「在视口上方挂载/卸载内容」不会自动粘住用户的阅读位置，必须自己补。
 * 详见 `useStickToBottom.ts` 顶部注释里那 4346px 的漂移。
 *
 * 纯函数：只做算术，不碰 DOM，因此四种方向（上方挂载 / 上方卸载 /
 * 下方卸载 / 首次布局）都能直接单测。
 */
export interface CompensationInput {
  prevScrollHeight: number;
  nextScrollHeight: number;
  prevViewportHeight: number;
  scrollTop: number;
  /** 变更前用户是否处于「贴底」状态。 */
  wasFollowing: boolean;
}

export interface CompensationPlan {
  scrollTop: number;
  /**
   * - `to-bottom`：原本贴底，补偿后重新贴底（同时重设基线）
   * - `release`：放弃跟随但不移动视口（把基线对齐到新布局）
   * - `keep`：无需变更
   */
  follow: "keep" | "to-bottom" | "release";
}

export function planCompensation(input: CompensationInput): CompensationPlan {
  const { prevScrollHeight, nextScrollHeight, prevViewportHeight, scrollTop, wasFollowing } = input;

  if (wasFollowing) {
    // 贴底时不按差值补：直接把视口放回新的底部更准（内容还在长高的情况也是）。
    return { scrollTop: Math.max(0, nextScrollHeight - prevViewportHeight), follow: "to-bottom" };
  }

  const delta = nextScrollHeight - prevScrollHeight;
  const maxScrollTop = Math.max(0, nextScrollHeight - prevViewportHeight);
  const target = Math.min(Math.max(0, scrollTop + delta), maxScrollTop);

  // 即使 delta 为 0 也要 release：它把 `lastTopRef`/`lastMaxRef` 对齐到新布局，
  // 下一次 onScroll 的基线才是干净的。
  return { scrollTop: target, follow: "release" };
}
