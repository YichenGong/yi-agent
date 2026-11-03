import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Popover } from "./Popover";

describe("Popover", () => {
  it("renders nothing while closed", () => {
    render(
      <Popover open={false} onClose={() => {}}>
        <div>menu</div>
      </Popover>,
    );
    expect(screen.queryByText("menu")).toBeNull();
  });

  it("closes on Escape", async () => {
    const onClose = vi.fn();
    const user = userEvent.setup();
    render(
      <Popover open onClose={onClose}>
        <div>menu</div>
      </Popover>,
    );
    await user.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("does not listen for Escape while closed", async () => {
    const onClose = vi.fn();
    const user = userEvent.setup();
    const { rerender } = render(
      <Popover open onClose={onClose}>
        <div>menu</div>
      </Popover>,
    );
    rerender(
      <Popover open={false} onClose={onClose}>
        <div>menu</div>
      </Popover>,
    );
    await user.keyboard("{Escape}");
    expect(onClose).not.toHaveBeenCalled();
  });

  it("closes when the backdrop is clicked", async () => {
    const onClose = vi.fn();
    const user = userEvent.setup();
    const { container } = render(
      <Popover open onClose={onClose} backdropClassName="my-backdrop">
        <div>menu</div>
      </Popover>,
    );
    const backdrop = container.querySelector(".my-backdrop");
    expect(backdrop).not.toBeNull();
    await user.click(backdrop as Element);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("omits the backdrop when asked, leaving the menu clickable", async () => {
    const onClose = vi.fn();
    const onClick = vi.fn();
    const user = userEvent.setup();
    const { container } = render(
      <Popover open onClose={onClose} backdrop={false} backdropClassName="my-backdrop">
        <button onClick={onClick}>menu item</button>
      </Popover>,
    );
    expect(container.querySelector(".my-backdrop")).toBeNull();
    await user.click(screen.getByRole("button", { name: "menu item" }));
    expect(onClick).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
  });
});
