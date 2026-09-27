import { describe, expect, it } from "vitest";
import { basename, groupCount } from "./workspaceGroups";
import type { WorkspaceGroup } from "./protocol";

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
