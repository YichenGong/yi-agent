/** @vitest-environment jsdom */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ThreadDetailPanel } from "./ThreadDetailPanel";

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

describe("ThreadDetailPanel", () => {
  it("renders both tabs and switches on click", () => {
    const onTabChange = vi.fn();
    render(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={onTabChange}
        onClose={() => {}}
        traceProps={traceProps}
        diffProps={diffProps}
        railCollapsed={false}
        onExpandRail={() => {}}
      />,
    );
    expect(screen.getByRole("tab", { name: "轨迹" })).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Git Diff" }));
    expect(onTabChange).toHaveBeenCalledWith("diff");
  });

  it("shows an empty trace state when no subagent is selected", () => {
    render(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={() => {}}
        onClose={() => {}}
        traceProps={null}
        diffProps={diffProps}
        railCollapsed={false}
        onExpandRail={() => {}}
      />,
    );
    expect(screen.getByText(/未选择子 agent/)).toBeTruthy();
  });

  it("shows the diff body on the diff tab", () => {
    render(
      <ThreadDetailPanel
        tab="diff"
        onTabChange={() => {}}
        onClose={() => {}}
        traceProps={null}
        diffProps={{ ...diffProps, error: "boom" }}
        railCollapsed={false}
        onExpandRail={() => {}}
      />,
    );
    expect(screen.getByText("boom")).toBeTruthy();
  });

  it("closes through the panel header", () => {
    const onClose = vi.fn();
    render(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={() => {}}
        onClose={onClose}
        traceProps={null}
        diffProps={diffProps}
        railCollapsed={false}
        onExpandRail={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "关闭详情" }));
    expect(onClose).toHaveBeenCalled();
  });

  // 状态栏那颗图标改管面板开合后，子 agent 栏的「收起」本是单向操作：栏收起后
  // 就再没有控件能把它放回来。面板给出一条退路——只在栏确实收起时出现。
  it("keeps the collapsed rail reversible from its own header", () => {
    const onExpandRail = vi.fn();
    const { rerender } = render(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={() => {}}
        onClose={() => {}}
        traceProps={null}
        diffProps={diffProps}
        railCollapsed={true}
        onExpandRail={onExpandRail}
      />,
    );
    const reopen = screen.getByRole("button", { name: "展开子 agent 栏" });
    fireEvent.click(reopen);
    expect(onExpandRail).toHaveBeenCalled();

    // 栏已展开时不再重复提供这个控件，免得和栏自身的「收起」打架。
    rerender(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={() => {}}
        onClose={() => {}}
        traceProps={null}
        diffProps={diffProps}
        railCollapsed={false}
        onExpandRail={onExpandRail}
      />,
    );
    expect(screen.queryByRole("button", { name: "展开子 agent 栏" })).toBeNull();
  });
});
