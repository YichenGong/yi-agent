/** 桌面与宿主通用插件通道之间的接缝。形状与 `BoardRpc` 一致。 */
export type PluginRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 看板插件名。宿主按名字转发，壳子不含看板语义。 */
export const KANBAN_PLUGIN = "superpowers-kanban";

/**
 * 「插件」Tab 可选的配置项目：最近目录 ∪ 已登记看板，去重且保序。
 *
 * 看板是按项目安装的，所以已登记看板的项目即使没进最近目录也必须在列；最近
 * 目录则覆盖任何「装了别的插件」的项目。顺序：看板项目在前（看板是当前主要
 * 用途），其余最近目录随后。纯函数，便于单测。
 */
export function pluginProjectPaths(
  workspaces: { path: string }[],
  boards: string[],
): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const push = (path: string | undefined) => {
    const trimmed = path?.trim();
    if (!trimmed || seen.has(trimmed)) return;
    seen.add(trimmed);
    out.push(trimmed);
  };
  for (const board of boards) push(board);
  for (const workspace of workspaces) push(workspace.path);
  return out;
}

export interface PluginSummary {
  name: string;
  queryable: boolean;
  switchKey: string;
}

export interface KanbanWindow {
  days: string;
  start: string;
  end: string;
  all_day: boolean;
  max_tasks: number;
}

export interface KanbanSettings {
  default_max_tasks: number;
  interval_secs: number;
  windows: KanbanWindow[];
}

/**
 * 通用插件通道的可选作用域。
 *
 * 插件清单按项目安装（`<项目>/.yi-agent/supervisors/`），而桌面侧车的 app-server
 * 以用户 home 为 workdir；不把「要配置的项目」传下去，`plugins/list` 会去 home
 * 下找清单、得到一个空列表——看板跑着，设置页却空着。缺省（undefined/空）时
 * 宿主回落到自身 workdir，保持 TUI 与旧调用方的原语义。
 */
function scopeParams(project?: string): Record<string, string> {
  const trimmed = project?.trim();
  return trimmed ? { project: trimmed } : {};
}

/** 列出已装插件。`project` 指定枚举哪个项目的清单；缺省用宿主 workdir。 */
export async function listPlugins(rpc: PluginRpc, project?: string): Promise<PluginSummary[]> {
  const result = await rpc<{ plugins?: Array<Record<string, unknown>> }>("plugins/list", {
    ...scopeParams(project),
  });
  const rows = Array.isArray(result?.plugins) ? result.plugins : [];
  return rows
    .filter((row) => typeof row.name === "string")
    .map((row) => ({
      name: row.name as string,
      queryable: row.queryable === true,
      switchKey: typeof row.switch_key === "string" ? (row.switch_key as string) : "",
    }));
}

/** 经插件通道读看板设置。`project` 指定读哪个项目的设置。 */
export async function readKanbanSettings(
  rpc: PluginRpc,
  project?: string,
): Promise<KanbanSettings> {
  const result = await rpc<{ settings?: KanbanSettings }>("plugin/settings/read", {
    plugin: KANBAN_PLUGIN,
    ...scopeParams(project),
  });
  if (!result?.settings) throw new Error("the plugin returned no settings");
  return result.settings;
}

/** 经插件通道写看板设置（全量替换）。`project` 指定写哪个项目的设置。 */
export async function writeKanbanSettings(
  rpc: PluginRpc,
  settings: KanbanSettings,
  project?: string,
): Promise<void> {
  await rpc("plugin/settings/write", {
    plugin: KANBAN_PLUGIN,
    settings,
    ...scopeParams(project),
  });
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
