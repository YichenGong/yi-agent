import { useCallback, useEffect, useRef, useState } from "react";
import {
  listSkills,
  readSkill,
  SCOPE_LABELS,
  type SkillGroup,
  type SkillList,
  type SkillsCall,
  type SkillsRpc,
} from "../lib/skills";
import { SettingsProjectPicker } from "./SettingsProjectPicker";
import { MarkdownText } from "./MarkdownText";

const EMPTY: SkillList = {
  groups: [],
  roots: { project: "", user: "", system: "" },
};

/**
 * 「技能」Tab。
 *
 * 三层根（项目 / 用户 / 系统）由服务端按 `project` 解析——桌面侧车的 workdir 是
 * home，所以必须让用户明确「在看哪个项目」，理由与「插件」Tab 完全相同。
 *
 * 正文**点开才拉**：列表响应里不含 body，一条 skill 的正文可能有十几 KB，全部预取
 * 只会让打开设置变慢。
 */
export function SettingsSkillsTab({
  call,
  projects,
  defaultProject,
}: {
  call?: SkillsCall;
  /** 可选项目（绝对路径）。空表时面板回落宿主 workdir。 */
  projects: string[];
  /** 首选项目（当前对话的工作目录）。不在 `projects` 里时回落到第一个。 */
  defaultProject?: string;
}) {
  const [listed, setListed] = useState<SkillList>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [listError, setListError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string>(() => defaultProject ?? projects[0] ?? "");
  /** path → 正文。折叠不丢，避免反复展开重复拉取。 */
  const [bodies, setBodies] = useState<Record<string, string>>({});
  /** path → 读取失败原因。 */
  const [bodyErrors, setBodyErrors] = useState<Record<string, string>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const rpc = call as SkillsRpc | undefined;

  // 选项 = 候选 ∪ 默认项目（当前对话目录可能不在候选里，只列候选就无法选中）。
  const options = (() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const path of [defaultProject, ...projects]) {
      const trimmed = path?.trim();
      if (!trimmed || seen.has(trimmed)) continue;
      seen.add(trimmed);
      out.push(trimmed);
    }
    return out;
  })();

  // `project` 进 ref，理由同插件面板：宿主每次重渲染可能换新接缝身份，但我们只在
  // 「项目真的换了」时重新枚举。
  const projectRef = useRef(selected);
  projectRef.current = selected;

  // 项目迟到（workspaces 异步到达）时补一个默认值；用户已选定的不覆盖。
  useEffect(() => {
    if (selected !== "") return;
    const next = defaultProject ?? projects[0] ?? "";
    if (next !== "") setSelected(next);
  }, [defaultProject, projects, selected]);

  useEffect(() => {
    if (!rpc) {
      setLoading(false);
      return;
    }
    let cancelled = false;
    setLoading(true);
    setListError(null);
    void (async () => {
      try {
        const result = await listSkills(rpc, projectRef.current || undefined);
        if (!cancelled) setListed(result);
      } catch {
        if (!cancelled) setListError("无法读取技能列表");
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc, selected]);

  /** 展开时按需拉正文；已有正文或已有错误则不重复请求。 */
  const toggle = useCallback(
    (path: string) => {
      const willExpand = !expanded[path];
      setExpanded((e) => ({ ...e, [path]: willExpand }));
      if (!willExpand || !rpc || bodies[path] !== undefined || bodyErrors[path] !== undefined) {
        return;
      }
      void (async () => {
        try {
          const body = await readSkill(rpc, path, projectRef.current || undefined);
          setBodies((b) => ({ ...b, [path]: body }));
        } catch {
          setBodyErrors((e) => ({ ...e, [path]: "无法读取该技能（文件可能已被删除或移动）" }));
        }
      })();
    },
    [expanded, rpc, bodies, bodyErrors],
  );

  if (!call) {
    return <p className="p-4 text-xs text-fg-subtle">当前连接无法读取技能列表</p>;
  }
  if (listError) {
    return (
      <p role="alert" className="p-4 text-xs text-amber-500">
        {listError}
      </p>
    );
  }

  const visible: SkillGroup[] = listed.groups.filter((g) => g.skills.length > 0);

  return (
    <div>
      <SettingsProjectPicker
        options={options}
        selected={selected}
        onSelect={setSelected}
        ariaLabel="技能设置项目"
      />
      {loading ? (
        <p className="p-4 text-xs text-fg-subtle">正在读取技能…</p>
      ) : visible.length === 0 ? (
        <p className="p-4 text-sm text-fg-muted">还没有安装任何技能</p>
      ) : (
        <div className="divide-y divide-line">
          {visible.map((group) => (
            <section key={group.scope} className="p-4">
              <h3 className="text-xs font-medium text-fg-subtle">{SCOPE_LABELS[group.scope]}</h3>
              <ul className="mt-2">
                {group.skills.map((skill) => {
                  const open = expanded[skill.path] === true;
                  return (
                    <li key={skill.path} className="border-b border-line/50 last:border-b-0">
                      {/* 折叠范式与 ChatView 的「思考」块一致：aria-expanded + ▸/▾。 */}
                      <button
                        type="button"
                        aria-expanded={open}
                        onClick={() => toggle(skill.path)}
                        className="flex w-full items-start gap-2 px-1 py-2 text-left"
                      >
                        <span aria-hidden="true" className="text-fg-subtle">
                          {open ? "▾" : "▸"}
                        </span>
                        <span className="min-w-0">
                          <span className="block text-sm font-medium text-fg">{skill.name}</span>
                          <span className="block text-xs text-fg-subtle">{skill.description}</span>
                        </span>
                      </button>
                      {open && (
                        <div className="px-1 pb-3 pl-6">
                          {bodyErrors[skill.path] ? (
                            <p role="alert" className="text-xs text-amber-500">
                              {bodyErrors[skill.path]}
                            </p>
                          ) : bodies[skill.path] === undefined ? (
                            <p className="text-xs text-fg-subtle">正在读取…</p>
                          ) : (
                            <div className="prose prose-sm max-w-none text-fg">
                              <MarkdownText text={bodies[skill.path]} />
                            </div>
                          )}
                        </div>
                      )}
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
        </div>
      )}
      <AddSkillHelp roots={listed.roots} />
    </div>
  );
}

/**
 * 「如何添加 Skill」说明。
 *
 * 路径一律取服务端返回的**真实解析值**，不硬编码 `~/.yi-agent/skills`：路径规则若
 * 变动，硬编码的文案会与事实脱节。
 */
function AddSkillHelp({ roots }: { roots: SkillList["roots"] }) {
  // 标签带「目录」后缀：上方的分组标题已经占用了「系统内置」，重名会让两者在
  // 可访问树与测试查询里无法区分。
  const directories = [
    { label: "项目级目录", path: roots.project, note: "只对这个项目生效" },
    { label: "用户级目录", path: roots.user, note: "对本机所有项目生效" },
    { label: "系统内置目录", path: roots.system, note: "随应用安装，升级时会被覆盖" },
  ].filter((d) => d.path !== "");

  return (
    <section className="border-t border-line p-4">
      <h3 className="text-sm font-medium text-fg">如何添加 Skill</h3>
      <p className="mt-2 text-xs text-fg-muted">
        把一个名为 <code className="font-mono">SKILL.md</code> 的文件放进下面任一层目录的
        子目录里即可生效（子目录名就是技能名）。下一轮对话开始时自动加载，无需重启。
      </p>
      <ul className="mt-3 space-y-1">
        {directories.map((d) => (
          <li key={d.label} className="text-xs text-fg-subtle">
            <span className="text-fg-muted">{d.label}</span>：
            <code className="font-mono">{d.path}/&lt;技能名&gt;/SKILL.md</code>
            <span className="ml-2">{d.note}</span>
          </li>
        ))}
      </ul>
      <p className="mt-3 text-xs text-fg-muted">
        技能名只能用小写字母、数字与连字符（如 <code className="font-mono">my-skill</code>
        ），最长 64 个字符。文件开头必须有 frontmatter：
      </p>
      <pre className="mt-2 overflow-x-auto rounded border border-line bg-surface p-2 font-mono text-xs text-fg-muted">
{`---
name: my-skill
description: 一句话说明这个技能做什么、什么时候该用。
---

正文：模型加载这个技能后会照做的操作步骤。`}
      </pre>
      <p className="mt-3 text-xs text-fg-subtle">
        也可以直接对话让 Yi-Agent 帮你写：用内置的 <code className="font-mono">skill-creator</code>
        {" "}创建新技能，或用内置的 <code className="font-mono">skill-installer</code>
        {" "}从外部仓库安装现成的技能。
      </p>
    </section>
  );
}
