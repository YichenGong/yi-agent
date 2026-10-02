/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import html from "../../index.html?raw";
import { parseTheme, THEME_STORAGE_KEY } from "./theme";

/**
 * 首帧防闪。设计验收项「给定 localStorage 值，<head> 脚本据此设置 data-theme」
 * 此前没有任何测试。这里【执行 index.html 里真实的 inline 脚本】（不复制、不重写），
 * 因此断言的正是随应用发布的那段代码——删掉/改坏 index.html 里的脚本，本测试即红。
 * 同时与 `parseTheme` 逐条比对，保证两边规则字符级等价。
 */
function inlineHeadScripts(htmlText: string): string[] {
  return [...htmlText.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/gi)].map((m) => m[1]);
}

/** 在 jsdom 里执行 index.html 的 <head> inline 脚本（模拟浏览器对首个 <head> 脚本的处理）。 */
function runHeadInlineScripts(htmlText: string): void {
  for (const src of inlineHeadScripts(htmlText)) {
    const el = document.createElement("script");
    el.textContent = src;
    document.head.appendChild(el);
  }
}

const CASES: Array<[string | null, "light" | "dark"]> = [
  ["light", "light"],
  [" LIGHT ", "light"],
  ["Light", "light"],
  ["dark", "dark"],
  ["sepia", "dark"],
  ["", "dark"],
  [null, "dark"],
  ["light2", "dark"],
];

afterEach(() => {
  localStorage.clear();
  delete document.documentElement.dataset.theme;
  delete (window as unknown as { appTheme?: unknown }).appTheme;
});

describe("first-paint anti-flash script", () => {
  it("is present as an inline <head> script in index.html", () => {
    expect(inlineHeadScripts(html).length).toBeGreaterThan(0);
  });

  it.each(CASES)("sets data-theme=%s for stored value %j", (stored, expected) => {
    if (stored === null) localStorage.removeItem(THEME_STORAGE_KEY);
    else localStorage.setItem(THEME_STORAGE_KEY, stored);
    delete document.documentElement.dataset.theme;

    runHeadInlineScripts(html);

    // 只允许把 <html> 标成 light；dark 保持默认（不设属性），这正是脚本的承诺。
    expect(document.documentElement.dataset.theme === "light" ? "light" : "dark").toBe(expected);
  });

  it("its decision matches parseTheme for every case (character-equivalent rule)", () => {
    for (const [stored, expected] of CASES) {
      expect(parseTheme(stored)).toBe(expected);
    }
  });
});
