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
