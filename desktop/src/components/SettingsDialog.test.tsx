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

  it("shows a 模型 tab backed by the injected modelCall", async () => {
    const modelCall = async (method: string) =>
      method === "model/list" ? { models: [], default_model: null, subagent_model: null } : { ok: true };
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        modelCall={modelCall}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "模型" }));
    expect(await screen.findByText("还没有配置任何模型")).toBeTruthy();
  });

it("shows a 技能 tab backed by the injected skillsCall", async () => {
    const skillsCall = async (method: string) =>
      method === "skill/list"
        ? {
            groups: [{ scope: "user", skills: [{ name: "user-skill", description: "用户级。", path: "/u/SKILL.md" }] }],
            roots: { project: "/p", user: "/u", system: "/s" },
          }
        : { body: "" };
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        skillsCall={skillsCall}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "技能" }));
    expect(await screen.findByText("user-skill")).toBeTruthy();
  });

  it("lists all five categories on the desktop", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    const names = screen.getAllByRole("tab").map((t) => t.textContent);
    expect(names).toEqual(["通用", "远程访问", "插件", "模型", "技能"]);
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

describe("SettingsDialog wires phone layout down to the models tab", () => {
  it("passes isMobile to the models panel, so it renders cards not a table", async () => {
    // 接线缺口：`SettingsModelsTab` 有了 `isMobile`，但如果 `SettingsDialog` 不把它
    // 传下去，模型 Tab 在手机上仍会渲染那张固有 463px 的表格、继续横向溢出。这条
    // 用例穿过真实接线（Dialog → Tab），比只测子组件更能挡住这类断线。
    const modelCall = async (method: string) =>
      method === "model/list"
        ? {
            models: [
              { name: "A", provider: "anthropic", api_url: "u", model: "m", has_key: true, api_key_masked: "••••1234" },
            ],
            default_model: null,
            subagent_model: null,
            effective: null,
          }
        : { ok: true };
    const { container } = render(
      <SettingsDialog
        open
        isMobile
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        modelCall={modelCall}
      />,
    );
    // 手机端没有 tablist，改从分类列表进入「模型」。
    fireEvent.click(
      document.querySelector('[data-settings-category="models"]') as HTMLElement,
    );
    await screen.findByText("••••1234");
    expect(container.querySelector("[data-model-card]")).toBeTruthy();
    expect(container.querySelector("table")).toBeNull();
  });
});

// 手机端把设置从「居中弹窗 + 左侧竖栏」改成「全屏页 + 顶部横条」：导航方向随之由
// 竖变横，`aria-orientation` 必须跟着翻——这是语义，CSS 表达不了。挂点类是外观
// 覆写（index.css 里 `[data-mobile="true"] .settings-*`）的抓手，两个平台都渲染，
// 桌面无样式命中。
describe("SettingsDialog phone layout hooks", () => {
/** 手机上按分类 id 取那一行（文本里含 `›` 装饰，故不按文本查）。 */
  const categoryRow = (id: string) =>
    document.querySelector(`[data-settings-category="${id}"]`) as HTMLElement;

  it("renders a category list, not a tablist, on the phone", () => {
    // 手机上分类很多，等宽横条放不下（5 格时 320pt 每格仅 64px，「远程访问」四字
    // 会溢出）。改成「分类列表 → 点进内容」的两级结构，横向宽度不再是约束。
    // 同时必须真的没有 tablist：留着一个隐藏的 tablist 会与可见列表争可访问名。
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(screen.queryByRole("tablist")).toBeNull();
    expect(screen.queryByRole("tab")).toBeNull();
    expect(document.querySelector(".settings-categories")).toBeTruthy();
    const rows = Array.from(document.querySelectorAll("[data-settings-category]"));
    expect(rows.map((r) => r.getAttribute("data-settings-category"))).toEqual([
      "general",
      "remote",
      "plugins",
      "models",
      "skills",
    ]);
  });

  it("drills into a category and comes back", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    fireEvent.click(categoryRow("general"));
    // 子页：返回按钮 + 该分类的面板（「通用」里是主题选择）。
    expect(screen.getByRole("button", { name: "返回设置分类" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "深色" })).toBeTruthy();
    // 列表已不在。
    expect(document.querySelector(".settings-categories")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "返回设置分类" }));
    expect(document.querySelector(".settings-categories")).toBeTruthy();
  });

  it("returns focus to the category row it came from", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    fireEvent.click(categoryRow("models"));
    fireEvent.click(screen.getByRole("button", { name: "返回设置分类" }));
    // 返回后焦点落在刚才那一行，键盘/读屏用户不用重新找位置。
    expect(document.activeElement).toBe(categoryRow("models"));
  });

  it("starts on the category list every time settings opens", () => {
    // 上次停在哪个子页与本次打开无关；留着会让人以为设置只有那一页。
    const { rerender } = render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    fireEvent.click(categoryRow("models"));
    expect(document.querySelector(".settings-categories")).toBeNull();
    rerender(
      <SettingsDialog open={false} isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    rerender(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    expect(document.querySelector(".settings-categories")).toBeTruthy();
  });

  it("renders the class hooks the phone CSS targets, on both platforms", () => {
    const { container, rerender } = render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={() => {}} />,
    );
    // 手机端首页是分类列表：shell 与 header 齐备，导航条与面板都不渲染
    // （面板要等点进某个分类才出现，见下一条用例）。
    expect(container.querySelector(".settings-shell")).toBeTruthy();
    expect(container.querySelector(".settings-header")).toBeTruthy();
    expect(container.querySelector(".settings-nav")).toBeNull();
    expect(container.querySelector(".settings-panel")).toBeNull();
    expect(container.querySelector(".settings-categories")).toBeTruthy();

    // 桌面端四类挂点齐备（`.settings-categories` 是手机专属，桌面不渲染）。
    rerender(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(container.querySelector(".settings-shell")).toBeTruthy();
    expect(container.querySelector(".settings-header")).toBeTruthy();
    expect(container.querySelector(".settings-nav")).toBeTruthy();
    expect(container.querySelector(".settings-panel")).toBeTruthy();
    expect(container.querySelector(".settings-categories")).toBeNull();
  });

  it("keeps the vertical orientation on the desktop", () => {
    render(<SettingsDialog open theme="dark" onThemeChange={() => {}} onClose={() => {}} />);
    expect(screen.getByRole("tablist").getAttribute("aria-orientation")).toBe("vertical");
  });

  it("keeps the close button reachable on the phone (44px touch target)", () => {
    render(
      <SettingsDialog open isMobile theme="dark" onThemeChange={() => {}} onClose={vi.fn()} />,
    );
    const close = screen.getByRole("button", { name: "关闭设置" });
    // 手机上遮罩被 CSS 隐藏，关闭只能靠这颗按钮或 Escape——触控目标不能还是桌面的
    // 小尺寸。44px 是 Apple HIG 下限，与抽屉按钮 `size-11` 同一标准。
    expect(close.className).toContain("size-11");
  });
});
