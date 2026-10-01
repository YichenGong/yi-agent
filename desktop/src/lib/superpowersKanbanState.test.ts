import { describe, expect, it } from "vitest";
import { boardJsonPath, parseBoard } from "./superpowersKanbanState";

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
          { id: "missing-plan", state: "Queued", order: 1 },
          { id: "", plan_path: "e.plan.md", state: "Queued", order: 2 },
        ],
        next_order: 3,
      }),
    );
    expect(cards.map((card) => card.id)).toEqual(["good"]);
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
});
