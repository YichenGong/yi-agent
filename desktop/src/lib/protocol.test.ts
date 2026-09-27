import { describe, it, expect } from "vitest";
import type { ThreadSummary, Workspace, WorkspaceGroup } from "./protocol";

describe("workspace types", () => {
  it("shapes compile", () => {
    const w: Workspace = { path: "/tmp/a", exists: true };
    const g: WorkspaceGroup = { workspace: "/tmp/a", exists: true, threads: [] };
    expect(w.path).toBe("/tmp/a");
    expect(g.threads).toEqual([]);
  });
});

describe("thread summary types", () => {
  it("thread summary carries optional permission_mode", () => {
    const yolo: ThreadSummary = { thread_id: "t", cwd: "/a", model: "m", created_at: 0, updated_at: 0, title: null, permission_mode: "yolo" };
    const legacy: ThreadSummary = { thread_id: "t", cwd: "/a", model: "m", created_at: 0, updated_at: 0, title: null };
    expect(yolo.permission_mode).toBe("yolo");
    expect(legacy.permission_mode).toBeUndefined();
  });
});
