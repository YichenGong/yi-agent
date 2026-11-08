/**
 * 一次「把视口补满」周期的状态机（Spec §4.2）。
 *
 * 为什么需要它：窗口提交只改变挂载区间，**不保证视口里已经有行**。用户把
 * `scrollTop` 直接设进 spacer 之后，浏览器不会再产生 scroll 事件（已经在顶/底），
 * 于是没有任何东西再要求重算，屏幕一直空白——直到用户切走再切回（重挂载强制
 * 回尾）。真实浏览器里 D02 就是这个形状：`scrollTop=0`、窗口 [201,600)、
 * 顶 spacer 41004px、视口内 0 行，且 `scroll` 事件只来了 1 次。
 *
 * 所以调度必须**自驱动**：每次窗口提交后再测一次，直到覆盖或确认无进展。但
 * 无限自驱动就是后台空转（每帧 setState，D09 会红），因此这里给出三条硬边界：
 *
 * 1. 预算 `ceil(N/page)+2`：固定条数最多有效开窗这么多次；
 * 2. 几何签名不变 → 上一步没有改变任何东西 → 先 focus 一次，再判 stalled；
 * 3. 未布局（hidden / 尺寸 0）→ paused，恢复可见时由调用方显式 nudge。
 *
 * 纯函数、零 DOM、零时间：判定只由调用方给的签名与布尔量决定。
 */

import { DEFAULT_LIMITS } from "./transcriptWindow";

export interface ConvergenceCycle {
  /** 会话/宿主身份。切 owner 的旧周期必须整体作废。 */
  owner: string;
  /** 内容代次：回放/快照安装会换 generation，旧周期的预算与签名不再可信。 */
  generation: number;
  /** 本轮已确认的方向。一个周期内不翻转，避免上翻—下翻互相抵消。 */
  direction: "up" | "down" | null;
  /** 剩余可用的有效开窗次数。 */
  remaining: number;
  /** 上一次推进后的几何签名；`null` 表示还没有推进过。 */
  lastSignature: string | null;
  /** 本周期是否已经用过那唯一一次 focus 重定位。 */
  focusedOnce: boolean;
}

export type ConvergenceDecision = "settled" | "advance" | "focus" | "stalled" | "paused";

/** `ceil(N/page)+2`：固定条数下顺序补页的安全上限（Spec §4.2 第 6 条）。 */
export function budgetFor(itemCount: number, pageSize = DEFAULT_LIMITS.pageSize): number {
  return Math.ceil(Math.max(itemCount, 1) / Math.max(pageSize, 1)) + 2;
}

export function beginConvergence(
  owner: string,
  itemCount: number,
  generation: number,
  direction: "up" | "down" | null = null,
): ConvergenceCycle {
  return {
    owner,
    generation,
    direction,
    remaining: budgetFor(itemCount),
    lastSignature: null,
    focusedOnce: false,
  };
}

/**
 * 决定这一步做什么。
 *
 * `signature` 必须描述**几何**（窗口区间 + 视口内首末行 + 有无可视行），而不是
 * 仅 `start/end`：区间没动但视口终于有了行，也是进展。
 */
export function decideConvergence(
  cycle: ConvergenceCycle,
  signature: string,
  covered: boolean,
  layoutReady: boolean,
): ConvergenceDecision {
  if (!layoutReady) return "paused";
  if (covered) return "settled";
  if (cycle.lastSignature === signature) {
    // 上一步推进后几何完全没变：再推一页大概率只是把同一段腾挪，先试一次
    // 位置重定位（用高度账本估目标位置），还不行就停，交给用户点「重定位」。
    return cycle.focusedOnce ? "stalled" : "focus";
  }
  if (cycle.remaining <= 0) return "stalled";
  return "advance";
}

/** 记录一次有效推进：消耗预算并留下签名。 */
export function noteAdvance(cycle: ConvergenceCycle, signature: string): ConvergenceCycle {
  return {
    ...cycle,
    remaining: Math.max(0, cycle.remaining - 1),
    lastSignature: signature,
    focusedOnce: false,
  };
}

export function noteFocus(cycle: ConvergenceCycle): ConvergenceCycle {
  return { ...cycle, focusedOnce: true };
}

/**
 * 视口没有可视行时，按高度账本估一个应该开窗到的 index。
 *
 * 估计以**历史区边界**为锚点累加，不直接 `scrollTop / median`：未挂载区的高度
 * 本来就是 spacer 的估算高度（同一本账本、同一个中位数），从已知边界推才不会
 * 把整段估算误差积进来。
 *
 * 返回 `null` 表示无从估计（没有历史区行、中位数非法），调用方应判 stalled
 * 而不是乱跳。
 */
export function estimateFocusIndex(
  measurement: {
    viewportTop: number;
    history: { first: number; last: number; gapTopPx: number; gapBottomPx: number } | null;
  } | null,
  medianPx: number,
  itemCount: number,
): number | null {
  if (!measurement || !measurement.history) return null;
  if (!Number.isFinite(medianPx) || medianPx <= 0 || itemCount <= 0) return null;
  const { history } = measurement;
  // gapBottomPx < 0：视口底越过历史区底边，视口在历史区**下方**，往后开窗。
  // gapTopPx > 0：视口顶越过历史区顶边，视口在历史区**上方**，往前开窗。
  const overflowBelow = -history.gapBottomPx;
  const overflowAbove = history.gapTopPx;
  let estimate: number;
  if (overflowBelow > 0) estimate = history.last + Math.ceil(overflowBelow / medianPx) + 1;
  else if (overflowAbove > 0) estimate = history.first - Math.ceil(overflowAbove / medianPx) - 1;
  else estimate = history.first;
  return Math.max(0, Math.min(itemCount - 1, estimate));
}

/**
 * 几何签名。只包含与「视口是否被覆盖」有关的量：
 * 挂载区间、视口内首末行、tail 是否可见。
 *
 * 刻意**不含** `scrollTop` 本身：程序化补偿会让它变化，但那不构成进展。
 */
export function geometrySignature(parts: {
  start: number;
  end: number;
  visible: { first: number; last: number } | null;
  tailVisible: boolean;
}): string {
  return `${parts.start}:${parts.end}:${
    parts.visible ? `${parts.visible.first}-${parts.visible.last}` : "none"
  }:${parts.tailVisible ? "t" : "-"}`;
}
