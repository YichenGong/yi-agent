import { describe, it, expect } from "vitest";
import {
  applyChildrenUpdated,
  childrenOf,
  foldTraceRows,
  isFinished,
  openTarget,
  SubagentRailStore,
  toRow,
  toTraceLine,
} from "./subagents";

const row = (eventId: number, kind: string, payload: unknown): {
  eventId: number;
  taskId: string;
  kind: string;
  payloadJson: string;
} => ({ eventId, taskId: "t", kind, payloadJson: JSON.stringify(payload) });

const child = (taskId: string, state: string, lastStep?: string): {
  taskId: string;
  state: string;
  objective?: string;
  lastStep?: string;
} => ({ taskId, state, objective: `objective of ${taskId}`, lastStep });

describe("subagents folding", () => {
  it("folds children/updated notifications into the rail list", () => {
    const rows = applyChildrenUpdated([child("a", "running", "step a"), child("b", "running")]);
    expect(rows.map((r) => r.taskId)).toEqual(["a", "b"]);
    expect(rows[0].objective).toBe("objective of a");
    expect(rows[0].lastStep).toBe("step a");
  });

  it("keeps a terminal child in the list but marks it finished", () => {
    const rows = applyChildrenUpdated([child("done", "completed", "should not show")]);
    expect(rows).toHaveLength(1);
    expect(rows[0].finished).toBe(true);
    expect(rows[0].lastStep).toBeNull();
  });

  it("treats every terminal state the daemon uses as finished", () => {
    for (const state of ["completed", "completed_no_changes", "failed", "cancelled"]) {
      expect(isFinished(state)).toBe(true);
    }
    for (const state of ["running", "queued", "waiting_for_children"]) {
      expect(isFinished(state)).toBe(false);
    }
  });

  it("replaces the whole list rather than merging, so a dropped child disappears", () => {
    const store = new SubagentRailStore();
    store.set("thread-1", [child("a", "running"), child("b", "running")]);
    store.applyNotification("thread-1", [child("a", "running")]);
    expect(store.get("thread-1").map((r) => r.taskId)).toEqual(["a"]);
  });

  it("keeps conversations separate", () => {
    const store = new SubagentRailStore();
    store.set("thread-1", [child("a", "running")]);
    store.set("thread-2", [child("b", "running")]);
    expect(store.get("thread-1").map((r) => r.taskId)).toEqual(["a"]);
    expect(store.get("thread-2").map((r) => r.taskId)).toEqual(["b"]);
    store.drop("thread-1");
    expect(store.get("thread-1")).toEqual([]);
  });

  it("opens the task a card stands for, finished or not", () => {
    expect(openTarget(toRow(child("a", "running")))).toBe("a");
    expect(openTarget(toRow(child("done", "completed")))).toBe("done");
  });

  it("renders each trace kind as its own line", () => {
    expect(
      toTraceLine({
        eventId: 1,
        taskId: "a",
        kind: "assistant_text",
        payloadJson: JSON.stringify({ type: "assistant_text", text: "hello" }),
      }),
    ).toEqual({ kind: "assistant_text", text: "hello", isError: false });

    expect(
      toTraceLine({
        eventId: 2,
        taskId: "a",
        kind: "tool_call",
        payloadJson: JSON.stringify({ type: "tool_call", name: "bash", summary: "cargo test" }),
      }).text,
    ).toBe("bash(cargo test)");

    const result = toTraceLine({
      eventId: 3,
      taskId: "a",
      kind: "tool_result",
      payloadJson: JSON.stringify({ type: "tool_result", name: "bash", is_error: true, summary: "boom" }),
    });
    expect(result.text).toBe("boom");
    expect(result.isError).toBe(true);

    expect(
      toTraceLine({
        eventId: 4,
        taskId: "a",
        kind: "state_note",
        payloadJson: JSON.stringify({ type: "state_note", note: "completed" }),
      }).text,
    ).toBe("· completed");
  });

  it("merges consecutive assistant_text rows into one block", () => {
    const blocks = foldTraceRows([
      row(1, "assistant_text", { type: "assistant_text", text: "one " }),
      row(2, "assistant_text", { type: "assistant_text", text: "two" }),
      row(3, "tool_call", { type: "tool_call", name: "bash", summary: "ls" }),
      row(4, "assistant_text", { type: "assistant_text", text: "after" }),
    ]);
    expect(blocks.map((b) => b.text)).toEqual(["one two", "bash(ls)", "after"]);
    expect(blocks[0].key).toBe("row-1");
  });

  it("keeps a tool call and its result as separate blocks", () => {
    const blocks = foldTraceRows([
      row(1, "tool_call", { type: "tool_call", name: "bash", summary: "ls" }),
      row(2, "tool_result", { type: "tool_result", name: "bash", is_error: false, summary: "ok" }),
    ]);
    expect(blocks).toHaveLength(2);
    expect(blocks[1].kind).toBe("tool_result");
  });

  it("returns the direct children of a task, not its whole subtree", () => {
    const kids = childrenOf(
      [
        { taskId: "a", parentTaskId: "root" },
        { taskId: "b", parentTaskId: "a" },
        { taskId: "c", parentTaskId: "a" },
        { taskId: "d", parentTaskId: "b" },
      ],
      "a",
    );
    expect(kids.map((c) => c.taskId)).toEqual(["b", "c"]);
    expect(childrenOf([{ taskId: "a", parentTaskId: "root" }], "d")).toEqual([]);
  });

  it("computes a row's parent so the rail can be walked as a tree", () => {
    expect(toRow({ taskId: "a", state: "running", parentTaskId: "root" }).parentTaskId).toBe("root");
    expect(toRow({ taskId: "b", state: "running" }).parentTaskId).toBeNull();
  });

  it("falls back to the raw payload when a row does not parse", () => {
    const line = toTraceLine({
      eventId: 9,
      taskId: "a",
      kind: "assistant_text",
      payloadJson: "not json",
    });
    expect(line.text).toBe("");
    const unknown = toTraceLine({
      eventId: 10,
      taskId: "a",
      kind: "future_kind",
      payloadJson: "raw",
    });
    expect(unknown.text).toBe("raw");
  });
});
