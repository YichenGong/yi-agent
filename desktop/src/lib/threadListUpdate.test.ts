import { describe, expect, it } from "vitest";
import { applyTitleChange } from "./threadListUpdate";
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