import { LOCAL_SLOT, type Slot } from "./slots";

/**
 * params for `thread/start`: include `cwd` only when provided **and** the slot is
 * local.
 *
 * 远端模式下**必须**省略 `cwd`（spec §4.4）：`cwd` 会在运行 app-server 的那台
 * 机器上被 `canonicalize`，本机路径在远端必然不存在，用户看到的是「新建会话
 * 失败」且完全看不出原因。省略后服务端走 `None => cfg.workdir`，即 ssh 会话
 * 自己的工作目录——那才是用户想打开的「这个项目的目录」。
 */
export function threadStartParams(
  cwd?: string,
  slot: Slot = LOCAL_SLOT,
): Record<string, string> {
  if (slot !== LOCAL_SLOT) return {};
  return cwd ? { cwd } : {};
}

/**
 * 同一个判据的布尔形式，给「要不要把这个路径当本机路径用」的调用方。
 *
 * 多余吗？不多：`threadStartParams` 决定「发不发」，但侧栏还要决定「显不显示
 * 本机路径」「加不加进最近目录索引」。三处必须用**同一个**判据，否则会出现
 * 「请求里省略了 cwd，但最近目录里仍写进了本机路径」这种半对的状态。
 */
export function usesLocalPaths(slot: Slot): boolean {
  return slot === LOCAL_SLOT;
}
