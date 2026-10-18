/** @vitest-environment node */
import { describe, expect, it } from "vitest";
import { listSkills, readSkill, SCOPE_LABELS, type SkillsRpc } from "./skills";

/** 非泛型测试接缝 → `SkillsRpc`：与 `pluginSettings.test.ts` 同一约定（那里是 `as unknown as Rpc`）。 */
const rpc = (fn: (method: string, params: unknown) => Promise<unknown>) =>
  fn as unknown as SkillsRpc;

/** 服务端原样形状的响应（snake 无、都是既有字段名）。 */
const response = {
  groups: [
    { scope: "project", skills: [{ name: "p", description: "项目级。", path: "/proj/p/SKILL.md" }] },
    { scope: "user", skills: [{ name: "u", description: "用户级。", path: "/home/u/SKILL.md" }] },
    { scope: "system", skills: [{ name: "s", description: "系统级。", path: "/home/.system/s/SKILL.md" }] },
  ],
  roots: { project: "/proj/.yi-agent/skills", user: "/home/.yi-agent/skills", system: "/home/.yi-agent/skills/.system" },
};

describe("listSkills", () => {
  it("keeps the server's group order and fields", async () => {
    const seen: Array<{ method: string; params: unknown }> = [];
    const client = async (method: string, params: unknown) => {
      seen.push({ method, params });
      return response;
    };
    const listed = await listSkills(rpc(client));
    expect(seen[0].method).toBe("skill/list");
    expect(listed.groups.map((g) => g.scope)).toEqual(["project", "user", "system"]);
    expect(listed.groups[1].skills[0].name).toBe("u");
    expect(listed.roots.user).toBe("/home/.yi-agent/skills");
  });

  it("passes the chosen project through", async () => {
    const seen: string[] = [];
    const client = async (_method: string, params: unknown) => {
      seen.push((params as { project?: string }).project ?? "");
      return response;
    };
    await listSkills(rpc(client), "/proj");
    expect(seen).toEqual(["/proj"]);
  });

  it("drops malformed rows instead of crashing", async () => {
    // 坏数据宁可少列，也不能让整个面板崩掉——与 parseDays 同一取向。
    const client = async () => ({
      groups: [
        { scope: "project", skills: [{ name: "ok", description: "x", path: "/p" }, { description: "no name" }] },
        { scope: "nonsense", skills: [{ name: "z", description: "x", path: "/z" }] },
      ],
      roots: {},
    });
    const listed = await listSkills(rpc(client));
    expect(listed.groups.map((g) => g.scope)).toEqual(["project"]);
    expect(listed.groups[0].skills.map((s) => s.name)).toEqual(["ok"]);
  });

  it("returns an empty list when the server sends nothing", async () => {
    const client = async () => ({});
    const listed = await listSkills(rpc(client));
    expect(listed.groups).toEqual([]);
    expect(listed.roots).toEqual({ project: "", user: "", system: "" });
  });
});

describe("readSkill", () => {
  it("returns the body", async () => {
    const client = (method: string, params: unknown) => {
      expect(method).toBe("skill/read");
      expect((params as { path: string }).path).toBe("/home/u/SKILL.md");
      return Promise.resolve({ body: "# u\n\n正文" });
    };
    expect(await readSkill(rpc(client), "/home/u/SKILL.md")).toBe("# u\n\n正文");
  });

  it("throws when the server sends no body", async () => {
    const client = async () => ({});
    await expect(readSkill(rpc(client), "/x/SKILL.md")).rejects.toThrow(/no body/);
  });
});

describe("SCOPE_LABELS", () => {
  it("names every scope in Chinese", () => {
    expect(SCOPE_LABELS.project).toBe("项目");
    expect(SCOPE_LABELS.user).toBe("用户");
    expect(SCOPE_LABELS.system).toBe("系统内置");
  });
});
