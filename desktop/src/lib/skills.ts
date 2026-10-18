/**
 * 桌面与宿主之间的 skill 通道。形状与 `PluginRpc` / `ModelRpc` 一致。
 *
 * 服务端返回的字段名保持原样（`groups` / `skills` / `roots` 与 `scope`），本模块
 * 只做一次形状收敛：坏行丢掉、缺失字段补空，绝不把半成品往下传——面板宁可少列，
 * 也不能因为一条脏数据整页崩掉（与 `pluginSettings.ts` 的 `parseDays` 同一取向）。
 */
export type SkillsRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/**
 * 非泛型的宿主/测试接缝（与 `PluginsCall` 同形的写法）。
 *
 * `SkillsRpc` 的「调用方自选返回类型」在编译期很好用，但 `vi.fn(async () => ...)`
 * 推断不出泛型函数，直接赋给 `SkillsRpc` 会报 TS2322。测试与宿主用这个非泛型形状，
 * 面板内部一次性断言补回泛型能力——与 `SettingsPluginsTab` 的既有做法一致。
 */
export type SkillsCall = (method: string, params: unknown) => Promise<unknown>;

/** 三层 skill 根，与服务端 `roots` 的键一一对应。 */
export type SkillScope = "project" | "user" | "system";

/** 分组标题：系统内置的 3 个不是用户装的，措辞上要区分开。 */
export const SCOPE_LABELS: Record<SkillScope, string> = {
  project: "项目",
  user: "用户",
  system: "系统内置",
};

export interface SkillSummary {
  name: string;
  description: string;
  path: string;
}

export interface SkillGroup {
  scope: SkillScope;
  skills: SkillSummary[];
}

export interface SkillRoots {
  project: string;
  user: string;
  system: string;
}

export interface SkillList {
  groups: SkillGroup[];
  roots: SkillRoots;
}

function asScope(value: unknown): SkillScope | undefined {
  return value === "project" || value === "user" || value === "system" ? value : undefined;
}

function asSkill(row: unknown): SkillSummary | undefined {
  if (typeof row !== "object" || row === null) return undefined;
  const r = row as Record<string, unknown>;
  if (typeof r.name !== "string" || r.name === "") return undefined;
  return {
    name: r.name,
    description: typeof r.description === "string" ? r.description : "",
    path: typeof r.path === "string" ? r.path : "",
  };
}

/** 枚举 skill。`project` 指定看哪个项目的项目级根；缺省由服务端回落宿主 workdir。 */
export async function listSkills(rpc: SkillsRpc, project?: string): Promise<SkillList> {
  const result = await rpc<{ groups?: unknown; roots?: unknown }>("skill/list", {
    ...(project ? { project } : {}),
  });
  const rows = Array.isArray(result?.groups) ? result.groups : [];
  const groups: SkillGroup[] = [];
  for (const row of rows) {
    if (typeof row !== "object" || row === null) continue;
    const r = row as Record<string, unknown>;
    const scope = asScope(r.scope);
    if (!scope) continue;
    const skills = (Array.isArray(r.skills) ? r.skills : [])
      .map(asSkill)
      .filter((s): s is SkillSummary => s !== undefined);
    groups.push({ scope, skills });
  }
  const rawRoots = (typeof result?.roots === "object" && result.roots !== null
    ? result.roots
    : {}) as Record<string, unknown>;
  const text = (value: unknown) => (typeof value === "string" ? value : "");
  return {
    groups,
    roots: {
      project: text(rawRoots.project),
      user: text(rawRoots.user),
      system: text(rawRoots.system),
    },
  };
}

/** 读某个 SKILL.md 的正文（服务端已剥掉 frontmatter）。 */
export async function readSkill(
  rpc: SkillsRpc,
  path: string,
  project?: string,
): Promise<string> {
  const result = await rpc<{ body?: unknown }>("skill/read", {
    path,
    ...(project ? { project } : {}),
  });
  if (typeof result?.body !== "string") throw new Error("the server returned no body");
  return result.body;
}
