import { useEffect, useRef, useState } from "react";
import {
  listMcpServers,
  type McpCall,
  type McpList,
  type McpRpc,
  type McpServerView,
} from "../lib/mcp";
import { SettingsProjectPicker } from "./SettingsProjectPicker";

const EMPTY: McpList = { configured: false, path: "", enabled: false, servers: [] };

/** stdio 的一行命令摘要（等宽显示、单行截断）。 */
function commandSummary(s: McpServerView): string {
  return [s.command ?? "", ...s.args].join(" ").trim();
}

/**
 * 「MCP」Tab。
 *
 * MCP 配置是**项目级文件**（`<项目>/.yi-agent/mcp.json`），而桌面侧车的 app-server
 * 以自己的 workdir（用户 home）为作用域，所以必须让用户明确「在看哪个项目」，并把它
 * 随每次 `mcp/list` 传下去——理由与「插件」「技能」两个 Tab 完全相同。
 *
 * **只读**：工具清单来自磁盘缓存（首次调用某个 server 时才探测），缓存缺失或指纹
 * 不匹配时显示「尚未探测」，而不是假装清单是最新的。不提供启停开关。
 */
export function SettingsMcpTab({
  call,
  projects,
  defaultProject,
}: {
  call?: McpCall;
  /** 可选项目（绝对路径）。空表时面板回落宿主 workdir。 */
  projects: string[];
  /** 首选项目（当前对话的工作目录）。不在 `projects` 里时回落到第一个。 */
  defaultProject?: string;
}) {
  const [listed, setListed] = useState<McpList>(EMPTY);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string>(() => defaultProject ?? projects[0] ?? "");
  /** server 名 → 是否展开工具清单。折叠状态不丢，避免反复展开时重复拉取。 */
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});

  const rpc = call as McpRpc | undefined;

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

  // `project` 进 ref，理由同插件/技能面板：宿主每次重渲染可能换新接缝身份，但我们
  // 只在「项目真的换了」时重新枚举。
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
    setError(null);
    void (async () => {
      try {
        const result = await listMcpServers(rpc, projectRef.current || undefined);
        if (!cancelled) setListed(result);
      } catch {
        if (!cancelled) setError("无法读取 MCP 配置");
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc, selected]);

  if (!call) {
    return <p className="p-4 text-xs text-fg-subtle">当前连接无法读取 MCP 配置</p>;
  }
  if (error) {
    return (
      <p role="alert" className="p-4 text-xs text-amber-500">
        {error}
      </p>
    );
  }

  const visible = listed.servers;

  return (
    <div>
      <SettingsProjectPicker
        options={options}
        selected={selected}
        onSelect={setSelected}
        ariaLabel="MCP 设置项目"
      />
      {loading ? (
        <p className="p-4 text-xs text-fg-subtle">正在读取 MCP 配置…</p>
      ) : !listed.configured || visible.length === 0 ? (
        <div>
          <p className="p-4 text-sm text-fg-muted">未安装任何 MCP server</p>
          <AddMcpHelp path={listed.path} />
        </div>
      ) : (
        <>
          <p className="border-b border-line p-4 text-xs text-fg-subtle">
            MCP：{listed.enabled ? "已启用" : "已停用"}
          </p>
          <div className="divide-y divide-line">
            {visible.map((server) => {
              const open = expanded[server.name] === true;
              return (
                <section key={server.name} className="p-4">
                  <div className="flex items-center gap-2">
                    <h2 className="min-w-0 truncate text-sm font-medium text-fg">{server.name}</h2>
                    <span className="shrink-0 rounded bg-raised px-1.5 py-0.5 text-[10px] text-fg-subtle">
                      {server.enabled ? "已启用" : "已停用"}
                    </span>
                  </div>
                  <p className="mt-1 truncate font-mono text-xs text-fg-subtle">
                    {server.kind === "http" ? (server.url ?? "") : commandSummary(server)}
                  </p>
                  {(server.envKeys.length > 0 || server.headerKeys.length > 0) && (
                    <p className="mt-1 truncate text-xs text-fg-subtle">
                      {server.envKeys.length > 0 && <>env: {server.envKeys.join(", ")}</>}
                      {server.envKeys.length > 0 && server.headerKeys.length > 0 && " · "}
                      {server.headerKeys.length > 0 && <>header: {server.headerKeys.join(", ")}</>}
                    </p>
                  )}
                  {/* 折叠范式与技能 Tab 一致：aria-expanded + ▸/▾。 */}
                  <button
                    type="button"
                    aria-expanded={open}
                    aria-label={`展开 ${server.name} 的工具清单`}
                    onClick={() => setExpanded((e) => ({ ...e, [server.name]: !open }))}
                    className="mt-2 flex w-full items-start gap-2 px-1 py-1 text-left"
                  >
                    <span aria-hidden="true" className="text-fg-subtle">
                      {open ? "▾" : "▸"}
                    </span>
                    <span className="text-xs text-fg-subtle">
                      {server.tools === null
                        ? "工具清单尚未探测（首次调用该 server 时自动拉取）"
                        : server.tools.length === 0
                          ? "该 server 未提供任何工具"
                          : `${server.toolCount} 个工具（${server.readOnlyCount} 个只读）`}
                    </span>
                  </button>
                  {open && server.tools !== null && server.tools.length > 0 && (
                    <ul className="mt-1 pl-6">
                      {server.tools.map((tool) => (
                        <li key={tool.name} className="py-1">
                          <span className="block text-sm text-fg">
                            {tool.name}
                            {tool.read_only && (
                              <span className="ml-2 text-xs text-fg-subtle">只读</span>
                            )}
                          </span>
                          <span className="block text-xs text-fg-subtle">{tool.description}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
              );
            })}
          </div>
        </>
      )}
    </div>
  );
}

/**
 * 「如何添加 MCP server」说明。
 *
 * 路径取服务端返回的**真实解析值**，不硬编码 `~/.yi-agent`：路径规则若变动，
 * 硬编码的文案会与事实脱节（与技能 Tab 的 `AddSkillHelp` 同一理由）。
 */
function AddMcpHelp({ path }: { path: string }) {
  return (
    <section className="border-t border-line p-4">
      <h3 className="text-sm font-medium text-fg">如何添加 MCP server</h3>
      <p className="mt-2 text-xs text-fg-muted">
        把 server 写进下面这个文件（Claude Desktop 兼容的 schema），重启侧车后生效：
      </p>
      {path !== "" && <p className="mt-1 break-all font-mono text-xs text-fg-subtle">{path}</p>}
      <pre className="mt-3 overflow-x-auto rounded border border-line bg-surface p-2 font-mono text-xs text-fg-muted">
        {`{
  "mcpServers": {
    "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path"] },
    "ws": { "type": "http", "url": "https://example.com/mcp" }
  }
}`}
      </pre>
      <p className="mt-2 text-xs text-fg-subtle">
        工具清单在首次调用该 server 时自动探测并缓存，本页的清单因此可能滞后于配置改动。
      </p>
    </section>
  );
}