/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import {
  applyFontScale,
  defaultFontScale,
  FONT_SCALE_STORAGE_KEY,
  parseFontScale,
  readCachedFontScale,
} from "./fontScale";

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.fontScale;
});

/**
 * `parseFontScale` 与 `parseTheme` 的**关键差异**：坏值返回 `null`，不折成默认档。
 *
 * 两个平台的默认值不一样（桌面 standard、配对手机 small），所以「没配」与
 * 「配成 standard」必须能区分。折成默认档会让手机端在缺键时误取桌面默认，
 * 也就是「iOS 一装上字号没变小」——正是本次要修的症状。
 */
describe("parseFontScale", () => {
  it("recognises the three tiers", () => {
    expect(parseFontScale("small")).toBe("small");
    expect(parseFontScale("standard")).toBe("standard");
    expect(parseFontScale("large")).toBe("large");
  });

  it("trims and lowercases", () => {
    expect(parseFontScale("  LARGE ")).toBe("large");
  });

  it("returns null for anything else, so callers can fall back per platform", () => {
    expect(parseFontScale("tiny")).toBeNull();
    expect(parseFontScale("")).toBeNull();
    expect(parseFontScale(null)).toBeNull();
    expect(parseFontScale(undefined)).toBeNull();
    expect(parseFontScale(3)).toBeNull();
  });
});

describe("defaultFontScale", () => {
  it("starts the phone smaller than the desktop", () => {
    // 用户报的就是「手机上字有点大」，所以手机首启必须落在小档；
    // 桌面必须停在标准档，改动不得改变桌面观感。
    expect(defaultFontScale(true)).toBe("small");
    expect(defaultFontScale(false)).toBe("standard");
  });
});

describe("applyFontScale", () => {
  it("mirrors the tier onto <html> and caches it", () => {
    applyFontScale("large");
    expect(document.documentElement.dataset.fontScale).toBe("large");
    expect(localStorage.getItem(FONT_SCALE_STORAGE_KEY)).toBe("large");
  });

  it("always writes the attribute, standard included", () => {
    // 取值只有一条路径（属性恒在）。标准档的 `--font-scale` 是 1，
    // 计算值与不设属性逐字节相同，故不必区分「未设」与「设为标准」。
    applyFontScale("standard");
    expect(document.documentElement.dataset.fontScale).toBe("standard");
  });

  it("survives a storage that refuses writes", () => {
    const setItem = localStorage.setItem;
    localStorage.setItem = () => {
      throw new Error("quota");
    };
    try {
      expect(() => applyFontScale("small")).not.toThrow();
      expect(document.documentElement.dataset.fontScale).toBe("small");
    } finally {
      localStorage.setItem = setItem;
    }
  });
});

describe("readCachedFontScale", () => {
  it("returns the cached tier, or null when absent", () => {
    expect(readCachedFontScale()).toBeNull();
    localStorage.setItem(FONT_SCALE_STORAGE_KEY, "small");
    expect(readCachedFontScale()).toBe("small");
  });

  it("treats a junk cache as absent", () => {
    localStorage.setItem(FONT_SCALE_STORAGE_KEY, "tiny");
    expect(readCachedFontScale()).toBeNull();
  });
});
