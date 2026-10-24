/** @vitest-environment jsdom */
import { describe, it, expect } from "vitest";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkRehype from "remark-rehype";
import { rehypeSearchMark } from "./rehypeSearchMark";

/** markdown → hast，带我们的插件。直接驱动管线，不经过 React。 */
async function toHast(markdown: string, query: string) {
  const processor = unified()
    .use(remarkParse)
    .use(remarkGfm)
    .use(remarkRehype)
    .use(rehypeSearchMark, { query });
  return processor.run(processor.parse(markdown));
}

/** hast 里收集所有 mark 的文本。 */
function marks(node: any, out: string[] = []): string[] {
  if (node.type === "element" && node.tagName === "mark") {
    out.push(textOf(node));
    return out;
  }
  for (const child of node.children ?? []) marks(child, out);
  return out;
}

function textOf(node: any): string {
  if (node.type === "text") return node.value;
  return (node.children ?? []).map(textOf).join("");
}

describe("rehypeSearchMark", () => {
  it("wraps each hit in a mark element", async () => {
    expect(marks(await toHast("hello world hello", "hello"))).toEqual(["hello", "hello"]);
  });

  it("marks text inside a fenced code block", async () => {
    expect(marks(await toHast("```\nconst hello = 1;\n```", "hello"))).toEqual(["hello"]);
  });

  it("marks link text", async () => {
    expect(marks(await toHast("[click hello](https://x.test)", "hello"))).toEqual(["hello"]);
  });

  it("emits no mark at all for an empty query", async () => {
    expect(marks(await toHast("hello world", ""))).toEqual([]);
  });

  it("does not match across inline element boundaries", async () => {
    // 跨行内元素边界的短语匹配**做不到**（逐文本节点匹配的已知限制）：
    // `foo **bar** baz` 里搜 `foo bar` 无命中，但搜 `bar` 有命中。
    expect(marks(await toHast("foo **bar** baz", "foo bar"))).toEqual([]);
    expect(marks(await toHast("foo **bar** baz", "bar"))).toEqual(["bar"]);
  });

  it("keeps headings and list text searchable", async () => {
    expect(marks(await toHast("# title\n\n- item hello\n", "hello"))).toEqual(["hello"]);
  });
});
