/**
 * 状态栏用量数字的滚动几何。
 *
 * 纯函数，无 DOM：拆位、进退位步数、时长与错峰。组件只负责把这些数字写成
 * 内联样式，因此「滚几格、滚多久、晚多久起步」都能在 jsdom 里直接断言。
 */

/** 数字带里第一份 `0-9` 的起始索引；位置从这里起算，上下各留 9 步余量。 */
export const STRIP_BASE = 10;

/** 滚一步的基础时长与每多一步的增量。 */
export const BASE_MS = 300;
export const PER_STEP_MS = 55;

/** 单次滚动的时长上限。 */
export const MAX_MS = 720;

/** 高位相对个位的起步延迟，及其上限。 */
export const STAGGER_MS = 20;
export const MAX_STAGGER_MS = 120;

export type Direction = -1 | 0 | 1;

/** 把数值拆成数字位，最高位在前；负数取绝对值，小数截断。 */
export function splitDigits(value: number): number[] {
  const magnitude = Math.trunc(Math.abs(value));
  return String(magnitude)
    .split("")
    .map((char) => Number(char));
}

/**
 * 某个数字位从 `from` 滚到 `to` 要走几步、往哪走。
 *
 * 步数按 10 取模到滚动方向：递增时 `9 -> 0` 只走 1 步（进位），`0 -> 9` 走
 * 9 步；递减时镜像。这正是机械里程表的行为——不能为了「最短路径」让 9 倒着
 * 退回 0，那样方向就错了。
 */
export function digitMove(
  from: number,
  to: number,
  direction: Direction,
): { steps: number; direction: Direction } {
  if (direction === 0 || from === to) return { steps: 0, direction: 0 };
  const steps = direction > 0 ? (to - from + 10) % 10 : (from - to + 10) % 10;
  return steps === 0 ? { steps: 0, direction: 0 } : { steps, direction };
}

/** 滚 `steps` 步要多久；0 步不产生过渡。 */
export function digitDurationMs(steps: number): number {
  if (steps <= 0) return 0;
  return Math.min(BASE_MS + (steps - 1) * PER_STEP_MS, MAX_MS);
}

/** 从右数第 `indexFromRight` 位的起步延迟（个位为 0）。 */
export function digitDelayMs(indexFromRight: number): number {
  if (indexFromRight <= 0) return 0;
  return Math.min(indexFromRight * STAGGER_MS, MAX_STAGGER_MS);
}

/** 数字带在位置 `pos` 处的位移。每行一个字符高，故 1 位 = 1em。 */
export function stripY(pos: number): string {
  return `translateY(-${pos}em)`;
}