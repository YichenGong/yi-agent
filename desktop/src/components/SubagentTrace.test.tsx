/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen, waitFor } from "@testing-library/react";
import { SubagentTrace } from "./SubagentTrace";
import { toRow } from "../lib/subagents";
import type { AgentTraceRow } from "../lib/protocol";

afterEach(cleanup);

const child = (taskId: string, state: string, lastStep?: string) =>
  toRow({ taskId, state, objective: `objective of ${taskId}`, lastStep, parentTaskId: "root" });

const textRow = (eventId: number, text: string): AgentTraceRow => ({
  eventId,
  taskId: "task-1",
  kind: "assistant_text",
  payloadJson: JSON.stringify({ type: "assistant_text", text }),
});

const baseProps = {
  taskId: "task-1",
  row: child("task-1", "running", "running tests"),
  children: [] as ReturnType<typeof child>[],
  rows: [] as AgentTraceRow[],
  onDrill: () => {},
  onMessage: async () => {},
  onCancel: async () => ({ confirmationToken: "tok", taskIds: ["task-1"], expiresInSecs: 30 }),
  onConfirmCancel: async () => {},
};

describe("SubagentTrace", () => {
  it("opens on the summary level and expands to the full trace on demand", () => {
    render(<SubagentTrace {...baseProps} rows={[textRow(1, "first line")]} />);
    expect(screen.getByText(/最近步骤/)).toBeTruthy();
    expect(screen.queryByText("first line")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /展开轨迹/ }));
    expect(screen.getByText("first line")).toBeTruthy();
  });

  it("appends streamed trace rows without re-rendering finalised ones", () => {
    const toolRow = (eventId: number, summary: string): AgentTraceRow => ({
      eventId,
      taskId: "task-1",
      kind: "tool_call",
      payloadJson: JSON.stringify({ type: "tool_call", name: "bash", summary }),
    });
    const { rerender } = render(
      <SubagentTrace {...baseProps} rows={[textRow(1, "settled"), toolRow(2, "cargo test")]} />,
    );
    fireEvent.click(screen.getByRole("button", { name: /展开轨迹/ }));
    const settledBefore = screen.getByText("settled");

    // A new block arrives: the settled text block keeps its DOM node, so the
    // memo boundary refused to re-render it.
    rerender(
      <SubagentTrace
        {...baseProps}
        rows={[textRow(1, "settled"), toolRow(2, "cargo test"), toolRow(3, "cargo build")]}
      />,
    );
    expect(screen.getByText("bash(cargo build)")).toBeTruthy();
    expect(screen.getByText("settled")).toBe(settledBefore);
  });

  it("merges consecutive assistant text rows into one block", () => {
    render(<SubagentTrace {...baseProps} rows={[textRow(1, "one "), textRow(2, "two")]} />);
    fireEvent.click(screen.getByRole("button", { name: /展开轨迹/ }));
    expect(screen.getByText("one two")).toBeTruthy();
  });

  it("sends a message to the child", async () => {
    const onMessage = vi.fn().mockResolvedValue(undefined);
    render(<SubagentTrace {...baseProps} onMessage={onMessage} />);
    fireEvent.click(screen.getByRole("button", { name: /发消息/ }));
    fireEvent.change(screen.getByLabelText("发给子 agent 的消息"), {
      target: { value: "please stop" },
    });
    fireEvent.click(screen.getByRole("button", { name: /发送/ }));
    await waitFor(() => expect(onMessage).toHaveBeenCalledWith("task-1", "please stop"));
  });

  it("cancels through the confirmation path, never directly", async () => {
    const onCancel = vi
      .fn()
      .mockResolvedValue({ confirmationToken: "tok-9", taskIds: ["task-1"], expiresInSecs: 30 });
    const onConfirmCancel = vi.fn().mockResolvedValue(undefined);
    render(
      <SubagentTrace {...baseProps} onCancel={onCancel} onConfirmCancel={onConfirmCancel} />,
    );

    fireEvent.click(screen.getByRole("button", { name: /取消该任务/ }));
    await waitFor(() => expect(onCancel).toHaveBeenCalledWith("task-1"));
    // The first press only previews; nothing is cancelled yet.
    expect(onConfirmCancel).not.toHaveBeenCalled();
    expect(screen.getByText(/确认取消该子 agent/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /确认取消/ }));
    await waitFor(() => expect(onConfirmCancel).toHaveBeenCalledWith("task-1", "tok-9"));
  });

  it("lets the user abandon a cancel before confirming", async () => {
    const onConfirmCancel = vi.fn();
    render(
      <SubagentTrace
        {...baseProps}
        onCancel={async () => ({
          confirmationToken: "tok",
          taskIds: ["task-1"],
          expiresInSecs: 30,
        })}
        onConfirmCancel={onConfirmCancel}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /取消该任务/ }));
    await waitFor(() => expect(screen.getByText(/确认取消该子 agent/)).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: /放弃/ }));
    expect(screen.queryByText(/确认取消该子 agent/)).toBeNull();
    expect(onConfirmCancel).not.toHaveBeenCalled();
  });

  it("lists the task's own children and drills into one", () => {
    const onDrill = vi.fn();
    render(
      <SubagentTrace
        {...baseProps}
        children={[child("task-child", "running")]}
        onDrill={onDrill}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /进入子任务 task-child/ }));
    expect(onDrill).toHaveBeenCalledWith("task-child");
  });

  it("leaves the frame and the close button to the panel that hosts it", () => {
    // 外框与「关闭」搬到了 ThreadDetailPanel：轨迹要能和 Git Diff 挤进同一排
    // Tab 之下，框架只能由面板统一出。这里守住「本组件不再自带关闭」这一点。
    render(<SubagentTrace {...baseProps} />);
    expect(screen.queryByRole("button", { name: "关闭详情" })).toBeNull();
  });

  it("says the trace is empty instead of rendering nothing", () => {
    render(<SubagentTrace {...baseProps} rows={[]} />);
    fireEvent.click(screen.getByRole("button", { name: /展开轨迹/ }));
    expect(screen.getByText("该任务暂无轨迹")).toBeTruthy();
  });
});
