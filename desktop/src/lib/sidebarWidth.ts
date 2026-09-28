/** Sidebar width persisted to localStorage, in CSS pixels. */
export const DEFAULT_SIDEBAR_WIDTH = 256;
export const MIN_SIDEBAR_WIDTH = 200;
export const MAX_SIDEBAR_WIDTH = 480;
export const SIDEBAR_WIDTH_STORAGE_KEY = "yi-agent.sidebarWidth";

/**
 * Normalize a candidate width: non-finite values (NaN/±Infinity) fall back to the
 * default, everything else is clamped into [MIN, MAX].
 */
export function clampSidebarWidth(px: number): number {
  if (!Number.isFinite(px)) return DEFAULT_SIDEBAR_WIDTH;
  return Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, px));
}

/** Read the persisted width, clamped. Falls back to the default if absent/corrupt/unavailable. */
export function loadSidebarWidth(): number {
  try {
    const raw = localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY);
    if (raw === null) return DEFAULT_SIDEBAR_WIDTH;
    return clampSidebarWidth(parseFloat(raw));
  } catch {
    // localStorage can throw in restricted contexts; never let that break rendering.
    return DEFAULT_SIDEBAR_WIDTH;
  }
}

/** Persist the width (clamped). Storage failures are non-fatal. */
export function saveSidebarWidth(px: number): void {
  try {
    localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, String(clampSidebarWidth(px)));
  } catch {
    // Ignore: losing the persisted width is not worth breaking the drag.
  }
}
