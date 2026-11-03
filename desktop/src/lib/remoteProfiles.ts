/**
 * 远端项目清单的前端视图与反查。
 *
 * 为什么需要反查：远端槽位下，`thread/listAll` 报的是**那台机器上**的路径，
 * 而侧栏「移除项目」按钮只知道路径。要把「移除这个分组」翻译成
 * `remote_remove({ profileId })`，必须按 `remote_path` 反查回 id。
 */

/** `remote_list` 的一行，只留前端用得到的字段。 */
export interface RemoteProfileRow {
  id: string;
  label: string;
  remote_path: string;
}

/**
 * 按远端路径反查 profile id。
 *
 * 找不到返回 `null` 而不是猜一个：传错 id 给 `remote_remove` 会删掉**另一个**
 * 项目的配置——那是不可撤销的数据丢失，而「什么都没删」只是用户再点一次。
 */
export function remoteProfileIdForPath(
  profiles: readonly RemoteProfileRow[],
  path: string,
): string | null {
  return profiles.find((row) => row.remote_path === path)?.id ?? null;
}

/**
 * 侧栏分组该不该按「本机 workdir」处理。
 *
 * 远端模式下**不能**调 `workspace/remove`：那是本机 workdir 索引的操作，远端
 * 路径在本机索引里根本不存在，调用只会静默无效果，用户点几次都不消失。
 */
export function sidebarMutation(
  usesLocalPaths: boolean,
): "workspace" | "remote" {
  return usesLocalPaths ? "workspace" : "remote";
}
