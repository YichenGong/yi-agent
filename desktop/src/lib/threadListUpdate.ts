/**
 * 会话列表的**就地**更新：把一条元数据变更写进本地 `groups` / `pinned`。
 *
 * 纯函数：不碰 React、不碰 RPC。服务端用 `thread/titleChanged` 推来权威标题后，
 * 就地改写比再拉一次 `thread/listAll` 便宜（后者要深扫全部目录），也与
 * `thread/modelChanged` / `thread/permissionModeChanged` 走的同族路径一致——
 * 通知带权威值，本地落笔。
 *
 * 返回新副本，绝不改动入参；未命中的 thread 为 no-op（原样返回入参）。
 */
import type { ThreadSummary, WorkspaceGroup } from "./protocol";

/** 把一条 thread 的标题改写进分组与置顶两份列表。 */
export function applyTitleChange(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  threadId: string,
  title: string,
): { groups: WorkspaceGroup[]; pinned: ThreadSummary[] } {
  const hit =
    groups.some((g) => g.threads.some((t) => t.thread_id === threadId)) ||
    pinned.some((t) => t.thread_id === threadId);
  if (!hit) return { groups, pinned };

  // 置顶项同时存在于分组里（服务端为向后兼容不摘除），故两份都要更新，
  // 否则侧栏两处会显示不一致。
  const retitle = (t: ThreadSummary): ThreadSummary =>
    t.thread_id === threadId ? { ...t, title } : t;

  return {
    groups: groups.map((g) =>
      g.threads.some((t) => t.thread_id === threadId)
        ? { ...g, threads: g.threads.map(retitle) }
        : g,
    ),
    pinned: pinned.some((t) => t.thread_id === threadId) ? pinned.map(retitle) : pinned,
  };
}
/** 把一条 thread 的颜色标签改写进分组与置顶两份列表（`null` = 清除）。 */
export function applyTagChange(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  threadId: string,
  tag: string | null,
): { groups: WorkspaceGroup[]; pinned: ThreadSummary[] } {
  const hit =
    groups.some((g) => g.threads.some((t) => t.thread_id === threadId)) ||
    pinned.some((t) => t.thread_id === threadId);
  if (!hit) return { groups, pinned };

  // 置顶项同时存在于分组里（服务端为向后兼容不摘除），故两份都要更新，
  // 与 applyTitleChange 同理。
  const retag = (t: ThreadSummary): ThreadSummary =>
    t.thread_id === threadId ? { ...t, tag } : t;

  return {
    groups: groups.map((g) =>
      g.threads.some((t) => t.thread_id === threadId)
        ? { ...g, threads: g.threads.map(retag) }
        : g,
    ),
    pinned: pinned.some((t) => t.thread_id === threadId) ? pinned.map(retag) : pinned,
  };
}
