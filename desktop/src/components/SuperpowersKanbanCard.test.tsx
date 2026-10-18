/** @vitest-environment jsdom */
import { describe, expect, it, afterEach } from "vitest";
import { render, cleanup, screen } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanCard } from "./SuperpowersKanbanCard";

afterEach(() => cleanup());

function card(extra: Partial<BoardCard> = {}): BoardCard {
  return { id: "c1", state: "needs_you", progress: null, detail: "", threadId: null, ...extra };
}

describe("SuperpowersKanbanCard", () => {
  it("合并卡显示 source → base", () => {
    // 合并卡没有 thread_id，卡片是用户唯一的落点：不显示 source→base 就
    // 只剩一串 id + 红色感叹号，看不出它要合什么、卡在哪。
    render(
      <SuperpowersKanbanCard
        card={card({ kind: "merge", source: "kanban/a", base: "main", detail: "kanban/a → main" })}
      />,
    );
    expect(screen.getByText("kanban/a → main")).toBeTruthy();
  });

  it("实现卡不显示合并行", () => {
    render(<SuperpowersKanbanCard card={card({ kind: "implementation", detail: "/work/tree" })} />);
    expect(screen.queryByText("/work/tree")).toBeNull();
  });

  it("合并卡没有会话时不渲染跳转按钮", () => {
    render(<SuperpowersKanbanCard card={card({ kind: "merge", detail: "kanban/a → main" })} />);
    expect(screen.queryByRole("button")).toBeNull();
  });
});

// 宽度自适应：卡片与标题行必须能收缩，标题的 `truncate` 才会生效；长会话链接
// 也要截断，否则它会把卡片（进而整列）撑宽——正是「左右过宽」的成因。
// jsdom 不布局，只能钉住这几处约束类名。
describe("SuperpowersKanbanCard shrinks under long content", () => {
  it("lets the card and its title row shrink", () => {
    const { container } = render(
      <SuperpowersKanbanCard card={card({ title: "x".repeat(200) })} />,
    );
    const li = container.firstElementChild as HTMLElement;
    expect(li.className.split(/\s+/)).toContain("min-w-0");
    const row = li.firstElementChild as HTMLElement;
    expect(row.className.split(/\s+/)).toContain("min-w-0");
  });

  it("truncates a long thread link instead of widening the card", () => {
    render(<SuperpowersKanbanCard card={card({ threadId: "t".repeat(120) })} />);
    const link = screen.getByRole("button");
    const classes = link.className.split(/\s+/);
    expect(classes).toContain("truncate");
    expect(classes).toContain("max-w-full");
  });
});
