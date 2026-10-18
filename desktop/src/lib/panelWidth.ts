import { createPersistedWidth } from "./persistedWidth";

/** Right rail (会话详情) width persisted to localStorage, in CSS pixels. */
export const DEFAULT_PANEL_WIDTH = 360;
export const MIN_PANEL_WIDTH = 260;
export const MAX_PANEL_WIDTH = 720;
export const PANEL_WIDTH_STORAGE_KEY = "yi-agent.panelWidth";

const width = createPersistedWidth({
  storageKey: PANEL_WIDTH_STORAGE_KEY,
  defaultPx: DEFAULT_PANEL_WIDTH,
  minPx: MIN_PANEL_WIDTH,
  maxPx: MAX_PANEL_WIDTH,
});

/**
 * Normalize a candidate width: non-finite values (NaN/±Infinity) fall back to the
 * default, everything else is clamped into [MIN, MAX].
 */
export const clampPanelWidth = width.clamp;

/** Read the persisted width, clamped. Falls back to the default if absent/corrupt/unavailable. */
export const loadPanelWidth = width.load;

/** Persist the width (clamped). Storage failures are non-fatal. */
export const savePanelWidth = width.save;
