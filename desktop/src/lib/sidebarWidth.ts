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

/**
 * 侧栏当前宽度的实时广播。
 *
 * 为什么不是每次都 `loadSidebarWidth()` 读一遍：宽度在侧栏里是**拖动中的本地
 * state**，只在松手时才落盘。旁支的返回条要跟「+ New thread」一样宽，就必须
 * 跟着拖动实时变；读 localStorage 只能拿到上一次松手时的值，拖到一半两边就
 * 对不齐了。
 *
 * 因此侧栏在 `setWidth` 的同时广播，返回条订阅。状态留在模块里而不是 React
 * 树上：侧栏与旁支视图是两条不相关的分支，为这一个数字把 App 整棵子树重渲染
 * 不划算。
 */
let liveSidebarWidth = DEFAULT_SIDEBAR_WIDTH;
const widthListeners = new Set<() => void>();

/** 侧栏宽度变化时广播；由 `ThreadSidebar` 在拖动与初始化时调用。 */
export function publishSidebarWidth(px: number): void {
  const next = clampSidebarWidth(px);
  if (next === liveSidebarWidth) return;
  liveSidebarWidth = next;
  for (const listener of [...widthListeners]) listener();
}

/** 供 `useSyncExternalStore` 读取的当前宽度。 */
export function currentSidebarWidth(): number {
  return liveSidebarWidth;
}

/** 订阅宽度变化；返回退订函数。 */
export function subscribeSidebarWidth(listener: () => void): () => void {
  widthListeners.add(listener);
  return () => {
    widthListeners.delete(listener);
  };
}

/** 仅测试用：把广播值复位，避免用例之间互相污染。 */
export function resetSidebarWidthForTest(): void {
  liveSidebarWidth = DEFAULT_SIDEBAR_WIDTH;
  for (const listener of [...widthListeners]) listener();
}
