import { findMatches } from "../lib/searchHits";
import { SEARCH_HIT_CLASS } from "../lib/rehypeSearchMark";
import type { ReactNode } from "react";

/**
 * 用户气泡的纯文本高亮。
 *
 * 不复用 `MarkdownText`：userMessage 的正文是**纯文本**（父容器靠
 * `whitespace-pre-wrap` 保留换行），交给 markdown 解析会把用户原样打的
 * `*` `#` `-` 当成语法，气泡显示的内容就变了。这里只按命中切分。
 *
 * fragment 用下标作 key：切分结果由 text/query 一对一派生，这条列表不重排。
 */
export function HighlightText({ text, query = "" }: { text: string; query?: string }) {
  const matches = findMatches(text, query);
  if (matches.length === 0) return <>{text}</>;
  const out: ReactNode[] = [];
  let cursor = 0;
  matches.forEach((m, i) => {
    if (m.start > cursor) out.push(text.slice(cursor, m.start));
    out.push(
      <mark key={i} className={SEARCH_HIT_CLASS}>
        {text.slice(m.start, m.end)}
      </mark>,
    );
    cursor = m.end;
  });
  if (cursor < text.length) out.push(text.slice(cursor));
  return <>{out}</>;
}
