/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ModeChip } from "./ModeChip";

afterEach(cleanup);

const trigger = () => screen.getByRole("button", { name: /mode/i });

describe("ModeChip", () => {
  it("renders a chip whose trigger is reachable by its mode aria-label", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    expect(trigger().textContent).toContain("Normal");
  });

  it("opens a dropdown listing both options and marks the current one selected", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    fireEvent.click(trigger());

    const menu = screen.getByRole("menu");
    expect(menu.textContent).toContain("Normal");
    expect(menu.textContent).toContain("YOLO");

    expect(
      screen.getByRole("menuitemradio", { name: /normal/i }).getAttribute("aria-checked"),
    ).toBe("true");
    expect(
      screen.getByRole("menuitemradio", { name: /yolo/i }).getAttribute("aria-checked"),
    ).toBe("false");
  });

  it("opens a confirmation when selecting YOLO instead of changing immediately", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(screen.getByText("YOLO"));

    expect(onChange).not.toHaveBeenCalled();
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toContain("YOLO");
  });

  it("does not call back when the confirmation is cancelled", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(screen.getByText("YOLO"));

    fireEvent.click(screen.getByRole("button", { name: /cancel|取消/i }));

    expect(onChange).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("calls back with 'yolo' when the confirmation is accepted", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(screen.getByText("YOLO"));
    fireEvent.click(screen.getByRole("button", { name: /confirm|确认/i }));

    expect(onChange).toHaveBeenCalledWith("yolo");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("shows YOLO with red styling when the mode is yolo", () => {
    render(<ModeChip mode="yolo" onChange={vi.fn()} />);
    const el = trigger();
    expect(el.textContent).toContain("YOLO");
    expect(el.className).toContain("bg-red-");
  });

  it("disables YOLO immediately without confirmation", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="yolo" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(screen.getByText("Normal"));

    expect(onChange).toHaveBeenCalledWith("normal");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes an open dropdown on Escape", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    fireEvent.click(trigger());
    expect(screen.getByRole("menu")).not.toBeNull();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
