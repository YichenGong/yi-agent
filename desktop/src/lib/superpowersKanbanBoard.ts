/**
 * 状态 → 四列的映射与列内排序。
 *
 * 纯函数、不碰 React：这些规则会被轮询反复重算（每次都换新对象），做成纯函数
 * 才既便宜又可断言。未知状态一律进 `doing`——把它放进 `done` 会让人误以为
 * 完成了而漏掉，放进 `doing` 最坏也只是"看着还在跑"。
 */

import type { BoardCard } from "./superpowersKanbanState";

export const DONE_COLLAPSED_LIMIT = 5;

export type BoardColumnKey = "queued" | "doing" | "needDecision" | "done";

export interface BoardColumns {
  queued: BoardCard[];
  doing: BoardCard[];
  needDecision: BoardCard[];
  done: BoardCard[];
}

export function columnForState(state: string): BoardColumnKey {
  switch (state) {
    case "queued":
    case "launching":
    case "paused":
      return "queued";
    case "needs_you":
    case "awaiting_merge":
      return "needDecision";
    case "done":
    case "failed":
    case "cancelled":
      return "done";
    case "running":
    case "merging":
      return "doing";
    default:
      // 未知（未来新增 / 拼错）：保守放进 doing，绝不放进 done。
      return "doing";
  }
}

/** 时刻串缺失时排到末尾；字符串 ISO 可直接按字典序比较（同带时区偏移）。 */
function ascending(left: string | undefined, right: string | undefined): number {
  if (left === undefined) return right === undefined ? 0 : 1;
  if (right === undefined) return -1;
  return left < right ? -1 : left > right ? 1 : 0;
}

/** 同上的倒序版本：时刻都有的按大的在前，都缺的卡排到列末（比任何有时刻的卡靠后）。 */
function descending(left: string | undefined, right: string | undefined): number {
  if (left === undefined) return right === undefined ? 0 : 1;
  if (right === undefined) return -1;
  return left < right ? 1 : left > right ? -1 : 0;
}

export function boardColumns(cards: BoardCard[]): BoardColumns {
  const columns: BoardColumns = { queued: [], doing: [], needDecision: [], done: [] };
  for (const card of cards) columns[columnForState(card.state)].push(card);

  columns.queued.sort((l, r) => (l.order ?? 0) - (r.order ?? 0));

  columns.doing.sort((l, r) => ascending(l.enqueuedAt, r.enqueuedAt));

  // needs_you（要人回话）比 awaiting_merge（要人确认合并）更急，置顶。
  columns.needDecision.sort((l, r) => {
    const rank = (card: BoardCard) => (card.state === "needs_you" ? 0 : 1);
    return rank(l) - rank(r) || ascending(l.enqueuedAt, r.enqueuedAt);
  });

  // 完成时刻倒序：terminalAt 优先，回退 enqueuedAt；两者都缺的卡排到列末。
  columns.done.sort((l, r) =>
    descending(l.terminalAt ?? l.enqueuedAt, r.terminalAt ?? r.enqueuedAt),
  );

  return columns;
}

export function collapseDone(done: BoardCard[], expanded: boolean): BoardCard[] {
  return expanded ? done : done.slice(0, DONE_COLLAPSED_LIMIT);
}
