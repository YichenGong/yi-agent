/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { fireEvent, render, cleanup, screen } from "@testing-library/react";
import { SuperpowersKanbanCollapsedStrip } from "./SuperpowersKanbanCollapsedStrip";

afterEach(() => {
  cleanup();
});

describe("SuperpowersKanbanCollapsedStrip", () => {
  it("offers exactly one way back: the expand button", () => {
    render(<SuperpowersKanbanCollapsedStrip onExpand={() => {}} />);
    // The whole point of the strip is that the panel is otherwise gone, so the
    // expand affordance must be present and reachable by its accessible name.
    expect(screen.getByRole("button", { name: "展开看板" })).toBeTruthy();
  });

  it("reports the expand request once per click", () => {
    const onExpand = vi.fn();
    render(<SuperpowersKanbanCollapsedStrip onExpand={onExpand} />);
    fireEvent.click(screen.getByRole("button", { name: "展开看板" }));
    expect(onExpand).toHaveBeenCalledTimes(1);
  });

  it("keeps the aside labelled so the strip is still identifiable", () => {
    render(<SuperpowersKanbanCollapsedStrip onExpand={() => {}} />);
    expect(screen.getByLabelText("Superpowers 看板")).toBeTruthy();
  });
});
