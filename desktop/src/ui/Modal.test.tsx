/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { useRef } from "react";
import { Modal } from "./Modal";

afterEach(cleanup);

function Harness({ open, onClose }: { open: boolean; onClose: () => void }) {
  const trigger = useRef<HTMLButtonElement>(null);
  return (
    <>
      <button ref={trigger} type="button">
        open
      </button>
      <Modal open={open} onClose={onClose} restoreFocusTo={trigger} labelledBy="h">
        <h2 id="h">title</h2>
      </Modal>
    </>
  );
}

describe("Modal", () => {
  it("renders a dialog with the given label when open", () => {
    render(<Harness open onClose={() => {}} />);
    expect(screen.getByRole("dialog")).toBeDefined();
    expect(screen.getByRole("dialog").getAttribute("aria-modal")).toBe("true");
  });

  it("renders nothing when closed", () => {
    render(<Harness open={false} onClose={() => {}} />);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on Escape", () => {
    const onClose = vi.fn();
    render(<Harness open onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("restores focus to the trigger once closed", () => {
    const { rerender } = render(<Harness open onClose={() => {}} />);
    const trigger = screen.getByRole("button", { name: "open" });
    rerender(<Harness open={false} onClose={() => {}} />);
    expect(document.activeElement).toBe(trigger);
  });
});
