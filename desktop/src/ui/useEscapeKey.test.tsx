/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook } from "@testing-library/react";
import { fireEvent } from "@testing-library/react";
import { useEscapeKey } from "./useEscapeKey";

afterEach(() => vi.restoreAllMocks());

describe("useEscapeKey", () => {
  it("calls onEscape when active", () => {
    const onEscape = vi.fn();
    renderHook(() => useEscapeKey(true, onEscape));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onEscape).toHaveBeenCalledTimes(1);
  });

  it("ignores Escape when inactive", () => {
    const onEscape = vi.fn();
    renderHook(() => useEscapeKey(false, onEscape));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onEscape).not.toHaveBeenCalled();
  });

  it("ignores other keys", () => {
    const onEscape = vi.fn();
    renderHook(() => useEscapeKey(true, onEscape));
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onEscape).not.toHaveBeenCalled();
  });
});
