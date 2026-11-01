/**
 * 显示字号档位（小 / 标准 / 大）。
 *
 * 与 `theme.ts` 同构：`preferences.json` 是权威，localStorage 只做首屏缓存。
 * 但有一处**关键差异**——两个平台的默认值不一样（桌面 `standard`、配对手机
 * `small`），所以读路径必须能区分「没配」与「配成标准」。这就是
 * {@link parseFontScale} 对坏值返回 `null` 而不是折成默认档的理由：
 * 折成默认档会让手机端在缺键时误取桌面默认，也就是「iOS 一装上字号没变小」。
 *
 * 键名与后端 `settings_store::FONT_SCALE_*_KEY` 对齐，一个平台只写自己那半：
 * 配对手机经 ws 写的是桌面主机上的同一个 `preferences.json`，共用一个键就会
 * 一端改字号、另一端观感跟着变。
 */
export type FontScale = "small" | "standard" | "large";

/** 界面上的顺序即数组顺序。 */
export const FONT_SCALES: ReadonlyArray<{ value: FontScale; label: string }> = [
  { value: "small", label: "小" },
  { value: "standard", label: "标准" },
  { value: "large", label: "大" },
];

export const FONT_SCALE_STORAGE_KEY = "app.fontScale";

/**
 * 严格的档位解析：未知值一律 `null`。
 *
 * 与 `parseTheme` 的「坏值也认一个」不同，理由见文件头：调用方要靠 `null`
 * 决定回落到哪个平台的默认档。
 */
export function parseFontScale(value: unknown): FontScale | null {
  if (typeof value !== "string") return null;
  switch (value.trim().toLowerCase()) {
    case "small":
      return "small";
    case "standard":
      return "standard";
    case "large":
      return "large";
    default:
      return null;
  }
}

/**
 * 某个平台首次运行时的档位。
 *
 * 手机默认小一档（用户报的就是「手机上字有点大」），桌面默认标准（改动不得
 * 改变桌面观感）。
 */
export function defaultFontScale(isMobile: boolean): FontScale {
  return isMobile ? "small" : "standard";
}

/** 首屏缓存读取；localStorage 在受限环境会抛，吞掉视为未缓存。 */
export function readCachedFontScale(): FontScale | null {
  try {
    return parseFontScale(localStorage.getItem(FONT_SCALE_STORAGE_KEY));
  } catch {
    return null;
  }
}

/**
 * 应用档位：切 `<html data-font-scale>` 并回写首屏缓存。
 *
 * **标准档也写属性**：`index.css` 的 `--font-scale` 在标准档下是 1，计算值与
 * 不设属性逐字节相同，所以取值只有一条路径，不必再维护「未设」与「设为标准」
 * 这两种等价状态。
 */
export function applyFontScale(scale: FontScale): void {
  document.documentElement.dataset.fontScale = scale;
  try {
    localStorage.setItem(FONT_SCALE_STORAGE_KEY, scale);
  } catch {
    /* 缓存写失败不影响渲染 */
  }
}
