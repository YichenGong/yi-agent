import { describe, expect, it } from "vitest";
import { boardJsonPath, parseBoard } from "./boardState";

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

  it("falls back to the plan path when there is no workdir", () => {
    const cards = parseBoard(
      JSON.stringify({ cards: [{ id: "a", plan_path: "a.plan.md", state: "Queued", order: 0 }] }),
    );
    expect(cards[0].detail).toBe("a.plan.md");
  });

  it("returns nothing for corrupt input instead of throwing", () => {
    expect(parseBoard("{ not json")).toEqual([]);
    expect(parseBoard(JSON.stringify({ cards: [] }))).toEqual([]);
  });
});
