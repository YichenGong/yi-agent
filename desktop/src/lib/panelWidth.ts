/** Right rail (会话详情) width persisted to localStorage, in CSS pixels. */
export const DEFAULT_PANEL_WIDTH = 360;
export const MIN_PANEL_WIDTH = 260;
export const MAX_PANEL_WIDTH = 720;
export const PANEL_WIDTH_STORAGE_KEY = "yi-agent.panelWidth";

/**
 * Normalize a candidate width: non-finite values (NaN/±Infinity) fall back to the
 * default, everything else is clamped into [MIN, MAX].
 */
export function clampPanelWidth(px: number): number {
  if (!Number.isFinite(px)) return DEFAULT_PANEL_WIDTH;
  return Math.min(MAX_PANEL_WIDTH, Math.max(MIN_PANEL_WIDTH, px));
}

/** Read the persisted width, clamped. Falls back to the default if absent/corrupt/unavailable. */
export function loadPanelWidth(): number {
  try {
    const raw = localStorage.getItem(PANEL_WIDTH_STORAGE_KEY);
    if (raw === null) return DEFAULT_PANEL_WIDTH;
    return clampPanelWidth(parseFloat(raw));
  } catch {
    // localStorage can throw in restricted contexts; never let that break rendering.
    return DEFAULT_PANEL_WIDTH;
  }
}

/** Persist the width (clamped). Storage failures are non-fatal. */
export function savePanelWidth(px: number): void {
  try {
    localStorage.setItem(PANEL_WIDTH_STORAGE_KEY, String(clampPanelWidth(px)));
  } catch {
    // Ignore: losing the persisted width is not worth breaking the drag.
  }
}
