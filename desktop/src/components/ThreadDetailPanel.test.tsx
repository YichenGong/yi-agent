/** @vitest-environment jsdom */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ThreadDetailPanel } from "./ThreadDetailPanel";
import { PANEL_WIDTH_STORAGE_KEY, DEFAULT_PANEL_WIDTH } from "../lib/panelWidth";

afterEach(cleanup);

// `SubagentTrace` no longer carries its own close button (the panel owns the
// frame and the close action), so its props have no `onClose`.
const traceProps = {
  taskId: "t1",
  row: null,
  children: [],
  rows: [],
  onDrill: () => {},
  onMessage: async () => {},
  onCancel: async () => ({ confirmationToken: "x", taskIds: [], expiresInSecs: 30 }),
  onConfirmCancel: async () => {},
};

const diffProps = {
  diff: null,
  loading: false,
  error: null,
  activeCommit: null,
  onRefresh: () => {},
  onOpenCommit: () => {},
  onCloseCommit: () => {},
  note: null,
};

const baseProps = {
  onTabChange: () => {},
  onClose: () => {},
  railRows: [],
  selectedTaskId: null,
  onOpenTask: () => {},
  onLeaveTask: () => {},
  traceProps: null,
  diffProps,
  isMobile: false,
};

describe("ThreadDetailPanel", () => {
  it("renders both tabs and switches on click", () => {
    const onTabChange = vi.fn();
    render(<ThreadDetailPanel {...baseProps} tab="subagents" onTabChange={onTabChange} />);
    expect(screen.getByRole("tab", { name: "子 agent" })).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Git Diff" }));
    expect(onTabChange).toHaveBeenCalledWith("diff");
  });

  it("shows the child list on the subagents tab and opens a card", () => {
    const onOpenTask = vi.fn();
    const rows = [
      { taskId: "task-7", objective: "整理日志", state: "running", lastStep: "读文件", finished: false, parentTaskId: null },
    ];
    render(<ThreadDetailPanel {...baseProps} tab="subagents" railRows={rows} onOpenTask={onOpenTask} />);
    fireEvent.click(screen.getByRole("button", { name: /查看子 agent task-7/ }));
    expect(onOpenTask).toHaveBeenCalledWith("task-7");
  });

  it("shows the trace with a way back to the list when a task is selected", () => {
    const onLeaveTask = vi.fn();
    render(
      <ThreadDetailPanel
        {...baseProps}
        tab="subagents"
        selectedTaskId="t1"
        traceProps={traceProps}
        onLeaveTask={onLeaveTask}
      />,
    );
    // 轨迹主体的摘要态在，且不显示卡片列表。
    expect(screen.getByText(/最近步骤/)).toBeTruthy();
    expect(screen.queryByText("暂无子 agent")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /返回子 agent 列表/ }));
    expect(onLeaveTask).toHaveBeenCalled();
  });

  it("shows the diff body on the diff tab", () => {
    render(
      <ThreadDetailPanel {...baseProps} tab="diff" diffProps={{ ...diffProps, error: "boom" }} />,
    );
    expect(screen.getByText("boom")).toBeTruthy();
  });

  it("closes through the panel header", () => {
    const onClose = vi.fn();
    render(<ThreadDetailPanel {...baseProps} tab="subagents" onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "关闭详情" }));
    expect(onClose).toHaveBeenCalled();
  });

  // 右栏宽度可拖拽：手柄在左缘，往左拖变宽。桌面端才有手柄。
  it("resizes by dragging the left-edge handle and persists the width", () => {
    localStorage.removeItem(PANEL_WIDTH_STORAGE_KEY);
    const { container } = render(<ThreadDetailPanel {...baseProps} tab="subagents" />);
    const section = container.querySelector('[aria-label="会话详情"]') as HTMLElement;
    expect(section.style.width).toBe(`${DEFAULT_PANEL_WIDTH}px`);

    const handle = screen.getByRole("separator", { name: "调整会话详情栏宽度" });
    fireEvent.mouseDown(handle, { clientX: 500 });
    fireEvent.mouseMove(document, { clientX: 440 }); // 往左 60px → 变宽 60px
    fireEvent.mouseUp(document);

    expect(section.style.width).toBe(`${DEFAULT_PANEL_WIDTH + 60}px`);
    expect(localStorage.getItem(PANEL_WIDTH_STORAGE_KEY)).toBe(String(DEFAULT_PANEL_WIDTH + 60));
  });

  it("omits the resize handle on mobile", () => {
    render(<ThreadDetailPanel {...baseProps} tab="subagents" isMobile />);
    expect(screen.queryByRole("separator", { name: "调整会话详情栏宽度" })).toBeNull();
  });
});
