/** 桌面与宿主通用插件通道之间的接缝。形状与 `BoardRpc` 一致。 */
export type PluginRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 看板插件名。宿主按名字转发，壳子不含看板语义。 */
export const KANBAN_PLUGIN = "superpowers-kanban";

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

/** 列出 app-server workdir 下已装的插件。 */
export async function listPlugins(rpc: PluginRpc): Promise<PluginSummary[]> {
  const result = await rpc<{ plugins?: Array<Record<string, unknown>> }>("plugins/list", {});
  const rows = Array.isArray(result?.plugins) ? result.plugins : [];
  return rows
    .filter((row) => typeof row.name === "string")
    .map((row) => ({
      name: row.name as string,
      queryable: row.queryable === true,
      switchKey: typeof row.switch_key === "string" ? (row.switch_key as string) : "",
    }));
}

/** 经插件通道读看板设置。 */
export async function readKanbanSettings(rpc: PluginRpc): Promise<KanbanSettings> {
  const result = await rpc<{ settings?: KanbanSettings }>("plugin/settings/read", {
    plugin: KANBAN_PLUGIN,
  });
  if (!result?.settings) throw new Error("the plugin returned no settings");
  return result.settings;
}

/** 经插件通道写看板设置（全量替换）。 */
export async function writeKanbanSettings(
  rpc: PluginRpc,
  settings: KanbanSettings,
): Promise<void> {
  await rpc("plugin/settings/write", { plugin: KANBAN_PLUGIN, settings });
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
