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
    expect(screen.getByText("[running]")).toBeTruthy();
    expect(screen.getByText("[completed]")).toBeTruthy();
    expect(screen.getByText("running tests")).toBeTruthy();
    expect(screen.getByText("已结束")).toBeTruthy();
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
        rows={applyChildrenUpdated([{ taskId: "task-9", state: "running" }])}
        onOpen={vi.fn()}
      />,
    );
    expect(screen.getByText("task-9")).toBeTruthy();
  });
});
