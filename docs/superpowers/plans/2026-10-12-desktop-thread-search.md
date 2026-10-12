# 会话搜索栏 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Mac 与手机端侧栏的「+ New thread」下方、会话列表上方加一条搜索栏，按关键词过滤会话树。

**Architecture:** 过滤逻辑抽成不碰 React/DOM 的纯函数 `filterThreadTree`（`desktop/src/lib/threadSearch.ts`），
`ThreadSidebar` 在**渲染时**调用它得到过滤后的 `groups` / `pinned`，因此不持有第二份列表副本，
服务端 `thread/listAll` 刷新后自动跟随。折叠状态全程不被修改，搜索期间仅在渲染上短路，清空即恢复。
一行手机端 CSS 把搜索框字号钉在 16px，避免 iOS 聚焦自动放大。

**Tech Stack:** React 19 + TypeScript + Tailwind CSS v4 + Vite；测试用 Vitest + @testing-library/react（jsdom）。

## Global Constraints

- 范围仅限 `desktop/`。**不改** `App.tsx` 的应用逻辑（`App.tsx` 一行不动）；唯一允许的例外是
  `App.test.tsx:458/:481` 两处裸 `input` 选择器改为 `[data-rename-input]`（见 Task 2 Step 1）。
  **不改** Rust、**不改** JSON-RPC 协议。
- 搜索栏位置固定：`ThreadSidebar.tsx` 中「+ New thread」的 `p-2` 区块**之后**、`flex-1 overflow-y-auto`
  滚动容器**之前**，两者是兄弟节点。
- 搜索栏外层**不得**加 `tabIndex`（会让既有 `div[tabindex]` 选择器选错组头）。
- 搜索栏区块内**不得**出现 `aria-haspopup`（会让既有 `button[aria-haspopup="menu"]` 选错 New thread 触发器）。
- 重命名输入框必须加标记属性 `data-rename-input=""`；既有 6 处裸 `input` 选择器改为 `[data-rename-input]`。
- 匹配规则：`query.trim()` 按 `/\s+/` 分词，**逐词 AND**，每词为 `toLowerCase()` 后的**子串**匹配。
- 匹配字段：会话标题（`title ?? "(untitled)"`）+ 所属目录的 `basename` 与完整路径。**不搜** `thread_id`。
- 目录命中 ⇒ 该目录下**全部**会话保留。
- 搜索期间**忽略**折叠状态（`collapsed` / `pinnedCollapsed` / `boardCollapsed` 三个 Set 全程不得被写）。
- 手机端 CSS 规则必须限定在 `[data-mobile="true"]` 下（桌面端逐字节不变）。
- 每个 Task 结束都要 `cd desktop && npx vitest run` 全绿才可提交。

**环境：** 本机 `npx` 不在默认 PATH，所有命令先 `export PATH="/opt/homebrew/bin:$PATH"`。
worktree 的 `desktop/node_modules` 已软链到主仓库。

---

### Task 1: 纯函数 `filterThreadTree`

**Files:**
- Create: `desktop/src/lib/threadSearch.ts`
- Test: `desktop/src/lib/threadSearch.test.ts`

**Interfaces:**
- Consumes: `WorkspaceGroup`、`ThreadSummary`（`desktop/src/lib/protocol.ts:212`、`:252`）、
  `basename`（`desktop/src/lib/workspaceGroups.ts`）。
- Produces: `ThreadSearchResult`（`{ pinned: ThreadSummary[]; groups: WorkspaceGroup[]; empty: boolean }`）
  与 `filterThreadTree(groups: WorkspaceGroup[], pinned: ThreadSummary[], query: string): ThreadSearchResult`。
  Task 2 只依赖这两个导出。

- [ ] **Step 1: 写失败的测试**

