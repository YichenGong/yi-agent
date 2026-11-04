import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { BoardCard } from "../lib/cards";
import { Board } from "./Board";

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return {
    id,
    state,
    progress: null,
    detail: "",
    threadId: null,
    originThreadId: null,
    title: id,
    ...extra,
  };
}

describe("Board", () => {
  it("renders the five column headers with counts", () => {
    render(
      <Board
        cards={[
          card("q", "queued"),
          card("r", "running"),
          card("n", "needs_you"),
          card("d", "done"),
        ]}
      />,
    );
    expect(screen.getByText("PRE-QUEUED · 0")).toBeTruthy();
    expect(screen.getByText("QUEUED · 1")).toBeTruthy();
    expect(screen.getByText("DOING · 1")).toBeTruthy();
    expect(screen.getByText("NEED DECISION · 1")).toBeTruthy();
    expect(screen.getByText("DONE · 1")).toBeTruthy();
  });

  it("keeps all five columns visible when empty", () => {
    render(<Board cards={[]} />);
    // Exact header text: `/queued/i` would match both PRE-QUEUED and QUEUED.
    expect(screen.getByText("PRE-QUEUED · 0")).toBeTruthy();
    expect(screen.getByText("QUEUED · 0")).toBeTruthy();
    expect(screen.getByText("DOING · 0")).toBeTruthy();
    expect(screen.getByText("NEED DECISION · 0")).toBeTruthy();
    expect(screen.getByText("DONE · 0")).toBeTruthy();
  });

  it("uses an urgent style for the need-decision column", () => {
    const { container } = render(<Board cards={[card("n", "needs_you")]} />);
    const column = container.querySelector('[aria-label="看板列 NEED DECISION"]');
    expect(column?.className).toContain("kanban-column-urgent");
  });

  it("collapses done to five cards and expands on toggle", () => {
    // Newest first by terminal time, so of seven the two oldest are hidden.
    const done = Array.from({ length: 7 }, (_, i) =>
      card(`d${i}`, "done", {
        title: `done-${i}`,
        terminalAt: `2026-10-0${i + 1}T00:00:00+08:00`,
      }),
    );
    const { rerender } = render(<Board cards={done} onToggleDone={() => {}} />);
    expect(screen.queryByText("done-0")).toBeNull();
    expect(screen.getByText("done-6")).toBeTruthy();
    rerender(<Board cards={done} expandedDone onToggleDone={() => {}} />);
    expect(screen.getByText("done-0")).toBeTruthy();
  });

  it("only offers the expand control when something is hidden", () => {
    const { rerender } = render(
      <Board cards={Array.from({ length: 3 }, (_, i) => card(`d${i}`, "done"))} />,
    );
    expect(screen.queryByRole("button", { name: /展开|收起/ })).toBeNull();
    rerender(<Board cards={Array.from({ length: 6 }, (_, i) => card(`d${i}`, "done"))} />);
    expect(screen.getByRole("button", { name: /展开|收起/ })).toBeTruthy();
  });

  it("reports the done toggle", async () => {
    const onToggleDone = vi.fn();
    const user = userEvent.setup();
    render(
      <Board
        cards={Array.from({ length: 6 }, (_, i) => card(`d${i}`, "done"))}
        onToggleDone={onToggleDone}
      />,
    );
    await user.click(screen.getByRole("button", { name: /展开|收起/ }));
    expect(onToggleDone).toHaveBeenCalled();
  });

  it("routes a card's session click to the handler", async () => {
    const onOpenThread = vi.fn();
    const user = userEvent.setup();
    render(
      <Board cards={[card("c", "running", { threadId: "thread-1" })]} onOpenThread={onOpenThread} />,
    );
    await user.click(screen.getByText("thread-1"));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");
  });
});

// Width adaptation. jsdom does no layout, so these pin the two causes of a
// horizontally overflowing board: a row that scrolls sideways, and columns that
// refuse to shrink. Either one alone brings the bug back.
describe("Board width adapts to its container", () => {
  it("never declares a horizontal scroller on the board row", () => {
    const { container } = render(<Board cards={[card("q", "queued")]} />);
    const row = container.firstElementChild as HTMLElement;
    expect(row.className).toContain("kanban-board");
  });

  it("stacks the columns on a narrow screen and lays them out in a row when wide", () => {
    // jsdom does no layout, so the media-query behaviour is asserted against the
    // stylesheet text: the stacking rule must exist, and it must be the narrow
    // branch — a rule that only applies when wide would leave phones scrolling
    // sideways, which is the bug this guards.
    // Vitest runs with the ui package as its root, so the stylesheet is at a
    // stable relative path from the working directory.
    const css = readFileSync(resolve(process.cwd(), "src/styles.css"), "utf8");
    expect(css).toMatch(/\.kanban-board\s*\{[^}]*display:\s*flex/);
    const narrow = css.slice(css.indexOf("@media (max-width: 768px)"));
    expect(narrow).toMatch(/\.kanban-board\s*\{[^}]*flex-direction:\s*column/);
  });

  it("gives every column the same structural class so none can outgrow the row", () => {
    const { container } = render(<Board cards={[card("q", "queued")]} />);
    for (const label of ["PRE-QUEUED", "QUEUED", "DOING", "NEED DECISION", "DONE"]) {
      const column = container.querySelector(`[aria-label="看板列 ${label}"]`);
      expect(column).not.toBeNull();
      expect(column?.className).toContain("kanban-column");
    }
  });
});
