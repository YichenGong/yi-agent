/**
 * 侧栏会话搜索的匹配与过滤。
 *
 * 纯函数：不碰 React、不碰 RPC。侧栏每次渲染都要重算一遍，做成纯函数才既
 * 便宜又可断言。过滤**不**修改传入的 groups/pinned，逐个复制分组外壳。
 */
import type { ThreadSummary, WorkspaceGroup } from "./protocol";
import { DEFAULT_TAG_NAMES, isValidTagId, tagLabel, type TagId } from "./threadTags";
import { basename } from "./workspaceGroups";

export interface ThreadSearchResult {
  pinned: ThreadSummary[];
  groups: WorkspaceGroup[];
  /** 查询非空但无任何命中；用于区分「没输入」与「搜不到」。 */
  empty: boolean;
}

/** 一条会话在界面上可被搜索到的全部文本：渲染标题 + 标签名 + 所属目录名与完整路径。 */
function haystack(
  thread: ThreadSummary,
  workspaceOfThread: string,
  tagNames: Record<TagId, string>,
): string {
  // 标题渲染的就是这个回退值（ThreadSidebar 的 `t.title ?? "(untitled)"`），
  // 所以搜「untitled」能命中无标题的会话。
  // 标签名并入匹配文本：搜「紧急」即筛出所有打了该标签的会话——这是「不做筛选
  // UI」方案下几乎零成本的收益。非法 tag 不并入（脏数据不该被搜到）。
  const tagText = isValidTagId(thread.tag) ? tagLabel(thread.tag, tagNames) : "";
  return [
    thread.title ?? "(untitled)",
    tagText,
    basename(workspaceOfThread),
    workspaceOfThread,
  ]
    .join("\n")
    .toLowerCase();
}

/**
 * 按关键词过滤会话树。
 *
 * `query.trim()` 为空 ⇒ 原样返回（引用相等），调用方据此完全跳过过滤。
 * 否则按空白分词、逐词 AND。
 *
 * 置顶会话不挂在任何分组对象上，故用**它自己的 `cwd`** 判定路径；分组内会话
 * 用所属分组的 `workspace`。两者在实际数据里是同一个路径，分开写只为两个
 * 数据源上都可判定、不依赖调用顺序。
 */
export function filterThreadTree(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  query: string,
  tagNames: Record<TagId, string> = DEFAULT_TAG_NAMES,
): ThreadSearchResult {
  const trimmed = query.trim();
  if (trimmed === "") return { pinned, groups, empty: false };

  const tokens = trimmed.toLowerCase().split(/\s+/);

  const groups2: WorkspaceGroup[] = [];
  for (const g of groups) {
    // 每条会话自身的 haystack 已含所属目录名与完整路径，故目录命中自然带出整组。
    const threads = g.threads.filter((t) =>
      tokens.every((tok) => haystack(t, g.workspace, tagNames).includes(tok)),
    );
    // 分组自身**只**渲染非置顶会话（ThreadSidebar 的 `.filter((t) => !t.pinned && !t.card_id)`）：
    // 置顶会话在 Pinned 区渲染，且按约定仍留在本分组的 `threads` 里。故只有置顶
    // 会话命中时本组一行都渲染不出来，必须连表头一起移除。卡片会话不在此列——
    // 它们由本组自己的「看板会话」小节渲染（搜索期间强制展开，见 renderBoardThreads），
    // 命中即算本组有行。
    if (threads.some((t) => !t.pinned)) groups2.push({ ...g, threads });
  }

  const pinned2 = pinned.filter((t) =>
    tokens.every((tok) => haystack(t, t.cwd, tagNames).includes(tok)),
  );

  return { pinned: pinned2, groups: groups2, empty: groups2.length === 0 && pinned2.length === 0 };
}