Create `desktop/src/lib/threadSearch.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { filterThreadTree } from "./threadSearch";
import type { ThreadSummary, WorkspaceGroup } from "./protocol";

function thread(id: string, title: string | null, cwd: string): ThreadSummary {
  return { thread_id: id, cwd, model: "m", created_at: 0, updated_at: 0, title };
}

const groups: WorkspaceGroup[] = [
  { workspace: "/work/projA", exists: true, threads: [thread("1", "fix login bug", "/work/projA")] },
  { workspace: "/work/projB", exists: true, threads: [thread("2", "write docs", "/work/projB")] },
];

/** 过滤后剩下的所有会话 id，按分组顺序摊平。 */
const idsOf = (r: ReturnType<typeof filterThreadTree>) =>
  r.groups.flatMap((g) => g.threads.map((t) => t.thread_id));

describe("filterThreadTree", () => {
  it("空查询原样返回，且 empty 为 false", () => {
    const r = filterThreadTree(groups, [], "");
    expect(r.groups).toBe(groups);
    expect(r.empty).toBe(false);
  });

  it("只有空白的查询同样原样返回", () => {
    const r = filterThreadTree(groups, [], "   \t ");
    expect(r.groups).toBe(groups);
    expect(r.empty).toBe(false);
  });

  it("大小写不敏感地匹配标题", () => {
    expect(idsOf(filterThreadTree(groups, [], "FIX"))).toEqual(["1"]);
  });

  it("匹配目录名（basename）时带出该目录全部会话", () => {
    const r = filterThreadTree(groups, [], "projA");
    expect(idsOf(r)).toEqual(["1"]);
  });

  it("匹配完整路径", () => {
    expect(idsOf(filterThreadTree(groups, [], "/work/projB"))).toEqual(["2"]);
  });

  it("目录命中时保留该目录下所有会话，而不只是标题也命中的", () => {
    const two: WorkspaceGroup[] = [
      {
        workspace: "/work/projA",
        exists: true,
        threads: [
          thread("1", "fix login bug", "/work/projA"),
          thread("2", "unrelated title", "/work/projA"),
        ],
      },
    ];
    expect(idsOf(filterThreadTree(two, [], "projA"))).toEqual(["1", "2"]);
  });

  it("多词 AND：两词分别命中标题与路径才保留", () => {
    expect(idsOf(filterThreadTree(groups, [], "fix projA"))).toEqual(["1"]);
    // "fix" 命中标题但 "projZ" 谁都不命中 → 全部剔除。
    expect(idsOf(filterThreadTree(groups, [], "fix projZ"))).toEqual([]);
  });

  it("title 为 null 时按 (untitled) 匹配", () => {
    const g: WorkspaceGroup[] = [
      { workspace: "/w", exists: true, threads: [thread("9", null, "/w")] },
    ];
    expect(idsOf(filterThreadTree(g, [], "untitled"))).toEqual(["9"]);
  });

  it("不搜 thread_id", () => {
    // 标题与目录都不含 "42"，只有 id 含；不得命中。
    const g: WorkspaceGroup[] = [
      { workspace: "/w", exists: true, threads: [thread("42", "hello", "/w")] },
    ];
    expect(idsOf(filterThreadTree(g, [], "42"))).toEqual([]);
  });

  it("一行不剩的分组被整组移除", () => {
    const r = filterThreadTree(groups, [], "fix");
    expect(r.groups.map((g) => g.workspace)).toEqual(["/work/projA"]);
  });

  it("无命中时 groups 为空且 empty 为 true", () => {
    const r = filterThreadTree(groups, [], "zzz-nothing");
    expect(r.groups).toEqual([]);
    expect(r.empty).toBe(true);
  });

  it("pinned 用自身 cwd 参与路径匹配", () => {
    const pin = thread("9", "pinned thing", "/work/projA");
    const r = filterThreadTree(groups, [pin], "projA");
    expect(r.pinned.map((t) => t.thread_id)).toEqual(["9"]);
  });

  it("pinned 全不命中时返回空数组，且不影响 empty 判定（分组仍命中）", () => {
    const pin = thread("9", "pinned thing", "/work/projZ");
    const r = filterThreadTree(groups, [pin], "fix");
    expect(r.pinned).toEqual([]);
    expect(r.empty).toBe(false);
  });

  it("分组与 pinned 都无命中时 empty 为 true", () => {
    const pin = thread("9", "pinned thing", "/work/projZ");
    expect(filterThreadTree(groups, [pin], "zzz").empty).toBe(true);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadSearch.test.ts`
Expected: FAIL — `Failed to resolve import "./threadSearch"`。

- [ ] **Step 3: 实现最小代码**

Create `desktop/src/lib/threadSearch.ts`:

