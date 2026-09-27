import type { WorkspaceGroup } from "./protocol";

export function groupCount(groups: WorkspaceGroup[]): number {
  return groups.reduce((n, g) => n + g.threads.length, 0);
}

export function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}
