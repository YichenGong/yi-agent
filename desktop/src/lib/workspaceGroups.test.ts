import { describe, expect, it } from "vitest";
import { basename, groupCount, nestLinkedGroups } from "./workspaceGroups";
import type { Workspace, WorkspaceGroup } from "./protocol";

const ws = (path: string, linked_to: string | null = null): Workspace => ({
  path,
  exists: true,
  linked_to,
});

const mk = (ws: string, ids: string[]): WorkspaceGroup => ({
  workspace: ws,
  exists: true,
  threads: ids.map((id, i) => ({
    thread_id: id, cwd: ws, model: "m", created_at: i, updated_at: i, title: null,
  })),
});

describe("groupCount", () => {
  it("counts total threads across groups", () => {
    expect(groupCount([mk("/a", ["1", "2"]), mk("/b", ["3"])])).toBe(3);
  });
  it("is zero for no groups", () => {
    expect(groupCount([])).toBe(0);
  });
});

describe("basename", () => {
  it("extracts the last path segment", () => {
    expect(basename("/Users/x/projectA")).toBe("projectA");
  });
  it("ignores a trailing slash", () => {
    expect(basename("/Users/x/")).toBe("x");
  });
});

describe("nestLinkedGroups", () => {
  it("hides a linked child from the top level and nests it under its primary", () => {
    const groups = [mk("/a", ["1"]), mk("/b", ["2"])];
    const out = nestLinkedGroups(groups, [ws("/a"), ws("/b", "/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/a"]);
    expect(out.top[0].children.map((c) => c.workspace)).toEqual(["/b"]);
  });

  it("keeps an unlinked dir at the top level", () => {
    const out = nestLinkedGroups([mk("/a", ["1"])], [ws("/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/a"]);
    expect(out.top[0].children).toEqual([]);
  });

  it("promotes a child back to the top when its primary is absent", () => {
    // 主根被 Remove from list 后，服务端不再上报 linked_to，但即便上报了、
    // 主根不在 groups 里也不能把子目录藏起来。
    const out = nestLinkedGroups([mk("/b", ["2"])], [ws("/b", "/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/b"]);
    expect(out.top[0].children).toEqual([]);
  });

  it("keeps group order stable", () => {
    const groups = [mk("/z", ["1"]), mk("/a", ["2"]), mk("/b", ["3"])];
    const out = nestLinkedGroups(groups, [ws("/z"), ws("/a"), ws("/b", "/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/z", "/a"]);
  });
});
