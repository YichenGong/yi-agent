/** Which layer supplied the effective switch value. */
export type SwitchSource = "project" | "global" | "default";

export interface ResolvedSwitch {
  value: boolean;
  source: SwitchSource;
}

/**
 * Two-layer resolution. The project layer wins; both unset means disabled,
 * matching the Rust side so the UI never disagrees with the plugin process.
 */
export function resolveSwitch(
  global: boolean | null,
  project: boolean | null,
): ResolvedSwitch {
  if (project !== null) return { value: project, source: "project" };
  if (global !== null) return { value: global, source: "global" };
  return { value: false, source: "default" };
}

export function formatSwitch(switchOn: boolean, source: SwitchSource): string {
  return `Superpowers 看板: ${switchOn ? "on" : "off"} (${source})`;
}

export interface BoardCardDto {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
}

type BoardRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/**
 * The plugin the kanban panel talks to. The host only forwards the query; no
 * board meaning lives above this line, so the plugin can be installed or
 * removed without the app learning a new RPC.
 */
export const KANBAN_PLUGIN = "superpowers-kanban";

/** One query through the generic channel, tagged with the plugin it is meant for. */
async function queryPlugin<T>(
  rpc: BoardRpc,
  method: string,
  params: unknown,
): Promise<T> {
  return rpc<T>("plugin/query", { plugin: KANBAN_PLUGIN, method, params });
}

/**
 * Whether the plugin is installed, judged by whether it answered at all.
 *
 * The daemon refuses the query when the plugin is not running or never declared
 * a query socket, which is exactly the case the panel has to name. This stays
 * string-based because the failure crosses a JSON-RPC boundary as a message.
 */
export function pluginIsUnavailable(error: unknown): boolean {
  const text = error instanceof Error ? error.message : String(error ?? "");
  return text.includes("PluginUnavailable") || text.includes("is not running");
}

/** 经插件读看板卡片。 */
export async function fetchBoard(rpc: BoardRpc): Promise<BoardCardDto[]> {
  const result = await queryPlugin<{ cards?: BoardCardDto[] }>(rpc, "list", {});
  return result?.cards ?? [];
}

/** 经插件读两层解析后的开关与来源。 */
export async function readBoardSwitch(
  rpc: BoardRpc,
): Promise<{ on: boolean; source: SwitchSource }> {
  return queryPlugin<{ on: boolean; source: SwitchSource }>(rpc, "switch.read", {});
}

/** 经插件写项目层开关。 */
export async function setBoardSwitch(rpc: BoardRpc, on: boolean): Promise<void> {
  await queryPlugin(rpc, "switch.write", { on });
}

/** 经插件投递一张卡片。 */
export async function enqueueBoardCard(
  rpc: BoardRpc,
  specPath: string,
  planPath: string,
): Promise<void> {
  await queryPlugin(rpc, "enqueue", { spec_path: specPath, plan_path: planPath });
}
