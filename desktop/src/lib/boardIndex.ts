/**
 * 看板的「哪些项目有」「有多少活」两个纯判断，以及把 RPC 失败翻译成三种
 * 用户能照着做点什么的状态。
 *
 * 这一层故意不碰 React、不碰 RPC：看板条目与摘要会在侧栏与主区域被反复
 * 重算（每次轮询都换新对象），把它们做成纯函数才既便宜又可断言。
 */

/**
 * 该项目是否登记了看板。
 *
 * 精确匹配：登记表里存的是项目根路径，而 `/a` 的看板不该让 `/a/b` 也冒出
 * 一个条目——点进子目录却看见父项目的队列，比没有条目更让人困惑。
 */
/** The summary shown when a project's board could not be read at all. */
export const BOARD_SUMMARY_UNREADABLE = "无法读取";

/**
 * 只给已登记的项目显示看板条目。
 *
 * 精确匹配而非前缀：登记 `/a` 不能让 `/a/b` 也长出看板条目——那是另一个项目，
 * 有它自己的登记表条目。
 */
export function kanbanItemFor(workspace: string, boards: string[]): boolean {
  return boards.includes(workspace);
}

/**
 * 侧栏条目上的摘要。
 *
 * 排队与运行分开数：两者混成一个数字就看不出「卡住了」还是「在跑」，
 * 而这正是用户扫一眼侧栏想知道的。其它状态（完成/失败）不进摘要——它们
 * 是历史，不是待办。
 */
export function summarize(cards: { state: string }[]): string {
  if (cards.length === 0) return "空";
  let queued = 0;
  let running = 0;
  for (const card of cards) {
    const state = card.state.toLowerCase();
    if (state === "queued") queued += 1;
    else if (state === "running") running += 1;
  }
  return `${queued} 排队 · ${running} 运行中`;
}

/** 看板读写失败后 UI 能采取的三条路，外加「说不清」。 */
export type BoardErrorKind = "not_created" | "daemon_down" | "plugin_missing" | "other";

/**
 * 把一次看板 RPC 的失败分到三种可操作的状态。
 *
 * 结构化码优先：desktop 的 `RpcClient` 抛的是整个 `RpcError` 对象，看板语义
 * 的码在 `data.code` 里，比 message 稳定。message 匹配作为退路——宿主在
 * Task 4 之前根本不发码，只有人话。
 *
 * 两种「连不上」必须分开：`daemon_unavailable` 是项目还没建看板（可以点
 * 「创建看板」解决），插件没装则要用户去装插件；把后者说成前者会让用户
 * 反复点一个永远不成功的按钮。
 */
export function boardErrorKind(error: unknown): BoardErrorKind {
  const code = (error as { data?: { code?: string } } | null)?.data?.code;
  if (code === "board_not_created") return "not_created";
  if (code === "daemon_unavailable") return "daemon_down";
  if (code === "plugin_unavailable") return "plugin_missing";

  const text = error instanceof Error ? error.message : String(error ?? "");
  if (text.includes("board_not_created")) return "not_created";
  if (text.includes("daemon_unavailable") || text.includes("daemon is unavailable")) {
    return "daemon_down";
  }
  if (text.includes("is not available") || text.includes("PluginUnavailable")) {
    return "plugin_missing";
  }
  return "other";
}
