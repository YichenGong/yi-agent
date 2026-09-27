import { describe, it, expect } from "vitest";
import type { Workspace, WorkspaceGroup } from "./protocol";

describe("workspace types", () => {
  it("shapes compile", () => {
    const w: Workspace = { path: "/tmp/a", exists: true };
    const g: WorkspaceGroup = { workspace: "/tmp/a", exists: true, threads: [] };
    expect(w.path).toBe("/tmp/a");
    expect(g.threads).toEqual([]);
  });
});
