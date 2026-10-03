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
  /** 该卡关联的会话；插件可能尚未起会话，故可为 null/缺省。 */
  thread_id?: string | null;
}

export type BoardRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/**
 * The plugin the kanban panel talks to. The host only forwards the query; no
 * board meaning lives above this line, so the plugin can be installed or
 * removed without the app learning a new RPC.
 */
export const KANBAN_PLUGIN = "superpowers-kanban";

/**
 * One query through the generic channel, tagged with the plugin it is meant for
 * *and the project it is about*.
 *
 * The project is a parameter, not ambient state: the host routes the query to
 * that project's runtime socket, and the plugin answers about its own queue.
 * Without it every project would read the same board — the app's own cwd —
 * which is exactly the "clicked and nothing happened" bug.
 */
async function queryPlugin<T>(
  rpc: BoardRpc,
  project: string,
  method: string,
  params: unknown,
): Promise<T> {
  return rpc<T>("plugin/query", { plugin: KANBAN_PLUGIN, project, method, params });
}

/**
 * Whether the plugin is installed, judged by whether it answered at all.
 *
 * The daemon refuses the query when the plugin is not running or never declared
 * a query socket, which is exactly the case the panel has to name. This stays
 * string-based because the failure crosses a JSON-RPC boundary as a message.
 */
export function pluginIsUnavailable(error: unknown): boolean {
  // The RPC client rejects with a raw `RpcError` object for JSON-RPC errors, not
  // an `Error`, so `instanceof Error` alone would stringify it to
  // `[object Object]` and never match. Read `message` (and the structured
  // `data.code`) off whatever shape we were handed.
  const record = error as { message?: unknown; data?: { code?: unknown } } | null;
  const code = typeof record?.data?.code === "string" ? record.data.code : "";
  const text =
    typeof record?.message === "string"
      ? record.message
      : error instanceof Error
        ? error.message
        : String(error ?? "");
  // The daemon answers `plugin <name> is not available` when it does not
  // supervise the plugin (or the plugin never declared a query socket). The
  // structured code is kept as a second signal in case the wording changes.
  return (
    code === "plugin_unavailable" ||
    text.includes("is not available") ||
    text.includes("PluginUnavailable")
  );
}

/** 经插件读某项目的看板卡片。 */
export async function fetchBoard(rpc: BoardRpc, project: string): Promise<BoardCardDto[]> {
  const result = await queryPlugin<{ cards?: BoardCardDto[] }>(rpc, project, "list", {});
  return result?.cards ?? [];
}

/** 经插件读某项目两层解析后的开关与来源。 */
export async function readBoardSwitch(
  rpc: BoardRpc,
  project: string,
): Promise<{ on: boolean; source: SwitchSource }> {
  return queryPlugin<{ on: boolean; source: SwitchSource }>(rpc, project, "switch.read", {});
}

/** 经插件写某项目层开关。 */
export async function setBoardSwitch(
  rpc: BoardRpc,
  project: string,
  on: boolean,
): Promise<void> {
  await queryPlugin(rpc, project, "switch.write", { on });
}

/** 经插件给某项目投递一张卡片。 */
export async function enqueueBoardCard(
  rpc: BoardRpc,
  project: string,
  specPath: string,
  planPath: string,
): Promise<void> {
  await queryPlugin(rpc, project, "enqueue", {
    spec_path: specPath,
    plan_path: planPath,
  });
}

/**
 * 归一化 `ui/settings/read` 里的 `board_watchman_enabled`。非布尔一律当开：
 * 老配置文件里没有这个键时，用户此前的行为是「一直在跑」，不能因为读不到就
 * 把它关掉。App 直接在握手时套用（那趟 read 已经拿到了整个 settings），
 * `readBoardWatchman` 也用它，保证桌面端只有一处缺省口径。
 */
export function parseBoardWatchman(value: unknown): boolean {
  return typeof value === "boolean" ? value : true;
}

/**
 * 读「后台值守」开关。
 *
 * 这是**宿主级**设置，不是某个项目的看板设置：值守盯的是宿主上的看板
 * daemon（随宿主起停），因此直接走 `ui/settings/*`，与主题同一条通道，
 * 不经 `plugin/query`——那条通道带 `project`，答的是「这个项目的队列」，
 * 与「宿主上有没有一个值守进程」是两码事。
 *
 * 也是给需要专门读一次开关的调用方用的公开读入口；App 握手时已从同一趟
 * `ui/settings/read` 里拿到 settings，直接套用 `parseBoardWatchman`，不再
 * 多发这一趟。
 */
export async function readBoardWatchman(rpc: BoardRpc): Promise<boolean> {
  const settings = await rpc<{ board_watchman_enabled?: unknown }>("ui/settings/read", {});
  return parseBoardWatchman(settings.board_watchman_enabled);
}

/**
 * 写「后台值守」开关。宿主在安装/卸载失败时把原因放在返回的 `warning` 里
 * （写入本身已落盘），调用方负责把它摆到面板上；成功时为 `null`/缺省。
 */
export async function writeBoardWatchman(
  rpc: BoardRpc,
  enabled: boolean,
): Promise<{ warning?: string | null }> {
  return rpc<{ warning?: string | null }>("ui/settings/write", {
    board_watchman_enabled: enabled,
  });
}
