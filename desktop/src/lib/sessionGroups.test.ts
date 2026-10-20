import { describe, expect, it } from "vitest";
import type { ThreadSummary, WorkspaceGroup } from "./protocol";
import { groupByThread, groupMembers, layoutWorkspace } from "./sessionGroups";

function thread(id: string, updated_at: number, extra: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: id,
    cwd: "/p",
    model: "m",
    created_at: 0,
    updated_at,
    title: id,
    ...extra,
  };
}

function ws(threads: ThreadSummary[], groups?: WorkspaceGroup["groups"]): WorkspaceGroup {
  return { workspace: "/p", exists: true, threads, groups };
}

describe("groupMembers", () => {
  it("按侧栏既有顺序（threads 原序）取成员，忽略组内 id 顺序", () => {
    const a = thread("a", 30);
    const b = thread("b", 20);
    const members = groupMembers({ id: "g", name: "g", description: "", threadIds: ["b", "a"] }, [
      a,
      b,
    ]);
    expect(members.map((t) => t.thread_id)).toEqual(["a", "b"]);
  });

  it("剔除不存在的成员", () => {
    const members = groupMembers(
      { id: "g", name: "g", description: "", threadIds: ["a", "gone"] },
      [thread("a", 1)],
    );
    expect(members.map((t) => t.thread_id)).toEqual(["a"]);
  });
});

describe("layoutWorkspace", () => {
  it("无分组时退化为逐条会话行", () => {
    const rows = layoutWorkspace(ws([thread("a", 30), thread("b", 20)]));
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : r.group.id))).toEqual([
      "a",
      "b",
    ]);
  });

  it("归组会话只出现一次，组框落在最靠前成员的位置", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 30), thread("b", 20), thread("c", 10)], [
        { id: "g", name: "g", description: "", threadIds: ["c", "b"] },
      ]),
    );
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : `[${r.group.id}]`))).toEqual([
      "a",
      "[g]",
    ]);
    const box = rows[1];
    expect(box.kind).toBe("group");
    if (box.kind === "group") {
      expect(box.members.map((t) => t.thread_id)).toEqual(["b", "c"]);
    }
  });

  it("多个组按各自最靠前成员的位置交错，组内成员不再单列", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 50), thread("b", 40), thread("c", 30), thread("d", 20)], [
        { id: "g1", name: "g1", description: "", threadIds: ["b", "d"] },
        { id: "g2", name: "g2", description: "", threadIds: ["c"] },
      ]),
    );
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : `[${r.group.id}]`))).toEqual([
      "a",
      "[g1]",
      "[g2]",
    ]);
  });

  it("置顶会话仍留在组框内（spec 刻意例外），且不因 pinned 而消失", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 10, { pinned: true }), thread("b", 20)], [
        { id: "g", name: "g", description: "", threadIds: ["a"] },
      ]),
    );
    const box = rows.find((r) => r.kind === "group");
    expect(box).toBeDefined();
    if (box && box.kind === "group") {
      expect(box.members.map((t) => t.thread_id)).toEqual(["a"]);
    }
  });

  it("成员全都不在 threads 里时整框不渲染", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 10)], [{ id: "g", name: "g", description: "", threadIds: ["gone"] }]),
    );
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : r.group.id))).toEqual(["a"]);
  });
});

describe("layoutWorkspace 的多对多", () => {
  it("同一会话属于多个组时，两个框都渲染，且该会话在每个框里都在", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 20), thread("b", 10)], [
        { id: "g1", name: "g1", description: "", threadIds: ["a"] },
        { id: "g2", name: "g2", description: "", threadIds: ["a"] },
      ]),
    );
    const boxes = rows.filter((r) => r.kind === "group");
    expect(boxes.length).toBe(2);
    for (const box of boxes) {
      if (box.kind === "group") expect(box.members.map((t) => t.thread_id)).toEqual(["a"]);
    }
    // 未归组的 b 仍单列一次。
    expect(rows.filter((r) => r.kind === "thread").map((r) => (r.kind === "thread" ? r.thread.thread_id : ""))).toEqual(["b"]);
  });
});

describe("groupByThread", () => {
  it("每个归组会话都能反查到它所在的组（多组时取第一个）", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 20), thread("b", 10)], [
        { id: "g1", name: "g1", description: "", threadIds: ["a"] },
        { id: "g2", name: "g2", description: "", threadIds: ["a"] },
      ]),
    );
    const map = groupByThread(rows);
    expect(map.get("a")?.id).toBe("g1");
  });
});
