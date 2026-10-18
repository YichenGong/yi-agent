/** @vitest-environment jsdom */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { SettingsSkillsTab } from "./SettingsSkillsTab";
import type { SkillsCall } from "../lib/skills";

/** `vi.fn` 推断不出泛型签名，按既有约定一次性断言（同 `pluginSettings.test.ts`）。 */
const seam = (fn: unknown) => fn as unknown as SkillsCall;

afterEach(cleanup);

const LIST = {
  groups: [
    { scope: "project", skills: [{ name: "proj-skill", description: "项目级。", path: "/proj/SKILL.md" }] },
    { scope: "user", skills: [{ name: "user-skill", description: "用户级。", path: "/home/u/SKILL.md" }] },
    { scope: "system", skills: [{ name: "skill-creator", description: "内置。", path: "/home/.system/c/SKILL.md" }] },
  ],
  roots: {
    project: "/proj/.yi-agent/skills",
    user: "/home/.yi-agent/skills",
    system: "/home/.yi-agent/skills/.system",
  },
};

describe("SettingsSkillsTab", () => {
  it("lists every group with its Chinese heading", async () => {
    const call = vi.fn(async () => LIST);
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByText("proj-skill")).toBeTruthy();
    expect(screen.getByText("项目")).toBeTruthy();
    expect(screen.getByText("用户")).toBeTruthy();
    expect(screen.getByText("系统内置")).toBeTruthy();
  });

  it("hides a group that has no skills", async () => {
    const call = vi.fn(async () => ({ ...LIST, groups: [LIST.groups[1]] }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByText("user-skill")).toBeTruthy();
    expect(screen.queryByText("项目")).toBeNull();
  });

  it("shows an empty state when nothing is installed", async () => {
    const call = vi.fn(async () => ({ groups: [], roots: LIST.roots }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByText("还没有安装任何技能")).toBeTruthy();
  });

  it("reports a list failure without crashing", async () => {
    const call = vi.fn(async () => {
      throw { code: -32603, message: "boom" };
    });
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    expect(await screen.findByRole("alert")).toBeTruthy();
  });

  it("renders nothing but a notice without a host seam", async () => {
    render(<SettingsSkillsTab projects={[]} />);
    expect(await screen.findByText("当前连接无法读取技能列表")).toBeTruthy();
  });

  it("fetches the body only when a row is expanded", async () => {
    const call = vi.fn(async (method: string) => (method === "skill/list" ? LIST : { body: "# user-skill\n\n正文" }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    const row = await screen.findByRole("button", { name: /user-skill/ });
    expect(row.getAttribute("aria-expanded")).toBe("false");
    // 折叠时正文不在 DOM 里，也从未请求过。
    expect(call.mock.calls.filter(([m]) => m === "skill/read")).toHaveLength(0);

    fireEvent.click(row);
    expect(await screen.findByText("正文")).toBeTruthy();
    expect(call.mock.calls.filter(([m]) => m === "skill/read")).toHaveLength(1);
    expect(row.getAttribute("aria-expanded")).toBe("true");
  });

  it("does not re-fetch after collapsing and expanding again", async () => {
    const call = vi.fn(async (method: string) => (method === "skill/list" ? LIST : { body: "正文" }));
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    const row = await screen.findByRole("button", { name: /user-skill/ });
    fireEvent.click(row);
    await screen.findByText("正文");
    fireEvent.click(row);
    fireEvent.click(row);
    await waitFor(() =>
      expect(call.mock.calls.filter(([m]) => m === "skill/read")).toHaveLength(1),
    );
  });

  it("surfaces a read failure instead of showing an empty body", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "skill/list") return LIST;
      throw { code: -32602, message: "path escapes the skill roots" };
    });
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    fireEvent.click(await screen.findByRole("button", { name: /user-skill/ }));
    expect(await screen.findByText(/无法读取该技能/)).toBeTruthy();
  });

  it("reloads when the project changes", async () => {
    // 显式声明两个参数:否则 `vi.fn` 的 calls 元组是空类型,下面的 `[m]`/`[, p]` 取不到。
    const call = vi.fn(async (_method: string, _params: unknown) => LIST);
    render(<SettingsSkillsTab call={seam(call)} projects={["/a", "/b"]} defaultProject="/a" />);
    await screen.findByText("user-skill");
    fireEvent.change(screen.getByLabelText("技能设置项目"), { target: { value: "/b" } });
    await waitFor(() => {
      const scopes = call.mock.calls
        .filter(([m]) => m === "skill/list")
        .map(([, p]) => (p as { project?: string }).project);
      expect(scopes).toEqual(["/a", "/b"]);
    });
  });

  it("explains how to add a skill, using the server's real paths", async () => {
    const call = vi.fn(async () => LIST);
    render(<SettingsSkillsTab call={seam(call)} projects={[]} />);
    const heading = await screen.findByText("如何添加 Skill");
    // 断言整段帮助区的文本：三条目录行都含 `/home/.yi-agent/skills` 前缀，逐个查会
    // 命中多个元素；这里要验的是「路径确实来自服务端响应，而不是硬编码 `~`」。
    const help = heading.closest("section")?.textContent ?? "";
    expect(help).toContain("/proj/.yi-agent/skills");
    expect(help).toContain("/home/.yi-agent/skills");
    expect(help).toContain("/home/.yi-agent/skills/.system");
    expect(help).toContain("skill-creator");
    expect(help).toContain("skill-installer");
  });
});
