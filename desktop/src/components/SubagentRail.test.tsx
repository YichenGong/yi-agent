/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { SubagentRail } from "./SubagentRail";
import { applyChildrenUpdated } from "../lib/subagents";

afterEach(cleanup);

const child = (taskId: string, state: string, lastStep?: string) => ({
  taskId,
  state,
  objective: `objective of ${taskId}`,
  lastStep,
  usage: { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0 },
});

describe("SubagentRail", () => {
  it("renders one card per child with its objective and status", () => {
    render(
      <SubagentRail
        rows={applyChildrenUpdated([child("a", "running", "running tests"), child("b", "completed")])}
        onOpen={vi.fn()}
      />,
    );
    expect(screen.getByText("objective of a")).toBeTruthy();
    expect(screen.getByText("objective of b")).toBeTruthy();
    // 中文状态，而不是内核的英文枚举：侧栏是给人看的。
    expect(screen.getByText("运行中")).toBeTruthy();
    expect(screen.getByText("已完成")).toBeTruthy();
    expect(screen.getByText("running tests")).toBeTruthy();
    // 终态不再展示旧步骤，也不编一个「已结束」：状态本身就是终点。
    expect(screen.queryByText("已结束")).toBeNull();
  });

  it("shows an empty state when the thread has no children", () => {
    render(<SubagentRail rows={[]} onOpen={vi.fn()} />);
    expect(screen.getByText("暂无子 agent")).toBeTruthy();
  });

  it("invokes onOpen with the task id when a card is clicked", () => {
    const onOpen = vi.fn();
    render(<SubagentRail rows={applyChildrenUpdated([child("task-7", "running")])} onOpen={onOpen} />);
    fireEvent.click(screen.getByRole("button", { name: /查看子 agent task-7/ }));
    expect(onOpen).toHaveBeenCalledWith("task-7");
  });

  it("offers a collapse control when the caller wants one", () => {
    const onCollapse = vi.fn();
    render(<SubagentRail rows={[]} onOpen={vi.fn()} onCollapse={onCollapse} />);
    fireEvent.click(screen.getByRole("button", { name: /收起子 agent/ }));
    expect(onCollapse).toHaveBeenCalled();
  });

  it("marks the card the user has opened", () => {
    render(
      <SubagentRail
        rows={applyChildrenUpdated([child("task-1", "running"), child("task-2", "running")])}
        onOpen={vi.fn()}
        selectedTaskId="task-2"
      />,
    );
    const opened = screen.getByRole("button", { name: /查看子 agent task-2/ });
    expect(opened.getAttribute("aria-current")).toBe("true");
    const other = screen.getByRole("button", { name: /查看子 agent task-1/ });
    expect(other.getAttribute("aria-current")).toBeNull();
  });

  it("falls back to the task id when a child has no objective", () => {
    render(
      <SubagentRail
        rows={applyChildrenUpdated([
          { taskId: "task-9", state: "running", usage: { inputTokens: 0, outputTokens: 0, cacheReadTokens: 0 } },
        ])}
        onOpen={vi.fn()}
      />,
    );
    expect(screen.getByText("task-9")).toBeTruthy();
  });

  it("labels every state through the shared table, not a local switch", () => {
    const states: Array<[string, string]> = [
      ["queued", "排队中"],
      ["running", "运行中"],
      ["waiting_for_permission", "等待授权"],
      ["waiting_for_children", "等待子任务"],
      ["paused", "已暂停"],
      ["completed", "已完成"],
      ["cancelled", "已取消"],
      ["blocked", "已阻塞"],
      ["budget_exhausted", "预算耗尽"],
      ["recovery_required", "需要恢复"],
    ];
    render(
      <SubagentRail
        rows={applyChildrenUpdated(states.map(([state], i) => child(`t${i}`, state)))}
        onOpen={vi.fn()}
      />,
    );
    for (const [, label] of states) {
      expect(screen.getByText(label)).toBeTruthy();
    }
    // 原始英文枚举不该出现在界面上。
    for (const [state] of states) {
      expect(screen.queryByText(`[${state}]`)).toBeNull();
    }
  });

  it("shows a paused task as paused rather than working", () => {
    render(
      <SubagentRail
        rows={applyChildrenUpdated([child("p", "paused", "running tests")])}
        onOpen={vi.fn()}
      />,
    );
    expect(screen.getByText("已暂停")).toBeTruthy();
    // 暂停时旧步骤会让人以为还在跑；状态已经说明一切。
    expect(screen.queryByText("running tests")).toBeNull();
  });

  it("keeps the last step for a live task", () => {
    render(
      <SubagentRail rows={applyChildrenUpdated([child("a", "running", "editing src/app.ts")])} onOpen={vi.fn()} />,
    );
    expect(screen.getByText("editing src/app.ts")).toBeTruthy();
  });

  it("falls back to the task id for an unknown state without inventing a label", () => {
    render(<SubagentRail rows={applyChildrenUpdated([child("weird", "brand_new_state")])} onOpen={vi.fn()} />);
    expect(screen.getByText("brand_new_state")).toBeTruthy();
  });

  it("counts only the states that are actually in progress", () => {
    render(
      <SubagentRail
        rows={applyChildrenUpdated([
          child("a", "running"),
          child("b", "waiting_for_permission"),
          child("c", "paused"),
          child("d", "completed"),
          child("e", "cancelled"),
        ])}
        onOpen={vi.fn()}
      />,
    );
    expect(screen.getByText("子 agent (2 进行中 / 5)")).toBeTruthy();
  });

  it("omits the progress count when nothing is running", () => {
    render(
      <SubagentRail
        rows={applyChildrenUpdated([child("a", "completed"), child("b", "failed")])}
        onOpen={vi.fn()}
      />,
    );
    expect(screen.getByText("子 agent (2)")).toBeTruthy();
  });

  it("exposes a heading and per-row refs so focus can come back", () => {
    const heading = { current: null as HTMLHeadingElement | null };
    const nodes = new Map<string, HTMLButtonElement | null>();
    render(
      <SubagentRail
        rows={applyChildrenUpdated([child("task-3", "running")])}
        onOpen={vi.fn()}
        headingRef={heading}
        rowRef={(taskId, node) => {
          nodes.set(taskId, node);
        }}
      />,
    );

    expect(heading.current?.textContent).toContain("子 agent");
    expect(nodes.get("task-3")?.getAttribute("aria-label")).toContain("task-3");
    expect(nodes.get("task-3")?.tabIndex).toBe(0);
  });

  it("truncates a long objective to two lines instead of pushing the row apart", () => {
    const long = "x".repeat(400);
    render(
      <SubagentRail
        rows={applyChildrenUpdated([{ ...child("long", "running"), objective: long }])}
        onOpen={vi.fn()}
      />,
    );
    const text = screen.getByText(long);
    expect(text.className).toContain("line-clamp-2");
  });
});
