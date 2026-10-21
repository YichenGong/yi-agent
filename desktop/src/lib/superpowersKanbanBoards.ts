/**
 * 看板的生命周期：建一个看板、删一个看板、列出已登记的项目。
 *
 * 生命周期**归插件**（Spec 4 §4.2）：建/删是插件自己的事（它要建/删自己的
 * 状态目录），宿主只提供通用通道。所以这里的两条写路径走通用 `plugin/query`
 * ——宿主只把 `method`/`params` 原样转给插件，不解释「看板」是什么。
 *
 * 列举则走宿主通用的 `resident/list`（「哪些项目要常驻 daemon」）：把「一个
 * 项目有没有看板」表达成登记表的**事实**，而不是一个看板专属的 RPC。
 */

import type { RpcCall } from "./rpcCall";
import { RPC } from "./routes";

type BoardRpc = RpcCall;

/** 插件名：`plugin/query` 据此找到该项目的插件 daemon。 */
const KANBAN_PLUGIN = "superpowers-kanban";

/**
 * 建一个项目的看板（幂等：已有则只把它拉起来）。
 *
 * **必须先装插件再建盘。** 一个全新项目没有任何 supervisor 清单，宿主就不会
 * 拉起该项目的插件 daemon，`plugin/query` 也就无处可拨（查询表里没有这个
 * socket）。所以第一步走宿主通用能力 `plugin/install`：宿主去问插件
 * `<plugin> manifest`，把清单落进项目、把清单声明的开关打开。
 *
 * 开关不是可选项：它默认关，且门控插件的整个推进循环。只装清单不打开开关，
 * 会得到一个「已登记、已拉起、永不推进」的看板——比直接失败更难查。
 *
 * 顶层 `project` 是**路由目标**（宿主据此拨哪个项目的 daemon socket）；
 * `params.project` 是**插件据此建盘的项目**。两件事，都要带。
 */
export async function createBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc(RPC.pluginInstall, { project, plugin: KANBAN_PLUGIN });
  await rpc(RPC.pluginQuery, {
    plugin: KANBAN_PLUGIN,
    project,
    method: "lifecycle/create",
    params: { project },
  });
}

/**
 * 彻底摘掉该项目的看板：卸载插件，而不只是撤销登记。
 *
 * 曾经的实现只发 `lifecycle/remove`（撤销常驻登记 + 插件删状态目录），于是
 * 「移除」之后 daemon 仍在跑、清单还在、开关还开着——插件下一拍就把目录重建
 * 出来，用户看到的是「删不掉」。
 *
 * 现在的顺序是「先卸载、后兜底」：
 * 1. `plugin/uninstall`：宿主侧删清单/开关/日志、停掉该插件子进程，并请插件
 *    自己撤登记。这一步之后 supervisor 不会再拉起它。
 * 2. `lifecycle/remove`：幂等兜底。插件在第 1 步里可能已经不在了（没跑、没装），
 *    那一拍的 `release` 就没做成；这里再补一次，且此时目录已清空，写不出东西。
 *
 * 兜底失败不改变结果：第 1 步成功后看板确实已经不在了，此时因为兜底调用失败
 * 而把整次移除报成失败，只会让用户以为没删掉、再点一次。故它被显式忽略。
 *
 * 不可逆（队列状态会被删掉），调用方必须先二次确认。
 */
export async function removeBoard(rpc: BoardRpc, project: string): Promise<void> {
  await rpc(RPC.pluginUninstall, { project, plugin: KANBAN_PLUGIN });
  try {
    await rpc(RPC.pluginQuery, {
      plugin: KANBAN_PLUGIN,
      project,
      method: "lifecycle/remove",
      params: { project },
    });
  } catch {
    // 插件已随卸载消失：这正是预期。兜底只为覆盖「它还活着但没登记」的窄窗口。
  }
}

/**
 * 已登记看板的项目路径。
 *
 * 通用 `resident/list` 的真实形状是 `{ projects: [{ project, required_by }] }`；
 * 裸字符串数组与 `items` 也一并容忍，形状不认识时返回空表——侧栏靠这个列表
 * 决定画不画条目，读不出来只能是「没有」，绝不能因此让整页崩掉。
 */
export async function listBoards(rpc: BoardRpc): Promise<string[]> {
  const result = await rpc<unknown>(RPC.residentList, {});
  return projectPaths(result);
}

function projectPaths(result: unknown): string[] {
  const container = result as { projects?: unknown; items?: unknown } | null | undefined;
  const raw = Array.isArray(result)
    ? result
    : Array.isArray(container?.projects)
      ? container.projects
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
