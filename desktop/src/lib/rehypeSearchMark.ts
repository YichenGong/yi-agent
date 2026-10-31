import { visit } from "unist-util-visit";
import { findMatches } from "./searchHits";
import type { Root, Element, Text } from "hast";

/** 命中片段的标签名。样式在 `index.css` 里按它取。 */
export const SEARCH_HIT_TAG = "mark";
/** 命中片段的 class，测试与样式共用。 */
export const SEARCH_HIT_CLASS = "search-hit";

function makeMark(value: string): Element {
  return {
    type: "element",
    tagName: SEARCH_HIT_TAG,
    properties: { className: [SEARCH_HIT_CLASS] },
    children: [{ type: "text", value }],
  };
}

/**
 * 把文本节点按命中切成 `text + mark + text …`，交回 React 自己渲染。
 *
 * **为什么不是渲染后改 DOM**：那种做法会让 React 与手插的 `<mark>` 抢同一批
 * 文本节点——流式追加期间 `LiveTail` 每帧都在替换尾部文本节点，React 随后要删
 * 的节点已被搬进 mark 里，会抛 `NotFoundError`；而且关闭搜索得逐个还原 DOM，
 * 漏一处就永久污染（比如 mark 留在 `HistoryPrefix` 的冻结子树里）。在这里生成，
 * 关闭搜索等于 `query` 置空后重渲染，零还原逻辑。
 *
 * 已包在 `mark` 里的子树直接跳过，避免重复包裹。
 *
 * **已知限制**：逐文本节点匹配，所以跨行内元素边界的短语搜不到
 * （`foo **bar** baz` 里搜 `foo bar` 无命中）。这与常见实现一致，不是缺陷。
 */
export function rehypeSearchMark(options: { query: string }) {
  const query = options?.query ?? "";
  return (tree: Root): void => {
    if (query.trim() === "") return;
    visit(tree, "text", (node: Text, index, parent) => {
      // `unist-util-visit` 的回调里 index/parent 是 `undefined` 而非 `null`；
      // 按 `null` 判断会让整个函数在类型层面被拒（TS2769）。
      if (index === undefined || parent === undefined) return;
      if (parent.type === "element" && parent.tagName === SEARCH_HIT_TAG) return;
      const value = node.value;
      const matches = findMatches(value, query);
      if (matches.length === 0) return;

      const replacement: Array<Text | Element> = [];
      let cursor = 0;
      for (const m of matches) {
        if (m.start > cursor) {
          replacement.push({ type: "text", value: value.slice(cursor, m.start) });
        }
        replacement.push(makeMark(value.slice(m.start, m.end)));
        cursor = m.end;
      }
      if (cursor < value.length) {
        replacement.push({ type: "text", value: value.slice(cursor) });
      }
      parent.children.splice(index, 1, ...replacement);
      // 跳过刚插入的节点：命中段是纯文本，里面没有需要再切的东西。
      return index + replacement.length;
    });
  };
}
