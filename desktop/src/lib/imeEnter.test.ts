import { describe, it, expect } from "vitest";
import { isImeCompositionKey, isImeEnter, type ImeKeyEvent } from "./imeEnter";

/** Minimal structural stand-in for a React.KeyboardEvent keydown. */
function keyEvent(overrides: Partial<ImeKeyEvent> = {}): ImeKeyEvent {
  return {
    key: "Enter",
    shiftKey: false,
    keyCode: 13,
    nativeEvent: { isComposing: false },
    ...overrides,
  };
}

describe("isImeCompositionKey", () => {
  it("flags the keydown the IME consumed while a composition is live", () => {
    expect(isImeCompositionKey(keyEvent({ nativeEvent: { isComposing: true } }), false)).toBe(true);
  });

  it("flags the WKWebView commit Enter by its keyCode 229", () => {
    // macOS WebKit rebuilds the keydown after the IME already confirmed the
    // marked text, so isComposing is false and keyCode is forced to 229.
    expect(isImeCompositionKey(keyEvent({ keyCode: 229 }), false)).toBe(true);
  });

  it("flags any keydown that arrives while we still hold a composition", () => {
    expect(isImeCompositionKey(keyEvent(), true)).toBe(true);
  });

  it("leaves a real Enter alone", () => {
    expect(isImeCompositionKey(keyEvent(), false)).toBe(false);
  });
});

describe("isImeEnter", () => {
  it("is true for the IME commit Enter", () => {
    expect(isImeEnter(keyEvent({ keyCode: 229 }), false)).toBe(true);
  });

  it("is false for Enter pressed with no composition", () => {
    expect(isImeEnter(keyEvent(), false)).toBe(false);
  });

  it("is false for non-Enter keys even during a composition", () => {
    expect(isImeEnter(keyEvent({ key: "a", nativeEvent: { isComposing: true } }), true)).toBe(false);
  });
});
