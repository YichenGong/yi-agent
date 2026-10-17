import { createPersistedWidth } from "./persistedWidth";

/** Sidebar width persisted to localStorage, in CSS pixels. */
export const DEFAULT_SIDEBAR_WIDTH = 256;
export const MIN_SIDEBAR_WIDTH = 200;
export const MAX_SIDEBAR_WIDTH = 480;
export const SIDEBAR_WIDTH_STORAGE_KEY = "yi-agent.sidebarWidth";

const width = createPersistedWidth({
  storageKey: SIDEBAR_WIDTH_STORAGE_KEY,
  defaultPx: DEFAULT_SIDEBAR_WIDTH,
  minPx: MIN_SIDEBAR_WIDTH,
  maxPx: MAX_SIDEBAR_WIDTH,
});

/**
 * Normalize a candidate width: non-finite values (NaN/±Infinity) fall back to the
 * default, everything else is clamped into [MIN, MAX].
 */
export const clampSidebarWidth = width.clamp;

/** Read the persisted width, clamped. Falls back to the default if absent/corrupt/unavailable. */
export const loadSidebarWidth = width.load;

/** Persist the width (clamped). Storage failures are non-fatal. */
export const saveSidebarWidth = width.save;
