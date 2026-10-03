/** @vitest-environment jsdom */
import { describe, expect, it, afterEach, vi } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanView } from "./SuperpowersKanbanView";

afterEach(() => cleanup());

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return { id, state, progress: null, detail: "", threadId: null, title: id, ...extra };
}

describe("SuperpowersKanbanView", () => {
  it("renders a card with its title, id and state", () => {
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[card("card-1", "running", { title: "do the thing" })]}
      />,
    );
    expect(screen.getByText("do the thing")).toBeTruthy();
    expect(screen.getByText("running")).toBeTruthy();
    expect(screen.getByText("card-1")).toBeTruthy();
  });

  it("explains itself instead of looking empty when disabled", () => {
    render(<SuperpowersKanbanView switchOn={false} source="default" cards={[]} />);
    expect(screen.getByText(/disabled/i)).toBeTruthy();
  });

  it("says the board is empty when enabled with no cards", () => {
    render(<SuperpowersKanbanView switchOn source="project" cards={[]} />);
    expect(screen.getByText(/empty/i)).toBeTruthy();
  });

  it("names the missing plugin instead of showing an empty board", () => {
    render(<SuperpowersKanbanView switchOn source="project" cards={[]} pluginMissing />);
    expect(screen.getByText(/插件未安装/)).toBeTruthy();
    expect(screen.queryByText(/empty/i)).toBeNull();
  });

  it("opens a card's linked thread on click", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[card("c1", "awaiting_merge", { threadId: "thread-1" })]}
        onOpenThread={onOpenThread}
      />,
    );
    fireEvent.click(screen.getByText("thread-1"));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");
  });

  it("renders no clickable thread link for a card without a thread", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[card("c1", "queued")]}
        onOpenThread={onOpenThread}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
    expect(onOpenThread).not.toHaveBeenCalled();
  });

  it("collapses a long done column and forwards the toggle", () => {
    const onToggleDone = vi.fn();
    // 完成时刻倒序：最新 d6..d0。折叠默认显示最近 5 张（d6..d2）。
    // 标题与 id 取不同文本：卡片同时渲染标题与 id，同文本会让 getByText 命中两处。
    const done = Array.from({ length: 7 }, (_, i) =>
      card(`d${i}`, "done", { title: `done-${i}`, terminalAt: `2026-10-0${i + 1}T00:00:00+08:00` }),
    );
    const { rerender } = render(
      <SuperpowersKanbanView switchOn source="project" cards={done} onToggleDone={onToggleDone} />,
    );
    expect(screen.queryByText("done-0")).toBeNull();
    expect(screen.getByText("done-6")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /展开|收起/ }));
    expect(onToggleDone).toHaveBeenCalled();
    rerender(
      <SuperpowersKanbanView switchOn source="project" cards={done} expandedDone onToggleDone={onToggleDone} />,
    );
    expect(screen.getByText("done-0")).toBeTruthy();
  });
});
