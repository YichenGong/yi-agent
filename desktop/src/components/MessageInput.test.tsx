/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import type { ComponentProps } from "react";
import { MessageInput } from "./MessageInput";

afterEach(cleanup);

function renderInput(overrides: Partial<ComponentProps<typeof MessageInput>> = {}) {
  const props: ComponentProps<typeof MessageInput> = {
    turnActive: false,
    onSend: vi.fn(async () => true),
    onInterrupt: vi.fn(),
    mode: "normal",
    onModeChange: vi.fn(),
    ...overrides,
  };
  return { ...render(<MessageInput {...props} />), props };
}

const modeTrigger = () => screen.getByRole("button", { name: /mode/i });

describe("MessageInput", () => {
  it("renders the ModeChip reflecting the current mode", () => {
    renderInput({ mode: "yolo" });
    expect(modeTrigger().textContent).toContain("YOLO");
  });

  it("forwards YOLO (after confirm) and Normal (immediately) to onModeChange", () => {
    const enable = vi.fn();
    const { unmount } = renderInput({ mode: "normal", onModeChange: enable });
    fireEvent.click(modeTrigger());
    fireEvent.click(screen.getByRole("menuitemradio", { name: /yolo/i }));
    fireEvent.click(screen.getByRole("button", { name: /confirm/i }));
    expect(enable).toHaveBeenCalledWith("yolo");
    unmount();

    const disable = vi.fn();
    renderInput({ mode: "yolo", onModeChange: disable });
    fireEvent.click(modeTrigger());
    fireEvent.click(screen.getByRole("menuitemradio", { name: /normal/i }));
    expect(disable).toHaveBeenCalledWith("normal");
  });

  it("sends the typed text when the Send button is clicked", async () => {
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });
    fireEvent.click(screen.getByRole("button", { name: /send/i }));

    expect(onSend).toHaveBeenCalledWith("hello");
  });

  it("sends on Enter but not on Shift+Enter", () => {
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hi" } });

    fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true });
    expect(onSend).not.toHaveBeenCalled();

    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hi");
  });
});
