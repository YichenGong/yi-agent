import { useEffect, useRef, useState } from "react";
import { listPlugins, type PluginRpc, type PluginSummary } from "../lib/pluginSettings";
import { PluginCatalog } from "./PluginCatalog";

/**
 * 宿主注入的插件 RPC 接缝。
 *
 * 非泛型签名（与 SettingsDialog 的 `pluginCall` 同形）：宿主传进来的与测试里
 * 手写的普通函数都能直接赋值。运行时形状与 `PluginRpc` 一致，缺的只是编译期的
 * 「调用方自选返回类型」，见下面的一次断言。
 */
export type PluginsCall = (method: string, params: unknown) => Promise<unknown>;

/**
 * 主程序**不**知道任何插件名对应的设置组件。
 *
 * 这里曾经有一张 `插件名 → 组件` 的表，加一个插件就得改一次主程序；而且它只能
 * 覆盖主程序自己打包进来的组件，装上去的插件永远走不到。现在每个插件的设置页
 * 由插件自己声明、由 `PluginCatalog` 经受管视图打开，主程序只提供"打开它"这个
 * 通用动作。
 */

/**
 * 「插件」Tab。
 *
 * 插件清单按项目安装（`<项目>/.yi-agent/supervisors/`），而桌面侧车的 app-server
 * 以自己的 workdir（用户 home）为作用域，所以这里必须让用户明确「在配置哪个
 * 项目」，并把它随每次 `plugins/list`、`plugin/settings/*` 传给宿主。少了这一步，
 * 看板跑着、设置页却是空的。
 */
export function SettingsPluginsTab({
  call,
  projects,
  defaultProject,
  projectHandle,
}: {
  call?: PluginsCall;
  /** 可选项目（绝对路径）。空表时面板回落宿主 workdir，行为与旧版一致。 */
  projects: string[];
  /** 首选项目（当前对话的工作目录）。不在 `projects` 里时回落到第一个项目。 */
  defaultProject?: string;
  /**
   * 当前项目的受管键。
   *
   * 与上面的**路径**是两个协议：旧 supervisor 协议按路径装清单，受管协议按
   * `ProjectId` 索引授权，而 `ProjectId` 由 `ProjectResolver::register` 取
   * **规范化后的绝对路径**（`yi-agent-plugins/src/project.rs`），不是 spec
   * 早期草案里的 `ph1_...`。两者在这里的取值恰好相同，但语义仍是"受管键"而
   * 非"旧协议路径"，缺省时不显示受管分区，而不是拿一个猜出来的键去冒充。
   */
  projectHandle?: string;
}) {
  const [plugins, setPlugins] = useState<PluginSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string>(() => defaultProject ?? projects[0] ?? "");

  // 接缝在运行时就是 PluginRpc 的形状；这里一次性补回调用方自选返回类型的
  // 泛型能力，供下面的加载器与面板共用——模块里只保留这一处断言。
  const rpc = call as PluginRpc;

  // 选项 = 候选 ∪ 默认项目（当前对话目录可能不在候选里，若只列候选就无法选中）。
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

  // `project` 进 ref，理由同插件面板：宿主每次重渲染可能换新接缝身份，但我们只
  // 想在「项目真的换了」时重新枚举插件列表。
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
      setPlugins([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await listPlugins(rpc, projectRef.current);
        if (!cancelled) setPlugins(loaded);
      } catch {
        if (!cancelled) setError("无法读取插件列表");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc, selected]);

  if (error) {
    return <p role="alert" className="p-4 text-xs text-amber-500">{error}</p>;
  }

  return (
    <div>
      {/*
        受管插件分区：由管理 API（`plugins/v1/*`）驱动，与下面那组旧的
        supervisor 插件互不替代。把它放在最前，是因为"装了什么、授权给谁、
        能不能跑"是用户先要回答的问题；旧面板是具体插件的业务配置。
      */}
      {projectHandle && (
        <section className="border-b border-line">
          <h2 className="px-4 pt-4 text-sm font-medium text-fg">受管插件</h2>
          <PluginCatalog call={rpc} project={projectHandle} />
        </section>
      )}
      {options.length > 0 && (
        <label className="flex items-center gap-3 border-b border-line p-4 text-sm">
          <span>项目</span>
          <select
            aria-label="插件设置项目"
            className="min-w-0 flex-1 rounded border border-line bg-surface px-2 py-1 font-mono text-xs"
            value={selected}
            onChange={(e) => setSelected(e.target.value)}
          >
            {options.map((project) => (
              <option key={project} value={project}>
                {project}
              </option>
            ))}
          </select>
        </label>
      )}
      {plugins === null ? (
        <p className="p-4 text-xs text-fg-subtle">正在读取插件…</p>
      ) : plugins.length === 0 ? (
        <p className="p-4 text-sm text-fg-muted">未安装任何插件</p>
      ) : (
        <div className="divide-y divide-line">
          {plugins.map((plugin) => (
            <section key={plugin.name} className="p-4">
              <h2 className="text-sm font-medium text-fg">
                {plugin.name}
                {plugin.enabled ? null : (
                  // 仍列出来：用户要能看见它、去「实验功能」把它打开。这里只标状态，
                  // 不藏面板——藏了就等于让人没法从这一页判断它为什么没在跑。
                  <span className="ml-2 text-xs font-normal text-fg-subtle">
                    （已停用：在「实验功能」里打开）
                  </span>
                )}
              </h2>
              <p className="mt-2 text-xs text-fg-subtle">
                该插件的设置由它自己提供，见上方「受管插件」里的设置入口。
              </p>
            </section>
          ))}
        </div>
      )}
    </div>
  );
}
