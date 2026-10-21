/** @vitest-environment jsdom */
import { describe, expect, it, afterEach } from "vitest";
import { render, cleanup, screen } from "@testing-library/react";
import { SuperpowersKanbanSettings } from "./SuperpowersKanbanSettings";

afterEach(() => {
  cleanup();
});

/**
 * 这块面板只剩一行只读状态。
 *
 * 「多余控件」的两个判据都写在这里：不能再有开关（创建时已经决定过），也不能
 * 再把值守塞进看板面板（它管的是宿主，不是这个项目）。
 */
describe("SuperpowersKanbanSettings", () => {
  it("shows the switch state and its source", () => {
    render(<SuperpowersKanbanSettings switchOn source="global" />);
    expect(screen.getByText("运行中")).toBeTruthy();
    expect(screen.getByText(/global/)).toBeTruthy();
  });

  it("says the board is stopped and how to bring it back", () => {
    render(<SuperpowersKanbanSettings switchOn={false} source="default" />);
    expect(screen.getByText("已停用")).toBeTruthy();
    // 没有开关可点，所以必须告诉用户正确的动作是移除后重建。
    expect(screen.getByText(/移除后重新创建即可恢复/)).toBeTruthy();
  });

  it("offers no control at all: creating the board already opened it", () => {
    render(<SuperpowersKanbanSettings switchOn source="project" />);
    expect(screen.queryAllByRole("checkbox")).toHaveLength(0);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("no longer carries the host-level watchman control", () => {
    render(<SuperpowersKanbanSettings switchOn source="project" />);
    // 值守已迁到设置页「通用」：它服务所有项目，与选中的看板无关。
    expect(screen.queryByText(/后台值守/)).toBeNull();
    expect(screen.queryByText(/后台持续运行/)).toBeNull();
  });
});
