/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen } from "@testing-library/react";
import { SuperpowersKanbanSettings } from "./SuperpowersKanbanSettings";

afterEach(() => {
  cleanup();
});

describe("SuperpowersKanbanSettings", () => {
  it("shows the switch state and its source", () => {
    render(<SuperpowersKanbanSettings switchOn source="global" onToggle={() => {}} />);
    expect(screen.getByText(/Superpowers 看板/)).toBeTruthy();
    expect(screen.getByText(/global/)).toBeTruthy();
  });

  it("reports the requested value when toggled", () => {
    const onToggle = vi.fn();
    render(<SuperpowersKanbanSettings switchOn={false} source="default" onToggle={onToggle} />);
    fireEvent.click(screen.getByRole("checkbox"));
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  it("renders the collapse affordance only when a handler is supplied", () => {
    const { unmount } = render(
      <SuperpowersKanbanSettings switchOn source="project" onToggle={() => {}} />,
    );
    expect(screen.queryByRole("button", { name: "收起看板" })).toBeNull();
    unmount();

    // Matches SubagentRail's convention: no handler, no button. The host that
    // cannot collapse the panel shouldn't advertise a button that does nothing.
    render(
      <SuperpowersKanbanSettings
        switchOn
        source="project"
        onToggle={() => {}}
        onCollapse={() => {}}
      />,
    );
    expect(screen.getByRole("button", { name: "收起看板" })).toBeTruthy();
  });

  it("reports the collapse request once per click", () => {
    const onCollapse = vi.fn();
    render(
      <SuperpowersKanbanSettings
        switchOn={false}
        source="default"
        onToggle={() => {}}
        onCollapse={onCollapse}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "收起看板" }));
    expect(onCollapse).toHaveBeenCalledTimes(1);
  });
});
