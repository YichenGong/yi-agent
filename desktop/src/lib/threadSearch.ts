/**
 * 侧栏会话搜索的匹配与过滤。
 *
 * 纯函数：不碰 React、不碰 RPC。侧栏每次渲染都要重算一遍，做成纯函数才既
 * 便宜又可断言。过滤**不**修改传入的 groups/pinned，逐个复制分组外壳。
 */
import type { SessionGroup, ThreadSummary, WorkspaceGroup } from "./protocol";
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
/**
 * 搜索时把组表收窄到"与查询有关"的框。
 *
 * 三种情况保留整框：
 * - 组名命中（名字本身是用户可搜的内容）；
 * - 描述命中；
 * - 框内至少一条成员命中（成员表按 `hit` 收窄到只剩命中项）。
 *
 * 一条都不命中的框整框消失——留一个空框只会告诉用户"这里有个叫 X 的组"，
 * 而 X 与被搜的词无关。组名/描述命中时保留**全部**成员：用户搜的正是这个组，
 * 展开后应当看到它的全貌。
 */
function narrowGroups(
  groups: SessionGroup[],
  allThreads: ThreadSummary[],
  hitThreads: ThreadSummary[],
  tokens: string[],
): { groups: SessionGroup[]; keepAll: string[] } {
  const hitIds = new Set(hitThreads.map((t) => t.thread_id));
  const present = new Set(allThreads.map((t) => t.thread_id));
  const out: SessionGroup[] = [];
  const keepAll: string[] = [];
  for (const group of groups) {
    const header = `${group.name}\n${group.description}`.toLowerCase();
    if (tokens.every((tok) => header.includes(tok))) {
      out.push(group);
      // 只把真实存在的成员纳入"强制保留"，残留 id 照旧不渲染。
      keepAll.push(...group.threadIds.filter((id) => present.has(id)));
      continue;
    }
    // 成员表里可能残留已不存在的 id（服务端还没清理），先按现存会话过滤，
    // 否则"命中"会算进一个永远不会渲染的 id。
    const threadIds = group.threadIds.filter((id) => present.has(id) && hitIds.has(id));
    if (threadIds.length > 0) out.push({ ...group, threadIds });
  }
  return { groups: out, keepAll };
}

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
    // 分组自身**只**渲染非置顶会话（ThreadSidebar 的 `.filter((t) => !t.pinned && !t.driver_ref)`）：
    // 置顶会话在 Pinned 区渲染，且按约定仍留在本分组的 `threads` 里。故只有置顶
    // 会话命中时本组一行都渲染不出来，必须连表头一起移除。卡片会话不在此列——
    // 它们由本组自己的「看板会话」小节渲染（搜索期间强制展开，见 renderBoardThreads），
    // 命中即算本组有行。
    const narrowed = narrowGroups(g.groups ?? [], g.threads, threads, tokens);
    // 组名/描述命中时整框保留，框内成员也必须留在 `threads` 里——否则框留下了、
    // 成员却被查询筛空，展开后是个空框（正是"整组保留"要避免的形态）。
    const threads2 =
      narrowed.keepAll.length === 0
        ? threads
        : g.threads.filter((t) => threads.includes(t) || narrowed.keepAll.includes(t.thread_id));
    const narrowedGroups = narrowed.groups;
    // 框也是行：只剩框，框里的成员却一条都没命中时，本组同样一行都画不出来
    // （框内行的渲染条件是 `!isCollapsed && layoutWorkspace(g)`）。
    if (threads2.some((t) => !t.pinned) || narrowedGroups.length > 0) {
      groups2.push({ ...g, threads: threads2, groups: narrowedGroups });
    }
  }

  const pinned2 = pinned.filter((t) =>
    tokens.every((tok) => haystack(t, t.cwd, tagNames).includes(tok)),
  );

  return { pinned: pinned2, groups: groups2, empty: groups2.length === 0 && pinned2.length === 0 };
}
