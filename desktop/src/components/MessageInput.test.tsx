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

  it("disables the chip when the mode is unknown (null)", () => {
    renderInput({ mode: null });
    expect((modeTrigger() as HTMLButtonElement).disabled).toBe(true);
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

  it("does not send when Enter confirms an IME candidate (keyCode 229)", () => {
    // macOS WKWebView (what Tauri uses) rebuilds the committing keydown after the
    // composition is torn down: isComposing is false, keyCode is forced to 229.
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hello" } });

    fireEvent.keyDown(textarea, { key: "Enter", keyCode: 229 });
    expect(onSend).not.toHaveBeenCalled();

    // The next Enter is the user's: it sends.
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hello");
  });

  it("does not send while a composition is live", () => {
    const onSend = vi.fn(async () => true);
    const onInterrupt = vi.fn();
    renderInput({ onSend, onInterrupt });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "nihao" } });

    fireEvent.compositionStart(textarea);
    fireEvent.keyDown(textarea, { key: "Enter", isComposing: true });
    expect(onSend).not.toHaveBeenCalled();
    expect(onInterrupt).not.toHaveBeenCalled();

    // A stray Enter before compositionend must still not act on the half-typed
    // composition, even if the engine forgets isComposing/keyCode.
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).not.toHaveBeenCalled();

    fireEvent.compositionEnd(textarea);
    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("nihao");
  });

  it("forgets a composition torn down without compositionend", () => {
    const onSend = vi.fn(async () => true);
    renderInput({ onSend });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "hi" } });
    fireEvent.compositionStart(textarea);
    fireEvent.blur(textarea);
    fireEvent.focus(textarea);

    fireEvent.keyDown(textarea, { key: "Enter" });
    expect(onSend).toHaveBeenCalledWith("hi");
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
