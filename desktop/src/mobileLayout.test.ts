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
  it("fits the shell to the dynamic viewport, with a vh fallback", () => {
    // `100vh` 在 iOS 上大于可视高度（Safari 工具栏 + 刘海区都不计入），底部输入框
    // 因此被顶出画面。`100dvh` 跟的是动态可视高度；但 Safari < 15.4 不认识 `dvh`，
    // 必须先给 `100vh` 回退，否则会落到 `height: auto`（比不修更糟）。
    const rule = /\[data-mobile="true"\]\s+body\s*\{([^}]*)\}/.exec(css())?.[1] ?? "";
    expect(rule).toMatch(/height:\s*100vh/);
    expect(rule).toMatch(/height:\s*100dvh/);
    // 覆盖必须晚于回退（后者写在前、前者写在后）。
    expect(rule.indexOf("100vh")).toBeLessThan(rule.indexOf("100dvh"));
  });

  it("boxes the safe areas into that height so they are not added on top", () => {
    // body 的上下安全区内边距必须与动态视口高度同处一个 `border-box` 高度的盒子里，
    // 否则两条留白会把内容整体撑高，抵消高度修正。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+body\s*\{[^}]*padding-top:\s*env\(safe-area-inset-top[^}]*padding-bottom:\s*env\(safe-area-inset-bottom/,
    );
  });

  it("hides the desktop-only titlebar strip on the phone", () => {
    // iOS 上没有红绿灯按钮，那条 32px 的空条只白占高度。
    expect(css()).toMatch(/\[data-mobile="true"\]\s+\.app-titlebar\s*\{[^}]*display:\s*none/);
  });

  it("gives the drawer's scrolling element its own safe-area insets", () => {
    // 抽屉是 `position: fixed`，脱离文档流，父级的内边距管不到它。安全区要落在
    // 真正会滚动的 `aside` 上，滚动到底时最后一行才不被 Home Indicator 压住。
    expect(css()).toMatch(
      /\[data-mobile="true"\]\s+\.app-sidebar\s+aside\s*\{[^}]*safe-area-inset-top[^}]*safe-area-inset-bottom/,
    );
  });

  it("constrains free-standing images so overflow-x-hidden cannot clip them", () => {
    // typography 基准里 `img` 只有上下外边距、没有 `max-width`；会话列一旦
    // `overflow-x-hidden`，超宽图片被裁掉的部分将永久不可达。
    const src = readFileSync(`${dir}/components/MarkdownText.tsx`, "utf8");
    expect(src).toContain("prose-img:max-w-full");
  });

  it("refuses pinch-zoom, without also refusing scrolling", () => {
    // WKWebView 默认允许双指缩放；放大后页面不再贴合屏幕，之后每次滑动都变成
    // 两轴平移（用户报的「缩放之后就需要拖拽」）。`pan-x pan-y` 只禁缩放：
    // 抽屉列表与 `overflow-x-auto` 的表格仍可滑动。
    expect(css()).toMatch(/html\[data-mobile="true"\]\s*\{[^}]*touch-action:\s*pan-x pan-y/);
  });
});
