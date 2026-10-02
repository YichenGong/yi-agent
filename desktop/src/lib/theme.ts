/** 桌面端主题。`preferences.json` 是权威，localStorage 只做首屏缓存。 */
export type Theme = "dark" | "light";

export const THEME_STORAGE_KEY = "app.theme";

/** 非 "light" 一律 dark —— 坏值不许把界面留在未定义状态。 */
export function parseTheme(value: unknown): Theme {
  return typeof value === "string" && value.trim().toLowerCase() === "light"
    ? "light"
    : "dark";
}

/** 首屏缓存读取；localStorage 在受限环境会抛，吞掉视为未缓存。 */
export function readCachedTheme(): Theme | null {
  try {
    const raw = localStorage.getItem(THEME_STORAGE_KEY);
    return raw === null ? null : parseTheme(raw);
  } catch {
    return null;
  }
}

/** 应用主题：切 <html> 的 data-theme 并回写首屏缓存。 */
export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    /* 缓存写失败不影响渲染 */
  }
}
