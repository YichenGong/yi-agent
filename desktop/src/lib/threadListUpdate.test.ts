import { describe, expect, it } from "vitest";
import { applyTagChange, applyTitleChange } from "./threadListUpdate";
import type { ThreadSummary, WorkspaceGroup } from "./protocol";

function thread(id: string, title: string | null, extra: Partial<ThreadSummary> = {}): ThreadSummary {
  return { thread_id: id, cwd: "/w", model: "m", created_at: 0, updated_at: 0, title, ...extra };
}

const groups: WorkspaceGroup[] = [
  { workspace: "/a", exists: true, threads: [thread("1", "one"), thread("2", "two")] },
  { workspace: "/b", exists: true, threads: [thread("3", "three")] },
];
const pinned: ThreadSummary[] = [thread("2", "two", { pinned: true })];

describe("applyTitleChange", () => {
  it("rewrites the target thread's title in its group", () => {
    const out = applyTitleChange(groups, pinned, "3", "new three");
    expect(out.groups[1].threads[0].title).toBe("new three");
  });

  it("rewrites the target thread's title in the pinned list too", () => {
    const out = applyTitleChange(groups, pinned, "2", "renamed two");
    expect(out.pinned[0].title).toBe("renamed two");
    // 置顶项同时存在于分组里，两份都要更新，否则侧栏两处会显示不一致。
    expect(out.groups[0].threads[1].title).toBe("renamed two");
  });

  it("leaves other threads and other fields untouched", () => {
    const out = applyTitleChange(groups, pinned, "3", "new three");
    expect(out.groups[0].threads[0]).toEqual(groups[0].threads[0]);
    expect(out.groups[0].threads[1]).toEqual(groups[0].threads[1]);
    expect(out.pinned).toBe(pinned);
  });

  it("does not mutate its inputs", () => {
    const out = applyTitleChange(groups, pinned, "1", "changed");
    expect(groups[0].threads[0].title).toBe("one");
    expect(out.groups).not.toBe(groups);
  });

  it("is a no-op for an unknown thread id", () => {
    const out = applyTitleChange(groups, pinned, "nope", "x");
    expect(out.groups).toEqual(groups);
    expect(out.pinned).toBe(pinned);
  });
});
describe("applyTagChange", () => {
  const groups: WorkspaceGroup[] = [
    {
      workspace: "/w",
      exists: true,
      threads: [
        { thread_id: "t1", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "one" },
        { thread_id: "t2", cwd: "/w", model: "m", created_at: 0, updated_at: 0, title: "two" },
      ],
    },
  ];

  it("writes the tag into the matching thread only", () => {
    const next = applyTagChange(groups, [], "t1", "red");
    expect(next.groups[0].threads[0].tag).toBe("red");
    expect(next.groups[0].threads[1].tag).toBeUndefined();
  });

  it("clears the tag with null", () => {
    const tagged = applyTagChange(groups, [], "t1", "red");
    const cleared = applyTagChange(tagged.groups, [], "t1", null);
    expect(cleared.groups[0].threads[0].tag).toBeNull();
  });

  it("updates both the group copy and the pinned copy", () => {
    const pinned = [{ ...groups[0].threads[0], pinned: true }];
    const next = applyTagChange(groups, pinned, "t1", "blue");
    expect(next.groups[0].threads[0].tag).toBe("blue");
    expect(next.pinned[0].tag).toBe("blue");
  });

  it("is a no-op for an unknown thread", () => {
    const next = applyTagChange(groups, [], "nope", "red");
    expect(next.groups).toBe(groups);
  });
});
