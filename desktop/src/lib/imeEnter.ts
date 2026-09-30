import { useRef } from "react";

/**
 * The subset of `React.KeyboardEvent` these guards read. Kept structural so the
 * rules stay unit-testable without a DOM.
 */
export type ImeKeyEvent = {
  key: string;
  shiftKey: boolean;
  keyCode: number;
  nativeEvent: { isComposing: boolean };
};

/**
 * True when the keydown was consumed by the input method, i.e. it is NOT a
 * keystroke the app should act on.
 *
 * Three independent signals, because no single one covers every engine:
 *
 * 1. `nativeEvent.isComposing` — the spec'd signal; true in Chromium-based
 *    WebViews while a composition is live.
 * 2. `keyCode === 229` — the legacy "VK_PROCESSKEY" code IE used for
 *    IME-handled keys. WebKit on macOS still forces it for the Enter that
 *    *confirms* a candidate, and that event is rebuilt after the composition is
 *    torn down, so `isComposing` is already false on it. WKWebView — what Tauri
 *    uses on macOS — is exactly this case, so this branch is the load-bearing
 *    one here.
 * 3. `composing` — our own composition bookkeeping, for an engine/IME that
 *    neither sets `isComposing` nor uses keyCode 229.
 */
export function isImeCompositionKey(e: ImeKeyEvent, composing: boolean): boolean {
  return e.nativeEvent.isComposing || e.keyCode === 229 || composing;
}

/** True when this Enter belongs to the IME rather than to the user. */
export function isImeEnter(e: ImeKeyEvent, composing: boolean): boolean {
  return e.key === "Enter" && isImeCompositionKey(e, composing);
}

/**
 * Composition bookkeeping to wire onto an input: `composing` is true from
 * `compositionstart` until `compositionend`. Call `resetComposition` from
 * `onBlur` too — a composition torn down by focus loss may skip
 * `compositionend`, and a latched-on guard would then swallow every later
 * Enter.
 */
export function useImeGuard() {
  const composing = useRef(false);
  return {
    composing,
    onCompositionStart: () => {
      composing.current = true;
    },
    onCompositionEnd: () => {
      composing.current = false;
    },
    resetComposition: () => {
      composing.current = false;
    },
  };
}
