/** @vitest-environment jsdom */
import { describe, expect, it, vi, afterEach } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsGeneralTab } from "./SettingsGeneralTab";

afterEach(() => {
  cleanup();
});

/**
 * 「通用」页现在有两个关注点：主题，以及从看板面板迁来的宿主级值守。
 *
 * 值守放在这里的理由是可测的：它服务所有已登记的项目，与用户当时选中的看板
 * 无关，所以它不该出现在看板页；而它又必须出现在某处，否则用户没法关掉它。
 */
describe("SettingsGeneralTab", () => {
  const theme = { theme: "dark" as const, onThemeChange: () => {} };

  it("keeps the theme choice", () => {
    const onThemeChange = vi.fn();
    render(<SettingsGeneralTab theme="dark" onThemeChange={onThemeChange} />);
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    expect(onThemeChange).toHaveBeenCalledWith("light");
  });

  it("shows the watchman toggle when the host offers it", () => {
    render(
      <SettingsGeneralTab
        {...theme}
        watchmanEnabled
        onToggleWatchman={() => {}}
      />,
    );
    const box = screen.getByLabelText("后台值守（开机自启）开关") as HTMLInputElement;
    expect(box.checked).toBe(true);
    // 默认就是开：用户不必去找这个开关，界面要说清楚这一点。
    expect(screen.getByText(/默认开启/)).toBeTruthy();
  });

  it("reports the requested watchman value", () => {
    const onToggleWatchman = vi.fn();
    render(
      <SettingsGeneralTab
        {...theme}
        watchmanEnabled
        onToggleWatchman={onToggleWatchman}
      />,
    );
    fireEvent.click(screen.getByLabelText("后台值守（开机自启）开关"));
    expect(onToggleWatchman).toHaveBeenCalledWith(false);
  });

  it("hides the section entirely when the host cannot toggle it", () => {
    // 只读客户端没有这个能力：显示一个拨不动的开关比不显示更糟。
    render(<SettingsGeneralTab {...theme} />);
    expect(screen.queryByLabelText("后台值守（开机自启）开关")).toBeNull();
  });

  it("surfaces the host's warning verbatim", () => {
    render(
      <SettingsGeneralTab
        {...theme}
        watchmanEnabled={false}
        onToggleWatchman={() => {}}
        watchmanWarning="未能在登录项中安装值守"
      />,
    );
    expect(screen.getByRole("alert").textContent).toBe("未能在登录项中安装值守");
  });
});
