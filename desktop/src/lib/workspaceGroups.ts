import type { Workspace, WorkspaceGroup } from "./protocol";

export function groupCount(groups: WorkspaceGroup[]): number {
  return groups.reduce((n, g) => n + g.threads.length, 0);
}

export function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}

/** 一个顶层组及其附属根小节。 */
export interface NestedGroup {
  group: WorkspaceGroup;
  children: WorkspaceGroup[];
}

export interface NestedGroups {
  top: NestedGroup[];
}

/**
 * 按 `linked_to` 把附属根从顶层摘下来，挂到主根的 `children` 里。
 *
 * 规则（与 spec §7 一致）：只有「主根也在本批 groups 里」时才嵌套，否则附属根
 * 留在顶层——否则主根被 Remove from list 后子目录会凭空消失。
 *
 * 在 `filterThreadTree` **之后**调用：搜索、置顶、看板会话三个既有分支都不受影响。
 */
export function nestLinkedGroups(
  groups: WorkspaceGroup[],
  workspaces: Workspace[],
): NestedGroups {
  const primaryOf = new Map<string, string>();
  for (const w of workspaces) {
    if (w.linked_to) primaryOf.set(w.path, w.linked_to);
  }
  const present = new Set(groups.map((g) => g.workspace));
  const childrenOf = new Map<string, WorkspaceGroup[]>();
  const top: NestedGroup[] = [];
  for (const g of groups) {
    const primary = primaryOf.get(g.workspace);
    if (primary && present.has(primary)) {
      const list = childrenOf.get(primary) ?? [];
      list.push(g);
      childrenOf.set(primary, list);
    } else {
      top.push({ group: g, children: [] });
    }
  }
  for (const node of top) {
    node.children = childrenOf.get(node.group.workspace) ?? [];
  }
  return { top };
}
