/** @vitest-environment node */
import { describe, expect, it } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

/**
 * 语义 token 的可读性门禁。
 *
 * 为什么需要这条测试：chrome 验收脚本能测真实合成背景下的对比度，但它跑在
 * `npm run test:browser:subagent-trace` 里，不是每次 `just desktop-check` 都执行。
 * 曾经 `--fg-faint` 在浅色主题压到 `--raised` 上只有 4.23:1 却通过了全量单测——
 * 因为没有任何单测读过 CSS。这里直接把「文字对**最差**底色 >= 4.5:1」钉死。
 *
 * 测的是最差底色而不是某个具体底色：token 的用处跨越 surface/panel/raised，
 * 只对上其中一层达标没有意义。
 */

const dir = String((import.meta as unknown as { dirname: string }).dirname);

const read = (file: string) =>
  readFileSync(`${dir}/${file}`, "utf8").replace(/\/\*[\s\S]*?\*\//g, "");

/** 从某段选择器块里取出全部 `--name: #rrggbb;`。 */
function tokens(block: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const match of block.matchAll(/(--[a-z-]+):\s*(#[0-9a-f]{6})\s*;/g)) {
    out[match[1]] = match[2];
  }
  return out;
}

function blockAfter(source: string, selector: string): string {
  const start = source.indexOf(selector);
  expect(start, `missing selector ${selector}`).toBeGreaterThanOrEqual(0);
  const open = source.indexOf("{", start);
  const close = source.indexOf("}", open);
  return source.slice(open + 1, close);
}

function channel(value: number): number {
  const c = value / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

function luminance(hex: string): number {
  const raw = hex.replace("#", "");
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(raw.slice(i, i + 2), 16));
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

function contrast(foreground: string, background: string): number {
  const [a, b] = [luminance(foreground), luminance(background)];
  const [hi, lo] = [Math.max(a, b), Math.min(a, b)];
  return (hi + 0.05) / (lo + 0.05);
}

const css = read("index.css");
const dark = tokens(blockAfter(css, ":root {"));
const light = tokens(blockAfter(css, '[data-theme="light"]'));

const themes = [
  { name: "dark", tokens: dark },
  { name: "light", tokens: light },
];

/** 每套主题里所有可能承载正文的底色。 */
const BACKGROUNDS = ["--surface", "--panel", "--raised"];

/** 会以文字形式出现在这些小字号位置的 token。 */
const FOREGROUNDS = ["--fg", "--fg-muted", "--fg-subtle", "--fg-faint"];

const FLOOR = 4.5;

describe("semantic text contrast", () => {
  for (const theme of themes) {
    describe(theme.name, () => {
      it("defines every token the panel reads", () => {
        for (const name of [...BACKGROUNDS, ...FOREGROUNDS]) {
          expect(theme.tokens[name], `${theme.name} is missing ${name}`).toMatch(/^#[0-9a-f]{6}$/);
        }
      });

      for (const fg of FOREGROUNDS) {
        it(`${fg} reaches ${FLOOR}:1 on every background`, () => {
          for (const bg of BACKGROUNDS) {
            const value = contrast(theme.tokens[fg], theme.tokens[bg]);
            expect(
              value,
              `${theme.name} ${fg}=${theme.tokens[fg]} on ${bg}=${theme.tokens[bg]}`,
            ).toBeGreaterThanOrEqual(FLOOR);
          }
        });
      }

      it("keeps a visible hierarchy between the muted steps", () => {
        // 全部提到同一个亮度就没有层级了：读者无法从明暗区分主次。
        //
        // 极性随主题翻转：深色主题里「更淡」= 更亮，浅色主题里「更淡」= 更暗，
        // 所以先按 --fg 相对 --surface 的方向定极性，再要求逐档单调。
        const steps = ["--fg", "--fg-muted", "--fg-subtle", "--fg-faint"].map((name) =>
          luminance(theme.tokens[name]),
        );
        const darkerIsWeaker =
          luminance(theme.tokens["--fg"]) < luminance(theme.tokens["--surface"]);
        for (let i = 1; i < steps.length; i += 1) {
          const moved = darkerIsWeaker ? steps[i] > steps[i - 1] : steps[i] < steps[i - 1];
          expect(
            moved,
            `${theme.name} step ${i} does not continue the hierarchy`,
          ).toBe(true);
        }
      });
    });
  }
});

describe("light code highlighting contrast", () => {
  const highlight = read("lib/highlight-light.css");

  /** 代码块可能落在这些底色上：自身底色最亮，raised 最暗。 */
  const CODE_BACKGROUNDS = ["--panel", "--raised"];

  const pairs: Array<[label: string, selector: string]> = [
    ["comment", ".hljs-comment"],
    ["keyword", ".hljs-keyword"],
  ];

  for (const [label, selector] of pairs) {
    it(`${label} tokens reach ${FLOOR}:1 on the code backgrounds`, () => {
      const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      const rule = highlight.match(
        new RegExp(`\\[data-theme="light"\\]\\s*${escaped}[^{}]*\\{([^{}]*)\\}`, "g"),
      );
      expect(rule, `missing rule for ${selector}`).not.toBeNull();
      const colors = [...rule!.join("").matchAll(/color:\s*(#[0-9a-f]{6})\s*;/g)].map(
        (match) => match[1],
      );
      expect(colors.length, `no color found for ${selector}`).toBeGreaterThan(0);
      for (const color of colors) {
        for (const bg of CODE_BACKGROUNDS) {
          const value = contrast(color, light[bg]);
          expect(
            value,
            `${label} ${color} on ${bg}=${light[bg]}`,
          ).toBeGreaterThanOrEqual(FLOOR);
        }
      }
    });
  }
});
