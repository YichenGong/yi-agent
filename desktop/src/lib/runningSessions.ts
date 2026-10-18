import type { ThreadStatus } from "./protocol";

/**
 * 「此刻有几个会话真的在跑」——严格 `running`。
 *
 * `awaiting_approval` 不计:那种会话在**等人**,没在干活(与侧栏徽标的语义刻意不同,
 * 徽标对两者都显示)。`idle` 自然不计。
 *
 * 入参是侧栏徽标用的同一张表(`App.tsx:1888` 每轮渲染由 `ThreadStore` 重建),故不存在
 * 第二条数据路径、也不会与徽标漂移。
 */
export function countRunningSessions(statuses: ReadonlyMap<string, ThreadStatus>): number {
  let n = 0;
  for (const s of statuses.values()) if (s === "running") n += 1;
  return n;
}

/**
 * 指示器文案。与 `countRunningSessions` 同处一个模块:文案是这条语义的一部分,
 * 两者一起改才不容易失配。
 */
export function runningSessionsLabel(n: number): string {
  return `${n} 个会话运行中`;
}