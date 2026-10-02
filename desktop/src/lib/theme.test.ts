/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { applyTheme, parseTheme, readCachedTheme, THEME_STORAGE_KEY } from "./theme";

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.theme;
});

describe("parseTheme", () => {
  it("only light is light; everything else is dark", () => {
    expect(parseTheme("light")).toBe("light");
    expect(parseTheme("dark")).toBe("dark");
    expect(parseTheme("sepia")).toBe("dark");
    expect(parseTheme(undefined)).toBe("dark");
    expect(parseTheme(5)).toBe("dark");
  });

  it("trims and lowercases", () => {
    expect(parseTheme(" LIGHT ")).toBe("light");
  });
});

describe("applyTheme", () => {
  it("sets data-theme and caches the value", () => {
    applyTheme("light");
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe("light");
  });
});

describe("readCachedTheme", () => {
  it("returns the cached theme, or null when absent", () => {
    expect(readCachedTheme()).toBeNull();
    localStorage.setItem(THEME_STORAGE_KEY, "light");
    expect(readCachedTheme()).toBe("light");
  });

  it("treats a junk cache as absent", () => {
    localStorage.setItem(THEME_STORAGE_KEY, "sepia");
    expect(readCachedTheme()).toBe("dark");
  });
});
