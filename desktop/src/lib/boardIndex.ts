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
 * 分批数，且**只数还没落地的卡**：预排队（最左列，等前面几轮跑完）、排队（含刚
 * 认领）、运行中（含正在合并的一轮）、待合并（等用户发话）、待处理（要用户回话）。
 * 合并卡曾整类落在计数之外——五张卡卡住时摘要仍显示「0 排队 · 0 运行中」，侧栏看
 * 起来什么都没发生；预排队卡也曾如此，于是一块只放预排队卡的看板在侧栏显示「无待
 * 办」。完成/失败/取消是历史，不进摘要。没有待办时明说「无待办」，而不是显示一排 0。
 */
export function summarize(cards: { state: string }[]): string {
  if (cards.length === 0) return "空";
  let preQueued = 0;
  let queued = 0;
  let active = 0;
  let awaitingMerge = 0;
  let needsYou = 0;
  for (const card of cards) {
    const state = card.state.toLowerCase();
    if (state === "pre_queued") preQueued += 1;
    else if (state === "queued" || state === "launching") queued += 1;
    else if (state === "running" || state === "merging") active += 1;
    else if (state === "awaiting_merge") awaitingMerge += 1;
    else if (state === "needs_you") needsYou += 1;
  }
  const parts: string[] = [];
  // 预排队排最前，与看板最左列（PRE-QUEUED）的顺序一致。
  if (preQueued > 0) parts.push(`${preQueued} 预排队`);
  if (queued > 0) parts.push(`${queued} 排队`);
  if (active > 0) parts.push(`${active} 运行中`);
  if (awaitingMerge > 0) parts.push(`${awaitingMerge} 待合并`);
  if (needsYou > 0) parts.push(`${needsYou} 待处理`);
  return parts.length > 0 ? parts.join(" · ") : "无待办";
}

/** 看板读写失败后 UI 能采取的两条路，外加「说不清」。 */
export type BoardErrorKind = "daemon_down" | "plugin_missing" | "other";

/**
 * 把一次看板 RPC 的失败分到两种可操作的状态。
 *
 * 结构化码优先：desktop 的 `RpcClient` 抛的是整个 `RpcError` 对象，看板语义
 * 的码在 `data.code` 里，比 message 稳定。message 匹配作为退路。
 *
 * **没有「未创建」这一支**：宿主已经不再回答「这个项目没有看板」（摘掉登记表
 * 门控后没有这个码）。「一个项目有没有看板」现在由 `listBoards` 的结果表达
 * 事实，不是一次读到的错误。
 *
 * 两种「连不上」必须分开：`daemon_unavailable` 是那个项目的 daemon 拨不通，
 * 插件没装则要用户去装插件；把后者说成前者会让用户反复点一个永远不成功的按钮。
 */
export function boardErrorKind(error: unknown): BoardErrorKind {
  const code = (error as { data?: { code?: string } } | null)?.data?.code;
  if (code === "daemon_unavailable") return "daemon_down";
  if (code === "plugin_unavailable") return "plugin_missing";

  const text = error instanceof Error ? error.message : String(error ?? "");
  if (text.includes("daemon_unavailable") || text.includes("daemon is unavailable")) {
    return "daemon_down";
  }
  if (text.includes("is not available") || text.includes("PluginUnavailable")) {
    return "plugin_missing";
  }
  return "other";
}
