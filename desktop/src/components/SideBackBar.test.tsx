/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, cleanup, act } from "@testing-library/react";
import { SideBackBar } from "./SideBackBar";
import {
  DEFAULT_SIDEBAR_WIDTH,
  MAX_SIDEBAR_WIDTH,
  publishSidebarWidth,
  resetSidebarWidthForTest,
} from "../lib/sidebarWidth";

afterEach(() => {
  cleanup();
  resetSidebarWidthForTest();
});

describe("SideBackBar", () => {
  it("is a real, clickable control with an accessible name", () => {
    const onBack = vi.fn();
    render(<SideBackBar parentTitle={null} onBack={onBack} />);

    const button = screen.getByRole("button", { name: "返回主会话" });
    button.click();
    expect(onBack).toHaveBeenCalledTimes(1);
  });

  it("names its destination when the parent title is known", () => {
    render(<SideBackBar parentTitle="修复登录超时" onBack={() => {}} />);

    // 可见文字与无障碍名都要指向同一条父会话：只说「返回」会让人不确定回到哪。
    const button = screen.getByRole("button", { name: "返回主会话" });
    expect(button.getAttribute("title")).toBe(
      "退出旁问并回到主会话：修复登录超时",
    );
    expect(screen.getByText("修复登录超时")).toBeTruthy();
  });

  it("keeps a stable accessible name even when the title is long", () => {
    const long = "一条非常非常长的父会话标题".repeat(6);
    render(<SideBackBar parentTitle={long} onBack={() => {}} />);

    // 标题被截断显示，但按钮的无障碍名不随标题变化——否则读屏用户每次听到的
    // 名字都不一样，无法靠名字定位这颗按钮。
    expect(screen.getByRole("button", { name: "返回主会话" })).toBeTruthy();
    expect(screen.getByText(long)).toBeTruthy();
  });

  it("is as wide as the sidebar's new-thread button", () => {
    render(<SideBackBar parentTitle={null} onBack={() => {}} />);

    // 与侧栏「+ New thread」同一个宽度值——这是「一样宽」这条要求的承重点。
    const button = screen.getByRole("button", { name: "返回主会话" });
    expect(button.style.width).toBe(`${DEFAULT_SIDEBAR_WIDTH}px`);
  });

  it("follows the sidebar while it is being resized", () => {
    render(<SideBackBar parentTitle={null} onBack={() => {}} />);

    // 拖动分隔条时两侧必须一起变：读一次 localStorage 只能拿到上次松手时的值。
    act(() => publishSidebarWidth(MAX_SIDEBAR_WIDTH));

    const button = screen.getByRole("button", { name: "返回主会话" });
    expect(button.style.width).toBe(`${MAX_SIDEBAR_WIDTH}px`);
  });

  it("never overflows a viewport narrower than the sidebar", () => {
    render(<SideBackBar parentTitle={null} onBack={() => {}} />);

    // 兜底：窗口比侧栏还窄时宁可略窄，也不能把正文挤出视口。
    const button = screen.getByRole("button", { name: "返回主会话" });
    expect(button.style.maxWidth).toBe("100%");
  });
});
