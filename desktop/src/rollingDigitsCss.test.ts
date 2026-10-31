/** @vitest-environment node */
import { describe, expect, it } from "vitest";
// @ts-ignore -- node 内置模块;本包不依赖 @types/node
import { readFileSync } from "node:fs";

/**
 * 翻滚数字的几何门禁。
 *
 * 与 `mobileLayout.test.ts` 同源：直接读**真实**的 `index.css`，断言真正会打包
 * 进去的产物，而不是测试里另写一份常量。
 *
 * 为什么窗口必须是 `1lh` 而不是 `1em`：
 *
 * 状态栏文字是 `text-xs`（12px）+ 继承行高 16px。字体本身的行距约 14px，故
 * `1em`（12px）的裁切窗口比字形还矮，只能靠「窗口在 16px 行盒里居中」把这个差
 * 补回来——(16-12)/2 = 2px 恰好抵消半行距的 2px，于是视觉上勉强对齐。这个抵消
 * 依赖两个巧合：行盒正好 16px，且居中正好取整到 2px。WKWebView 下按钮行盒实测
 * 是 15px，居中落在 1.5px 上，数字窗口因此停在半像素处（9.5–21.5），与 `in /`
 * 的 8–23 错开——正是用户报的「数字和 in out 没有对齐」。
 *
 * `1lh` 让窗口等于**自己所在行盒**的高度：字形、基线与相邻文本走同一套行盒规则，
 * 不再依赖任何居中抵消，也不受引擎取整差异影响。
 */
const dir = String((import.meta as unknown as { dirname: string }).dirname);
const css = () => readFileSync(`${dir}/index.css`, "utf8");

/** 取某条规则的声明块；选择器里的正则元字符会被转义。 */
function declarations(selector: string): string {
  const pattern = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`${pattern}\\s*\\{([^}]*)\\}`).exec(css())?.[1] ?? "";
}

describe("rolling digit geometry", () => {
  it("makes the clip window exactly one line box tall", () => {
    const rule = declarations(".rolling-digit");
    expect(rule).toMatch(/height:\s*1lh/);
    // `1em`（12px）比字形矮，会重新引入「靠居中抵消」的脆弱对齐。
    expect(rule).not.toMatch(/height:\s*1em/);
    expect(rule).toMatch(/overflow:\s*hidden/);
  });

  it("keeps each strip cell as tall as the window", () => {
    const rule = declarations(".rolling-strip > span");
    expect(rule).toMatch(/height:\s*1lh/);
    expect(rule).not.toMatch(/height:\s*1em/);
    // 格子自己再写死一个像素行高，就又把字形与行盒解耦了；继承即可。
    expect(rule).toMatch(/line-height:\s*inherit/);
    expect(rule).not.toMatch(/line-height:\s*1em/);
  });
});

/**
 * 小数点与单位的几何。
 *
 * 两者都是 `.rolling-number`（`inline-flex`）的直接子项，所以任何"按视觉宽度
 * 收窄"的尺寸都会直接改变字形的位置——百分比 width 在这里尤其危险。
 */
describe("rolling point and unit geometry", () => {
  it("leaves the point at its natural advance width", () => {
    const rule = declarations(".rolling-point");
    // 回归：`.rolling-point` 曾写成 `width: 0.5ch`。等宽字体里 `.` 的前进宽度是
    // `1ch`，把字形塞进半个宽度的盒子里会让它左移 0.25ch，前进盒同时压到左右
    // 两位数字上——小数点与相邻数字粘成一团，而普通文本里不会出现这种重叠。
    expect(rule).not.toMatch(/width\s*:/);
    expect(rule).toMatch(/display:\s*inline-block/);
  });

  it("keeps the unit's spacing from shrinking to nothing", () => {
    const rule = declarations(".rolling-unit");
    expect(rule).toMatch(/margin-left:\s*0\.1em/);
  });
});
