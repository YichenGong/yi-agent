/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { Popover } from "./Popover";

afterEach(cleanup);

describe("Popover", () => {
  it("renders nothing when closed", () => {
    render(
      <Popover open={false} onClose={() => {}}>
        <span data-testid="menu">menu</span>
      </Popover>,
    );
    expect(screen.queryByTestId("menu")).toBeNull();
    expect(document.querySelector(".fixed")).toBeNull();
  });

  it("renders the default backdrop and the children when open", () => {
    const { container } = render(
      <Popover open onClose={() => {}}>
        <span data-testid="menu">menu</span>
      </Popover>,
    );
    // The default backdrop is the `fixed inset-0 z-10` layer the anchored
    // dropdowns used before the extraction.
    expect(container.firstElementChild?.className).toBe("fixed inset-0 z-10");
    expect(screen.getByTestId("menu")).toBeDefined();
  });

  it("renders no backdrop when backdrop={false}", () => {
    const { container } = render(
      <Popover open onClose={() => {}} backdrop={false}>
        <span data-testid="menu">menu</span>
      </Popover>,
    );
    expect(container.querySelector(".fixed")).toBeNull();
    expect(container.firstElementChild).toBe(screen.getByTestId("menu"));
  });

  it("closes when the backdrop is clicked", () => {
    const onClose = vi.fn();
    const { container } = render(
      <Popover open onClose={onClose}>
        <span>menu</span>
      </Popover>,
    );
    fireEvent.click(container.firstElementChild as Element);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape", () => {
    const onClose = vi.fn();
    render(
      <Popover open onClose={onClose}>
        <span>menu</span>
      </Popover>,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("passes backdropProps to the backdrop div, whose onClick takes over", () => {
    // Ruling A: ThreadSidebar's overlay carries `data-thread-menu-overlay=""`
    // (the swallow-click whitelist and its test selector) and stops propagation
    // before closing, so a caller-supplied onClick must win over `onClose`.
    const onClose = vi.fn();
    const onBackdropClick = vi.fn();
    // `data-*` keys type-check at the call site without a cast.
    const { container } = render(
      <Popover
        open
        onClose={onClose}
        backdropProps={{ "data-thread-menu-overlay": "", onClick: onBackdropClick }}
      >
        <span>menu</span>
      </Popover>,
    );
    const backdrop = container.firstElementChild as HTMLElement;
    expect(backdrop.getAttribute("data-thread-menu-overlay")).toBe("");
    expect(backdrop.className).toBe("fixed inset-0 z-10");
    fireEvent.click(backdrop);
    expect(onBackdropClick).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
  });
});