```ts
/**
 * 侧栏会话搜索的匹配与过滤。
 *
 * 纯函数：不碰 React、不碰 RPC。侧栏每次渲染都要重算一遍，做成纯函数才既
 * 便宜又可断言。过滤**不**修改传入的 groups/pinned，逐个复制分组外壳。
 */
import type { ThreadSummary, WorkspaceGroup } from "./protocol";
import { basename } from "./workspaceGroups";

export interface ThreadSearchResult {
  pinned: ThreadSummary[];
  groups: WorkspaceGroup[];
  /** 查询非空但无任何命中；用于区分「没输入」与「搜不到」。 */
  empty: boolean;
}

/** 一条会话在界面上可被搜索到的全部文本：渲染标题 + 所属目录名与完整路径。 */
function haystack(thread: ThreadSummary, workspaceOfThread: string): string {
  // 标题渲染的就是这个回退值（ThreadSidebar 的 `t.title ?? "(untitled)"`），
  // 所以搜「untitled」能命中无标题的会话。
  return [
    thread.title ?? "(untitled)",
    basename(workspaceOfThread),
    workspaceOfThread,
  ]
    .join("\n")
    .toLowerCase();
}

/** 某目录名或完整路径是否命中全部关键词。 */
function workspaceMatches(workspace: string, tokens: string[]): boolean {
  const text = `${basename(workspace)}\n${workspace}`.toLowerCase();
  return tokens.every((t) => text.includes(t));
}

/**
 * 按关键词过滤会话树。
 *
 * `query.trim()` 为空 ⇒ 原样返回（引用相等），调用方据此完全跳过过滤。
 * 否则按空白分词、逐词 AND；目录命中 ⇒ 该目录下全部会话保留，避免搜目录名
 * 时只剩标题恰好也含该词的零星几条。
 *
 * 置顶会话不挂在任何分组对象上，故用**它自己的 `cwd`** 判定路径；分组内会话
 * 用所属分组的 `workspace`。两者在实际数据里是同一个路径，分开写只为两个
 * 数据源上都可判定、不依赖调用顺序。
 */
export function filterThreadTree(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  query: string,
): ThreadSearchResult {
  const trimmed = query.trim();
  if (trimmed === "") return { pinned, groups, empty: false };

  const tokens = trimmed.toLowerCase().split(/\s+/);

  const groups2: WorkspaceGroup[] = [];
  for (const g of groups) {
    // 分组自身命中 ⇒ 整组保留；否则逐条按标题/路径判定。
    const wholeGroup = workspaceMatches(g.workspace, tokens);
    const threads = wholeGroup
      ? g.threads
      : g.threads.filter((t) => tokens.every((tok) => haystack(t, g.workspace).includes(tok)));
    if (threads.length > 0) groups2.push({ ...g, threads });
  }

  const pinned2 = pinned.filter((t) => tokens.every((tok) => haystack(t, t.cwd).includes(tok)));

  return { pinned: pinned2, groups: groups2, empty: groups2.length === 0 && pinned2.length === 0 };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadSearch.test.ts`
Expected: PASS（14 个用例）。

- [ ] **Step 5: 提交**

```bash
export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/threadSearch.test.ts
cd .. && git add desktop/src/lib/threadSearch.ts desktop/src/lib/threadSearch.test.ts
git commit -m "feat(desktop): add the pure sidebar thread-search filter"
```

---

### Task 2: 搜索栏接入侧栏

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx:82-93`、`:791`（改选择器）、文件末尾（加用例）

**Interfaces:**
- Consumes: `filterThreadTree`、`ThreadSearchResult`（Task 1）。
- Produces: 侧栏 DOM 契约 —— 搜索框 `input[type="search"][aria-label="搜索会话"]`、
  清除按钮 `[aria-label="清除搜索"]`、无匹配提示文本 `无匹配会话`、重命名输入框标记 `[data-rename-input]`。
  Task 3 只读 `index.css`，不依赖本任务的 DOM。

- [ ] **Step 1: 改既有 6 处裸 `input` 选择器（先改测试，此时应失败）**

`desktop/src/components/ThreadSidebar.test.tsx` 的 4 处：

- `:82` → `const field = container.querySelector<HTMLInputElement>("[data-rename-input]")!;`
- `:89` → `expect(container.querySelector("[data-rename-input]")).not.toBeNull();`
- `:93` → `expect(container.querySelector("[data-rename-input]")).toBeNull();`
- `:791` → `expect(container.querySelector("[data-rename-input]")).not.toBeNull();`

`desktop/src/App.test.tsx` 的 2 处：

- `:458` → `const field = document.querySelector<HTMLInputElement>("[data-rename-input]")!;`
- `:481` → `const stillThere = document.querySelector<HTMLInputElement>("[data-rename-input]");`

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx src/App.test.tsx`
Expected: FAIL — 属性尚不存在，`querySelector` 返回 null。

