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

/** 经 app-server 读看板卡片（宿主侧读 board.json）。 */
export async function fetchBoard(rpc: BoardRpc): Promise<BoardCardDto[]> {
  const result = await rpc<{ cards?: BoardCardDto[] }>("superpowers-kanban/list", {});
  return result?.cards ?? [];
}

/** 经 app-server 读两层解析后的开关与来源。 */
export async function readBoardSwitch(
  rpc: BoardRpc,
): Promise<{ on: boolean; source: SwitchSource }> {
  return rpc<{ on: boolean; source: SwitchSource }>("superpowers-kanban/switch/read", {});
}

/** 经 app-server 写项目层开关。 */
export async function setBoardSwitch(rpc: BoardRpc, on: boolean): Promise<void> {
  await rpc("superpowers-kanban/switch/write", { on });
}

/** 经 app-server 投递一张卡片。 */
export async function enqueueBoardCard(
  rpc: BoardRpc,
  specPath: string,
  planPath: string,
): Promise<void> {
  await rpc("superpowers-kanban/enqueue", { spec_path: specPath, plan_path: planPath });
}
