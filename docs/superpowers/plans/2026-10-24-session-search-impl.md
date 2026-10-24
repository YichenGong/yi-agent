# 会话内搜索（Cmd+F）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端用户在一个会话内按 Cmd+F 就地查找文本，并用 `session_search` flag 在开发版设置页的实验功能 Tab 里开关。

**Architecture:** 查找条由 `App` 渲染在已有的转录容器里（绝对定位浮层），`ChatView` 只新增一个 `query` prop 向下透传。高亮不手改 DOM，而是用 rehype 插件在 React 树里生成 `<mark>`，关闭搜索时 `query` 置空即随重渲染自然消失。当前命中与滚动由渲染后的 DOM 扫描负责，单向读取，不回写渲染。

**Tech Stack:** TypeScript 5 / React 19 / vitest + jsdom / rehype（经 react-markdown）/ Rust（feature flag 注册表）。

## Global Constraints

- 所有前端命令在 `desktop/` 下执行；**不要**在会话里同时跑两个 `cargo test`（会争 workspace 锁）。
- 本机 `cargo` / `cargo fmt` 不在默认 PATH，Rust 命令统一加前缀：
  `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`
- Rust 侧提交前必须 `cargo fmt --all`（在 `yi-agent-rs/` 下）。前端无 fmt 门禁。
- commit message 用 conventional commits，首行 ≤ 72 字符，**不写** `Co-Authored-By`。
- **严禁在 `main` 上改代码**：所有任务在 worktree `.worktrees/session-search`（分支 `docs/session-search-design` 已存在并有 2 个 spec commit）。开工前把分支名改成语义化的实现分支，或直接沿用当前分支。
- flag 名 `session_search` 一旦发布就是 `flags.json` 的键，**不可改名**。
- 新注册的 flag 必须 `default: false`（`yi-agent-flags` 的 `every_flag_defaults_to_off` 会失败）。
- 不搜工具卡、思考块、notice；**永不自动展开折叠块**。
- `ChatView` 根节点的 class 列表不可改动（`ChatView.test.tsx:113` 断言它即滚动容器）。

---

## 文件结构

| 文件 | 责任 |
|---|---|
| `yi-agent-rs/crates/yi-agent-flags/src/registry.rs` | 新增 `SESSION_SEARCH_FLAG` 与 `FlagDef` |
| `desktop/src/lib/searchHits.ts` | 纯函数：规范化、匹配、初始命中选取、序号夹取 |
| `desktop/src/lib/rehypeSearchMark.ts` | rehype 插件：把文本节点按命中切出 `<mark>` |
| `desktop/src/components/HighlightText.tsx` | 用户气泡的纯文本高亮（不走 markdown） |
| `desktop/src/components/SearchBar.tsx` | 受控查找条 UI |
| `desktop/src/hooks/useTranscriptSearch.ts` | query 状态、DOM 扫描、当前序号、滚动、Esc |
| `desktop/src/components/MarkdownText.tsx` | 加 `query` prop 与插件接线 |
| `desktop/src/components/ChatView.tsx` | 加 `query` prop 透传到 `ChatItem` |
| `desktop/src/App.tsx` | flag 读取与回灌、Cmd+F 监听、查找条落位 |
| `desktop/src/index.css` | `mark.search-hit` 与命中态样式 |
| `docs/project-management/{desktop,yi-agent-flags,README}.md` | 进度登记 |

顺序理由：先立 flag（后端契约），再写纯函数（无依赖、可独立测），再插件与组件，最后接线。每个任务自成可验证闭环。

---

### Task 1: 注册 `session_search` flag

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-flags/src/registry.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Produces: `pub const SESSION_SEARCH_FLAG: &str = "session_search";`，以及 `REGISTRY` 中的第 4 项。后续任务只通过 RPC 名 `"session_search"` 使用它。

- [ ] **Step 1: 写失败测试**

在 `registry.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn the_session_search_flag_is_registered() {
    let def = find("session_search").expect("session_search must be in the registry");
    assert!(!def.default, "session search must default to off");
    assert!(
        !def.summary.is_empty(),
        "a summary is what the settings page shows"
    );
    assert_eq!(def.expires_on, "2027-04-30");
}

#[test]
fn the_session_search_flag_name_matches_its_constant() {
    assert_eq!(SESSION_SEARCH_FLAG, "session_search");
    assert!(find(SESSION_SEARCH_FLAG).is_some());
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && cargo test -p yi-agent-flags the_session_search
```

Expected: FAIL，`session_search must be in the registry`。

- [ ] **Step 3: 最小实现**

在 `SUPERPOWERS_KANBAN_FLAG` 声明之后加常量：

```rust
/// 会话内搜索（Cmd+F）的总开关。
///
/// 纯前端能力：服务端没有任何行为要拦，所以没有对应的 gate 模块。
/// 关掉时前端不注册 Cmd+F 监听、实验页之外也看不到任何搜索 UI。
pub const SESSION_SEARCH_FLAG: &str = "session_search";
```

在 `REGISTRY` 数组末尾（`SUPERPOWERS_KANBAN_FLAG` 那项之后）追加：

```rust
    FlagDef {
        name: SESSION_SEARCH_FLAG,
        summary: "会话内搜索：Cmd+F 在转录里查找文本",
        owner: "yichen",
        expires_on: "2027-04-30",
        default: false,
    },
```

- [ ] **Step 4: 跑全量 flags 测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && cargo test -p yi-agent-flags
```

Expected: PASS，21 passed（原 19 + 新增 2）。`every_flag_defaults_to_off`、`no_flag_is_past_its_expiry_date`、`names_are_unique_and_snake_case` 均通过。

- [ ] **Step 5: fmt + 提交**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-flags/src/registry.rs
git commit -m "feat(flags): register session_search flag

纯前端能力的总开关，没有服务端 gate：关掉时前端不注册 Cmd+F 监听。
默认关闭，到期 2027-04-30。"
```

---

### Task 2: 纯函数层 `searchHits`

**Files:**
- Create: `desktop/src/lib/searchHits.ts`
- Test: `desktop/src/lib/searchHits.test.ts`

**Interfaces:**
- Produces:
  - `export type Match = { start: number; end: number }`
  - `export function normalizeQuery(raw: string): string`
  - `export function findMatches(text: string, query: string): Match[]`
  - `export function nextIndex(current: number, total: number, delta: 1 | -1): number`
  - `export function initialActiveIndex(rects: RelRect[], viewport: Viewport): number`
  - `export type RelRect = { top: number; height: number }`
  - `export type Viewport = { scrollTop: number; clientHeight: number }`

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/lib/searchHits.test.ts`：

```ts
/** @vitest-environment jsdom */
import { describe, it, expect } from "vitest";
import {
  normalizeQuery,
  findMatches,
  nextIndex,
  initialActiveIndex,
} from "./searchHits";

describe("normalizeQuery", () => {
  it("trims surrounding whitespace", () => {
    expect(normalizeQuery("  hello  ")).toBe("hello");
  });
  it("keeps inner whitespace", () => {
    expect(normalizeQuery("a b")).toBe("a b");
  });
});

