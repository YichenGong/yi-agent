/**
 * 看板的生命周期：登记一个项目、撤销登记、列出已登记的项目。
 *
 * 与 `superpowersKanbanSwitch.ts` 的分工：那边是「经插件问这个项目的队列」，
 * 这边是「问宿主这个项目有没有看板」。生命周期属于宿主（登记表 + 每个项目
 * 一个 daemon），插件不知道也不该知道别人登记了什么。
 */

type BoardRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 登记一个项目的看板（幂等：已有则只把它拉起来）。 */
export async function createBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc("board/create", { project });
}

/** 撤销登记并删掉该项目的队列状态。不可逆，调用方必须先二次确认。 */
export async function removeBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc("board/remove", { project });
}

/**
 * 已登记看板的项目路径。
 *
 * app-server 的真实形状是 `{ boards: [{ project, created_at, status }] }`；
 * 裸字符串数组与 `items` 也一并容忍，形状不认识时返回空表——侧栏靠这个列表
 * 决定画不画条目，读不出来只能是「没有」，绝不能因此让整页崩掉。
 */
export async function listBoards(rpc: BoardRpc): Promise<string[]> {
  const result = await rpc<unknown>("board/list", {});
  return projectPaths(result);
}

function projectPaths(result: unknown): string[] {
  const container = result as { boards?: unknown; items?: unknown } | null | undefined;
  const raw = Array.isArray(result)
    ? result
    : Array.isArray(container?.boards)
      ? container.boards
      : Array.isArray(container?.items)
        ? container.items
        : [];
  const paths: string[] = [];
  for (const entry of raw) {
    if (typeof entry === "string") {
      if (entry !== "") paths.push(entry);
      continue;
    }
    if (typeof entry !== "object" || entry === null) continue;
    const project = (entry as { project?: unknown }).project;
    if (typeof project === "string" && project !== "") paths.push(project);
  }
  return paths;
}
