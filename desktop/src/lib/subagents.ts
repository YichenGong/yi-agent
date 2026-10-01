/**
 * Client-side folding of the `agent/*` notifications into the subagent rail.
 *
 * Pure functions and a tiny mutable store, kept out of the component so the
 * folding rules are testable without a DOM: a rail that renders the wrong
 * shape is a rendering bug, but a rail that folds notifications wrongly is a
 * state bug, and the two deserve separate tests.
 */
import type { AgentChild, AgentTraceRow } from "./protocol";

/** A child as the rail shows it: identity, progress, and whether it is done. */
export interface SubagentRow {
  taskId: string;
  objective: string | null;
  state: string;
  lastStep: string | null;
  /** True once the child can no longer make progress. */
  finished: boolean;
  /** The row's own parent, used to find a task's direct children for drilling. */
  parentTaskId: string | null;
}

/**
 * The states a task never leaves. Mirrors the daemon's own terminal vocabulary
 * so the rail agrees with `agent/children/list` and the TUI tab.
 */
export const TERMINAL_STATES = ["completed", "completed_no_changes", "failed", "cancelled"];

export function isFinished(state: string): boolean {
  return TERMINAL_STATES.includes(state);
}

/**
 * Fold one child into a rail row.
 *
 * A finished child stays in the rail (the user may still want to read what it
 * did) but stops advertising a current step: showing "running tests" next to a
 * cancelled task reads as if it were still working.
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

/**
 * One block of the trace detail: what the user reads, not what was persisted.
 *
 * A run of assistant text rows is one block (the writer flushes text every 512
 * bytes or 200ms, so consecutive rows are one message split by its flush
 * threshold, not two messages). Tool calls and results stay separate blocks so
 * a tool's state is visible without expanding anything.
 */
export interface TraceBlock {
  /** Stable within a view; the first row id that produced the block. */
  key: string;
  kind: "assistant_text" | "tool_call" | "tool_result" | "state_note" | string;
  text: string;
  isError: boolean;
}

/**
 * Fold trace rows into blocks, merging consecutive assistant text.
 *
 * The fold is a pure function of the whole row list. A streaming client that
 * appended to a mutable block would have to know which block the previous frame
 * ended on; folding the list it already holds cannot drift, and the memoization
 * upstream keeps it from re-running per delta.
 */
export function foldTraceRows(rows: AgentTraceRow[]): TraceBlock[] {
  const blocks: TraceBlock[] = [];
  for (const row of rows) {
    const line = toTraceLine(row);
    const last = blocks[blocks.length - 1];
    if (line.kind === "assistant_text" && last?.kind === "assistant_text") {
      blocks[blocks.length - 1] = { ...last, text: last.text + line.text };
      continue;
    }
    blocks.push({
      key: `row-${row.eventId}`,
      kind: line.kind,
      text: line.text,
      isError: line.isError,
    });
  }
  return blocks;
}

/**
 * Fold a trace row into a display line.
 *
 * The payload is the fact's own serde encoding (an internal `type` tag), so the
 * kind column is only used to pick a label: a row whose payload does not parse
 * still renders its raw text rather than disappearing.
 */
export interface TraceLine {
  kind: string;
  text: string;
  isError: boolean;
}

export function toTraceLine(row: AgentTraceRow): TraceLine {
  let payload: Record<string, unknown> | null = null;
  try {
    payload = JSON.parse(row.payloadJson) as Record<string, unknown>;
  } catch {
    payload = null;
  }
  const asString = (v: unknown): string => (typeof v === "string" ? v : "");
  switch (row.kind) {
    case "assistant_text":
      return { kind: row.kind, text: asString(payload?.text), isError: false };
    case "tool_call": {
      const name = asString(payload?.name);
      const summary = asString(payload?.summary);
      return { kind: row.kind, text: `${name}(${summary})`, isError: false };
    }
    case "tool_result": {
      const isError = payload?.is_error === true;
      const summary = asString(payload?.summary);
      return { kind: row.kind, text: summary, isError };
    }
    case "state_note":
      return { kind: row.kind, text: `· ${asString(payload?.note)}`, isError: false };
    default:
      return { kind: row.kind, text: row.payloadJson, isError: false };
  }
}
