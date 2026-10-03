/** @vitest-environment node */
import { describe, it, expect } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

/**
 * 手机端「贴合画面」的静态门禁。
 *
 * 与 `MarkdownText.test.tsx` 的浅色主题用例同源：直接读**真实**的 `index.html`
 * 与 `index.css`，断言的是真正会打包进去的产物，而不是测试里另写一份常量。
 *
 * 三条都是 iOS 上「上下高度不对」的直接成因，任何一条回退都会让手机端再次
 * 出现刘海遮挡、底部输入框被顶出画面或整页可上下滑动。桌面端不匹配 `[data-mobile]`
 * 前缀，因此这些选择器对桌面逐字节无影响。
 */
const dir = String((import.meta as unknown as { dirname: string }).dirname);
const html = () => readFileSync(`${dir}/../index.html`, "utf8");
const css = () => readFileSync(`${dir}/index.css`, "utf8");

describe("phone viewport metadata", () => {
  it("opts into the notch/rounded-corner safe area", () => {
    // 没有 viewport-fit=cover，`env(safe-area-inset-*)` 恒为 0，下面所有安全区留白
    // 全部失效。
    expect(html()).toMatch(/viewport-fit=cover/);
  });
});

describe("phone layout CSS", () => {
  it("fits the shell to the dynamic viewport instead of the iOS 100vh", () => {
    // `100vh` 在 iOS 上大于可视高度（Safari 工具栏 + 刘海区都不计入），底部输入框
    // 因此被顶出画面。`100dvh` 跟的是动态可视高度。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+body\s*\{[^}]*height:\s*100dvh/);
  });

  it("boxes the safe areas into that height so they are not added on top", () => {
    // body 的上下安全区内边距必须与 `100dvh` 同处一个 `border-box` 高度的盒子里，
    // 否则两条留白会把内容整体撑高，抵消 `100dvh` 的修正。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+body\s*\{[^}]*padding-top:\s*env\(safe-area-inset-top[^}]*padding-bottom:\s*env\(safe-area-inset-bottom/,
    );
  });

  it("hides the desktop-only titlebar strip on the phone", () => {
    // iOS 上没有红绿灯按钮，那条 32px 的空条只白占高度。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+\.app-titlebar\s*\{[^}]*display:\s*none/);
  });

  it("gives the fixed drawer its own safe-area insets", () => {
    // 抽屉是 `position: fixed`，脱离文档流，父级的内边距管不到它——刘海会盖住
    // 「New thread」。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+\.app-sidebar\s*\{[^}]*safe-area-inset-top[^}]*safe-area-inset-bottom/,
    );
  });
});
