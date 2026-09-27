/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ModeChip } from "./ModeChip";

afterEach(cleanup);

const trigger = () => screen.getByRole("button", { name: /mode/i });
const normalItem = () => screen.getByRole("menuitemradio", { name: /normal/i });
const yoloItem = () => screen.getByRole("menuitemradio", { name: /yolo/i });
const cancelButton = () => screen.getByRole("button", { name: /cancel/i });

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

    expect(normalItem().getAttribute("aria-checked")).toBe("true");
    expect(yoloItem().getAttribute("aria-checked")).toBe("false");
  });

  it("opens a confirmation when selecting YOLO instead of changing immediately", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());

    expect(onChange).not.toHaveBeenCalled();
    const dialog = screen.getByRole("dialog");
    expect(dialog.textContent).toContain("YOLO");
  });

  it("does not call back when the confirmation is cancelled", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());

    fireEvent.click(cancelButton());

    expect(onChange).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("calls back with 'yolo' when the confirmation is accepted", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());
    fireEvent.click(screen.getByRole("button", { name: /confirm/i }));

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
    fireEvent.click(normalItem());

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

  it("closes the confirm dialog on Escape without calling back", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());
    expect(screen.getByRole("dialog")).not.toBeNull();

    fireEvent.keyDown(document, { key: "Escape" });

    expect(onChange).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("does not open the menu when the trigger is disabled", () => {
    const { container } = render(
      <ModeChip mode="normal" onChange={vi.fn()} disabled />,
    );
    fireEvent.click(trigger());

    expect(container.querySelector('[role="menu"]')).toBeNull();
  });

  it("reopens after a cancel and confirms exactly once", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="normal" onChange={onChange} />);

    fireEvent.click(trigger());
    fireEvent.click(yoloItem());
    fireEvent.click(cancelButton());

    fireEvent.click(trigger());
    expect(screen.getByRole("menu")).not.toBeNull();
    fireEvent.click(yoloItem());
    fireEvent.click(screen.getByRole("button", { name: /confirm/i }));

    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith("yolo");
  });

  it("does nothing when YOLO is selected while already in yolo mode", () => {
    const onChange = vi.fn();
    render(<ModeChip mode="yolo" onChange={onChange} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onChange).not.toHaveBeenCalled();
  });

  it("navigates the menu with arrow keys and confirms via the focused item", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    fireEvent.click(trigger());

    // Focus starts on the currently selected item (Normal).
    expect(document.activeElement).toBe(normalItem());

    fireEvent.keyDown(screen.getByRole("menu"), { key: "ArrowDown" });
    expect(document.activeElement).toBe(yoloItem());

    // Enter/Space activation is native to the button; click mirrors that here.
    fireEvent.click(yoloItem());
    expect(screen.getByRole("dialog")).not.toBeNull();
  });

  it("wraps focus with ArrowUp from the first item", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    fireEvent.click(trigger());
    expect(document.activeElement).toBe(normalItem());

    fireEvent.keyDown(screen.getByRole("menu"), { key: "ArrowUp" });
    expect(document.activeElement).toBe(yoloItem());
  });

  it("moves focus onto the safe Cancel action and back to the trigger", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());

    expect(document.activeElement).toBe(cancelButton());

    fireEvent.click(cancelButton());
    expect(document.activeElement).toBe(trigger());
  });

  it("restores focus to the trigger when Escape dismisses the dialog", () => {
    render(<ModeChip mode="normal" onChange={vi.fn()} />);
    fireEvent.click(trigger());
    fireEvent.click(yoloItem());

    fireEvent.keyDown(document, { key: "Escape" });
    expect(document.activeElement).toBe(trigger());
  });

  it("renders a neutral, disabled trigger when the mode is unknown (null)", () => {
    const { container } = render(
      <ModeChip mode={null} onChange={vi.fn()} disabled />,
    );
    const el = trigger() as HTMLButtonElement;
    expect(el.disabled).toBe(true);
    // The unknown state must never masquerade as a real mode.
    expect(el.textContent).not.toContain("Normal");
    expect(el.textContent).not.toContain("YOLO");
    // …and it must positively advertise the neutral unknown label (the pre-fix
    // code rendered empty text here, so this pins the new behavior).
    expect(el.textContent).toBe("Mode");
    expect(el.getAttribute("aria-label")).toMatch(/unknown/i);

    fireEvent.click(el);
    expect(container.querySelector('[role="menu"]')).toBeNull();
  });
});