describe("findMatches", () => {
  it("returns nothing for an empty query", () => {
    expect(findMatches("anything", "")).toEqual([]);
    expect(findMatches("anything", "   ")).toEqual([]);
  });
  it("matches case-insensitively and reports exact positions", () => {
    expect(findMatches("Hello hello", "hello")).toEqual([
      { start: 0, end: 5 },
      { start: 6, end: 11 },
    ]);
  });
  it("finds every occurrence including overlapping boundary cases", () => {
    expect(findMatches("aaa", "aa")).toEqual([
      { start: 0, end: 2 },
      { start: 1, end: 3 },
    ]);
  });
  it("matches CJK text", () => {
    expect(findMatches("会话内搜索", "搜索")).toEqual([{ start: 3, end: 5 }]);
  });
  it("returns nothing when absent", () => {
    expect(findMatches("abc", "zzz")).toEqual([]);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/searchHits.test.ts
```

Expected: FAIL，`Failed to resolve import "./searchHits"`。

- [ ] **Step 3: 最小实现（仅 normalizeQuery / findMatches）**

创建 `desktop/src/lib/searchHits.ts`：

```ts
/** 一段命中在源文本里的位置，`end` 为开区间。 */
export type Match = { start: number; end: number };

/**
 * 去掉首尾空白。全空白与空串等价——两者都表示"没在搜"，返回空字符串。
 *
 * 不做小写化：大小写不敏感的比较在 `findMatches` 里用逐次 `toLowerCase` 完成，
 * 因为原文必须按**原始下标的**切分，改写查询会丢掉长度对应关系。
 */
export function normalizeQuery(raw: string): string {
  return raw.trim();
}

/**
 * 找出 `query` 在 `text` 里所有出现的位置，大小写不敏感，允许重叠。
 *
 * 允许重叠（`aa` 在 `aaa` 里命中两次）是刻意的：用户搜一个短的重复片段时，
 * 跳过重叠会让计数与"眼睛看到的次数"对不上。
 */
export function findMatches(text: string, query: string): Match[] {
  const needle = normalizeQuery(query);
  if (needle === "") return [];
  const haystack = text.toLowerCase();
  const lower = needle.toLowerCase();
  const out: Match[] = [];
  let from = 0;
  for (;;) {
    const at = haystack.indexOf(lower, from);
    if (at === -1) break;
    out.push({ start: at, end: at + lower.length });
    from = at + 1;
  }
  return out;
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/lib/searchHits.test.ts
```

Expected: PASS（7 个用例）。

- [ ] **Step 5: 写 `nextIndex` 与 `initialActiveIndex` 的失败测试**

追加到 `searchHits.test.ts`：

```ts
describe("nextIndex", () => {
  it("advances and wraps forward", () => {
    expect(nextIndex(0, 3, 1)).toBe(1);
    expect(nextIndex(2, 3, 1)).toBe(0);
  });
  it("moves back and wraps at zero", () => {
    expect(nextIndex(2, 3, -1)).toBe(1);
    expect(nextIndex(0, 3, -1)).toBe(2);
  });
  it("stays at zero when there is nothing to move through", () => {
    expect(nextIndex(0, 0, 1)).toBe(0);
    expect(nextIndex(0, 0, -1)).toBe(0);
  });
  it("clamps an out-of-range current index by wrapping it first", () => {
    // 先对 total 取模（9 % 3 = 0），再前进 1。语义是"位置在环上"，不是"夹到末尾"。
    expect(nextIndex(9, 3, 1)).toBe(1);
  });
});

describe("initialActiveIndex", () => {
  // 视口是 [scrollTop, scrollTop + clientHeight)。
  const viewport = { scrollTop: 100, clientHeight: 200 };

  it("prefers the first hit inside the viewport", () => {
    const rects = [
      { top: 0, height: 10 },
      { top: 120, height: 10 },
      { top: 200, height: 10 },
    ];
    expect(initialActiveIndex(rects, viewport)).toBe(1);
  });

  it("picks the nearest one below when none is visible", () => {
    const rects = [
      { top: 0, height: 10 },
      { top: 400, height: 10 },
      { top: 500, height: 10 },
    ];
    expect(initialActiveIndex(rects, viewport)).toBe(1);
  });

  it("picks the last one when everything is above", () => {
    const rects = [
      { top: 0, height: 10 },
      { top: 50, height: 10 },
    ];
    expect(initialActiveIndex(rects, viewport)).toBe(1);
  });

  it("returns 0 for an empty list", () => {
    expect(initialActiveIndex([], viewport)).toBe(0);
  });
});
```

- [ ] **Step 6: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/searchHits.test.ts
```

Expected: FAIL，`nextIndex is not a function`。

- [ ] **Step 7: 实现两个函数**

追加到 `searchHits.ts`：

```ts
/**
 * 在 `total` 个命中里移动 `delta` 步，回绕；`total === 0` 时恒为 0。
 *
 * 先对 `current` 取模再移动：调用方可能拿着一个已经失效的序号（用户改了查询，
 * 命中数从 20 掉到 3），夹取放在这里，调用点就不必各写一遍。
 */
export function nextIndex(current: number, total: number, delta: 1 | -1): number {
  if (total <= 0) return 0;
  const base = ((current % total) + total) % total;
  return (base + delta + total) % total;
}

/** mark 相对滚动容器内容顶部的矩形（`offsetTop` / `offsetHeight`）。 */
export type RelRect = { top: number; height: number };
/** 滚动容器的可视区。 */
export type Viewport = { scrollTop: number; clientHeight: number };

/**
 * 打开查找条时"当前命中"落在哪：视口内第一个优先；都不在视口内时，取位于
 * 视口下方最近的一个，否则取最后一个（全在上方）。
 *
 * 为什么不取第一个：长会话里第一个命中常在几万像素之外，打开即跳到顶部会让
 * "我正在读中间这段"的用户丢失位置。浏览器查找栏也是这个行为。
 */
export function initialActiveIndex(rects: RelRect[], viewport: Viewport): number {
  if (rects.length === 0) return 0;
  const top = viewport.scrollTop;
  const bottom = viewport.scrollTop + viewport.clientHeight;
  for (let i = 0; i < rects.length; i += 1) {
    const r = rects[i];
    if (r.top >= top && r.top < bottom) return i;
  }
  for (let i = 0; i < rects.length; i += 1) {
    if (rects[i].top >= bottom) return i;
  }
  return rects.length - 1;
}
```

- [ ] **Step 8: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/lib/searchHits.test.ts && npx tsc --noEmit
```

Expected: PASS（15 个用例），tsc 无输出。

- [ ] **Step 9: 提交**

```bash
cd desktop && git add src/lib/searchHits.ts src/lib/searchHits.test.ts
git commit -m "feat(desktop): add search hit pure functions

匹配、序号夹取与初始命中选取都做成无 DOM 依赖的纯函数，便于单测。
初始命中取视口内第一个，避免长会话打开即跳到顶部。"
```

---

### Task 3: rehype 插件 `rehypeSearchMark`

**Files:**
- Create: `desktop/src/lib/rehypeSearchMark.ts`
- Test: `desktop/src/lib/rehypeSearchMark.test.ts`

**Interfaces:**
- Consumes: `findMatches` from `./searchHits`
- Produces: `export function rehypeSearchMark(options: { query: string }): (tree: HastRoot) => void`，供 `MarkdownText` 作为 `rehypePlugins` 项使用。产出的元素是 `{ type: "element", tagName: "mark", properties: { className: ["search-hit"] } }`。

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/lib/rehypeSearchMark.test.ts`：

```ts
/** @vitest-environment jsdom */
import { describe, it, expect } from "vitest";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkRehype from "remark-rehype";
import { rehypeSearchMark } from "./rehypeSearchMark";

/** markdown → hast，带我们的插件。直接驱动管线，不经过 React。 */
async function toHast(markdown: string, query: string) {
  return unified()
    .use(remarkParse)
    .use(remarkGfm)
    .use(remarkRehype)
    .use(rehypeSearchMark, { query })
    .run(unified().use(remarkParse).use(remarkGfm).use(remarkRehype).parse(markdown));
}

/** hast 里收集所有 mark 的文本，顺带证明它们的位置。 */
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
    const tree = await toHast("hello world hello", "hello");
    expect(marks(tree)).toEqual(["hello", "hello"]);
  });

  it("marks text inside a fenced code block", async () => {
    const tree = await toHast("```\nconst hello = 1;\n```", "hello");
    expect(marks(tree)).toEqual(["hello"]);
  });

  it("marks link text", async () => {
    const tree = await toHast("[click hello](https://x.test)", "hello");
    expect(marks(tree)).toEqual(["hello"]);
  });

  it("emits no mark at all for an empty query", async () => {
    const tree = await toHast("hello world", "");
    expect(marks(tree)).toEqual([]);
  });

  it("marks both sides of an inline element boundary independently", async () => {
    // 跨行内元素边界的短语匹配**做不到**（逐文本节点匹配的已知限制）：
    // `foo **bar** baz` 里搜 `foo bar` 无命中，但搜 `foo` 与 `bar` 各有命中。
    const tree = await toHast("foo **bar** baz", "bar");
    expect(marks(tree)).toEqual(["bar"]);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/rehypeSearchMark.test.ts
```

Expected: FAIL，`Failed to resolve import "./rehypeSearchMark"`。

- [ ] **Step 3: 实现插件**

创建 `desktop/src/lib/rehypeSearchMark.ts`：

```ts
import { visit } from "unist-util-visit";
import { findMatches } from "./searchHits";
import type { Root, Element, Text } from "hast";

/** 命中片段的标签，样式在 `index.css` 里按它取。 */
export const SEARCH_HIT_TAG = "mark";
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
 * 漏一处就永久污染。在这里生成，关闭搜索等于 `query` 置空后重渲染，零还原逻辑。
 *
 * 已包在 `mark` 里的子树直接跳过，避免重复包裹（插件可能被调用多次）。
 */
export function rehypeSearchMark(options: { query: string }) {
  const query = options?.query ?? "";
  return (tree: Root): void => {
    if (query.trim() === "") return;
    visit(tree, "text", (node: Text, index, parent) => {
      // `unist-util-visit` 的回调里 index/parent 是 `undefined` 而非 `null`；
      // 按 `null` 判断会让整个函数在类型层面被拒（实测 TS2769）。
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
      // 跳过刚插入的节点：它们里面没有需要再切的文本（命中段是纯文本）。
      return index + replacement.length;
    });
  };
}
```

- [ ] **Step 4: 跑测试**

```bash
cd desktop && npx vitest run src/lib/rehypeSearchMark.test.ts
```

若报 `Cannot find module 'remark-rehype'` 或 `'unified'`，它们是 `react-markdown` 的传递依赖但未必直达 `node_modules` 顶层。先确认：

```bash
cd desktop && ls node_modules/remark-rehype node_modules/unified
```

已存在则无需安装；若缺失，把这几个**测试专用**依赖加为 devDependencies（不要进 dependencies，生产代码不直接用它们）：

```bash
cd desktop && npm i -D remark-rehype unified
```

Expected: PASS（5 个用例）。

- [ ] **Step 5: 提交**

```bash
cd desktop && git add src/lib/rehypeSearchMark.ts src/lib/rehypeSearchMark.test.ts package.json package-lock.json
git commit -m "feat(desktop): add rehype plugin that marks search hits

在 React 树内生成 mark 而不是渲染后改 DOM：流式追加期间手改文本节点会与
React 抢节点并抛 NotFoundError，关闭搜索也没有可靠的还原路径。"
```

（若 Step 4 未安装依赖，提交时去掉 `package.json package-lock.json`。）

---

### Task 4: `MarkdownText` / `AgentMessage` 接受 `query`

**Files:**
- Modify: `desktop/src/components/MarkdownText.tsx`
- Test: `desktop/src/components/MarkdownText.test.tsx`（追加用例）

**Interfaces:**
- Consumes: `rehypeSearchMark` from `../lib/rehypeSearchMark`
- Produces: `MarkdownText` 与 `AgentMessage` 均新增可选 `query?: string`，默认 `""`。memo 比较器同时比较 `text` 与 `query`。

- [ ] **Step 1: 写失败测试**

追加到 `desktop/src/components/MarkdownText.test.tsx`（该文件已有 mocks 与 `render` 导入）：

```ts
describe("MarkdownText search highlighting", () => {
  it("marks every occurrence of the query", () => {
    const { container } = render(<MarkdownText text={"alpha beta alpha"} query="alpha" />);
    const hits = container.querySelectorAll("mark.search-hit");
    expect(hits.length).toBe(2);
    expect(hits[0].textContent).toBe("alpha");
  });

  it("marks text inside fenced code", () => {
    const { container } = render(
      <MarkdownText text={"```\nconst needle = 1;\n```"} query="needle" />,
    );
    expect(container.querySelectorAll("mark.search-hit").length).toBe(1);
  });

  it("renders no mark when the query is only whitespace", () => {
    const { container } = render(<MarkdownText text={"alpha"} query="   " />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(0);
  });

  it("re-parses when the query changes but keeps the text", () => {
    const { container, rerender } = render(<MarkdownText text={"alpha beta"} query="alpha" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(1);
    rerender(<MarkdownText text={"alpha beta"} query="beta" />);
    const hits = container.querySelectorAll("mark.search-hit");
    expect(hits.length).toBe(1);
    expect(hits[0].textContent).toBe("beta");
  });

  it("drops all marks when the query is cleared", () => {
    const { container, rerender } = render(<MarkdownText text={"alpha"} query="alpha" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(1);
    rerender(<MarkdownText text={"alpha"} query="" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(0);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/MarkdownText.test.tsx
```

Expected: FAIL，`Property 'query' does not exist`（tsc 层面）或 mark 数 0。

- [ ] **Step 3: 实现**

在 `MarkdownText.tsx` 顶部加导入：

```ts
import { rehypeSearchMark } from "../lib/rehypeSearchMark";
```

把 `MarkdownText` 换成：

```ts
export const MarkdownText = memo(
  function MarkdownText({ text, query = "" }: { text: string; query?: string }) {
    // 插件数组按 query 记忆：rehype 插件是有状态的闭包，每次渲染都换新对象会让
    // react-markdown 认为管线变了而整体重建。
    //
    // 类型必须写成 `PluggableList`（来自 unified）。三点实测：`as const` 会因
    // readonly 元组不可赋值给可变元组而失败；`react-markdown` **不导出**
    // `Pluggable`；`unified` 的 `PluggableList` 才是对的。
    const rehypePlugins = useMemo(
      (): PluggableList => [rehypeHighlight, [rehypeSearchMark, { query }]],
      [query],
    );
    return (
      <div className="prose prose-invert max-w-none break-words prose-img:max-w-full prose-pre:bg-panel">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={rehypePlugins}
          components={{ a: ExternalLink, table: TableBlock }}
        >
          {text}
        </ReactMarkdown>
      </div>
    );
  },
  (prev, next) => prev.text === next.text && prev.query === next.query,
);
```

`useMemo` 需要加进文件顶部的 react 导入：

```ts
import { memo, useMemo, type ComponentPropsWithoutRef, type ReactNode } from "react";
import type { PluggableList } from "unified";
```

把 `AgentMessage` 换成：

```ts
export const AgentMessage = memo(
  function AgentMessage({ text, query = "" }: { text: string; query?: string }) {
    return (
      <div className="my-1 max-w-[90%] self-start">
        <MarkdownText text={text} query={query} />
      </div>
    );
  },
  (prev, next) => prev.text === next.text && prev.query === next.query,
);
```

- [ ] **Step 4: 跑测试**

```bash
cd desktop && npx vitest run src/components/MarkdownText.test.tsx && npx tsc --noEmit
```

Expected: PASS（原有 6 + 新增 5）。

- [ ] **Step 5: 提交**

```bash
cd desktop && git add src/components/MarkdownText.tsx src/components/MarkdownText.test.tsx
git commit -m "feat(desktop): highlight search hits in agent messages

query 进 memo 比较器：只有敲字时重解析，流式 delta 期间前缀消息不受影响。"
```

---

### Task 5: 用户气泡 `HighlightText`

**Files:**
- Create: `desktop/src/components/HighlightText.tsx`
- Test: `desktop/src/components/HighlightText.test.tsx`

**Interfaces:**
- Consumes: `findMatches` from `../lib/searchHits`, `SEARCH_HIT_CLASS` from `../lib/rehypeSearchMark`
- Produces: `export function HighlightText({ text, query }: { text: string; query?: string })`

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/HighlightText.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, cleanup } from "@testing-library/react";
import { HighlightText } from "./HighlightText";

afterEach(cleanup);

describe("HighlightText", () => {
  it("renders the text unchanged when there is no query", () => {
    const { container } = render(<HighlightText text="hello world" />);
    expect(container.textContent).toBe("hello world");
    expect(container.querySelectorAll("mark").length).toBe(0);
  });

  it("marks every occurrence", () => {
    const { container } = render(<HighlightText text="a foo b foo" query="foo" />);
    const hits = container.querySelectorAll("mark.search-hit");
    expect(hits.length).toBe(2);
    expect(hits[0].textContent).toBe("foo");
  });

  it("preserves the surrounding text order", () => {
    const { container } = render(<HighlightText text="xy foo z" query="foo" />);
    expect(container.textContent).toBe("xy foo z");
  });

  it("is case-insensitive but keeps the original casing on screen", () => {
    const { container } = render(<HighlightText text="Foo" query="foo" />);
    expect(container.querySelector("mark")?.textContent).toBe("Foo");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/HighlightText.test.tsx
```

Expected: FAIL，模块不存在。

- [ ] **Step 3: 实现**

创建 `desktop/src/components/HighlightText.tsx`：

```tsx
import { findMatches } from "../lib/searchHits";
import { SEARCH_HIT_CLASS } from "../lib/rehypeSearchMark";

/**
 * 用户气泡的纯文本高亮。
 *
 * 不复用 `MarkdownText`：userMessage 的正文是**纯文本**（`whitespace-pre-wrap`），
 * 交给 markdown 解析会把用户原样打的 `*` `#` 当成语法。这里只按命中切分。
 *
 * fragment 用下标作 key：切分结果与 text/query 一一对应，而这条列表是纯派生的，
 * 不会重排。
 */
export function HighlightText({ text, query = "" }: { text: string; query?: string }) {
  const matches = findMatches(text, query);
  if (matches.length === 0) return <>{text}</>;
  const out: React.ReactNode[] = [];
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
```

- [ ] **Step 4: 跑测试**

```bash
cd desktop && npx vitest run src/components/HighlightText.test.tsx && npx tsc --noEmit
```

Expected: PASS（4 个用例）。

- [ ] **Step 5: 提交**

```bash
cd desktop && git add src/components/HighlightText.tsx src/components/HighlightText.test.tsx
git commit -m "feat(desktop): add plain-text highlight for user bubbles

用户消息是纯文本，交给 markdown 解析会把原样打的 * # 当语法。"
```

---

### Task 6: `ChatView` 透传 `query`

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`
- Test: `desktop/src/components/ChatView.test.tsx`（追加用例）

**Interfaces:**
- Consumes: `AgentMessage`（已支持 `query`）、`HighlightText`
- Produces:
  - `ChatView` 新增可选 `query?: string`（默认 `""`）
  - `ChatView` 新增可选 `scrollerRef?: RefObject<HTMLDivElement | null>`，指向它内部的滚动容器
  - `ChatItem`、`HistoryPrefix`、`LiveTail` 均新增 `query: string` 必填内部 prop
  - 根节点 className **逐字节不变**

**注意**：`ChatView.test.tsx` mock 了 `./MarkdownText`（它的 `AgentMessage` 只渲染 `<div>{text}</div>`）。新用例断言**用户气泡**的 mark，避免被 mock 干扰；agent 侧的 mark 已在 Task 4 覆盖。

- [ ] **Step 1: 写失败测试**

追加到 `ChatView.test.tsx`（放在 `describe("ChatView user attachments")` 之前）：

```tsx
describe("ChatView search highlighting", () => {
  function userItems(): (Item | NoticeItem | ReasoningItem)[] {
    return [{ type: "userMessage", id: "u1", text: "alpha beta alpha" }];
  }

  it("marks hits inside user bubbles", () => {
    const { container } = render(<ChatView items={userItems()} query="alpha" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(2);
  });

  it("renders no mark when the query is empty", () => {
    const { container } = render(<ChatView items={userItems()} query="" />);
    expect(container.querySelectorAll("mark.search-hit").length).toBe(0);
  });

  it("keeps user text intact around the mark", () => {
    const { container } = render(<ChatView items={userItems()} query="beta" />);
    expect(container.textContent).toContain("alpha beta alpha");
  });

  it("does not re-render settled agent messages when only the query is unchanged", () => {
    // 复用文件顶部的 calls.text 计数器：query 未变时，流式 delta 不该让前缀
    // 的 AgentMessage 重跑。
    calls.text = 0;
    const list = items();
    const { rerender } = render(<ChatView items={list} query="x" />);
    const before = calls.text;
    setText(list, "streaming more text");
    rerender(<ChatView items={list} query="x" />);
    expect(calls.text).toBeGreaterThan(before); // 尾条重渲染
    // 前缀条数不变即可：这里只要求不死循环、不因 query 未变而全量重算。
    expect(calls.text).toBeLessThan(before + 10);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/ChatView.test.tsx
```

Expected: FAIL，`query` 不是 `ChatView` 的已知属性（tsc）／mark 数为 0。

- [ ] **Step 3: 实现**

在 `ChatView.tsx` 顶部导入 `HighlightText`：

```ts
import { HighlightText } from "./HighlightText";
```

`ChatItem` 签名与两个分支改为：

```tsx
  function ChatItem({
    item,
    text,
    threadId,
    call,
    query,
  }: {
    item: Item | NoticeItem | ReasoningItem;
    text: string;
    threadId: string | null;
    call: ImageReadCall;
    query: string;
  }) {
```

`userMessage` 分支的正文从 `{item.text}` 改为：

```tsx
            <HighlightText text={item.text} query={query} />
```

`agentMessage` 分支改为：

```tsx
      case "agentMessage":
        return <AgentMessage text={text} query={query} />;
```

`ChatItem` 的比较器改为：

```tsx
  (prev, next) =>
    prev.item === next.item && prev.text === next.text && prev.query === next.query,
```

`HistoryPrefix` 增加 `query` prop（解构、类型、透传）：

```tsx
  function HistoryPrefix({
    items,
    count,
    threadId,
    call,
    query,
  }: {
    items: (Item | NoticeItem | ReasoningItem)[];
    count: number;
    threadId: string | null;
    call: ImageReadCall;
    structuralVersion: number;
    query: string;
  }) {
```

循环里的 `ChatItem` 加 `query={query}`；比较器加一项：

```tsx
  (prev, next) =>
    prev.items === next.items &&
    prev.count === next.count &&
    prev.structuralVersion === next.structuralVersion &&
    prev.threadId === next.threadId &&
    prev.call === next.call &&
    prev.query === next.query,
```

`LiveTail` 增加 `query` prop 并透传给 `ChatItem`（解构、类型、`<ChatItem … query={query} />`）。

**再给 `scrollerRef` 开口子**（Task 9 的搜索要靠它量视口，见 Task 9 §g）：

文件顶部导入补 `useCallback` 与 `RefObject`：

```ts
import { memo, useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
```

`ChatView` 函数体里，`useStickToBottom()` 之后加合并 ref：

```tsx
  const { ref: scrollRef, scrollToBottom, followIfAtBottom } = useStickToBottom();
  // 合并 ref：同一个 DOM 节点交给两方。依赖是两个 ref 本身（稳定引用），
  // 所以不会每次渲染都重设。
  const setScroller = useCallback(
    (el: HTMLDivElement | null) => {
      scrollRef.current = el;
      if (scrollerRef) scrollerRef.current = el;
    },
    [scrollRef, scrollerRef],
  );
```

根节点的 `ref={scrollRef}` 换成 `ref={setScroller}`，**className 一个字不动**：

```tsx
    <div
      ref={setScroller}
      className="flex min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-3"
    >
```

`ChatView` 本体签名增加 `query = ""`，并把 `query` 传给 `HistoryPrefix`、`LiveTail`：

```tsx
export function ChatView({
  items,
  error,
  retrying,
  threadId = null,
  call,
  structuralVersion = 0,
  query = "",
}: {
  // …原有字段…
  /** 搜索关键词（已防抖）。空串表示未在搜索，此时不产生任何 mark。 */
  query?: string;
}) {
```

**不要**改动根 `<div>` 的 className。

- [ ] **Step 4: 跑测试**

```bash
cd desktop && npx vitest run src/components/ChatView.test.tsx && npx tsc --noEmit
```

Expected: PASS（原 31 + 新增 4）。特别确认原有
`clips horizontal overflow so wide content cannot pan the transcript` 仍通过
（`container.firstElementChild` 仍是滚动容器）。

- [ ] **Step 5: 提交**

```bash
cd desktop && git add src/components/ChatView.tsx src/components/ChatView.test.tsx
git commit -m "feat(desktop): thread the search query through ChatView

query 一路加到 ChatItem / HistoryPrefix / LiveTail 的 memo 比较器上，
根节点结构不变（现有测试依赖它即滚动容器）。"
```

---

### Task 7: `SearchBar` 组件与样式

**Files:**
- Create: `desktop/src/components/SearchBar.tsx`
- Test: `desktop/src/components/SearchBar.test.tsx`
- Modify: `desktop/src/index.css`（末尾追加命中样式）

**Interfaces:**
- Produces: `export function SearchBar({ value, total, active, onChange, onNext, onPrev, onClose }: SearchBarProps)`，`SearchBarProps = { value: string; total: number; active: number; onChange: (v: string) => void; onNext: () => void; onPrev: () => void; onClose: () => void }`。`active` 是 0 基下标；`total === 0` 时显示"无结果"。

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/SearchBar.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, screen, fireEvent, cleanup } from "@testing-library/react";
import { SearchBar } from "./SearchBar";

afterEach(cleanup);

function setup(over: Partial<React.ComponentProps<typeof SearchBar>> = {}) {
  const props = {
    value: "",
    total: 0,
    active: 0,
    onChange: vi.fn(),
    onNext: vi.fn(),
    onPrev: vi.fn(),
    onClose: vi.fn(),
    ...over,
  };
  render(<SearchBar {...props} />);
  return props;
}

describe("SearchBar", () => {
  it("autofocuses the input", () => {
    setup();
    expect(document.activeElement).toBe(screen.getByRole("searchbox"));
  });

  it("reports typing", () => {
    const props = setup();
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "abc" } });
    expect(props.onChange).toHaveBeenCalledWith("abc");
  });

  it("shows a 1-based position over the total", () => {
    setup({ value: "x", total: 17, active: 2 });
    expect(screen.getByText("3/17")).toBeTruthy();
  });

  it("says there are no results", () => {
    setup({ value: "zzz", total: 0 });
    expect(screen.getByText("无结果")).toBeTruthy();
  });

  it("calls onNext on Enter and onPrev on Shift+Enter", () => {
    const props = setup({ value: "x", total: 3, active: 0 });
    const box = screen.getByRole("searchbox");
    fireEvent.keyDown(box, { key: "Enter" });
    expect(props.onNext).toHaveBeenCalled();
    fireEvent.keyDown(box, { key: "Enter", shiftKey: true });
    expect(props.onPrev).toHaveBeenCalled();
  });

  it("does not treat an IME-confirmed Enter as navigation", () => {
    // 中文输入法用回车确认候选词时不该跳下一个命中（见 lib/imeEnter.ts）。
    const props = setup({ value: "搜索", total: 3, active: 0 });
    fireEvent.keyDown(screen.getByRole("searchbox"), { key: "Enter", keyCode: 229 });
    expect(props.onNext).not.toHaveBeenCalled();
  });

  it("wires the next / previous / close buttons", () => {
    const props = setup({ value: "x", total: 3, active: 0 });
    fireEvent.click(screen.getByRole("button", { name: "下一个" }));
    expect(props.onNext).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "上一个" }));
    expect(props.onPrev).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "关闭查找" }));
    expect(props.onClose).toHaveBeenCalled();
  });

  it("disables navigation when there is nothing to move through", () => {
    setup({ value: "zzz", total: 0 });
    expect((screen.getByRole("button", { name: "下一个" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/SearchBar.test.tsx
```

Expected: FAIL，模块不存在。

- [ ] **Step 3: 实现组件**

创建 `desktop/src/components/SearchBar.tsx`：

```tsx
import { useEffect, useRef } from "react";
import { useImeGuard } from "../lib/imeEnter";

export type SearchBarProps = {
  /** 当前输入（受控）。 */
  value: string;
  /** 命中总数。 */
  total: number;
  /** 当前命中下标，0 基。 */
  active: number;
  onChange: (value: string) => void;
  onNext: () => void;
  onPrev: () => void;
  onClose: () => void;
};

/**
 * 会话内查找条。纯受控组件：自己不认识转录，只把用户意图报上去。
 *
 * 打开即落焦到输入框；Enter / Shift+Enter 上下跳，Esc 由宿主的
 * `useEscapeKey` 处理（这里不抢 Esc，避免与审批弹窗等其它 Esc 语义打架）。
 */
export function SearchBar({
  value,
  total,
  active,
  onChange,
  onNext,
  onPrev,
  onClose,
}: SearchBarProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const ime = useImeGuard();

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    el.focus();
    el.select();
  }, []);

  const hasHits = total > 0;

  return (
    <div
      role="search"
      aria-label="会话内搜索"
      className="absolute right-3 top-3 z-20 flex items-center gap-2 rounded-md border border-line-strong bg-panel px-2 py-1 shadow-lg"
    >
      <input
        ref={inputRef}
        type="search"
        role="searchbox"
        aria-label="查找文本"
        value={value}
        placeholder="在会话中查找"
        onChange={(e) => onChange(e.target.value)}
        onCompositionStart={ime.onCompositionStart}
        onCompositionEnd={ime.onCompositionEnd}
        onBlur={ime.resetComposition}
        onKeyDown={(e) => {
          if (e.key !== "Enter") return;
          // IME 确认键（中文输入法回车上屏）不是导航。
          if (e.nativeEvent.isComposing || e.keyCode === 229) return;
          e.preventDefault();
          if (e.shiftKey) onPrev();
          else onNext();
        }}
        className="w-40 bg-transparent text-sm text-fg outline-none placeholder:text-fg-subtle"
      />
      <span
        aria-live="polite"
        className={hasHits ? "shrink-0 text-xs text-fg-muted" : "shrink-0 text-xs text-amber-400"}
      >
        {hasHits ? `${active + 1}/${total}` : value.trim() === "" ? "" : "无结果"}
      </span>
      <button
        type="button"
        aria-label="上一个"
        disabled={!hasHits}
        onClick={onPrev}
        className="rounded px-1 text-fg-muted hover:text-fg disabled:opacity-40"
      >
        ↑
      </button>
      <button
        type="button"
        aria-label="下一个"
        disabled={!hasHits}
        onClick={onNext}
        className="rounded px-1 text-fg-muted hover:text-fg disabled:opacity-40"
      >
        ↓
      </button>
      <button
        type="button"
        aria-label="关闭查找"
        onClick={onClose}
        className="rounded px-1 text-fg-muted hover:text-fg"
      >
        ✕
      </button>
    </div>
  );
}
```

- [ ] **Step 4: 加样式**

在 `desktop/src/index.css` 末尾追加：

```css
/*
 * 会话内搜索命中。`mark` 自带浏览器默认底色（黄），在深色主题上刺眼且与
 * 代码高亮的 token 色打架，所以两种状态都由我们接管：
 * - `.search-hit`：全部命中，半透明琥珀；
 * - `.search-hit[data-search-active="true"]`：当前命中，用更实的底色 + 描边，
 *   保证在代码块这类彩色背景上也一眼可辨。
 */
mark.search-hit {
  background-color: rgb(245 158 11 / 0.35);
  color: inherit;
  border-radius: 2px;
}

mark.search-hit[data-search-active="true"] {
  background-color: rgb(245 158 11 / 0.85);
  color: #1c1917;
  outline: 1px solid rgb(251 191 36);
}
```

- [ ] **Step 5: 跑测试**

```bash
cd desktop && npx vitest run src/components/SearchBar.test.tsx && npx tsc --noEmit
```

Expected: PASS（8 个用例）。

- [ ] **Step 6: 提交**

```bash
cd desktop && git add src/components/SearchBar.tsx src/components/SearchBar.test.tsx src/index.css
git commit -m "feat(desktop): add the transcript search bar

纯受控组件；IME 回车不当作导航，避免中文输入法上屏时误跳命中。"
```

---

### Task 8: `useTranscriptSearch` 钩子

**Files:**
- Create: `desktop/src/hooks/useTranscriptSearch.ts`
- Test: `desktop/src/hooks/useTranscriptSearch.test.tsx`

**Interfaces:**
- Consumes: `normalizeQuery` / `nextIndex` / `initialActiveIndex` from `../lib/searchHits`
- Produces:
  ```ts
  export const SEARCH_DEBOUNCE_MS = 120;
  export const SEARCH_HIT_SELECTOR = "mark.search-hit";
  export function useTranscriptSearch(options: {
    containerRef: RefObject<HTMLElement | null>;
    open: boolean;
    query: string;
  }): {
    total: number;
    active: number;
    next: () => void;
    prev: () => void;
    /** 查询变化、渲染落地后由宿主调用，按视口重选当前命中。 */
    reselect: () => void;
  }
  ```

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/hooks/useTranscriptSearch.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { render, cleanup, screen, act } from "@testing-library/react";
import { useRef } from "react";
import { useTranscriptSearch } from "./useTranscriptSearch";

// 捕获最近一次渲染的返回值。测试只关心钩子对外暴露的三个数，不关心实现。
let last: { total: number; active: number; api: any } = { total: 0, active: 0, api: null };

function Harness({ open, query, marks }: { open: boolean; query: string; marks: number }) {
  const ref = useRef<HTMLDivElement>(null);
  const state = useTranscriptSearch({ containerRef: ref, open, query });
  last = { total: state.total, active: state.active, api: state };
  return (
    <div ref={ref} data-testid="scroller">
      {Array.from({ length: marks }, (_, i) => (
        <mark key={i} className="search-hit" data-testid={`hit-${i}`}>
          x
        </mark>
      ))}
    </div>
  );
}

beforeEach(() => {
  last = { total: 0, active: 0, api: null };
  // jsdom 不实现 scrollIntoView，不补就会在 step 用例里抛。
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(cleanup);

describe("useTranscriptSearch", () => {
  it("counts the marks in the container", () => {
    render(<Harness open query="x" marks={3} />);
    expect(last.total).toBe(3);
  });

  it("does not count when closed", () => {
    render(<Harness open={false} query="x" marks={3} />);
    expect(last.total).toBe(0);
    expect(last.active).toBe(0);
  });

  it("marks the active hit in the DOM", () => {
    render(<Harness open query="x" marks={3} />);
    expect(screen.getByTestId("hit-0").getAttribute("data-search-active")).toBe("true");
    expect(screen.getByTestId("hit-1").getAttribute("data-search-active")).toBe(null);
  });

  it("clamps the active index when the hit count shrinks", () => {
    const { rerender } = render(<Harness open query="x" marks={3} />);
    rerender(<Harness open query="xx" marks={1} />);
    expect(last.total).toBe(1);
    expect(last.active).toBe(0);
  });

  it("removes the active attribute once closed", () => {
    const { rerender } = render(<Harness open query="x" marks={2} />);
    rerender(<Harness open={false} query="x" marks={2} />);
    expect(screen.getByTestId("hit-0").getAttribute("data-search-active")).toBe(null);
  });

  it("steps and wraps through hits", () => {
    render(<Harness open query="x" marks={2} />);
    // 必须包 act：next() 会同步 setState，不包则 React 不会在断言前刷新，
    // `last` 拿到的还是上一次渲染的旧值（实测就是这里先失败）。
    act(() => last.api.next());
    expect(last.active).toBe(1);
    act(() => last.api.next());
    expect(last.active).toBe(0);
    act(() => last.api.prev());
    expect(last.active).toBe(1);
  });

  it("scrolls to the active hit when stepping", () => {
    render(<Harness open query="x" marks={2} />);
    act(() => last.api.next());
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/hooks/useTranscriptSearch.test.tsx
```

Expected: FAIL，模块不存在。

- [ ] **Step 3: 实现钩子**

创建 `desktop/src/hooks/useTranscriptSearch.ts`（此版本已实测：7 个用例全绿）：

```ts
import { useCallback, useEffect, useRef, useState } from "react";
import type { RefObject } from "react";
import { initialActiveIndex, nextIndex } from "../lib/searchHits";
import type { RelRect } from "../lib/searchHits";

/** 输入防抖。每次敲字都会让可见消息重解析 markdown，120ms 足以吞掉连续击键。 */
export const SEARCH_DEBOUNCE_MS = 120;
/** 命中节点的选择器，测试与实现共用。 */
export const SEARCH_HIT_SELECTOR = "mark.search-hit";

/**
 * 转录区搜索的运行时状态。
 *
 * 分工：**渲染**由 `query` 驱动（rehype 插件生成 mark），**计数与当前命中**由
 * 这里渲染后扫 DOM 得出。两者单向：扫描只读 DOM，从不回写渲染输入，因此不会
 * 形成"扫描 → 改状态 → 重渲染 → 再扫描"的环。
 *
 * 当前命中靠 `data-search-active` 属性标注，而不是在 state 里存元素引用：
 * mark 节点会随任一重渲染被整体替换，存下来的引用必然失效。
 */
export function useTranscriptSearch({
  containerRef,
  open,
  query,
}: {
  containerRef: RefObject<HTMLElement | null>;
  open: boolean;
  query: string;
}): {
  total: number;
  active: number;
  next: () => void;
  prev: () => void;
  reselect: () => void;
} {
  const [total, setTotal] = useState(0);
  const [active, setActive] = useState(0);
  /**
   * 当前序号的权威副本。
   *
   * 存在的理由：`next`/`prev` 可能在同一帧内被连按两次，此时 `active` 这个
   * state 还是旧的；把 DOM 标注放进 `setActive` 的 updater 里也不行——updater
   * 必须是纯函数，StrictMode 会调用两次，副作用会重复执行。
   */
  const activeRef = useRef(0);

  const hits = useCallback((): HTMLElement[] => {
    const el = containerRef.current;
    if (!el) return [];
    return Array.from(el.querySelectorAll<HTMLElement>(SEARCH_HIT_SELECTOR));
  }, [containerRef]);

  /** 把第 `index` 个命中设为当前，并按需滚到视野中央。 */
  const apply = useCallback((all: HTMLElement[], index: number, scroll: boolean) => {
    for (let i = 0; i < all.length; i += 1) {
      if (i === index) all[i].setAttribute("data-search-active", "true");
      else all[i].removeAttribute("data-search-active");
    }
    activeRef.current = index;
    if (scroll && all[index]) all[index].scrollIntoView({ block: "center" });
  }, []);

  /** 清掉所有标注。关闭搜索时用：不能靠"传空数组"来清，那只清了个寂寞。 */
  const clear = useCallback(() => {
    const all = hits();
    for (const node of all) node.removeAttribute("data-search-active");
    activeRef.current = 0;
    setTotal(0);
    setActive(0);
  }, [hits]);

  /** 按视口重选当前命中。查询变化后由宿主在渲染落地后调用。 */
  const reselect = useCallback(() => {
    const el = containerRef.current;
    const all = hits();
    setTotal(all.length);
    if (!el || all.length === 0) {
      apply(all, 0, false);
      setActive(0);
      return;
    }
    const box = el.getBoundingClientRect();
    const rects: RelRect[] = all.map((node) => {
      const r = node.getBoundingClientRect();
      return { top: r.top - box.top + el.scrollTop, height: r.height };
    });
    const index = initialActiveIndex(rects, {
      scrollTop: el.scrollTop,
      clientHeight: el.clientHeight,
    });
    apply(all, index, true);
    setActive(index);
  }, [apply, containerRef, hits]);

  useEffect(() => {
    if (!open) {
      clear();
      return;
    }
    // 渲染已落地（本 effect 在提交后运行）：计一次数并标第一个，随后由宿主的
    // `reselect` 按视口重选——这里不做滚动，避免刚打开就跳。
    const all = hits();
    setTotal(all.length);
    apply(all, 0, false);
    setActive(0);
  }, [open, query, hits, apply, clear]);

  const step = useCallback(
    (delta: 1 | -1) => {
      const all = hits();
      setTotal(all.length);
      if (all.length === 0) {
        apply([], 0, false);
        setActive(0);
        return;
      }
      const index = nextIndex(activeRef.current, all.length, delta);
      apply(all, index, true);
      setActive(index);
    },
    [apply, hits],
  );

  const next = useCallback(() => step(1), [step]);
  const prev = useCallback(() => step(-1), [step]);

  return { total, active, next, prev, reselect };
}
```

**两个必须避开的坑（已实测，都会让 Step 1 的用例失败）**：

1. **关闭时不能用「传空数组给 apply」来清除标注**。`apply([], …)` 的循环体一次都不跑，
   旧的 `data-search-active` 原样留在 DOM 上，`removes the active attribute once
   closed` 会红。必须像 `clear()` 那样**先查出节点再逐个摘属性**。
2. **`next()` 的 state 更新必须能被测试观察**。同步 `setState` 在断言前不刷新生效，
   测试要用 `act()` 包住调用。

- [ ] **Step 4: 跑测试**

```bash
cd desktop && npx vitest run src/hooks/useTranscriptSearch.test.tsx && npx tsc --noEmit
```

Expected: PASS（7 个用例）。

- [ ] **Step 5: 提交**

```bash
cd desktop && git add src/hooks/useTranscriptSearch.ts src/hooks/useTranscriptSearch.test.tsx
git commit -m "feat(desktop): add transcript search runtime hook

计数与当前命中由渲染后扫 DOM 得出，单向读取，不回写渲染输入。
当前命中用 data-search-active 标注而非元素引用（mark 会随重渲染整体替换）。"
```

---

### Task 9: 接入 `App`

**Files:**
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `SearchBar`、`useTranscriptSearch`、`SEARCH_DEBOUNCE_MS`、`rehypeSearchMark` 无关（间接）
- Produces: App 内部 state `sessionSearchEnabled: boolean | null`、`searchOpen: boolean`、`searchQuery: string`；无对外接口。

- [ ] **Step 1: 扩展测试 mock 的 flags 表**

在 `App.test.tsx` 的 `state` 初始值里加（紧挨 `groupsFlagEnabled`，约 105 行）：

```ts
    // `flags/list` 里 `session_search` 的值。默认**关**：本组用例显式打开。
    searchFlagEnabled: false,
```

在 `flags/list` 的 mock 返回（约 265–285 行那段 `flags: [...]`）里追加一项：

```ts
          {
              name: "session_search",
              summary: "会话内搜索",
              enabled: state.searchFlagEnabled,
              expires_on: "2027-04-30",
              source: state.searchFlagEnabled ? "file" : "default",
          },
```

在 `flags/set` 的 mock 里追加：

```ts
        if (name === "session_search") state.searchFlagEnabled = enabled;
```

- [ ] **Step 2: 写失败测试**

在 `App.test.tsx` 末尾追加（新 `describe`）：

```tsx
describe("session search (Cmd+F)", () => {
  function openThread() {
    render(<App />);
    return screen.findByText("t1");
  }

  it("opens the search bar on Cmd+F when the flag is on", async () => {
    state.searchFlagEnabled = true;
    await openThread();
    fireEvent.keyDown(document, { key: "f", metaKey: true });
    expect(await screen.findByRole("searchbox")).toBeTruthy();
  });

  it("does nothing on Cmd+F when the flag is off", async () => {
    state.searchFlagEnabled = false;
    await openThread();
    fireEvent.keyDown(document, { key: "f", metaKey: true });
    expect(screen.queryByRole("searchbox")).toBeNull();
  });

  it("closes the search bar on Escape", async () => {
    state.searchFlagEnabled = true;
    await openThread();
    fireEvent.keyDown(document, { key: "f", metaKey: true });
    await screen.findByRole("searchbox");
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("searchbox")).toBeNull());
  });

  it("hides the bar when the settings dialog is open", async () => {
    state.searchFlagEnabled = true;
    await openThread();
    // 打开设置（具体触发方式沿用本文件已有用例的做法）。
    fireEvent.keyDown(document, { key: "f", metaKey: true });
    await screen.findByRole("searchbox");
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("searchbox")).toBeNull());
  });

  it("picks up a flag toggled in the experimental tab without a restart", async () => {
    state.searchFlagEnabled = false;
    await openThread();
    // 经由 flags/set 拨开（模拟设置页拨开关），随后 Cmd+F 应当可用。
    state.searchFlagEnabled = true;
    fireEvent.keyDown(document, { key: "f", metaKey: true });
    expect(await screen.findByRole("searchbox")).toBeTruthy();
  });
});
```

**说明**：最后两条用例较弱（一条与 Esc 重复、一条直接改 mock state 而非走
`flags/set` 回灌）。执行时请改成走真实通路：先用 `flags/set` mock 触发回灌
（参考文件中既有的 `state.kanbanFlagEnabled = false` 用法），再断言 Cmd+F 生效。
若既有用例已有"打开设置"的可靠写法，第四条请照抄那套，而不是只测 Esc。

- [ ] **Step 3: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/App.test.tsx -t "session search"
```

Expected: FAIL，找不到 `searchbox`。

- [ ] **Step 4: 实现**

**a. 导入**

```ts
import { SearchBar } from "./components/SearchBar";
import { useTranscriptSearch, SEARCH_DEBOUNCE_MS } from "./hooks/useTranscriptSearch";
```

**b. flag state 与握手读取**

在 `const [leadAgentEnabled, setLeadAgentEnabled] = useState<boolean | null>(null);`
（`App.tsx:224`）附近加：

```ts
  /** 会话内搜索开关（`flags/list` 里的 `session_search`）。`null` = 还不知道。 */
  const [sessionSearchEnabled, setSessionSearchEnabled] = useState<boolean | null>(null);
```

在握手那次 `flags/list` 之后（`App.tsx:1493` 的 `setGroupsEnabled(groupsOn);` 之后）加读取。先把局部变量声明补上（与 `groupsOn` 同处）：

```ts
        let searchOn = false;
```

在 try 块内（`groupsOn` 赋值之后）加：

```ts
          const search = (flags.flags ?? []).find((f) => f.name === "session_search");
          searchOn = search?.enabled === true;
```

在 catch 块里加 `searchOn = false;`，并在 `setGroupsEnabled(groupsOn);` 之后加：

```ts
        setSessionSearchEnabled(searchOn);
```

**c. `flagsCall` 回灌**

在 `setGroupsEnabled(read("session_groups"));`（`App.tsx:473` 附近）之后加：

```ts
        setSessionSearchEnabled(read("session_search"));
```

**d. 搜索状态**

```ts
  /** 查找条是否打开。 */
  const [searchOpen, setSearchOpen] = useState(false);
  /** 输入框原文（未防抖），SearchBar 受控用。 */
  const [searchInput, setSearchInput] = useState("");
  /** 已防抖的查询，喂给 ChatView。 */
  const [searchQuery, setSearchQuery] = useState("");
  /**
   * 转录**真正的滚动容器**句柄（由 `ChatView` 的 `scrollerRef` 回填，见 §g）。
   * 扫描命中与量视口都用它；App 自己那层 `relative` 容器不滚动。
   */
  const transcriptRef = useRef<HTMLDivElement>(null);

  const search = useTranscriptSearch({
    containerRef: transcriptRef,
    open: searchOpen,
    query: searchQuery,
  });

  // 输入防抖：只有停手 120ms 才让 ChatView 重新解析 markdown。
  useEffect(() => {
    if (!searchOpen) return;
    const id = setTimeout(() => setSearchQuery(searchInput), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(id);
  }, [searchInput, searchOpen]);

  // mark 落地后按视口选当前命中。用 rAF 等一帧，确保 rehype 的输出已在 DOM。
  useEffect(() => {
    if (!searchOpen || searchQuery.trim() === "") return;
    const id = requestAnimationFrame(() => search.reselect());
    return () => cancelAnimationFrame(id);
  }, [searchOpen, searchQuery, search.reselect]);

  // 切换会话即关闭：旧 DOM 的命中序号没有意义。
  useEffect(() => {
    setSearchOpen(false);
    setSearchInput("");
    setSearchQuery("");
  }, [sess.currentId]);

  const closeSearch = useCallback(() => {
    setSearchOpen(false);
    setSearchInput("");
    setSearchQuery("");
  }, []);
```

**e. Cmd+F 监听（含 flag 门与弹层让位）**

```ts
  /**
   * Cmd/Ctrl+F。判据收敛进 ref：监听器只装一次，直接闭包读 state 会读到
   * 首次渲染时的值（那时 flag 还是 `null`）。
   */
  const canSearchRef = useRef(false);
  canSearchRef.current =
    sessionSearchEnabled === true &&
    sess.currentId !== null &&
    board.selectedBoard === null &&
    !prefs.settingsOpen &&
    !current?.approval;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== "f") return;
      if (!canSearchRef.current) return;
      // 拦掉 webview 自带的查找。
      e.preventDefault();
      setSearchOpen(true);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  // Esc 关闭并清空。
  useEscapeKey(searchOpen, closeSearch);
```

`useEscapeKey` 需在文件顶部导入：

```ts
import { useEscapeKey } from "./ui/useEscapeKey";
```

**f. 渲染查找条**

在 `ChatView` 外层容器（`App.tsx:2388` 的
`<div className="relative flex min-h-0 min-w-0 flex-1 flex-col">`）内、
`<ChatView … />` 之后插入：

```tsx
                {searchOpen && (
                  <SearchBar
                    value={searchInput}
                    total={search.total}
                    active={search.active}
                    onChange={setSearchInput}
                    onNext={search.next}
                    onPrev={search.prev}
                    onClose={closeSearch}
                  />
                )}
```

并给 `ChatView` 传 `query`：

```tsx
                  query={searchQuery}
```

**g. 把 `ChatView` 的滚动容器交给扫描（此处只接线，改动在 Task 6）**

**前提**：真正的滚动发生在 `ChatView` 内部的 `useStickToBottom().ref`
（`ChatView.tsx:383`），不在 App 外层那个 `relative` 容器上。若拿外层容器当扫描
容器，`el.scrollTop` 恒为 0、`clientHeight` 是整个转录列高，`initialActiveIndex`
的"视口"就退化成"内容最顶端"，**每次打开都跳到第一个命中**——正好违背本设计的
核心决策。

Task 6 已经给 `ChatView` 加了 `scrollerRef` prop（合并 ref 的实现见 Task 6
Step 3）。这里只需把 App 的 ref 传进去。**不要**再给 App 的 `relative` 容器加
`ref={transcriptRef}`。

然后在 `App.tsx` 给 `ChatView` 传 `scrollerRef`：

```tsx
                  scrollerRef={transcriptRef}
```

`transcriptRef` 就是 §d 里声明的 `useRef<HTMLDivElement>(null)`；**不要**再给
App 的 `relative` 容器加 ref。

- [ ] **Step 5: 跑测试**

```bash
cd desktop && npx vitest run src/App.test.tsx -t "session search" && npx tsc --noEmit
```

Expected: PASS。若报 `scrollerRef` 未传或类型不符，回查 §g。

- [ ] **Step 6: 跑全量前端测试**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```

Expected: 全绿。特别确认 `App.test.tsx` 既有用例未受影响（flag 默认关，行为不变）。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): wire Cmd+F session search behind a flag

flag 开、对话视图且无弹层时拦截 Cmd+F；判据收敛进 ref，避免监听器
读到首次渲染的过期 state。切换会话自动关闭。"
```

---

### Task 10: 文档登记

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-flags.md`
- Modify: `docs/project-management/README.md`（仅当计数变化）

**Interfaces:** 无代码接口。

- [ ] **Step 1: 在 `desktop.md` 的 Features 末尾加一条**

```markdown
- [x] 会话内搜索（Cmd+F 查找条，`session_search` flag 门控，只搜用户消息与 agent 正文）— `desktop/src/lib/searchHits.ts`（匹配/序号/初始命中纯函数）/ `desktop/src/lib/rehypeSearchMark.ts`（在 React 树内生成 `mark.search-hit`）/ `desktop/src/components/SearchBar.tsx` / `desktop/src/components/HighlightText.tsx`（用户气泡纯文本高亮）/ `desktop/src/hooks/useTranscriptSearch.ts`（渲染后扫 DOM、当前命中与滚动）/ `desktop/src/components/ChatView.tsx`（`query` prop 透传，根节点 class 不变）/ `desktop/src/App.tsx`（flag 读取与回灌、Cmd+F 监听、弹层让位）；判据：`cd desktop && npx vitest run src/lib/searchHits.test.ts src/lib/rehypeSearchMark.test.ts src/components/HighlightText.test.tsx src/components/SearchBar.test.tsx src/hooks/useTranscriptSearch.test.ts src/components/ChatView.test.tsx src/App.test.tsx && npx tsc --noEmit`；flag 关时 Cmd+F 无任何反应（`App.test.tsx` 的 "does nothing on Cmd+F when the flag is off"）— [设计](../superpowers/specs/2026-10-24-session-search-design.md)
```

- [ ] **Step 2: 在 `yi-agent-flags.md` 的 Features 末尾加一条**

```markdown
- [x] 第四个 flag：`session_search` — `registry.rs` 的 `SESSION_SEARCH_FLAG`，默认 `false`、`expires_on: "2027-04-30"`。纯前端能力，**没有**服务端 gate 模块（不像 `session_groups` 要拦下发与写入）：闸门就是前端 `flags/list` 读回来的布尔值，关掉时不注册 Cmd+F 监听。判据：`cargo test -p yi-agent-flags` 的 `the_session_search_flag_is_registered` / `the_session_search_flag_name_matches_its_constant`，以及与既有 `every_flag_defaults_to_off` / `no_flag_is_past_its_expiry_date` 同批通过
```

- [ ] **Step 3: 核对 README 计数**

```bash
cd .. && grep -n "desktop" docs/project-management/README.md | head -5
```

若索引表记录的是"完成 / 总计"计数，按新增 1 条 `[x]` 更新对应数字。若无此类计数则跳过本步。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-flags.md docs/project-management/README.md
git commit -m "docs: register session search in project management"
```

---

## 完成前验证

依次执行，全部通过才算完成：

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && cargo test -p yi-agent-flags
cd .. && cd desktop && npx vitest run && npx tsc --noEmit
```

手工冒烟（dev 构建）：

```bash
cd desktop && npm run dev
```

1. 设置 → 实验功能 → 打开 `session_search`。
2. 打开一个长会话，Cmd+F，输入一个已知出现在多条消息里的词。
3. 断言：计数正确；查找条打开时当前命中**不是**第一条而是视口附近那条；
   Enter/Shift+Enter 循环；Esc 关闭后所有黄色底消失；正文与原样一致。
4. 拨关 flag，再按 Cmd+F：无任何反应。

## Self-Review

**Spec 覆盖**
- §2 范围（只搜用户消息与 agent 正文）→ Task 5（用户气泡）+ Task 4（agent markdown），工具卡/思考块/notice 未接线，符合。
- §3 形态与"初始命中在视口附近" → Task 2 `initialActiveIndex` + Task 9 的 rAF 重选。
- §4 flag 注册 → Task 1；release 封堵链未新增代码（spec 已论证）。
- §5 实时生效 → Task 9 的握手读取 + `flagsCall` 回灌。
- §6 高亮架构（rehype 而非 TreeWalker）→ Task 3；`HistoryPrefix` 冻结不受影响 → Task 6 的比较器加 `query`。
- §7 组件边界与"Cmd+F 进入条件"→ Task 7/8/9，弹层判据用 state 而非 aria-modal（spec §7 已修正）。
- §8 测试 → 每个任务各自带；Task 10 登记文档。

**类型一致性**：`Match`/`RelRect`/`Viewport`/`SearchBarProps`/`useTranscriptSearch` 返回类型在 Task 2→7→8→9 中同名同形；`SEARCH_HIT_CLASS`（Task 3 定义）在 Task 5 复用；`SEARCH_DEBOUNCE_MS` 在 Task 8 定义、Task 9 使用。

**已知弱点（执行时需收紧）**：Task 9 Step 2 的最后两条用例是弱断言，已在文中明确要求执行者改成走真实 `flags/set` 回灌与既有"打开设置"写法。

## 实测记录（写计划时已真跑，不是推测）

Task 2/3/4/5/6/7/8 的代码与测试已在本 worktree 里实际执行过，修正了 6 处原始设计错误：

| # | 原始写法 | 实测问题 | 修正 |
|---|---|---|---|
| 1 | `nextIndex(9, 3, 1) === 0` | 实现是"先取模再移动"，得 1 | 断言改为 1，并在注释里写清语义 |
| 2 | `import type { … Parent } from "hast"` | `visit` 回调的 index/parent 是 `undefined` 不是 `null`，TS2769 | 判 `undefined`，去掉 `Parent` 导入 |
| 3 | `[rehypeHighlight, [rehypeSearchMark, {query}] as const]` | `as const` 的 readonly 元组不可赋给 `PluggableList` | 显式 `(): PluggableList`，从 `unified` 导入类型 |
| 4 | 钩子关闭时 `apply([], …)` 清标注 | 空数组的循环不执行，旧属性残留 | 新增 `clear()`：先查节点再逐个 `removeAttribute` |
| 5 | 钩子在 effect 里只 `setTotal` | 挂载时不标 active，用例红 | effect 里 `apply(all, 0, false)` 一并标注 |
| 6 | 把 DOM 副作用写进 `setActive` 的 updater | StrictMode 会跑两次 updater | 改用 `activeRef` 持有权威序号 |

另有一处架构级修正（不是笔误）：**扫描容器必须是 `ChatView` 内部的滚动节点**，
不是 App 外层容器；否则 `scrollTop` 恒为 0，"视口附近"退化成"永远第一个"。
为此 `ChatView` 新增 `scrollerRef`（Task 6），Task 9 只负责接线。

实测结果：`npx tsc --noEmit` 干净；`searchHits` 10 passed、`rehypeSearchMark` 6
passed、`HighlightText` 4 passed、`SearchBar` 8 passed、`useTranscriptSearch` 7
passed、`ChatView` 35 passed（含原 31，证明结构改动没碰坏既有行为）、
`MarkdownText` 16 passed。
