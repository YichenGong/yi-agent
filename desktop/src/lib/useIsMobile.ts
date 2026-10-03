/**
 * Detect the phone client and drive the mobile layout.
 *
 * The desktop build and the iOS build share one bundle; on a phone the two
 * desktop side columns (the 256px session list and the 288px sub-agent rail)
 * leave the chat almost no width, so the phone treats them as off-canvas
 * drawers instead. This hook answers "is this the phone client" and mirrors the
 * answer onto `<html data-mobile>` so the CSS can switch the layout.
 *
 * The signal is the one `platform.isRemoteClient()` already uses — the persisted
 * relay binding, which the desktop build never writes — plus an iOS
 * user-agent guard so the very first frame already looks like a phone, before
 * storage is consulted. On the desktop this is `false` and the layout is
 * byte-for-byte unchanged.
 */
import { useEffect, useState } from "react";
import { isIos, isRemoteClient } from "./platform";

/**
 * Pure predicate behind {@link useIsMobile}. Takes both signals as arguments so
 * the decision is unit-testable without a DOM, storage, or a real navigator.
 */
export function detectMobile(remote: boolean, onIos: boolean): boolean {
  return remote || onIos;
}

/** The minimum shape `lockZoom` needs, so tests can pass a bare document. */
type ViewportHost = Pick<Document, "querySelector">;

const ZOOM_OFF: ReadonlyArray<[string, string]> = [
  ["maximum-scale", "1"],
  ["user-scalable", "no"],
];

/**
 * Refuse pinch-zoom on the phone client, in place.
 *
 * WKWebView pinch-zooms by default; once magnified the webview no longer fits the
 * screen and every scroll turns into two-axis panning — which is exactly the
 * "after zooming it needs dragging" symptom. No CSS property can forbid user
 * scaling; the viewport meta is the only switch.
 *
 * The directive is *edited*, never rewritten: `width` and `viewport-fit` are
 * load-bearing for the layout (the latter gates every `env(safe-area-inset-*)`),
 * and stale `maximum-scale`/`user-scalable` pairs are dropped first — two rival
 * declarations of the same key resolve by parser whim, not by order.
 *
 * No-op when there is no viewport meta (the desktop build is a Tauri webview
 * whose file:// HTML we do not own).
 */
export function lockZoom(doc: ViewportHost = document): void {
  const meta = doc.querySelector<HTMLMetaElement>('meta[name="viewport"]');
  if (!meta) return;

  const parts = (meta.getAttribute("content") ?? "")
    .split(",")
    .map((part) => part.trim())
    .filter(Boolean)
    .filter((part) => {
      const key = part.split("=")[0].trim().toLowerCase();
      return !ZOOM_OFF.some(([forbidden]) => forbidden === key);
    });

  for (const [key, value] of ZOOM_OFF) parts.push(`${key}=${value}`);
  meta.setAttribute("content", parts.join(", "));
}

/** True when the UI runs on the paired phone client. */
export function useIsMobile(): boolean {
  const [mobile] = useState<boolean>(() => detectMobile(isRemoteClient(), isIos()));
  useEffect(() => {
    const root = document.documentElement;
    root.dataset.mobile = mobile ? "true" : "false";
    if (mobile) lockZoom(document);
    return () => {
      delete root.dataset.mobile;
    };
  }, [mobile]);
  return mobile;
}
