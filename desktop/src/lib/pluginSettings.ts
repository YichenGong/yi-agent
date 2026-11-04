import { RPC } from "./routes";
import type { RpcCall } from "./rpcCall";

/** 桌面与宿主通用插件通道之间的接缝。 */
export type PluginRpc = RpcCall;

/**
 * 「插件」Tab 可选的配置项目：额外项目 ∪ 最近目录，去重且保序。
 *
 * 插件按项目安装（`<项目>/.yi-agent/supervisors/`），所以调用方可以额外点名
 * 一些项目（例如刚从别处打开过的），它们不必出现在最近目录里。顺序：额外
 * 项目在前，最近目录随后；同一路径只出现一次。纯函数，便于单测。
 */
export function pluginProjectPaths(
  workspaces: { path: string }[],
  extra: string[] = [],
): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const push = (path: string | undefined) => {
    const trimmed = path?.trim();
    if (!trimmed || seen.has(trimmed)) return;
    seen.add(trimmed);
    out.push(trimmed);
  };
  for (const path of extra) push(path);
  for (const workspace of workspaces) push(workspace.path);
  return out;
}

export interface PluginSummary {
  name: string;
  queryable: boolean;
  switchKey: string;
  /**
   * 这个插件此刻是否有在跑（`flags/list` 里它自己那个开关的值）。
   *
   * 旧宿主不回这个字段。缺省按 `true`：把"不知道"画成"已停用"会让升级前的
   * 宿主上每个插件都显示成关着。
   */
  enabled: boolean;
}

export interface PluginSummary {
  name: string;
  queryable: boolean;
  switchKey: string;
  /**
   * 这个插件此刻是否有在跑（`flags/list` 里它自己那个开关的值）。
   *
   * 旧宿主不回这个字段。缺省按 `true`：把"不知道"画成"已停用"会让升级前的
   * 宿主上每个插件都显示成关着。
   */
  enabled: boolean;
}

/**
 * 通用插件通道的可选作用域。
 *
 * 插件清单按项目安装（`<项目>/.yi-agent/supervisors/`），而桌面侧车的 app-server
 * 以用户 home 为 workdir；不把「要配置的项目」传下去，`plugins/list` 会去 home
 * 下找清单、得到一个空列表。缺省（undefined/空）时宿主回落到自身 workdir，保持
 * TUI 与旧调用方的原语义。
 */
function scopeParams(project?: string): Record<string, string> {
  const trimmed = project?.trim();
  return trimmed ? { project: trimmed } : {};
}

/** 列出已装插件。`project` 指定枚举哪个项目的清单；缺省用宿主 workdir。 */
export async function listPlugins(rpc: PluginRpc, project?: string): Promise<PluginSummary[]> {
  const result = await rpc<{ plugins?: Array<Record<string, unknown>> }>(RPC.pluginsList, {
    ...scopeParams(project),
  });
  const rows = Array.isArray(result?.plugins) ? result.plugins : [];
  return rows
    .filter((row) => typeof row.name === "string")
    .map((row) => ({
      name: row.name as string,
      queryable: row.queryable === true,
      switchKey: typeof row.switch_key === "string" ? (row.switch_key as string) : "",
      enabled: row.enabled !== false,
    }));
}

/** 插件设置的四种失败，UI 各给一句话。 */
export type PluginErrorKind =
  | "not_installed"
  | "not_running"
  | "plugin_rejected"
  | "other";

/**
 * 读 `data.code`（结构化优先），退路是 message 文本——宿主在补上码之前
 * 只有人话。
 */
export function pluginErrorKind(error: unknown): PluginErrorKind {
  const record = error as { message?: unknown; data?: { code?: unknown } } | null;
  const code = typeof record?.data?.code === "string" ? record.data.code : "";
  if (code === "plugin_not_installed") return "not_installed";
  if (code === "plugin_unavailable") return "not_running";
  // 插件自己拒绝（例如设置载荷非法）不等于插件没运行：分开一档，
  // 面板好把插件给的理由原样展示。
  if (code === "plugin_rejected") return "plugin_rejected";
  const text =
    typeof record?.message === "string"
      ? record.message
      : error instanceof Error
        ? error.message
        : String(error ?? "");
  if (text.includes("plugin_not_installed") || text.includes("is not installed")) {
    return "not_installed";
  }
  if (text.includes("plugin_unavailable") || text.includes("is not available")) {
    return "not_running";
  }
  if (text.includes("plugin_rejected") || text.includes("rejected")) {
    return "plugin_rejected";
  }
  return "other";
}
