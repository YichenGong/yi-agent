import { RPC } from "./routes";

/**
 * 宿主接缝的最小形状。
 *
 * 不用 `RpcCall`（带调用方自选返回类型的泛型）：`App` 拿到的 `pluginCall` 是
 * 非泛型的 `(method, params) => Promise<unknown>`，泛型签名会让它无法直接赋值。
 * 这里只声明真正用到的部分。
 */
export type RpcCallFn = (method: string, params: unknown) => Promise<unknown>;

/**
 * 「后台值守」开关——宿主级设置，不是某个插件的项目设置。
 *
 * 值守盯的是宿主上的常驻登记（谁该在开机/登录后起来），因此走
 * `ui/settings/*`，与主题同一条通道；`plugin/query` 带 `project`，答的是
 * 「这个项目的队列」，与「宿主上有没有一个值守进程」是两码事。
 *
 * 这一层原先是看板开关模块的一部分。它本身不含任何看板语义——值守安装的是
 * 一个通用的登录项，服务所有已登记的常驻进程——所以随看板业务代码一起留在
 * 宿主是错的。这里只保留与插件无关的那部分。
 */

export const WATCHMAN_KEY = "resident_daemon_watchman_enabled";

/**
 * 通用化之前的旧字段名。只读兼容：旧版宿主仍只回它。
 *
 * 读写方向是「新名优先、旧名兜底」，与宿主 `ui/settings/write` 同时认两个名字的
 * 策略对称。
 */
export const LEGACY_WATCHMAN_KEY = "board_watchman_enabled";

/**
 * 归一化值守开关的值。非布尔一律当开：老配置文件里没有这个键时，用户此前的
 * 行为是「一直在跑」，不能因为读不到就把它关掉。App 直接在握手时套用（那趟
 * read 已经拿到了整个 settings），`readWatchman` 也用它，保证桌面端只有一处
 * 缺省口径。
 */
export function parseWatchman(value: unknown): boolean {
  return typeof value === "boolean" ? value : true;
}

/**
 * 从一整份 `ui/settings/read` 应答里取值：新名优先，缺失才看旧名。
 *
 * 单独成函数而不是让每个调用方各写一遍 `??`：读序一旦漂移，就会回到「用户关掉
 * 的开关重启后显示成开」的老 bug。
 */
export function watchmanFromSettings(settings: Record<string, unknown>): boolean {
  const value = settings[WATCHMAN_KEY] ?? settings[LEGACY_WATCHMAN_KEY];
  return parseWatchman(value);
}

/** 读「后台值守」开关。App 握手时已从同一趟 read 拿到 settings，可不必再发。 */
export async function readWatchman(rpc: RpcCallFn): Promise<boolean> {
  const settings = (await rpc(RPC.uiSettingsRead, {})) as Record<string, unknown>;
  return watchmanFromSettings(settings);
}

/**
 * 写「后台值守」开关。宿主在安装/卸载失败时把原因放在返回的 `warning` 里
 * （写入本身已落盘），调用方负责把它摆到面板上；成功时为 `null`/缺省。
 */
export async function writeWatchman(
  rpc: RpcCallFn,
  enabled: boolean,
): Promise<{ warning?: string | null }> {
  return (await rpc(RPC.uiSettingsWrite, {
    [WATCHMAN_KEY]: enabled,
  })) as { warning?: string | null };
}
