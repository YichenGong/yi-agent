/**
 * 搜索的**数据层**索引。
 *
 * 为什么要下沉到这里：窗口化之后未挂载的历史不在 DOM 里，原来的
 * `querySelectorAll("mark.search-hit")` 会静默缩小搜索范围——不报错，只是搜不到。
 *
 * 这里只做「哪些条命中、第几个字」，高亮仍由 `rehypeSearchMark` 在 markdown 管线
 * 里完成（那是渲染层的事）。
 */
import { findMatches } from "./searchHits";
import type { RelRect } from "./searchHits";
import { itemText } from "./itemText";
import type { HeightBook } from "./heightBook";
import type { Item } from "./protocol";
import type { NoticeItem, ReasoningItem } from "./session";

export interface SearchHit {
  /** 在 `session.items` 里的下标。 */
  index: number;
  itemId: string;
  /** 该条内的第几个命中，用于在同一条里定位到具体 `mark`。 */
  offset: number;
}

type AnyItem = Item | NoticeItem | ReasoningItem;

/**
 * 在全部 items 上找命中。与 `rehypeSearchMark` 用**同一套 `findMatches`**
 * （大小写不敏感、允许重叠，一条匹配一个 `SearchHit`）。
 *
 * **注意这不是「命中数 == 可见 mark 数」的保证。** 这里匹配的是 `itemText`
 * 返回的**源文本**，而渲染层匹配的是 markdown 解析后的文本节点，两者会分叉：
 *
 * - markdown 语法字符（`**`、`#`、反引号）与链接 URL 在源文本里存在，
 *   渲染后不存在 → 有命中但没有 mark。实测 `"**bold** and **more**"` 搜 `**`
 *   得 4 个命中、0 个 mark。
 * - `reasoning` / `thinking` 走 `ReasoningBlock` 的纯文本渲染，不经过
 *   `rehypeSearchMark`，且默认折叠 → 有命中但没有 mark。
 * - 普通文本 `notice` 在 `ChatView` 里渲染 `{item.text}`（不套 `HighlightText`）
 *   → 有命中但没有 mark；带结构化载荷的 notice 走卡片，`itemText` 返回 `""`。
 * - `toolCall` 两边都不匹配。
 *
 * 所以跳转到这类命中时「高亮看不见」，需要挂载后可能还要展开块。这是设计接受的
 * 取舍（覆盖面优先），但调用方不能假设计数等于可见高亮数。
 */
export function collectHits(items: readonly AnyItem[], query: string): SearchHit[] {
  const out: SearchHit[] = [];
  for (let index = 0; index < items.length; index += 1) {
    const item = items[index];
    const text = itemText(item);
    if (text === "") continue;
    const matches = findMatches(text, query);
    for (let offset = 0; offset < matches.length; offset += 1) {
      out.push({ index, itemId: item.id, offset });
    }
  }
  return out;
}

/** 每条 item 顶边相对内容顶部的估计位置。O(n) 预算一次，供多个命中共用。 */
export function prefixTops(items: readonly AnyItem[], book: HeightBook): number[] {
  const tops = new Array<number>(items.length);
  let y = 0;
  for (let i = 0; i < items.length; i += 1) {
    tops[i] = y;
    y += book.estimate(items[i].id);
  }
  return tops;
}

/**
 * 命中 → 估算矩形。
 *
 * `initialActiveIndex` 需要「每个命中在内容里的 top」，窗口化后绝大多数命中没有
 * DOM，只能用 `HeightBook` + `prefixTops` 估算。误差只影响「打开查找条时先选中
 * 哪个命中」，不影响命中总数。
 */
export function estimatedRects(
  hits: readonly SearchHit[],
  prefixTopsList: readonly number[],
  book: HeightBook,
): RelRect[] {
  return hits.map((hit) => ({
    top: prefixTopsList[hit.index] ?? 0,
    height: book.estimate(hit.itemId),
  }));
}
