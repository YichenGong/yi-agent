import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { BoardCard } from "../lib/cards";
import { BoardView } from "./BoardView";

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

const base = { switchOn: true, source: "project" as const, cards: [] };

describe("BoardView", () => {
  it("renders a card with its title, id and state", () => {
    render(
      <BoardView
        {...base}
        cards={[card("card-1", "running", { title: "do the thing" })]}
      />,
    );
    expect(screen.getByText("do the thing")).toBeTruthy();
    expect(screen.getByText("running")).toBeTruthy();
    expect(screen.getByText("card-1")).toBeTruthy();
  });

  it("says the board is empty when enabled with no cards", () => {
    render(<BoardView {...base} />);
    expect(screen.getByText(/看板是空的/)).toBeTruthy();
  });

  it("explains itself instead of looking empty when the switch is off", () => {
    render(<BoardView {...base} switchOn={false} source="default" />);
    // Scoped to the notice: "停用" also appears in the header control, and the
    // point here is what the body says.
    expect(screen.getByRole("status")).toHaveTextContent(/看板推进已停用/);
    // "Off" and "empty" are different actions for the user, so the off state
    // must not claim the board is empty.
    expect(screen.queryByText(/看板是空的/)).toBeNull();
  });

  it("names the missing backend instead of showing an empty board", () => {
    render(<BoardView {...base} pluginMissing />);
    expect(screen.getByText(/看板后端未就绪/)).toBeTruthy();
    expect(screen.queryByText(/看板是空的/)).toBeNull();
  });

  it("shows the switch state and its source", () => {
    render(<BoardView {...base} source="global" />);
    expect(screen.getByText("运行中")).toBeTruthy();
    expect(screen.getByText("(global)")).toBeTruthy();
  });

  it("reports an error above the board without hiding the cards", () => {
    render(<BoardView {...base} cards={[card("c1", "queued")]} error="后端未就绪" />);
    expect(screen.getByRole("alert")).toHaveTextContent("后端未就绪");
    expect(screen.getByText("QUEUED · 1")).toBeTruthy();
  });

  it("opens a card's linked session", async () => {
    const onOpenThread = vi.fn();
    const user = userEvent.setup();
    render(
      <BoardView
        {...base}
        cards={[card("c1", "awaiting_merge", { threadId: "thread-1" })]}
        onOpenThread={onOpenThread}
      />,
    );
    await user.click(screen.getByRole("button", { name: "thread-1" }));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");
  });

  it("toggles the switch through the header control", async () => {
    const onToggleSwitch = vi.fn();
    const user = userEvent.setup();
    render(<BoardView {...base} onToggleSwitch={onToggleSwitch} />);
    await user.click(screen.getByRole("button", { name: "停用看板推进" }));
    expect(onToggleSwitch).toHaveBeenCalledWith(false);
  });

  it("offers to enable a stopped board the same way", async () => {
    const onToggleSwitch = vi.fn();
    const user = userEvent.setup();
    render(<BoardView {...base} switchOn={false} onToggleSwitch={onToggleSwitch} />);
    await user.click(screen.getByRole("button", { name: "启用看板推进" }));
    expect(onToggleSwitch).toHaveBeenCalledWith(true);
  });

  it("shows the enqueue entry only when delivery is wired up", () => {
    const { rerender } = render(<BoardView {...base} />);
    expect(screen.queryByRole("button", { name: "添加到看板" })).toBeNull();
    rerender(<BoardView {...base} onEnqueue={async () => {}} onPrequeue={async () => {}} />);
    expect(screen.getByRole("button", { name: "添加到看板" })).toBeTruthy();
  });

  it("keeps the done column collapsed until asked to expand", () => {
    // Newest first by terminal time, so the oldest card is the one collapsed.
    const done = Array.from({ length: 6 }, (_, i) =>
      card(`d${i}`, "done", {
        title: `done-${i}`,
        terminalAt: `2026-10-0${i + 1}T00:00:00+08:00`,
      }),
    );
    const { rerender } = render(<BoardView {...base} cards={done} onToggleDone={() => {}} />);
    expect(screen.queryByText("done-0")).toBeNull();
    rerender(<BoardView {...base} cards={done} expandedDone onToggleDone={() => {}} />);
    expect(screen.getByText("done-0")).toBeTruthy();
  });

  it("closes an open menu on Escape", async () => {
    const user = userEvent.setup();
    render(
      <BoardView {...base} onEnqueue={async () => {}} onPrequeue={async () => {}} />,
    );
    await user.click(screen.getByRole("button", { name: "添加到看板" }));
    expect(screen.getByRole("button", { name: "加入看板" })).toBeTruthy();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("button", { name: "加入看板" })).toBeNull();
  });
});
