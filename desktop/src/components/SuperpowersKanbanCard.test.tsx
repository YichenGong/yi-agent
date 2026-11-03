/** @vitest-environment jsdom */
import { describe, expect, it, afterEach, vi } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanCard } from "./SuperpowersKanbanCard";

afterEach(() => cleanup());

function card(extra: Partial<BoardCard> = {}): BoardCard {
  return { id: "c1", state: "needs_you", progress: null, detail: "", threadId: null, originThreadId: null, ...extra };
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

  it("正在合并的卡有独立角标", () => {
    // 合并是一次会话回合，卡此时仍是 running；没有角标就与"在写代码"没差别。
    render(<SuperpowersKanbanCard card={card({ state: "running", merging: true })} />);
    expect(screen.getByLabelText("正在合并")).toBeTruthy();
  });

  it("没拿到名额的卡不显示合并角标", () => {
    render(<SuperpowersKanbanCard card={card({ state: "merge_queued", merging: false })} />);
    expect(screen.queryByLabelText("正在合并")).toBeNull();
    expect(screen.getByText("merge_queued")).toBeTruthy();
  });

  it("被当前宿主杀掉热会话的卡：明确说未加载，不说已删除", () => {
    // 会话没了但卡还写着 running：看板不改状态，只给人和模型一个可判读的信号。
    render(
      <SuperpowersKanbanCard
        card={card({
          state: "running",
          sessionObservation: "not_loaded",
          observedAt: "2026-10-22T10:00:00+08:00",
        })}
      />,
    );
    expect(screen.getByLabelText("会话未加载")).toBeTruthy();
    expect(screen.getByText(/最后观测到已加载/)).toBeTruthy();
  });

  it("旧插件载荷（没有 session_observation）仍按 stale 显示未加载", () => {
    // 混装：旧 runner 只发 `stale: true`。新 UI 必须继续认这个旧信号——若把
    // 「缺新字段」一律当成"未知"，升级 UI 反而会把已知的未加载降级成不提示。
    render(
      <SuperpowersKanbanCard
        card={card({ state: "running", stale: true, sessionObservation: undefined })}
      />,
    );
    expect(screen.getByLabelText("会话未加载")).toBeTruthy();
  });

  it("活着的运行卡不显示加载角标", () => {
    render(
      <SuperpowersKanbanCard
        card={card({ state: "running", sessionObservation: "loaded", observedAt: "2026-10-22T10:00:00+08:00" })}
      />,
    );
    expect(screen.queryByLabelText("会话未加载")).toBeNull();
    expect(screen.queryByLabelText("会话未观测到")).toBeNull();
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

describe("SuperpowersKanbanCard session link", () => {
  it("shows the discussion session while the card is only pre-queued", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanCard
        card={card({ id: "idea", state: "pre_queued", originThreadId: "thread-abc", threadId: null })}
        onOpenThread={onOpenThread}
      />,
    );
    const button = screen.getByRole("button", { name: "thread-abc" });
    fireEvent.click(button);
    expect(onOpenThread).toHaveBeenCalledWith("thread-abc");
  });

  it("prefers the execution session once the card is running", () => {
    render(
      <SuperpowersKanbanCard
        card={card({ id: "c1", state: "running", threadId: "thread-exec", originThreadId: "thread-abc" })}
      />,
    );
    expect(screen.getByRole("button", { name: "thread-exec" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "thread-abc" })).toBeNull();
  });

  it("renders no session button when neither session is known", () => {
    render(
      <SuperpowersKanbanCard
        card={card({ id: "idea", state: "pre_queued", threadId: null, originThreadId: null })}
      />,
    );
    expect(screen.queryByRole("button", { name: /thread-/ })).toBeNull();
  });
});
