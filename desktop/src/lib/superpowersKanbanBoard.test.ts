import { describe, expect, it } from "vitest";
import type { BoardCard } from "./superpowersKanbanState";
import {
  DONE_COLLAPSED_LIMIT,
  boardColumns,
  collapseDone,
  columnForState,
} from "./superpowersKanbanBoard";

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return { id, state, progress: null, detail: "", threadId: null, ...extra };
}

describe("columnForState", () => {
  it("maps every known state to its column", () => {
    expect(columnForState("queued")).toBe("queued");
    expect(columnForState("launching")).toBe("queued");
    expect(columnForState("paused")).toBe("queued");
    expect(columnForState("running")).toBe("doing");
    expect(columnForState("merging")).toBe("doing");
    expect(columnForState("needs_you")).toBe("needDecision");
    expect(columnForState("awaiting_merge")).toBe("needDecision");
    expect(columnForState("done")).toBe("done");
    expect(columnForState("failed")).toBe("done");
    expect(columnForState("cancelled")).toBe("done");
  });

  it("sends an unknown state to doing, never to done", () => {
    expect(columnForState("brand_new_state")).toBe("doing");
    expect(columnForState("")).toBe("doing");
  });
});

describe("boardColumns", () => {
  it("splits cards into the four columns", () => {
    const cols = boardColumns([
      card("q", "queued"),
      card("r", "running"),
      card("n", "needs_you"),
      card("d", "done"),
    ]);
    expect(cols.queued.map((c) => c.id)).toEqual(["q"]);
    expect(cols.doing.map((c) => c.id)).toEqual(["r"]);
    expect(cols.needDecision.map((c) => c.id)).toEqual(["n"]);
    expect(cols.done.map((c) => c.id)).toEqual(["d"]);
  });

  it("orders queued by FIFO order ascending", () => {
    const cols = boardColumns([
      card("b", "queued", { order: 5, enqueuedAt: "2026-10-01T00:00:00+08:00" }),
      card("a", "queued", { order: 1, enqueuedAt: "2026-10-02T00:00:00+08:00" }),
    ]);
    expect(cols.queued.map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("orders doing by enqueued_at ascending", () => {
    const cols = boardColumns([
      card("late", "running", { enqueuedAt: "2026-10-02T00:00:00+08:00" }),
      card("early", "running", { enqueuedAt: "2026-10-01T00:00:00+08:00" }),
    ]);
    expect(cols.doing.map((c) => c.id)).toEqual(["early", "late"]);
  });

  it("pins needs_you above awaiting_merge in need decision", () => {
    const cols = boardColumns([
      card("merge", "awaiting_merge", { enqueuedAt: "2026-10-01T00:00:00+08:00" }),
      card("you", "needs_you", { enqueuedAt: "2026-10-05T00:00:00+08:00" }),
    ]);
    expect(cols.needDecision.map((c) => c.id)).toEqual(["you", "merge"]);
  });

  it("orders done by terminal_at descending, falling back to enqueued_at", () => {
    const cols = boardColumns([
      card("old", "done", { terminalAt: "2026-10-01T00:00:00+08:00" }),
      card("new", "done", { terminalAt: "2026-10-03T00:00:00+08:00" }),
      card("no-terminal", "failed", { enqueuedAt: "2026-10-02T00:00:00+08:00" }),
    ]);
    expect(cols.done.map((c) => c.id)).toEqual(["new", "no-terminal", "old"]);
  });

  it("keeps input order when timestamps are absent", () => {
    const cols = boardColumns([card("first", "running"), card("second", "running")]);
    expect(cols.doing.map((c) => c.id)).toEqual(["first", "second"]);
  });

  it("sorts done cards with no timestamp at all to the end", () => {
    const cols = boardColumns([
      card("untimed", "cancelled"),
      card("old", "done", { terminalAt: "2026-10-01T00:00:00+08:00" }),
      card("new", "failed", { terminalAt: "2026-10-03T00:00:00+08:00" }),
    ]);
    expect(cols.done.map((c) => c.id)).toEqual(["new", "old", "untimed"]);
  });
});

describe("collapseDone", () => {
  const many = Array.from({ length: 8 }, (_, i) => card(`c${i}`, "done"));

  it("shows only the first N when collapsed", () => {
    expect(collapseDone(many, false)).toHaveLength(DONE_COLLAPSED_LIMIT);
    expect(collapseDone(many, false).map((c) => c.id)).toEqual([
      "c0", "c1", "c2", "c3", "c4",
    ]);
  });

  it("shows all when expanded", () => {
    expect(collapseDone(many, true)).toHaveLength(8);
  });

  it("shows all when there are fewer than the limit", () => {
    expect(collapseDone(many.slice(0, 2), false)).toHaveLength(2);
  });
});
