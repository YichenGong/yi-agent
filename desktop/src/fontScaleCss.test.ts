/** @vitest-environment node */
import { describe, it, expect } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

/**
 * 字号档位的静态门禁。
 *
 * 与 `mobileLayout.test.ts` 同源：直接读**真实**的 `index.css`，断言的是真正会
 * 打包进去的产物，而不是测试里另写一份常量。
 *
 * 这里守三件会在真机上立刻显形、但单测看不见的事：
 * 1. 四个 Tailwind 字号变量都乘上了 `--font-scale`——漏一个，
 *    「小」档下那一档文字就不动，看上去像局部没生效。
 * 2. `.prose` 接上了 `--text-base`——typography 把正文硬编码成 `1rem`，
 *    不接的话**最需要变小的聊天区反而不变**。
 * 3. 两条 iOS 16px 输入框兜底仍在——它们防的是聚焦自动放大，被字号规则
 *    顺带覆盖掉就会在手机上触发整页放大。
 */
const dir = String((import.meta as unknown as { dirname: string }).dirname);
const css = () => readFileSync(`${dir}/index.css`, "utf8");

/**
 * 取某个选择器块的内容（**最后一个**匹配）。
 *
 * `:root` 在文件里出现两次（语义色 token 一处、字号档位一处），取最后一个
 * 才是本次新增的那块；其它选择器都只出现一次，故对它们等价于「首个」。
 */
function rule(selector: string): string {
  const all = [
    ...css().matchAll(new RegExp(`${selector}\\s*\\{([^}]*)\\}`, "g")),
  ];
  return all.length > 0 ? all[all.length - 1][1] : "";
}

describe("font scale CSS", () => {
  it("scales every Tailwind text size through one variable", () => {
    const root = rule(":root");
    expect(root).toMatch(/--font-scale:\s*1/);
    for (const name of ["xs", "sm", "base", "lg"]) {
      expect(root).toMatch(
        new RegExp(`--text-${name}:\\s*calc\\([^)]*\\*\\s*var\\(--font-scale\\)`),
      );
    }
  });

  it("defines the two non-default tiers on the html element", () => {
    // 限定在 `html[...]` 上而不是 `:root`：`applyFontScale` 写的是
    // `<html data-font-scale>`，两个选择器特异度不同（0,1,1 对 0,1,0），
    // 写错层会被上一条 `:root` 的 `--font-scale: 1` 压掉。
    expect(css()).toMatch(
      /html\[data-font-scale="small"\]\s*\{[^}]*--font-scale:\s*0?\.9375/,
    );
    expect(css()).toMatch(
      /html\[data-font-scale="large"\]\s*\{[^}]*--font-scale:\s*1\.125/,
    );
  });

  it("wires the markdown body onto --text-base", () => {
    // typography 插件的 `.prose` 是 `font-size: 1rem`，不引用 Tailwind 变量。
    const block = /html\[data-font-scale\]\s+\.prose\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(block).toMatch(/font-size:\s*var\(--text-base\)/);
  });

  it("keeps the iOS 16px floors on inputs", () => {
    // 低于 16px 时 iOS 会在聚焦时自动放大页面，而 `lockZoom` 只能拦手动捏合。
    // 这两条必须继续存在，且不能被 `--font-scale` 影响。
    const search =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+input\[type="search"\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(search).toMatch(/font-size:\s*16px/);

    const panel =
      /\[data-mobile="true"\]\s+\.settings-panel\s+input,\s*\[data-mobile="true"\]\s+\.settings-panel\s+select,\s*\[data-mobile="true"\]\s+\.settings-panel\s+textarea\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(panel).toMatch(/font-size:\s*16px/);
  });
});
