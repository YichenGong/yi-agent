/**
 * 转录窗口的纯函数内核。
 *
 * **零 React、零 DOM、零时间依赖**：所有输入都由调用方给出（视口测量结果、
 * 会话条数、参数），因此每条规则都能用普通单测覆盖，不需要真实布局。
 *
 * 为什么不自己接管 `scrollTop` 做虚拟滚动：这里只决定「挂载哪一段」，
 * 未挂载区间用两个 spacer 撑高度，滚动仍由浏览器原生完成。这样
 * `useStickToBottom` 的 `scrollHeight` 语义不用改写（见设计 §2）。
 */

export interface WindowState {
  /** 已挂载区间的起点（含）。 */
  start: number;
  /** 已挂载区间的终点（不含）。最后一条 item 恒由 `LiveTail` 渲染。 */
  end: number;
  /**
   * 是否跟随尾部。**必须是显式状态，不能由 `end === itemCount` 推导**：
   * 流式追加时 `itemCount` 先变大、`end` 还没跟上，两者不等恰恰说明「正在跟随」。
   * 用派生值判断会把每一次 delta 都读成「用户离开了尾部」，跟随就此关闭。
   */
  following: boolean;
}

/** 一次「把某个 index 摆进视口」的请求（锚点恢复、搜索跳转）。 */
export interface FocusRequest {
  index: number;
  /** 该 index 之上/之下额外保留多少条。 */
  before: number;
  after: number;
}

/**
 * 当前视口对应的挂载区状态（由组件层测量 DOM 得出）。
 *
 * 同时给「index 距离」和「像素 gap」：前者用于决定截哪端，
 * 后者用于决定是否分页。只看 index 会错——item 高度是重尾分布。
 */
export interface VisibleRange {
  /**
   * 视口内第一条的**绝对** item index（不是挂载区相对序号）。
   *
   * 组件层只能看见挂载区，数不出前面还有多少条，所以它给的是相对序号；
   * `useTranscriptWindow` 负责加上 `state.start` 再传进来。两处口径必须一致，
   * 否则截断判据会比错对象。
   */
  first: number;
  /** 视口内最后一条的绝对 item index。 */
  last: number;
  /** 挂载区顶边到视口顶的距离（px，≥0）。0 表示顶边已进入视口。 */
  gapTopPx: number;
  /** 挂载区底边到视口底的距离（px，≥0）。0 表示底边已进入视口。 */
  gapBottomPx: number;
}

export interface WindowLimits {
  /** 冷启动首屏条数。 */
  initialSize: number;
  /** 向上/向下分页一页的条数。 */
  pageSize: number;
  /** 挂载上限，超出后从离视口远的一端卸载。 */
  maxMounted: number;
  /** 定点开窗时上下各留多少条。 */
  buffer: number;
  /** 分页触发的像素阈值（组件层按 `buffer * 中位高度` 算好传进来）。 */
  bufferPx: number;
}

/** 用户已确认的参数，不得擅自调整。 */
export const DEFAULT_LIMITS: WindowLimits = {
  initialSize: 60,
  pageSize: 200,
  maxMounted: 400,
  buffer: 20,
  // 默认按 40px 一条估；组件层会用真实中位数重算后覆盖。
  bufferPx: 20 * 40,
};

export interface NextWindowInput {
  current: WindowState | null;
  itemCount: number;
  limits: WindowLimits;
  /** 当前视口覆盖的挂载区区间；未测量出时为 null。 */
  visible: VisibleRange | null;
  /** 一次性开窗请求（锚点恢复 / 搜索跳转）。 */
  focus?: FocusRequest | null;
}

const clamp = (v: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, v));

/**
 * 超限时该截掉哪一端。
 *
 * 不能无条件截 `start`：用户往上读时那会卸掉他正在看的内容。
 * 也不能只比「离视口多远」：向上翻页后视口落在新内容中间，按距离会截 `start`，
 * 把刚加载进来的历史立刻丢掉，形成「翻页—卸载—再翻页」的抖动。
 */
export function decideTrimStart(args: {
  following: boolean;
  visible: VisibleRange | null;
  start: number;
  end: number;
  limits: WindowLimits;
}): boolean {
  const { following, visible, start, end, limits } = args;
  if (following) return true; // 用户在看最新
  if (!visible) return true; // 尚未测量，保守保住尾部
  if (visible.gapTopPx <= limits.bufferPx) return false; // 刚向上分页 -> 截 end
  if (visible.gapBottomPx <= limits.bufferPx) return true; // 刚向下分页 -> 截 start
  const nearStart = visible.first - start;
  const nearEnd = end - visible.last;
  return nearEnd >= nearStart;
}

/** 算出下一个窗口。纯函数：不读 DOM、不读时间、无副作用。 */
export function nextWindow(input: NextWindowInput): WindowState {
  const { current, itemCount, limits, visible, focus } = input;

  if (itemCount <= 0) return { start: 0, end: 0, following: true };

  let start: number;
  let end: number;
  let following: boolean;

  if (current === null) {
    if (focus) {
      following = false;
      start = clamp(focus.index - focus.before, 0, itemCount - 1);
      end = clamp(focus.index + focus.after + 1, start + 1, itemCount);
    } else {
      following = true;
      end = itemCount;
      start = Math.max(end - limits.initialSize, 0);
    }
  } else {
    ({ start, end, following } = current);

    // 收缩：窗口整体落在新末尾之后（/clear、compact）时重新锚到尾部，
    // 否则会剩下一个空窗口，用户看不到任何内容。
    if (start >= itemCount) {
      following = true;
      end = itemCount;
      start = Math.max(end - limits.initialSize, 0);
    }
    end = Math.min(end, itemCount);
    start = Math.min(start, end);

    if (focus) {
      following = false;
      start = clamp(focus.index - focus.before, 0, itemCount - 1);
      end = clamp(focus.index + focus.after + 1, start + 1, itemCount);
    } else {
      // 跟随尾部：itemCount 增长时 end 跟上，start 不动。
      if (following) end = itemCount;

      if (visible) {
        if (start > 0 && visible.gapTopPx <= limits.bufferPx) {
          start = Math.max(start - limits.pageSize, 0);
          following = false; // 用户明确往上读了
        }
        if (end < itemCount && visible.gapBottomPx <= limits.bufferPx) {
          end = Math.min(end + limits.pageSize, itemCount);
          // 滚回底部就是「要继续看最新」，与 useStickToBottom 里
          // 「gap 回到阈值内则恢复跟随」是同一条语义，两处必须一致。
          if (end === itemCount) following = true;
        }
      }
    }
  }

  if (end - start > limits.maxMounted) {
    const trimStart = decideTrimStart({ following, visible, start, end, limits });
    if (trimStart) start = end - limits.maxMounted;
    else end = start + limits.maxMounted;
  }

  end = Math.min(end, itemCount);
  start = clamp(start, 0, end);
  return { start, end, following };
}
