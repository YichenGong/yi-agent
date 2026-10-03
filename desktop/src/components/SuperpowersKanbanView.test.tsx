/** @vitest-environment jsdom */
import { describe, expect, it, afterEach, vi } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import { SuperpowersKanbanView } from "./SuperpowersKanbanView";

afterEach(() => {
  cleanup();
});

describe("SuperpowersKanbanView", () => {
  it("renders a card with its state, progress and detail", () => {
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[
          {
            id: "card-1",
            state: "running",
            progress: "3/7 tasks",
            detail: "kanban/card-1-foo",
          },
        ]}
      />,
    );
    // `card-1` also appears inside the detail string, so match element text
    // exactly rather than as a substring across the row.
    expect(screen.getByText("card-1")).toBeTruthy();
    expect(screen.getByText(/3\/7 tasks/)).toBeTruthy();
    expect(screen.getByText("kanban/card-1-foo")).toBeTruthy();
  });

  it("explains itself instead of looking empty when disabled", () => {
    render(<SuperpowersKanbanView switchOn={false} source="default" cards={[]} />);
    expect(screen.getByText(/disabled/i)).toBeTruthy();
  });

  it("says the board is empty when enabled with no cards", () => {
    render(<SuperpowersKanbanView switchOn source="project" cards={[]} />);
    expect(screen.getByText(/empty/i)).toBeTruthy();
  });

  it("names the missing plugin instead of showing an empty board", () => {
    // The plugin answers every board question, so an unanswered query means it
    // is not installed. That is a different state than "installed, no cards".
    render(
      <SuperpowersKanbanView switchOn source="project" cards={[]} pluginMissing />,
    );
    expect(screen.getByText(/插件未安装/)).toBeTruthy();
    expect(screen.queryByText(/empty/i)).toBeNull();
  });

  it("shows a card's linked thread and opens it on click", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[
          { id: "c1", state: "awaiting_merge", progress: null, detail: "", threadId: "thread-1" },
        ]}
        onOpenThread={onOpenThread}
      />,
    );
    fireEvent.click(screen.getByText("thread-1"));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");
  });

  it("renders no clickable thread link for a card without a thread", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[{ id: "c1", state: "queued", progress: null, detail: "" }]}
        onOpenThread={onOpenThread}
      />,
    );
    // No thread link exists, so nothing can be clicked to open a phantom thread.
    expect(screen.queryByRole("button")).toBeNull();
    expect(onOpenThread).not.toHaveBeenCalled();
  });
});
