import { describe, expect, it } from "vitest";
import { boardJsonPath, normalizeCard, parseBoard } from "./superpowersKanbanState";

describe("boardJsonPath", () => {
  it("points at board.json inside the state directory", () => {
    expect(boardJsonPath("/proj/.yi-agent")).toBe("/proj/.yi-agent/board.json");
  });
});

describe("parseBoard", () => {
  it("maps cards in queue order", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [
          { id: "b", plan_path: "b.plan.md", state: "Queued", order: 5 },
          { id: "a", plan_path: "a.plan.md", state: "Queued", order: 1 },
        ],
        next_order: 6,
      }),
    );
    expect(cards.map((card) => card.id)).toEqual(["a", "b"]);
  });

  it("lowercases the state the way the Rust side does", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [{ id: "a", plan_path: "a.plan.md", state: "Running", order: 0, workdir: "/w" }],
        next_order: 1,
      }),
    );
    expect(cards[0].state).toBe("running");
    expect(cards[0].detail).toBe("/w");
  });

  it("falls back to the plan path when workdir is missing", () => {
    const cards = parseBoard(
      JSON.stringify({ cards: [{ id: "a", plan_path: "a.plan.md", state: "Queued", order: 0 }] }),
    );
    expect(cards[0].detail).toBe("a.plan.md");
  });

  it("lowercases a mixed-case state", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [{ id: "c", plan_path: "c.plan.md", state: "Awaiting_Merge", order: 0 }],
        next_order: 1,
      }),
    );
    expect(cards[0].state).toBe("awaiting_merge");
  });

  it("returns nothing for corrupt input instead of throwing", () => {
    expect(parseBoard("{ not json")).toEqual([]);
    expect(parseBoard(JSON.stringify({ cards: [] }))).toEqual([]);
  });

  it("returns nothing when the root is not an object", () => {
    // Rust serde also rejects these; neither side may throw.
    expect(parseBoard("null")).toEqual([]);
    expect(parseBoard('"just a string"')).toEqual([]);
    expect(parseBoard(JSON.stringify({ cards: {} }))).toEqual([]);
  });

  it("skips a bad card without dropping the good ones", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [
          { id: "good", plan_path: "g.plan.md", state: "Queued", order: 0 },
          { id: "", plan_path: "e.plan.md", state: "Queued", order: 1 },
          { id: "no-state", plan_path: "n.plan.md", state: "", order: 2 },
        ],
        next_order: 3,
      }),
    );
    expect(cards.map((card) => card.id)).toEqual(["good"]);
  });

  it("keeps a card whose only missing field is plan_path", () => {
    // 合并卡没有 plan_path，因此缺 plan_path 不再是丢弃理由。
    const cards = parseBoard(
      JSON.stringify({
        cards: [
          { id: "good", plan_path: "g.plan.md", state: "Queued", order: 0 },
          { id: "missing-plan", state: "Queued", order: 1 },
          { id: "", plan_path: "e.plan.md", state: "Queued", order: 2 },
        ],
        next_order: 3,
      }),
    );
    expect(cards.map((card) => card.id)).toEqual(["good", "missing-plan"]);
  });

  it("keeps a merge card that has no plan_path and shows its refs", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [
          { id: "m1", state: "queued", kind: "merge", source: "kanban/a", base: "main", order: 0 },
        ],
      }),
    );
    expect(cards).toHaveLength(1);
    expect(cards[0].id).toBe("m1");
    expect(cards[0].detail).toBe("kanban/a → main");
  });

  it("falls back to the plan path when the workdir is an empty string", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [{ id: "c", plan_path: "c.plan.md", state: "Queued", order: 0, workdir: "" }],
        next_order: 1,
      }),
    );
    expect(cards[0].detail).toBe("c.plan.md");
  });

  it("keeps a card's thread id and nulls it when absent or empty", () => {
    const cards = parseBoard(
      JSON.stringify({
        cards: [
          { id: "linked", plan_path: "l.plan.md", state: "Awaiting_Merge", order: 0, thread_id: "t-1" },
          { id: "unlinked", plan_path: "u.plan.md", state: "Queued", order: 1, thread_id: null },
          { id: "blank", plan_path: "b.plan.md", state: "Queued", order: 2, thread_id: "" },
        ],
        next_order: 3,
      }),
    );
    expect(cards.map((card) => card.threadId)).toEqual(["t-1", null, null]);
  });
});

describe("normalizeCard", () => {
  it("derives a title from the spec file name", () => {
    const card = normalizeCard({
      id: "2026-10-03-board-smoke-spec-2026-10-03-board-smoke-plan",
      state: "Awaiting_Merge",
      spec_path: "/p/docs/superpowers/smoke/2026-10-03-board-smoke.spec.md",
      plan_path: "/p/docs/superpowers/smoke/2026-10-03-board-smoke.plan.md",
      thread_id: "thread-1",
      kind: "implementation",
      order: 2,
    });
    expect(card).not.toBeNull();
    expect(card!.state).toBe("awaiting_merge");
    expect(card!.title).toBe("2026-10-03-board-smoke");
    expect(card!.specPath).toBe("/p/docs/superpowers/smoke/2026-10-03-board-smoke.spec.md");
    expect(card!.threadId).toBe("thread-1");
    expect(card!.order).toBe(2);
  });

  it("falls back to the id when there is no spec path", () => {
    const card = normalizeCard({ id: "bare-id", state: "queued" });
    expect(card!.title).toBe("bare-id");
    expect(card!.detail).toBe("");
  });

  it("prefers spec_path over plan_path for detail", () => {
    const card = normalizeCard({
      id: "c", state: "queued", spec_path: "c.spec.md", plan_path: "c.plan.md",
    });
    expect(card!.detail).toBe("c.spec.md");
  });

  it("keeps the merge-card detail fallback (source → base)", () => {
    const card = normalizeCard({
      id: "m", state: "merging", kind: "merge", source: "kanban/a", base: "main",
    });
    expect(card!.detail).toBe("kanban/a → main");
  });

  it("carries terminal_at and enqueued_at when the plugin sends them", () => {
    const card = normalizeCard({
      id: "c", state: "done", enqueued_at: "2026-10-01T00:00:00+08:00",
      terminal_at: "2026-10-03T00:00:00+08:00",
    });
    expect(card!.enqueuedAt).toBe("2026-10-01T00:00:00+08:00");
    expect(card!.terminalAt).toBe("2026-10-03T00:00:00+08:00");
  });

  it("returns null only when id or state is missing", () => {
    expect(normalizeCard({ state: "queued" })).toBeNull();
    expect(normalizeCard({ id: "c" })).toBeNull();
    expect(normalizeCard(null)).toBeNull();
    expect(normalizeCard("nope")).toBeNull();
  });
});
