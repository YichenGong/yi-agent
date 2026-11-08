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

import type { WindowMeasurement } from "./transcriptGeometry";

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
   * 视口内第一条的**绝对** item index。
   *
   * 保留这个窄形状只为旧调用点与纯函数测试：`measureTranscript` 直接读
   * `data-item-index`，不再做 `window.start + 相对序号` 的换算（tail 不在
   * 历史窗口内，那样换算会把边界比错）。
   */
  first: number;
  /** 视口内最后一条的绝对 item index。 */
  last: number;
  /**
   * 挂载区顶边相对视口顶的有符号距离（px）：`firstTop - viewTop`。
   *
   * 用有符号而不是夹到 ≥0，是因为「顶边正好在视口顶」与「顶边已在视口上方」
   * 必须区分开：60 条消息通常比视口高，用户停在底部时第一条挂载行本来就在
   * 视口**上方**（负值）。把它夹成 0，规则会读成「用户翻到顶了」，于是自己
   * 往上分页并逐帧来回抖动。负值 = 视口已越过顶边多少像素。
   */
  gapTopPx: number;
  /**
   * 挂载区底边相对视口底的有符号距离（px）：`viewBottom - lastBottom`。
   *
   * 同 `gapTopPx`：负值 = 视口已越过底边多少像素，0 = 底边正好贴住视口底。
   */
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

/** 历史区边界的两种来源：新测量结果，或旧 `VisibleRange` 形状（只有边界）。 */
export type EdgeLike = WindowMeasurement | VisibleRange;

function edgesOf(v: EdgeLike | null): { first: number; last: number; gapTopPx: number; gapBottomPx: number } | null {
  if (!v) return null;
  if ("history" in v) return v.history;
  return { first: v.first, last: v.last, gapTopPx: v.gapTopPx, gapBottomPx: v.gapBottomPx };
}

/** 一个方向的最大位移（px）：`ceil(条数/页) + 2` 页，不是无限滚。 */
export function cycleBudget(itemCount: number, pageSize: number): number {
  return Math.ceil(Math.max(itemCount, 1) / Math.max(pageSize, 1)) + 2;
}

export interface NextWindowInput {
  current: WindowState | null;
  itemCount: number;
  limits: WindowLimits;
  /**
   * 当前视口测量。`history` 只来自历史区行——恒挂的 LiveTail 不参与边界，
   * 否则「首末挂载行」会把底边拉进视口，读成已覆盖而停止开窗。
   */
  visible: EdgeLike | null;
  /**
   * 用户是否在跟随尾部。**必须显式传入**：它来自 ScrollCoordinator 的意图，
   * 不能由 `end === itemCount` 推导，也不能在读到底部时自动恢复。
   * 省略时沿用 `current.following`（旧调用点的兼容行为）。
   */
  following?: boolean;
  /** 本轮只允许一个方向。省略时按 gap 推导（旧调用点）。 */
  direction?: "up" | "down" | null;
  /** 一次性开窗请求（锚点恢复 / 搜索跳转）。 */
  focus?: FocusRequest | null;
}

const clamp = (v: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, v));

/** 视口离历史区哪端更近；`true` = 该截 start。 */
export function decideTrimStart(args: {
  following: boolean;
  visible: EdgeLike | null;
  start: number;
  end: number;
  limits: WindowLimits;
}): boolean {
  const { following, start, end, limits } = args;
  const visible = edgesOf(args.visible);
  if (following) return true;
  if (!visible) return true;
  if (visible.gapTopPx >= -limits.bufferPx) return false;
  if (visible.gapBottomPx >= -limits.bufferPx) return true;
  const nearStart = visible.first - start;
  const nearEnd = end - visible.last;
  return nearStart > nearEnd;
}

/** 算出下一个窗口。纯函数：不读 DOM、不读时间、无副作用。 */
export function nextWindow(input: NextWindowInput): WindowState {
  const { current, itemCount, limits, focus } = input;

  if (itemCount <= 0) return { start: 0, end: 0, following: true };

  const edge = edgesOf(input.visible);
  let start: number;
  let end: number;
  let following: boolean;

  if (current === null) {
    if (focus) {
      following = false;
      start = clamp(focus.index - focus.before, 0, itemCount - 1);
      end = clamp(focus.index + focus.after + 1, start + 1, itemCount);
    } else {
      following = input.following ?? true;
      end = itemCount;
      start = Math.max(end - limits.initialSize, 0);
    }
  } else {
    start = current.start;
    end = current.end;
    following = input.following ?? current.following;

    // 收缩：窗口整体落在新末尾之后（/clear、compact）时重新锚到尾部。
    if (start >= itemCount) {
      following = input.following ?? true;
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

      const wanted = input.direction ?? null;
      const up = edge !== null && start > 0 && edge.gapTopPx >= -limits.bufferPx;
      const down = edge !== null && end < itemCount && edge.gapBottomPx >= -limits.bufferPx;
      // **一个周期只走一个方向**：两个方向同一帧各自加一页，会在窄视口里
      // 「上翻—下卸」互相抵消，表现为翻不动或来回抖。direction 显式给出时
      // 只认它（用户手势/开窗意图），否则按 gap 推一个。
      // 两端同时越界（历史区比视口矮，或 tail 远离历史）时按越界更远的一端
      // 决定方向；相等就原地不动，宁可少开一页也不要一帧上、一帧下地抖。
      const dir =
        wanted ??
        (up && down
          ? edge!.gapBottomPx > -edge!.gapTopPx
            ? "down"
            : -edge!.gapTopPx > edge!.gapBottomPx
              ? "up"
              : null
          : up
            ? "up"
            : down
              ? "down"
              : null);
      if (dir === "up" && up) {
        start = Math.max(start - limits.pageSize, 0);
        following = false;
      } else if (dir === "down" && down) {
        end = Math.min(end + limits.pageSize, itemCount);
        // 读到底部**不等于**要跟随：意图只由用户/显式命令改变。
      }
    }
  }

  // 实际 DOM 上限含恒挂的 LiveTail：tail 在窗口外时给它留 1 个名额。
  const reservedTail = end < itemCount ? 1 : 0;
  const capacity = Math.max(1, limits.maxMounted - reservedTail);
  if (end - start > capacity) {
    // 裁掉**远离视口**的一端，不按翻页方向裁：方向只决定往哪加，裁哪端要看
    // 用户实际在读哪里。按方向裁会立刻删掉刚补进来的那一页，表现为「翻不动」。
    const trimStart = decideTrimStart({ following, visible: input.visible, start, end, limits });
    if (trimStart) start = end - capacity;
    else end = start + capacity;
  }

  end = Math.min(end, itemCount);
  start = clamp(start, 0, end);
  return { start, end, following };
}
