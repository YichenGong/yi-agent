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
});
