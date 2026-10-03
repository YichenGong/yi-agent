/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen } from "@testing-library/react";
import { SuperpowersKanbanSettings } from "./SuperpowersKanbanSettings";

afterEach(() => {
  cleanup();
});

// `@testing-library/jest-dom` is not a dependency of this workspace, so the
// `toBeChecked()` matcher is unavailable. Reading `.checked` keeps the same
// precision without pulling in a new package.
function checkbox(accessibleName: string): HTMLInputElement {
  const box = screen.getByLabelText(accessibleName);
  expect(box).toBeInstanceOf(HTMLInputElement);
  return box as HTMLInputElement;
}

describe("SuperpowersKanbanSettings", () => {
  it("shows the switch state and its source", () => {
    render(
      <SuperpowersKanbanSettings
        switchOn
        source="global"
        onToggle={() => {}}
        watchmanEnabled
        onToggleWatchman={() => {}}
      />,
    );
    expect(screen.getByText(/Superpowers 看板/)).toBeTruthy();
    expect(screen.getByText(/global/)).toBeTruthy();
  });

  it("reports the requested value when toggled", () => {
    const onToggle = vi.fn();
    render(
      <SuperpowersKanbanSettings
        switchOn={false}
        source="default"
        onToggle={onToggle}
        watchmanEnabled
        onToggleWatchman={() => {}}
      />,
    );
    // Two checkboxes share this panel now; ask for the board one by name so the
    // assertion can't silently land on the watchman row.
    fireEvent.click(screen.getByLabelText("Superpowers 看板开关"));
    expect(onToggle).toHaveBeenCalledWith(true);
  });

  it("toggles the background watchman and explains it", () => {
    const onToggleWatchman = vi.fn();
    render(
      <SuperpowersKanbanSettings
        switchOn
        source="project"
        onToggle={() => {}}
        watchmanEnabled
        onToggleWatchman={onToggleWatchman}
      />,
    );
    const box = checkbox("后台值守（开机自启）开关");
    expect(box.checked).toBe(true);
    fireEvent.click(box);
    expect(onToggleWatchman).toHaveBeenCalledWith(false);
    expect(screen.getByText(/后台持续运行/)).toBeTruthy();
  });

  it("surfaces the host's watchman warning when the write reports one", () => {
    render(
      <SuperpowersKanbanSettings
        switchOn
        source="project"
        onToggle={() => {}}
        watchmanEnabled={false}
        onToggleWatchman={() => {}}
        watchmanWarning="未能在登录项中安装值守"
      />,
    );
    expect(screen.getByText("未能在登录项中安装值守")).toBeTruthy();
  });

  it("renders the collapse affordance only when a handler is supplied", () => {
    const { unmount } = render(
      <SuperpowersKanbanSettings
        switchOn
        source="project"
        onToggle={() => {}}
        watchmanEnabled
        onToggleWatchman={() => {}}
      />,
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
        watchmanEnabled
        onToggleWatchman={() => {}}
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
        watchmanEnabled
        onToggleWatchman={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "收起看板" }));
    expect(onCollapse).toHaveBeenCalledTimes(1);
  });
});
