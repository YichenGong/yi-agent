/**
 * 会话分组的布局纯函数。不碰 React、不碰 RPC。
 *
 * 侧栏的既有行序由服务端给定（`threads` 已按 `updated_at` 倒序）。分组只改变
 * 「哪些行被收进框里」，**不重排**：框落在它最靠前成员原来的位置上，框内成员
 * 按同一顺序排列。这样用户下次找会话时，位置相对稳定。
 */
import type { SessionGroup, ThreadSummary, WorkspaceGroup } from "./protocol";

export type SidebarRow =
  | { kind: "thread"; thread: ThreadSummary }
  | { kind: "group"; group: SessionGroup; members: ThreadSummary[] };

/** 组的成员，按 `threads` 的既有顺序；剔除已不存在的 id。 */
export function groupMembers(group: SessionGroup, threads: ThreadSummary[]): ThreadSummary[] {
  const wanted = new Set(group.threadIds);
  return threads.filter((t) => wanted.has(t.thread_id));
}

/**
 * 把一个 workspace 分组展开成侧栏行序。
 *
 * - 归组会话只在框内出现一次（不再单列）。
 * - 框的位置 = 它最靠前成员在 `threads` 里的下标。
 * - 空框（成员全部消失）不渲染。
 */
export function layoutWorkspace(g: WorkspaceGroup): SidebarRow[] {
  const groups = g.groups ?? [];
  const indexOf = new Map<string, number>();
  g.threads.forEach((t, i) => indexOf.set(t.thread_id, i));

  const boxes: { group: SessionGroup; members: ThreadSummary[]; at: number }[] = [];
  for (const group of groups) {
    const members = groupMembers(group, g.threads);
    if (members.length === 0) continue;
    const at = Math.min(...members.map((t) => indexOf.get(t.thread_id) ?? Number.MAX_SAFE_INTEGER));
    boxes.push({ group, members, at });
  }
  // 同一起点时保持服务端下发的组顺序（稳定排序）。
  boxes.sort((a, b) => a.at - b.at);

  // 一个下标可能挂着多个框：两条会话同属两个不同的组时（多对多），它们的
  // 「最靠前成员」可以落在同一行号上。用 Map<下标, 框数组> 而不是单值，
  // 否则后一个会把前一个顶掉——那正是「同一会话属于多个组」最常见的形态。
  const boxesAt = new Map<number, (typeof boxes)[number][]>();
  for (const box of boxes) {
    const list = boxesAt.get(box.at) ?? [];
    list.push(box);
    boxesAt.set(box.at, list);
  }
  const grouped = new Set(boxes.flatMap((b) => b.members.map((t) => t.thread_id)));

  const rows: SidebarRow[] = [];
  g.threads.forEach((thread, i) => {
    for (const box of boxesAt.get(i) ?? []) {
      rows.push({ kind: "group", group: box.group, members: box.members });
    }
    if (grouped.has(thread.thread_id)) return;
    rows.push({ kind: "thread", thread });
  });
  return rows;
}

/** thread_id → 它所在的组（同一会话属于多组时取行序最靠前的那个）。 */
export function groupByThread(rows: SidebarRow[]): Map<string, SessionGroup> {
  const map = new Map<string, SessionGroup>();
  for (const row of rows) {
    if (row.kind !== "group") continue;
    for (const t of row.members) {
      if (!map.has(t.thread_id)) map.set(t.thread_id, row.group);
    }
  }
  return map;
}