- [ ] **Step 2: 给重命名输入框加标记属性**

`desktop/src/components/ThreadSidebar.tsx` 的 `<input autoFocus value={draft}` 起那段，加一行属性：

```tsx
          <input
            data-rename-input=""
            autoFocus
            value={draft}
```

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx src/App.test.tsx`
Expected: PASS（既有用例全绿，回到基线 939 个）。

- [ ] **Step 3: 写搜索栏的失败测试**

在 `desktop/src/components/ThreadSidebar.test.tsx` 末尾追加：

```tsx
describe("ThreadSidebar 搜索", () => {
  const searchBox = () => screen.getByLabelText("搜索会话") as HTMLInputElement;

  it("搜索栏在 New thread 之后、会话列表之前", () => {
    const { container } = renderSidebar();
    const newThread = newThreadTrigger(container);
    const rows = container.querySelector('[data-group-row]')!;
    // 4 = DOCUMENT_POSITION_FOLLOWING
    expect(newThread.compareDocumentPosition(searchBox()) & 4).toBe(4);
    expect(searchBox().compareDocumentPosition(rows) & 4).toBe(4);
  });

  it("输入关键词后只留命中的会话与分组", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "beta" } });
    expect(container.textContent).toContain("beta-thread");
    expect(container.textContent).not.toContain("alpha-thread");
    // alpha 所在的 /work/projA 一行不剩 → 组头一并消失。
    expect(container.textContent).not.toContain("projA");
    expect(container.textContent).toContain("projB");
  });

  it("目录名命中时带出该目录全部会话", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "projA" } });
    expect(container.textContent).toContain("alpha-thread");
    expect(container.textContent).not.toContain("beta-thread");
  });

  it("无匹配时显示「无匹配会话」", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "zzz-nothing" } });
    expect(container.textContent).toContain("无匹配会话");
  });

  it("列表本身为空且正在搜索时，显示「无匹配会话」而不是「选择一个目录开始」", () => {
    // 这条锁住空态判据里的 `!searching` 那一半：没有它，一个空列表在搜索时
    // 会同时冒出「选择一个目录开始」，与「无匹配会话」互相打脸。
    const { container } = renderSidebar({ groups: [], pinned: [] });
    expect(container.textContent).toContain("选择一个目录开始");

    fireEvent.change(searchBox(), { target: { value: "anything" } });
    expect(container.textContent).toContain("无匹配会话");
    expect(container.textContent).not.toContain("选择一个目录开始");
  });

  it("折叠的分组在搜索期间自动展开，清空后恢复折叠", () => {
    const { container } = renderSidebar();
    // 先折叠 projA（第一个 Collapse 按钮属于它）。
    fireEvent.click(container.querySelector('[aria-label="Collapse"]')!);
    expect(container.textContent).not.toContain("alpha-thread");

    fireEvent.change(searchBox(), { target: { value: "alpha" } });
    expect(container.textContent).toContain("alpha-thread");

    fireEvent.change(searchBox(), { target: { value: "" } });
    expect(container.textContent).not.toContain("alpha-thread");
  });

  it("搜索期间隐藏「看板」条目行", () => {
    const { container } = renderSidebar({ groups: boardGroups, boards: ["/proj"] });
    expect(container.querySelector('[aria-label="看板"]')).not.toBeNull();
    fireEvent.change(searchBox(), { target: { value: "proj" } });
    expect(container.querySelector('[aria-label="看板"]')).toBeNull();
  });

  it("清除按钮仅在有内容时出现，点击后恢复完整列表", () => {
    const { container } = renderSidebar();
    expect(screen.queryByLabelText("清除搜索")).toBeNull();

    fireEvent.change(searchBox(), { target: { value: "beta" } });
    expect(screen.queryByLabelText("清除搜索")).not.toBeNull();
    expect(container.textContent).not.toContain("alpha-thread");

    fireEvent.click(screen.getByLabelText("清除搜索"));
    expect(searchBox().value).toBe("");
    expect(container.textContent).toContain("alpha-thread");
    expect(screen.queryByLabelText("清除搜索")).toBeNull();
  });

  it("Escape 清空查询", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "beta" } });
    fireEvent.keyDown(searchBox(), { key: "Escape" });
    expect(searchBox().value).toBe("");
    expect(container.textContent).toContain("alpha-thread");
  });

  it("搜索期间双击会话行，[data-rename-input] 仍指向该行输入框", () => {
    const { container } = renderSidebar();
    fireEvent.change(searchBox(), { target: { value: "projA" } });
    fireEvent.doubleClick(screen.getByText("alpha-thread"));
    expect(container.querySelector("[data-rename-input]")).not.toBeNull();
    expect(container.querySelectorAll("[data-rename-input]")).toHaveLength(1);
  });
});
```

注意：`newThreadTrigger`、`boardGroups`、`renderSidebar` 都是该文件里已有的 helper，直接复用；
第一条用例里 `searchBox` 与 `rows` 的 `compareDocumentPosition` 断言用的是 DOM 顺序，不依赖像素。

- [ ] **Step 4: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL — `Unable to find a label with the text of: 搜索会话`。

- [ ] **Step 5: 实现搜索状态与过滤渲染**

`desktop/src/components/ThreadSidebar.tsx`，四处改动：

(a) 顶部 import 增加：

```tsx
import { filterThreadTree } from "../lib/threadSearch";
```

(b) 状态区（`const [editingId, setEditingId] = ...` 附近）增加：

```tsx
  // 侧栏搜索关键词。仅影响渲染，不进 ThreadStore，也不落盘。
  const [query, setQuery] = useState("");
