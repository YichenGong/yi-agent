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

/** True when the UI runs on the paired phone client. */
export function useIsMobile(): boolean {
  const [mobile] = useState<boolean>(() => detectMobile(isRemoteClient(), isIos()));
  useEffect(() => {
    const root = document.documentElement;
    root.dataset.mobile = mobile ? "true" : "false";
    return () => {
      delete root.dataset.mobile;
    };
  }, [mobile]);
  return mobile;
}
