/** @vitest-environment node */
import { describe, it, expect } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readdirSync, readFileSync } from "node:fs";

/**
 * 语义色 token 的静态门禁。
 *
 * `index.css` 的约定是：组件**只**引用语义名（`text-fg`、`bg-raised`…），
 * 不写字面色阶（`text-neutral-<shade>`）。字面色阶不随 `data-theme` 变化，浅色主题下
 * 会留下「近白字压浅底」这类失读的角落——侧栏右键菜单里的「创建/移除看板」就是
 * 这么被落下的（见 `ThreadSidebar.tsx` 的菜单项，同一批迁移漏改）。
 *
 * 直接扫**真实**源码，而不是另写一份常量清单：任何新增的字面色阶都会在此变红。
 */

/** src 下所有会打包进应用的源码（测试自身不算）。 */
function sourceFiles(root: string): string[] {
  return readdirSync(root, { withFileTypes: true })
    .flatMap((entry) => {
      const path = `${root}/${entry.name}`;
      if (entry.isDirectory()) return sourceFiles(path);
      if (!/\.(ts|tsx)$/.test(entry.name)) return [];
      if (/\.test\.(ts|tsx)$/.test(entry.name)) return [];
      return [path];
    })
    .filter((p) => !p.endsWith(".d.ts"));
}

/**
 * Tailwind 的**字面色阶**工具类：`<utility>-neutral-<number>`。
 *
 * 只盯 `neutral` 这一族——它正是与语义 token 一一对应的那套灰阶（浅色阶对 `fg`、
 * 深色阶对 `raised`…）。刻意排除「`data-theme` 无法覆盖、又在浅色底上失读」的用法，
 * 而不是全盘禁止 Tailwind 调色板。
 */
const LITERAL_NEUTRAL =
  /\b(?:text|bg|border|ring|divide|from|via|to|outline|placeholder|shadow|accent|caret|fill|stroke|decoration)-neutral-\d{2,3}\b/g;

describe("theme tokens", () => {
  const dir = String((import.meta as unknown as { dirname: string }).dirname);
  const files = sourceFiles(`${dir}/..`);

  it("finds the source tree (guard is not vacuous)", () => {
    expect(files.length).toBeGreaterThan(20);
    expect(files.some((f) => f.endsWith("/App.tsx"))).toBe(true);
  });

  it("defines every semantic token it exposes to components", () => {
    const css = readFileSync(`${dir}/../index.css`, "utf8");
    for (const token of ["surface", "panel", "raised", "line", "line-strong", "fg", "fg-muted", "fg-subtle", "fg-faint"]) {
      expect(css).toContain(`--color-${token}:`);
    }
    // 亮色主题必须重指同一批 token，否则「切主题」只是半个换肤。
    expect(css).toMatch(/\[data-theme="light"\]\s*\{/);
  });

  it("uses no literal Tailwind neutral scale in components", () => {
    const offenders: string[] = [];
    for (const file of files) {
      const text = readFileSync(file, "utf8");
      for (const match of text.matchAll(LITERAL_NEUTRAL)) {
        const line = text.slice(0, match.index).split("\n").length;
        offenders.push(`${file.slice(dir.length + 1)}:${line} ${match[0]}`);
      }
    }
    expect(offenders, `改用语义 token（text-fg / bg-raised / border-line …）:\n${offenders.join("\n")}`).toEqual([]);
  });
});