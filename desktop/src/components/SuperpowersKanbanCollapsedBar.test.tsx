/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen } from "@testing-library/react";
import { SuperpowersKanbanCollapsedBar } from "./SuperpowersKanbanCollapsedBar";

afterEach(() => {
  cleanup();
});

describe("SuperpowersKanbanCollapsedBar", () => {
  it("names the collapsed board and offers a way back", () => {
    render(<SuperpowersKanbanCollapsedBar board="/proj" onExpand={() => {}} />);
    // 路径要露出来：多个项目各有一块看板，横条得说清收的是哪一块。
    expect(screen.getByText(/\/proj/)).toBeTruthy();
    expect(screen.getByLabelText("展开看板")).toBeTruthy();
  });

  it("reports the expand request once per click", () => {
    const onExpand = vi.fn();
    render(<SuperpowersKanbanCollapsedBar board="/proj" onExpand={onExpand} />);
    fireEvent.click(screen.getByLabelText("展开看板"));
    expect(onExpand).toHaveBeenCalledTimes(1);
  });
});
