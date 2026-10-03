/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach, beforeEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { MarkdownText } from "./MarkdownText";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";
// @ts-ignore -- @tailwindcss/typography 以未类型化的 CJS 发布其样式表
import typography from "@tailwindcss/typography/src/styles.js";

vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: vi.fn().mockResolvedValue(undefined),
}));

afterEach(cleanup);

describe("MarkdownText", () => {
  it("renders a fenced code block with highlight classes", () => {
    const { container } = render(<MarkdownText text={"```js\nconst x = 1;\n```"} />);
    const code = container.querySelector("pre code");
    expect(code).not.toBeNull();
    expect(code!.className).toContain("hljs");
  });

  it("renders a GFM table", () => {
    const { container } = render(<MarkdownText text={"| a | b |\n| - | - |\n| 1 | 2 |"} />);
    expect(container.querySelector("table")).not.toBeNull();
  });

  it("lets wide fenced code scroll inside the bubble instead of widening it", () => {
    // 长命令/长 URL 的代码块宽达数百像素（实测 579px vs 390pt 视口）。没有
    // `overflow-x-auto` 兜底时它会把 markdown 容器（进而是整个会话列）撑宽，
    // 手机端于是能横向拖动、右侧留白。
    const { container } = render(<MarkdownText text={"```bash\n" + "x".repeat(400) + "\n```"} />);
    const wrap = container.querySelector(".prose") as HTMLElement;
    expect(wrap.className).toContain("prose-pre:overflow-x-auto");
    expect(wrap.className).toContain("break-words");
  });

  it("scrolls a wide table inside its own box instead of widening the column", () => {
    // markdown 表格实测 615px（视口 390pt）。表格必须在自己的框内横向滚动，
    // 否则会把会话列撑宽——手机端整页可横向拖动、右移后右侧全是空白。
    const wide = "| a | b |\n| - | - |\n| " + "x".repeat(300) + " | 2 |";
    const { container } = render(<MarkdownText text={wide} />);
    const table = container.querySelector("table")!;
    expect(table).not.toBeNull();
    const box = table.parentElement as HTMLElement;
    expect(box.className).toContain("overflow-x-auto");
  });

  it("does not render raw HTML (XSS guard)", () => {
    const { container } = render(
      <MarkdownText text={"before <script>window.__x = 1</script> after"} />,
    );
    expect(container.querySelector("script")).toBeNull();
  });

  it("opens http(s) links via the opener and prevents default", () => {
    const { container } = render(<MarkdownText text={"[x](https://example.com)"} />);
    const a = container.querySelector("a");
    expect(a).not.toBeNull();
    const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
    a!.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(openUrl).toHaveBeenCalledWith("https://example.com");
  });

  it("does not render a clickable anchor for non-http(s) links", () => {
    const { container } = render(<MarkdownText text={"[x](javascript:alert(1))"} />);
    expect(container.querySelector("a")).toBeNull();
  });
});

/**
 * 浅色主题下的正文可读性。
 *
 * `prose-invert` 会把每个 `--tw-prose-*` 变量无条件重指到 invert(浅字)值；
 * 这里在 jsdom 里注入【真实】的 `@tailwindcss/typography` 样式表（直接读插件源，
 * 与 `index.css` 的 `@plugin` 同源）+【真实】的 `src/index.css`，让 jsdom 自己跑
 * 级联，再把元素上解析后的 `--tw-prose-*` 解析成实际 sRGB、按 WCAG 算对比度。
 * 因此它不是「断言一个自己写在测试里的常量」，而是断言真实级联的产物：
 * 移除修复后 `[data-theme="light"] .prose` 覆盖，`--tw-prose-body` 会落回
 * `var(--tw-prose-invert-body)`（invert 的 near-white），测试即失败。
 */

const plugin = typography as {
  DEFAULT: { css: unknown };
  invert: { css: unknown };
};

function readIndexCss(): string {
  return readFileSync(String((import.meta as unknown as { dirname: string }).dirname) + "/../index.css", "utf8");
}

/** 从插件的 `{ '--tw-prose-x': value }` 对象里挑出 `--tw-prose-*` 声明。 */
function twDecls(css: unknown): string {
  const merged = Array.isArray(css) ? Object.assign({}, ...(css as object[])) : (css as object);
  return Object.entries(merged)
    .filter(([k]) => k.startsWith("--tw-prose-"))
    .map(([k, v]) => `${k}:${v}`)
    .join(";");
}

const TYPOGRAPHY_CSS = `.prose{${twDecls(plugin.DEFAULT.css)}}\n.prose-invert{${twDecls(plugin.invert.css)}}`;

