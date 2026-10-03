/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach } from "vitest";
import { detectMobile, lockZoom } from "./useIsMobile";

describe("detectMobile", () => {
  it("is true for a paired remote client", () => {
    expect(detectMobile(true, false)).toBe(true);
  });

  it("is true on iOS before a relay binding exists", () => {
    expect(detectMobile(false, true)).toBe(true);
  });

  it("is false on the desktop build", () => {
    expect(detectMobile(false, false)).toBe(false);
  });
});

/**
 * 手机端禁止缩放。
 *
 * WKWebView 默认允许双指缩放；一旦放大，页面就不再贴合屏幕，之后每次滚动都变成
 * 两轴平移（用户报的「缩放之后就需要拖拽」）。CSS 没有任何属性可以关掉用户缩放，
 * viewport meta 是唯一的开关；这里**原地补写**它，而不是整体重写——`width` 与
 * `viewport-fit` 是布局依赖的既有声明，丢掉它们会连带弄坏安全区。
 */
describe("lockZoom", () => {
  const meta = () => document.querySelector('meta[name="viewport"]');

  beforeEach(() => {
    document.head.innerHTML =
      '<meta name="viewport" content="width=device-width, initial-scale=1.0, viewport-fit=cover" />';
  });

  it("pins the scale so pinch-zoom is refused", () => {
    lockZoom(document);
    const content = meta()!.getAttribute("content")!;
    expect(content).toMatch(/maximum-scale=1/);
    expect(content).toMatch(/user-scalable=no/);
  });

  it("keeps the width and safe-area declarations the layout depends on", () => {
    lockZoom(document);
    const content = meta()!.getAttribute("content")!;
    expect(content).toMatch(/width=device-width/);
    expect(content).toMatch(/viewport-fit=cover/);
  });

  it("replaces a zoom-permitting directive instead of adding a rival one", () => {
    document.head.innerHTML =
      '<meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=5, user-scalable=yes" />';
    lockZoom(document);
    const content = meta()!.getAttribute("content")!;
    // 两个互相矛盾的声明谁生效取决于解析器，必须把旧的清掉。
    expect(content).not.toMatch(/maximum-scale=5/);
    expect(content).not.toMatch(/user-scalable=yes/);
    expect(content).toMatch(/maximum-scale=1/);
  });

  it("is idempotent: repeat calls neither duplicate nor reshuffle the directive", () => {
    lockZoom(document);
    const once = meta()!.getAttribute("content")!;
    lockZoom(document);
    expect(document.querySelectorAll('meta[name="viewport"]')).toHaveLength(1);
    expect(meta()!.getAttribute("content")).toBe(once);
    expect(once.match(/maximum-scale=1/g)).toHaveLength(1);
    expect(once.match(/user-scalable=no/g)).toHaveLength(1);
  });

  it("is a no-op when the document has no viewport meta", () => {
    document.head.innerHTML = "";
    expect(() => lockZoom(document)).not.toThrow();
    expect(meta()).toBeNull();
  });
});
