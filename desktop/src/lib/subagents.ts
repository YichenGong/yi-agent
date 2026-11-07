/**
 * Client-side folding of the `agent/*` notifications into the subagent rail.
 *
 * Pure functions and a tiny mutable store, kept out of the component so the
 * folding rules are testable without a DOM: a rail that renders the wrong
 * shape is a rendering bug, but a rail that folds notifications wrongly is a
 * state bug, and the two deserve separate tests.
 */
import type { AgentChild } from "./protocol";

/** A child as the rail shows it: identity, progress, and whether it is done. */
export interface SubagentRow {
  taskId: string;
  objective: string | null;
  state: string;
  lastStep: string | null;
  /** True once the child's current execution state is terminal. */
  finished: boolean;
  /** The row's own parent, used to find a task's direct children for drilling. */
  parentTaskId: string | null;
}

/** How one execution state reads in the rail and the detail header. */
export interface StatePresentation {
  /** The Chinese label; the daemon's states are an internal vocabulary. */
  label: string;
  /** Which emphasis token the row uses. Never the only signal (label always shows). */
  tone: "neutral" | "success" | "warning" | "danger";
  /** True when this execution state is terminal (see `TERMINAL_STATES`). */
  finished: boolean;
  /** True when the task counts toward "进行中" in the rail header. */
  active: boolean;
}

/**
 * The one table both the rail and the detail read.
 *
 * It mirrors the core's `TaskState::is_terminal` and the supervisor's
 * `task_state_label` one-for-one, so the desktop cannot drift from the TUI:
 * `queued` is live but not yet doing work, `paused` is live-but-idle (it must
 * not read as "working", nor be counted as in progress), and every wait state
 * is active because the task is still going to progress on its own.
 *
 * Terminal here describes the current execution state only — it says nothing
 * about the container's lifetime, and whether a message can still be delivered
 * stays the server's call.
 */
const STATE_PRESENTATION: Readonly<Record<string, StatePresentation>> = {
  queued: { label: "排队中", tone: "neutral", finished: false, active: true },
  running: { label: "运行中", tone: "warning", finished: false, active: true },
  waiting_for_resource: { label: "等待资源", tone: "warning", finished: false, active: true },
  waiting_for_permission: { label: "等待授权", tone: "warning", finished: false, active: true },
  waiting_for_children: { label: "等待子任务", tone: "warning", finished: false, active: true },
  paused: { label: "已暂停", tone: "neutral", finished: false, active: false },
  completed: { label: "已完成", tone: "success", finished: true, active: false },
  completed_no_changes: { label: "已完成，无变更", tone: "success", finished: true, active: false },
  blocked: { label: "已阻塞", tone: "danger", finished: true, active: false },
  stalled: { label: "已停滞", tone: "danger", finished: true, active: false },
  timed_out: { label: "已超时", tone: "danger", finished: true, active: false },
  budget_exhausted: { label: "预算耗尽", tone: "danger", finished: true, active: false },
  failed: { label: "失败", tone: "danger", finished: true, active: false },
  cancelled: { label: "已取消", tone: "neutral", finished: true, active: false },
  recovery_required: { label: "需要恢复", tone: "danger", finished: true, active: false },
};

/** What an unrecognised state reads as: unknown, neither terminal nor active. */
const UNKNOWN = { label: "状态未知", tone: "neutral", finished: false, active: false } as const;

/**
 * How one state reads, looked up in the shared table.
 *
 * The lookup is an own-property check on purpose: a state named `constructor`
 * or `toString` must fall through to "unknown" rather than answer with a
 * prototype member. An unrecognised state keeps its raw value as the label so
 * a newer daemon is visible rather than silently indistinguishable.
 */
export function subagentState(state: string): StatePresentation {
  return Object.prototype.hasOwnProperty.call(STATE_PRESENTATION, state)
    ? STATE_PRESENTATION[state]
    : { ...UNKNOWN, label: state || UNKNOWN.label };
}

/**
 * The states a task's current execution can end in.
 *
 * Derived from `STATE_PRESENTATION` so the terminal list and the labels cannot
 * disagree; adding a state to the table cannot leave the rail behind.
 */
export const TERMINAL_STATES: string[] = Object.keys(STATE_PRESENTATION).filter(
  (state) => STATE_PRESENTATION[state].finished,
);

export function isFinished(state: string): boolean {
  return subagentState(state).finished;
}

/**
 * Fold one child into a rail row.
 *
 * A finished child stays in the rail (the user may still want to read what it
 * did) but stops advertising a current step: showing "running tests" next to a
 * cancelled task reads as if it were still working.
 *
 * `objective` and `lastStep` are passed through as the server sent them,
 * including `null`: production does not populate them today, and inventing a
 * title or a step would be a lie the rail cannot back up.
 */
export function toRow(child: AgentChild): SubagentRow {
  const finished = isFinished(child.state);
  return {
    taskId: child.taskId,
    objective: child.objective ?? null,
    state: child.state,
    lastStep: finished ? null : (child.lastStep ?? null),
    finished,
    parentTaskId: child.parentTaskId ?? null,
  };
}

/**
 * Apply a whole-list `agent/children/updated` payload.
 *
 * The server pushes the entire list rather than a delta, so this replaces
 * rather than merges: a client that merged could keep a child the server has
 * already dropped.
 */
export function applyChildrenUpdated(children: AgentChild[]): SubagentRow[] {
  return children.map(toRow);
}

/**
 * Which task a click on a rail card should open.
 *
 * A finished child is still worth opening — its trace is the record of what it
 * did — so every card opens its own task.
 */
export function openTarget(row: SubagentRow): string {
  return row.taskId;
}

/**
 * The rail's per-thread view of one conversation's children.
 *
 * Mutable and keyed by conversation, mirroring `ThreadStore`: notifications
 * arrive for background conversations too, and the rail must be current when
 * the user switches back. Callers re-render after each change.
 */
export class SubagentRailStore {
  private byThread = new Map<string, SubagentRow[]>();

  /** Replace a conversation's list, as a whole-list push or a fresh read does. */
  set(threadId: string, children: AgentChild[]): void {
    this.byThread.set(threadId, applyChildrenUpdated(children));
  }

  get(threadId: string): SubagentRow[] {
    return this.byThread.get(threadId) ?? [];
  }

  /** Fold one notification. Returns true when it changed the rail. */
  applyNotification(threadId: string, children: AgentChild[]): boolean {
    this.set(threadId, children);
    return true;
  }

  drop(threadId: string): void {
    this.byThread.delete(threadId);
  }
}

/**
 * The direct children of one task, which are a detail's drill-down targets.
 *
 * The server hands the client one flat list per conversation; the tree is
 * recovered by parent, so drilling down walks that list rather than requesting
 * a new one per level.
 */
export function childrenOf<T extends { taskId: string; parentTaskId?: string | null }>(
  children: T[],
  taskId: string,
): T[] {
  return children.filter((child) => (child.parentTaskId ?? null) === taskId);
}