function oklchToRgb(L: number, C: number, hDeg: number): [number, number, number] {
  const h = (hDeg * Math.PI) / 180;
  const a = C * Math.cos(h);
  const b = C * Math.sin(h);
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  const lin = [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
  const encode = (v: number) => {
    const c = Math.min(1, Math.max(0, v));
    return c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055;
  };
  return lin.map(encode) as [number, number, number];
}

const clamp255 = (v: number) => Math.min(255, Math.max(0, Math.round(v * 255)));

/** 把 getComputedStyle 返回的颜色串解析成 sRGB（0-255）。 */
function toRgb(value: string): [number, number, number] | null {
  const s = value.trim().toLowerCase();
  const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/.exec(s);
  if (hex) {
    const h = hex[1].length === 3 ? hex[1].replace(/./g, (c) => c + c) : hex[1];
    const n = parseInt(h, 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  }
  const rgb = /^rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)/.exec(s);
  if (rgb) return [Number(rgb[1]), Number(rgb[2]), Number(rgb[3])];
  const oklch = /^oklch\(\s*([\d.]+)%\s+([\d.]+)\s+([\d.]+)/.exec(s);
  if (oklch) {
    const [r, g, b] = oklchToRgb(Number(oklch[1]) / 100, Number(oklch[2]), Number(oklch[3]));
    return [clamp255(r), clamp255(g), clamp255(b)];
  }
  return null;
}

/**
 * `--tw-prose-body` 可能落回 `var(--tw-prose-invert-body)`（在 `.prose` 上声明），
 * 也可能指向 `var(--fg)`（在 `:root`/`[data-theme="light"]` 上声明、应继承下来）。
 * jsdom 的 getComputedStyle 不会把自定义属性继承给后代，所以逐跳解析时先看元素自身、
 * 再回落到 <html>——值仍然全部来自真实的 index.css，只是补上继承语义。
 */
function resolveVar(el: HTMLElement, name: string): string {
  const own = getComputedStyle(el);
  const root = getComputedStyle(document.documentElement);
  const read = (n: string) => own.getPropertyValue(n).trim() || root.getPropertyValue(n).trim();
  let value = read(name);
  for (let hops = 0; hops < 8; hops++) {
    const m = /^var\(\s*(--[\w-]+)\s*\)$/.exec(value);
    if (!m) break;
    value = read(m[1]);
  }
  return value;
}

function relativeLuminance([r, g, b]: [number, number, number]): number {
  const lin = [r, g, b].map((v) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
}

function contrastRatio(a: [number, number, number], b: [number, number, number]): number {
  const la = relativeLuminance(a);
  const lb = relativeLuminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/** 真实的 `index.css` 会让 jsdom 跑出 `[data-theme="light"]` 下的真实解析值。 */
function installRealStylesheets(): void {
  const style = document.createElement("style");
  style.textContent = `${TYPOGRAPHY_CSS}\n${readIndexCss()}`;
  document.head.appendChild(style);
}

describe("MarkdownText light-theme transcript readability", () => {
  beforeEach(() => {
    delete document.documentElement.dataset.theme;
  });
  afterEach(() => {
    delete document.documentElement.dataset.theme;
    document.head.querySelectorAll("style").forEach((s) => s.remove());
  });

  it("resolves the body variable to a dark-on-light color under [data-theme=light]", () => {
    installRealStylesheets();
    document.documentElement.dataset.theme = "light";
    const { container } = render(<MarkdownText text={"plain **bold** body"} />);
    const wrap = container.querySelector(".prose") as HTMLElement;
    expect(wrap).not.toBeNull();

    const resolved = resolveVar(wrap, "--tw-prose-body");
    // 关掉 theme 后同一元素应解到 invert 的浅色值——用它作为「near-white」基准。
    delete document.documentElement.dataset.theme;
    const invertBody = getComputedStyle(wrap).getPropertyValue("--tw-prose-invert-body").trim();

    const bodyColor = toRgb(resolved);
    const invertColor = toRgb(invertBody);
    expect(bodyColor).not.toBeNull();
    expect(invertColor).not.toBeNull();

    const ratio = contrastRatio(bodyColor!, invertColor!);
    // 修复前 resolved === var(--tw-prose-invert-body) → 对比度 1:1。
    expect(
      ratio,
      `light-mode prose body ${resolved} is ~identical to the invert value ${invertBody} (ratio ${ratio.toFixed(2)}:1) — the transcript is light-on-light`,
    ).toBeGreaterThan(2);
  });

  it("keeps every prose body/heading/link/pre token dark relative to the light panel", () => {
    installRealStylesheets();
    document.documentElement.dataset.theme = "light";
    const { container } = render(<MarkdownText text={"# title [x](https://e.com)\n\n```js\n1\n```"} />);
    const wrap = container.querySelector(".prose") as HTMLElement;
    const cs = getComputedStyle(wrap);
    const panel = toRgb(getComputedStyle(document.documentElement).getPropertyValue("--surface").trim())!;
    expect(panel).not.toBeNull();

    for (const token of ["--tw-prose-body", "--tw-prose-headings", "--tw-prose-bold", "--tw-prose-links", "--tw-prose-pre-code"]) {
      const raw = cs.getPropertyValue(token).trim();
      const color = toRgb(resolveVar(wrap, token));
      expect(color, `${token} could not be resolved to a color (raw: ${raw})`).not.toBeNull();
      const ratio = contrastRatio(color!, panel);
      expect(
        ratio,
        `${token} = ${raw} has only ${ratio.toFixed(2)}:1 against the light surface — unreadable`,
      ).toBeGreaterThan(3);
    }
  });

  it("does not regress the dark default: the invert mapping still applies", () => {
    installRealStylesheets();
    // 不设 data-theme —— 深色是默认。
    const { container } = render(<MarkdownText text={"plain body"} />);
    const wrap = container.querySelector(".prose") as HTMLElement;
    expect(getComputedStyle(wrap).getPropertyValue("--tw-prose-body").trim()).toBe(
      "var(--tw-prose-invert-body)",
    );
    const color = toRgb(resolveVar(wrap, "--tw-prose-body"));
    expect(color).not.toBeNull();
    // 深色下正文与近黑 surface 的对比度依然足够。
    const surface = toRgb(getComputedStyle(document.documentElement).getPropertyValue("--surface").trim())!;
    expect(contrastRatio(color!, surface)).toBeGreaterThan(3);
  });
});