```

(c) 在 `return (` 之前、`renderBoardThreads` 定义之后，加入过滤与搜索栏渲染块：

```tsx
  // 渲染时过滤：不持有第二份列表副本，服务端刷新后自动跟随。
  const searching = query.trim() !== "";
  const filtered = filterThreadTree(groups, pinned, query);
  // 搜索期间忽略折叠状态；三个 Set 全程不被写入，清空后原样恢复。
  const pinnedCollapsedNow = searching ? false : pinnedCollapsed;

  const clearQuery = () => setQuery("");

  const searchBar = (
    <div className="relative px-2 pb-2">
      <input
        type="search"
        aria-label="搜索会话"
        placeholder="搜索会话…"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        onKeyDown={(e) => {
          // 清空是幂等动作，不做 IME 特判：输入法组合期按 Esc 取消候选也不会损坏状态。
          if (e.key === "Escape") clearQuery();
        }}
        className="w-full rounded-md border border-line bg-surface px-2 py-1.5 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none"
      />
      {query !== "" && (
        <button
          type="button"
          aria-label="清除搜索"
          title="清除搜索"
          onClick={clearQuery}
          className="absolute top-1/2 right-3.5 -translate-y-1/2 rounded px-1 text-fg-subtle hover:text-fg"
        >
          ×
        </button>
      )}
    </div>
  );
```

(d) 渲染处接线，共 6 点：

1. New thread 的 `p-2` 区块闭合之后、`<div className="flex-1 overflow-y-auto">` 之前插入 `{searchBar}`。
2. 空态：`{groupCount(groups) === 0 && (` 改为
   `{groupCount(groups) === 0 && !searching && (`，并在其后新增：

   ```tsx
   {searching && filtered.empty && (
     <p className="px-3 py-2 text-xs text-fg-faint">无匹配会话</p>
   )}
   ```

3. Pinned 区（`pinned.length > 0 && (` 起、到该分区闭合的整块）——把这一整块里出现的 `pinned` 全部换成
   `filtered.pinned`，共 3 处：`pinned.length > 0`、`{pinnedCollapsed ? … }`、`pinned.map((t, i) => …)`。
   `pinnedCollapsedNow` 已在 (c) 里于组件级声明，此处直接引用即可（**不要**再声明一次）；
   把分区里读 `pinnedCollapsed` 的两处（caret 图标与 `aria-label`）都改用 `pinnedCollapsedNow`，
   但 `onClick={() => setPinnedCollapsed((v) => !v)}` 保持不变——搜索期间它被展开覆盖，
   Set 值不动，清空后仍是用户原先的折叠状态。
4. 分组循环：`groups.map((g) => {` → `filtered.groups.map((g) => {`。
5. 组内折叠判定：`const isCollapsed = collapsed.has(g.workspace);` →
   `const isCollapsed = searching ? false : collapsed.has(g.workspace);`。
6. 看板条目与看板小节：
   `{!isCollapsed && kanbanItemFor(g.workspace, boards) && renderBoardEntry(g.workspace)}` →
   `{!isCollapsed && !searching && kanbanItemFor(g.workspace, boards) && renderBoardEntry(g.workspace)}`；
   并把 `renderBoardThreads` 里的折叠量改为在搜索时短路：

   ```tsx
     const isBoardCollapsed = searching ? false : !boardCollapsed.has(g.workspace);
   ```

- [ ] **Step 6: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（原有 62 个 + 新增 10 个 = 72 个）。

- [ ] **Step 7: 全量回归 + 类型检查**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run && npx tsc --noEmit`
Expected: 全部通过（82 个测试文件 + 新增 1 个 = 83；用例 939 + 14（Task 1）+ 10（本任务）= 963；
Task 3 再加 1 例 → 964），tsc 无输出。

- [ ] **Step 8: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): filter the sidebar thread tree by a search box"
```

---

### Task 3: 手机端字号（iOS 聚焦不放大）

**Files:**
- Modify: `desktop/src/index.css`（追加到 `[data-mobile="true"] .app-sidebar [data-ws-header]` 规则之后，即文件里手机端那段末尾）
- Test: `desktop/src/mobileLayout.test.ts`（在 `describe("phone layout CSS")` 内追加一条）

**Interfaces:**
- Consumes: Task 2 产出的搜索框（`input[type="search"]` 位于 `.app-sidebar` 内）。
- Produces: 无后续任务依赖。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/mobileLayout.test.ts` 的 `describe("phone layout CSS")` 内追加：

```ts
  it("keeps the search box at 16px so iOS cannot auto-zoom on focus", () => {
    // WKWebView 对字号 < 16px 的输入框会在聚焦时自动放大页面，而 `lockZoom`
    // 只能禁掉用户的手动捏合缩放，挡不住这条自动放大——放大后页面不再贴合屏幕，
    // 每次滑动变成两轴平移，正是「缩放之后就需要拖拽」的同一个症状。
    const rule =
      /\[data-mobile="true"\]\s+\.app-sidebar\s+input\[type="search"\]\s*\{([^}]*)\}/.exec(
        css(),
      )?.[1] ?? "";
    expect(rule).toMatch(/font-size:\s*16px/);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/mobileLayout.test.ts`
Expected: FAIL — 正则匹配不到规则，`rule` 为空字符串。

- [ ] **Step 3: 加 CSS**

`desktop/src/index.css` 末尾（`[data-mobile="true"] .app-sidebar [data-ws-header]` 那条之后）追加：

```css
/*
 * 侧栏搜索框的字号下限：iOS 在聚焦字号 < 16px 的输入框时会自动放大页面，而
 * `useIsMobile` 的 `lockZoom` 只能禁用用户的手动捏合缩放，挡不住这条自动放大。
 * 16px 是 iOS 不触发自动放大的下限。桌面端不匹配 `[data-mobile]`，逐字节不变。
 */
[data-mobile="true"] .app-sidebar input[type="search"] {
  font-size: 16px;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/mobileLayout.test.ts`
Expected: PASS（原有 15 个 + 新增 1 个）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/index.css desktop/src/mobileLayout.test.ts
git commit -m "fix(desktop): keep the phone search box at 16px so iOS will not auto-zoom"
```

---

### Task 4: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `README.md`（模块索引表 `desktop` 那一行）

**Interfaces:**
- Consumes: Task 1–3 的实际代码位置与验证命令。
- Produces: 无后续任务依赖。

- [ ] **Step 1: 记录基线计数**

Run:

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat-desktop-thread-search
grep -c '^- \[x\]' docs/project-management/desktop.md
grep -c '^- \[\.\]' docs/project-management/desktop.md
grep -n '^| desktop |' README.md
```

基线（2026-10-12，本分支起点，实测）：`[x]` = **45**，全部条目 = **57**。
注意 `README.md` 第 265 行**已经滞后**于模块文件——它写的是 `| desktop | 42 / 54 |`，比模块文件少 3 条。
本次一并同步（见 Step 3），不要照抄 README 的旧数字。

- [ ] **Step 2: 在 `docs/project-management/desktop.md` 追加一条已完成条目**

追加到文件里既有 `[x]` 条目之后（与「手机端侧栏长按」等同级的位置），按该文件既有风格写成一行，
必须带**代码位置**与**可执行判据**：

```markdown
- [x] 侧栏会话搜索栏（Mac 与手机端同一组件，一处实现两端生效）— 侧栏「+ New thread」下方、会话列表上方的
  `input[type="search"]`（`aria-label="搜索会话"`、清除按钮 `aria-label="清除搜索"`、`Esc` 清空），按关键词过滤会话树；
  匹配规则抽成纯函数 `desktop/src/lib/threadSearch.ts:44`（`filterThreadTree`）——`trim` 后按空白**逐词 AND**、
  每词不区分大小写子串匹配「标题（`title ?? "(untitled)"`）+ 目录名 + 完整路径」，**不搜** `thread_id`；
  目录命中 ⇒ 带出该目录全部会话；`pinned` 用自身 `cwd` 参与路径匹配。渲染侧 `desktop/src/components/ThreadSidebar.tsx`
  在**渲染时**过滤（不持有第二份列表副本，服务端刷新自动跟随），空分组连组头一起消失、`Pinned` 区与「看板会话」小节
  同样过滤，搜索期间**忽略**折叠状态（`collapsed`/`pinnedCollapsed`/`boardCollapsed` 三个 Set 全程不写入，清空即恢复）、
  「看板」条目行隐藏、无匹配显示「无匹配会话」；顺带给重命名输入框加标记属性 `data-rename-input`（搜索框是文件里更早的
  `<input>`，否则 6 处既有裸 `input` 选择器会选错）。手机端聚焦不自动放大：`desktop/src/index.css` 末尾
  `[data-mobile="true"] .app-sidebar input[type="search"] { font-size: 16px }`（桌面不匹配，逐字节不变）。
  判据：`cd desktop && npx vitest run src/lib/threadSearch.test.ts src/components/ThreadSidebar.test.tsx src/mobileLayout.test.ts src/App.test.tsx && npx tsc --noEmit`
```

- [ ] **Step 3: 更新 README 索引计数**

`README.md` 第 265 行的 `| desktop | 42 / 54 | [详情](docs/project-management/desktop.md) |` 改为
`| desktop | 46 / 58 | [详情](docs/project-management/desktop.md) |`。

46/58 = 模块文件本次改完后的真实数量（起点实测 `[x]` 45 / 共 57，本任务新增 1 条 → 46/58）。
**同时修掉 README 既有 3 条滞后**：它原本写 42/54，比模块文件少 3 条。
若 Step 1 实测基线与上面不同，则以「实测基线 +1、并与模块文件完全一致」为准。

- [ ] **Step 4: 更新 desktop.md 末尾的「验证命令」计数**

`docs/project-management/desktop.md` 末尾那段 `**验证命令：**` 里列了逐文件用例数与总数（现为 930/81）。
跑一遍拿到新数字并同步：

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run --reporter=dot`

把 `（930 个前端单测 / 81 个测试文件：…）` 里的总数、文件数，以及 `ThreadSidebar.test.tsx 62`、
`mobileLayout.test.ts 15` 两个分项，改成实测值，并补上 `lib/threadSearch.test.ts 14` 一项。

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/desktop.md README.md
git commit -m "docs: record the sidebar thread search bar in the desktop module"
```

---

### Task 5: 手机端搜索栏随列表滚动（定位修订）

> 追加任务（2026-10-12 修订）：用户要求手机端搜索栏像 iOS 抽屉一样**随列表一起滑动**
> （上滑即滚出视野），**Mac 保持原样**钉在列表上方。spec 见
> `docs/superpowers/specs/2026-10-12-desktop-thread-search-design.md`「修订记录」。

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`（把 `{searchBar}` 一处渲染改成按平台两处）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`（`describe("ThreadSidebar 搜索")` 内追加 3 条）

**Interfaces:**
- Consumes: Task 2 定义的 `searchBar` 元素、`isMobile` prop（`ThreadSidebar.tsx:81`，默认 `false`）。
- Produces: 无后续任务依赖。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/components/ThreadSidebar.test.tsx` 的 `describe("ThreadSidebar 搜索")` 内
（`const searchBox = () => screen.getByLabelText("搜索会话") as HTMLInputElement;` 之后）追加：

```tsx
  it("手机端：搜索框与会话行同属滚动容器（随列表滚动）", () => {
    const { container } = renderSidebar({ isMobile: true });
    const row = container.querySelector("[data-group-row]")!;
    const scroller = row.closest(".overflow-y-auto");
    expect(scroller).not.toBeNull();
    expect(searchBox().closest(".overflow-y-auto")).toBe(scroller);
  });

  it("桌面端：搜索框在任何滚动容器之外（钉在列表上方）", () => {
    const { container } = renderSidebar();
    const row = container.querySelector("[data-group-row]")!;
    expect(row.closest(".overflow-y-auto")).not.toBeNull();
    expect(searchBox().closest(".overflow-y-auto")).toBeNull();
  });

  it("搜索命中藏在「看板会话」折叠小节里时，该小节搜索期间自动展开", () => {
    const withCard: WorkspaceGroup[] = [
      {
        workspace: "/proj",
        exists: true,
        threads: [
          { ...thread("t-card", "看板 · card-1", "/proj"), card_id: "card-1", board_project: "/proj" },
        ],
      },
    ];
    const { container } = renderSidebar({ groups: withCard, boards: ["/proj"] });
    // 默认折叠：空闲时命中行不可见。
    expect(container.textContent).not.toContain("看板 · card-1");
    fireEvent.change(searchBox(), { target: { value: "card-1" } });
    // 搜索期间小节自动展开：命中行可见。
    expect(container.textContent).toContain("看板 · card-1");
    fireEvent.change(searchBox(), { target: { value: "" } });
    // 清空后恢复折叠。
    expect(container.textContent).not.toContain("看板 · card-1");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

Expected: **前 2 条失败**（手机端搜索框此时还在滚动容器之外：`closest(".overflow-y-auto")` 为
`null`，不等于会话行的容器），**第 3 条通过**。

第 3 条**预期立即通过**——这不是缺陷，它是**锁定性测试**：现有代码 `searching ? false : !boardCollapsed.has(...)`
（`ThreadSidebar.tsx:584`）在搜索期间已经把该小节当作展开。已用探针实测确认：空闲 `false` →
搜索 `true` → 清空 `false`。**若第 3 条反而失败**，说明现有折叠逻辑与我的判断不符，
**停下来查清原因再继续**，不要改测试去迁就代码。

- [ ] **Step 3: 实现（按平台两处渲染）**

在 `desktop/src/components/ThreadSidebar.tsx` 中，把：

```tsx
      {searchBar}
      <div className="flex-1 overflow-y-auto">
```

改为：

```tsx
      {!isMobile && searchBar}
      <div className="flex-1 overflow-y-auto">
        {isMobile && searchBar}
```

要点：
- 桌面（`isMobile=false`）：第一行求值为 `searchBar`，渲染位置与 className 与改动前**逐字节一致**；
  容器内那行求值为 `false`，不产生任何节点。
- 手机（`isMobile=true`）：容器外那行为 `false`，搜索框成为滚动容器的**首个子元素**，随列表滚动。
- 搜索框区块的 className（`relative px-2 pb-2`）不需要改：作为容器首个子元素，上下内边距自然生效，
  下方紧接列表内容。
- 手机端 16px 字号规则是**后代选择器** `[data-mobile="true"] .app-sidebar input[type="search"]`，
  移入容器后依然命中，**CSS 无需改动**。

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

Expected: PASS，`74 + 3 = 77` 个用例。

- [ ] **Step 5: 全量回归 + 类型检查**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run && npx tsc --noEmit`

Expected: 全绿，`tsc` 无输出。**特别确认既有 `搜索栏在 New thread 之后、会话列表之前` 用例仍通过**
——其"之后/之前"用的是 `compareDocumentPosition` 文档顺序，手机端把搜索框移入容器后，
它仍排在第一个会话行之前，故该断言两端都成立。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): let the phone search bar scroll with the thread list"
```

---

## 验收（全部任务完成后跑一次）

```bash
export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run && npx tsc --noEmit
```

对着 spec §6 的 8 条验收标准逐条核对；第 1、7 条（两端都有搜索栏、手机端聚焦不放大）与
**第 2 条（手机端搜索框随列表滚动、Mac 固定）**需要人工在 Mac 应用与手机端各点一遍确认。

> 注：原 Task 4（同步 `docs/project-management/desktop.md` 与 `README.md` 索引）**尚未执行**
> （`README.md:265` 仍为 `| desktop | 42 / 54 |`，模块文件里也还没有搜索栏条目）。
> 若之后执行，请把手机端「随列表滚动、Mac 固定」这一行为一并写进该条目。
