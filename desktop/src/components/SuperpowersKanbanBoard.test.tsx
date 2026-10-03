/** @vitest-environment jsdom */
import { describe, expect, it, afterEach, vi } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanBoard } from "./SuperpowersKanbanBoard";

afterEach(() => cleanup());

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return { id, state, progress: null, detail: "", threadId: null, title: id, ...extra };
}

describe("SuperpowersKanbanBoard", () => {
  it("renders the four column headers with counts", () => {
    render(
      <SuperpowersKanbanBoard
        cards={[card("q", "queued"), card("r", "running"), card("n", "needs_you"), card("d", "done")]}
      />,
    );
    expect(screen.getByText(/queued/i)).toBeTruthy();
    expect(screen.getByText(/doing/i)).toBeTruthy();
    expect(screen.getByText(/need decision/i)).toBeTruthy();
    expect(screen.getByText(/done/i)).toBeTruthy();
  });

  it("shows a card's title and marks need-decision cards", () => {
    render(
      <SuperpowersKanbanBoard cards={[card("n1", "needs_you", { title: "fix the thing" })]} />,
    );
    expect(screen.getByText("fix the thing")).toBeTruthy();
    // 该列有强调标记（aria-label 稳定可断言）。
    expect(screen.getByLabelText(/需你处理/)).toBeTruthy();
  });

  it("collapses done to five cards and expands on toggle", () => {
    // 不传 expandedDone（默认 false = 折叠）；完成时刻倒序后，最新的是 d6。
    const done = Array.from({ length: 7 }, (_, i) =>
      card(`d${i}`, "done", { title: `done-${i}`, terminalAt: `2026-10-0${i + 1}T00:00:00+08:00` }),
    );
    const { rerender } = render(
      <SuperpowersKanbanBoard cards={done} onToggleDone={() => {}} />,
    );
    // 折叠只显示最近 5 张：最近的是 d6..d2，最旧的两张 d0/d1 不显示。
    expect(screen.queryByText("done-0")).toBeNull();
    expect(screen.getByText("done-6")).toBeTruthy();
    rerender(<SuperpowersKanbanBoard cards={done} expandedDone onToggleDone={() => {}} />);
    expect(screen.getByText("done-0")).toBeTruthy();
  });

  it("reports the done toggle", () => {
    const onToggleDone = vi.fn();
    // 需要超过 5 张 done 卡，展开开关才出现（不足 5 张无需折叠）。
    const done = Array.from({ length: 6 }, (_, i) => card(`d${i}`, "done", { title: `d${i}` }));
    render(<SuperpowersKanbanBoard cards={done} onToggleDone={onToggleDone} />);
    fireEvent.click(screen.getByRole("button", { name: /展开|收起/ }));
    expect(onToggleDone).toHaveBeenCalled();
  });

  it("opens a card's linked thread and renders none without one", () => {
    const onOpenThread = vi.fn();
    const { rerender } = render(
      <SuperpowersKanbanBoard
        cards={[card("c", "running", { threadId: "thread-1" })]}
        onOpenThread={onOpenThread}
      />,
    );
    fireEvent.click(screen.getByText("thread-1"));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");

    rerender(<SuperpowersKanbanBoard cards={[card("c", "running")]} onOpenThread={onOpenThread} />);
    expect(screen.queryByText("thread-1")).toBeNull();
  });

  it("keeps all four columns visible when empty", () => {
    render(<SuperpowersKanbanBoard cards={[]} />);
    expect(screen.getByText(/queued/i)).toBeTruthy();
    expect(screen.getByText(/done/i)).toBeTruthy();
  });
});
