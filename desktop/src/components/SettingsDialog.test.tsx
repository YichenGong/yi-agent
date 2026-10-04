/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { SettingsDialog } from "./SettingsDialog";

afterEach(cleanup);

describe("SettingsDialog", () => {
  it("renders nothing while closed", () => {
    render(
      <SettingsDialog open={false} theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("shows the 通用 tab with both theme choices", () => {
    render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.getByRole("dialog")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "通用" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "深色" }).getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: "浅色" }).getAttribute("aria-pressed")).toBe("false");
  });

  it("reports the chosen theme", () => {
    const onThemeChange = vi.fn();
    render(
      <SettingsDialog open theme="dark" onThemeChange={onThemeChange} onClose={() => {}} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "浅色" }));
    expect(onThemeChange).toHaveBeenCalledWith("light");
  });

  it("closes on Escape and on the backdrop", () => {
    const onClose = vi.fn();
    const { container } = render(
      <SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={onClose} />,
    );
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
    const backdrop = container.querySelector("[data-settings-backdrop]");
    expect(backdrop).toBeTruthy();
    fireEvent.click(backdrop!);
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("closes from the explicit close button", () => {
    const onClose = vi.fn();
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "关闭设置" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("starts on the 通用 tab", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(screen.getByRole("tab", { name: "通用" }).getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("tab", { name: "远程访问" }).getAttribute("aria-selected")).toBe("false");
  });

  it("switches to the 远程访问 tab and shows its panel", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);

    const tab = screen.getByRole("tab", { name: "远程访问" });
    fireEvent.click(tab);

    expect(tab.getAttribute("aria-selected")).toBe("true");
    expect(screen.getByRole("button", { name: "生成配对码" })).toBeTruthy();
    expect(screen.getByLabelText("中继地址")).toBeTruthy();
    // 面板互斥：离开「通用」后它的主题按钮不再渲染。
    expect(screen.queryByRole("button", { name: "深色" })).toBeNull();
  });

  it("shows a 插件 tab that reports the empty state", async () => {
    const pluginCall = async () => ({ plugins: [] });
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        pluginCall={pluginCall}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "插件" }));
    expect(await screen.findByText("未安装任何插件")).toBeTruthy();
  });
});

describe("SettingsDialog focus management & tab wiring", () => {
  it("moves focus into the dialog on open", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    const dialog = screen.getByRole("dialog");
    expect(dialog.contains(document.activeElement)).toBe(true);
  });

  it("restores focus to the previously focused element on close", () => {
    const trigger = document.createElement("button");
    trigger.textContent = "打开设置";
    document.body.appendChild(trigger);
    // 打开前的焦点落在触发按钮上——关闭后必须还给它。
    trigger.focus();
    expect(document.activeElement).toBe(trigger);

    const { rerender } = render(
      <SettingsDialog open={false} theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    rerender(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(screen.getByRole("dialog").contains(document.activeElement)).toBe(true);

    rerender(
      <SettingsDialog open={false} theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(document.activeElement).toBe(trigger);

    trigger.remove();
  });

  it("wires each tab to its panel with aria-controls / role=tabpanel / aria-labelledby", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);

    const generalTab = screen.getByRole("tab", { name: "通用" });
    const panel = screen.getByRole("tabpanel");
    const panelId = panel.getAttribute("id");
    expect(panelId).toBeTruthy();
    expect(generalTab.getAttribute("aria-controls")).toBe(panelId);
    expect(panel.getAttribute("aria-labelledby")).toBe(generalTab.getAttribute("id"));

    // 选中「远程访问」后接线跟着走，且同一时刻只有一个面板。
    fireEvent.click(screen.getByRole("tab", { name: "远程访问" }));
    const remoteTab = screen.getByRole("tab", { name: "远程访问" });
    const remotePanel = screen.getByRole("tabpanel");
    expect(remoteTab.getAttribute("aria-controls")).toBe(remotePanel.getAttribute("id"));
    expect(remotePanel.getAttribute("aria-labelledby")).toBe(remoteTab.getAttribute("id"));
    expect(screen.getAllByRole("tabpanel")).toHaveLength(1);
  });

  it("moves selection with ArrowRight and keeps roving tabIndex", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    const generalTab = screen.getByRole("tab", { name: "通用" });
    const remoteTab = screen.getByRole("tab", { name: "远程访问" });
    // tablist 只占一个 Tab 停靠点：选中的是 0，其余是 -1。
    expect(generalTab.getAttribute("tabindex")).toBe("0");
    expect(remoteTab.getAttribute("tabindex")).toBe("-1");

    fireEvent.keyDown(generalTab, { key: "ArrowRight" });
    expect(remoteTab.getAttribute("aria-selected")).toBe("true");
    expect(remoteTab.getAttribute("tabindex")).toBe("0");
    expect(generalTab.getAttribute("tabindex")).toBe("-1");
    // 焦点跟着选区走，键盘用户才看得到。
    expect(document.activeElement).toBe(remoteTab);
  });
});
