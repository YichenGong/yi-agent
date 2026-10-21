import { RPC } from "./routes";
import type { RpcCall } from "./rpcCall";

/** 桌面与宿主通用插件通道之间的接缝。形状与 `BoardRpc` 一致。 */
export type PluginRpc = RpcCall;

/** 看板插件名。宿主按名字转发，壳子不含看板语义。 */
export const KANBAN_PLUGIN = "superpowers-kanban";

/**
 * 星期的规范顺序（周一到周日），与插件 `render_days` 的压缩顺序一致。
 * 界面上用中文单字展示，但线上格式始终是 `Mon`…`Sun`。
 */
export const WEEKDAYS = [
  { en: "Mon", zh: "一" },
  { en: "Tue", zh: "二" },
  { en: "Wed", zh: "三" },
  { en: "Thu", zh: "四" },
  { en: "Fri", zh: "五" },
  { en: "Sat", zh: "六" },
  { en: "Sun", zh: "日" },
] as const;

/** `Mon`/`monday` → 0…6；不认识返回 undefined。与插件 `weekday_index` 同一套别名。 */
function weekdayIndex(name: string): number | undefined {
  switch (name.trim().toLowerCase()) {
    case "mon":
    case "monday":
      return 0;
    case "tue":
    case "tuesday":
      return 1;
    case "wed":
    case "wednesday":
      return 2;
    case "thu":
    case "thursday":
      return 3;
    case "fri":
    case "friday":
      return 4;
    case "sat":
    case "saturday":
      return 5;
    case "sun":
    case "sunday":
      return 6;
    default:
      return undefined;
  }
}

/**
 * 把插件的星期串（`Mon-Fri` / `Sat,Sun` / `Mon,Wed,Fri`）解析成长度 7 的布尔数组。
 *
 * 纯客户端逻辑：插件永远返回压缩后的规范串，本函数只负责把它变成点按控件要的
 * 选中集合。无法识别的段（含倒序区间）一律忽略——坏数据宁可少选，也不能崩。
 */
export function parseDays(spec: string): boolean[] {
  const selected = [false, false, false, false, false, false, false];
  for (const raw of spec.split(",")) {
    const part = raw.trim();
    if (part === "") continue;
    const dash = part.indexOf("-");
    if (dash === -1) {
      const day = weekdayIndex(part);
      if (day !== undefined) selected[day] = true;
      continue;
    }
    const from = weekdayIndex(part.slice(0, dash));
    const to = weekdayIndex(part.slice(dash + 1));
    if (from === undefined || to === undefined || from > to) continue;
    for (let day = from; day <= to; day += 1) selected[day] = true;
  }
  return selected;
}

/** {@link parseDays} 的逆：把选中集合压缩回插件认的规范串（全选 → `Mon-Sun`）。 */
export function renderDays(selected: boolean[]): string {
  const parts: string[] = [];
  let i = 0;
  while (i < WEEKDAYS.length) {
    if (!selected[i]) {
      i += 1;
      continue;
    }
    const start = i;
    while (i + 1 < WEEKDAYS.length && selected[i + 1]) i += 1;
    parts.push(
      i === start ? WEEKDAYS[start].en : `${WEEKDAYS[start].en}-${WEEKDAYS[i].en}`,
    );
    i += 1;
  }
  return parts.join(",");
}

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
  /**
   * 这个插件此刻是否有在跑（`flags/list` 里它自己那个开关的值）。
   *
   * 旧宿主不回这个字段。缺省按 `true`：把"不知道"画成"已停用"会让升级前的
   * 宿主上每个插件都显示成关着。
   */
  enabled: boolean;
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

/** 经插件通道读看板设置。`project` 指定读哪个项目的设置。 */
export async function readKanbanSettings(
  rpc: PluginRpc,
  project?: string,
): Promise<KanbanSettings> {
  const result = await rpc<{ settings?: KanbanSettings }>(RPC.pluginSettingsRead, {
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
  await rpc(RPC.pluginSettingsWrite, {
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
