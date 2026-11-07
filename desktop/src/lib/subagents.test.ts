import { describe, it, expect } from "vitest";
import {
  applyChildrenUpdated,
  childrenOf,
  isFinished,
  openTarget,
  SubagentRailStore,
  subagentState,
  TERMINAL_STATES,
  toRow,
} from "./subagents";

/** 测试用的子任务。`usage` 恒为零：本文件测的是折叠逻辑，不关心用量。 */
const ZERO_USAGE = { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0 };

const child = (
  taskId: string,
  state: string,
  lastStep?: string,
): {
  taskId: string;
  state: string;
  objective?: string;
  lastStep?: string;
  usage: typeof ZERO_USAGE;
} => ({
  taskId,
  state,
  objective: `objective of ${taskId}`,
  lastStep,
  usage: ZERO_USAGE,
});

describe("subagents folding", () => {
  it("folds children/updated notifications into the rail list", () => {
    const rows = applyChildrenUpdated([child("a", "running", "step a"), child("b", "running")]);
    expect(rows.map((r) => r.taskId)).toEqual(["a", "b"]);
    expect(rows[0].objective).toBe("objective of a");
    expect(rows[0].lastStep).toBe("step a");
  });

  it("presents the full core state table with the same labels the TUI uses", () => {
    const expected: Array<[string, string, string, boolean, boolean]> = [
      ["queued", "排队中", "neutral", false, true],
      ["running", "运行中", "warning", false, true],
      ["waiting_for_resource", "等待资源", "warning", false, true],
      ["waiting_for_permission", "等待授权", "warning", false, true],
      ["waiting_for_children", "等待子任务", "warning", false, true],
      ["paused", "已暂停", "neutral", false, false],
      ["completed", "已完成", "success", true, false],
      ["completed_no_changes", "已完成，无变更", "success", true, false],
      ["blocked", "已阻塞", "danger", true, false],
      ["stalled", "已停滞", "danger", true, false],
      ["timed_out", "已超时", "danger", true, false],
      ["budget_exhausted", "预算耗尽", "danger", true, false],
      ["failed", "失败", "danger", true, false],
      ["cancelled", "已取消", "neutral", true, false],
      ["recovery_required", "需要恢复", "danger", true, false],
    ];
    for (const [state, label, tone, finished, active] of expected) {
      expect(subagentState(state)).toEqual({ label, tone, finished, active });
    }
    expect(expected).toHaveLength(15);
  });

  it("classifies paused, terminal waits and unknown without counting them active", () => {
    expect(isFinished("blocked")).toBe(true);
    expect(isFinished("recovery_required")).toBe(true);
    expect(subagentState("paused")).toMatchObject({ label: "已暂停", active: false, finished: false });
    expect(subagentState("waiting_for_permission")).toMatchObject({ label: "等待授权", active: true });
    expect(subagentState("future_state")).toMatchObject({ label: "future_state", active: false, finished: false });
    expect(subagentState("").label).toBe("状态未知");
    expect(subagentState("constructor")).toMatchObject({ label: "constructor", active: false, finished: false });
  });

  it("only counts the five live states as active", () => {
    const states = [
      "queued",
      "running",
      "waiting_for_resource",
      "waiting_for_permission",
      "waiting_for_children",
      "paused",
      "completed",
      "completed_no_changes",
      "blocked",
      "stalled",
      "timed_out",
      "budget_exhausted",
      "failed",
      "cancelled",
      "recovery_required",
      "future_state",
      "",
    ];
    const active = states.filter((state) => subagentState(state).active);
    expect(active).toEqual([
      "queued",
      "running",
      "waiting_for_resource",
      "waiting_for_permission",
      "waiting_for_children",
    ]);
  });

  it("derives TERMINAL_STATES from the same table so the two cannot drift", () => {
    expect([...TERMINAL_STATES].sort()).toEqual(
      [
        "completed",
        "completed_no_changes",
        "blocked",
        "stalled",
        "timed_out",
        "budget_exhausted",
        "failed",
        "cancelled",
        "recovery_required",
      ].sort(),
    );
    for (const state of TERMINAL_STATES) {
      expect(subagentState(state).finished).toBe(true);
    }
  });

  it("keeps a terminal child in the list but marks it finished", () => {
    const rows = applyChildrenUpdated([child("done", "completed", "should not show")]);
    expect(rows).toHaveLength(1);
    expect(rows[0].finished).toBe(true);
    expect(rows[0].lastStep).toBeNull();
  });

  it("treats every terminal state core reports as finished", () => {
    // spec §3.1 的 15 条状态；终态 9 条，其余 6 条都不是终态。
    const terminal = [
      "completed",
      "completed_no_changes",
      "blocked",
      "stalled",
      "timed_out",
      "budget_exhausted",
      "failed",
      "cancelled",
      "recovery_required",
    ];
    const live = [
      "queued",
      "running",
      "waiting_for_resource",
      "waiting_for_permission",
      "waiting_for_children",
      "paused",
    ];
    for (const state of terminal) {
      expect(isFinished(state)).toBe(true);
    }
    for (const state of live) {
      expect(isFinished(state)).toBe(false);
    }
    // 未知状态不推定终态：不在表里就不是「已完成」。
    expect(isFinished("future_state")).toBe(false);
    expect(isFinished("constructor")).toBe(false);
  });

  it("clears lastStep for all nine terminal states", () => {
    const terminal = [
      "completed",
      "completed_no_changes",
      "blocked",
      "stalled",
      "timed_out",
      "budget_exhausted",
      "failed",
      "cancelled",
      "recovery_required",
    ];
    for (const state of terminal) {
      expect(toRow(child(`t-${state}`, state, "still running tests")).lastStep).toBeNull();
    }
    expect(toRow(child("t-paused", "paused", "已提交的步骤")).lastStep).toBe("已提交的步骤");
  });

  it("keeps null objective and step null instead of inventing text", () => {
    const bare = toRow({
      taskId: "task-a31f",
      state: "running",
      objective: null,
      lastStep: null,
      usage: ZERO_USAGE,
    });
    expect(bare.objective).toBeNull();
    expect(bare.lastStep).toBeNull();

    const bareFinished = toRow({
      taskId: "task-b31f",
      state: "blocked",
      objective: null,
      lastStep: null,
      usage: ZERO_USAGE,
    });
    expect(bareFinished.objective).toBeNull();
    expect(bareFinished.lastStep).toBeNull();
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
    expect(childrenOf([{ taskId: "a", parentTaskId: "root", usage: ZERO_USAGE }], "d")).toEqual([]);
  });

  it("computes a row's parent so the rail can be walked as a tree", () => {
    expect(toRow({ taskId: "a", state: "running", parentTaskId: "root", usage: ZERO_USAGE }).parentTaskId).toBe("root");
    expect(toRow({ taskId: "b", state: "running", usage: ZERO_USAGE }).parentTaskId).toBeNull();
  });
});
