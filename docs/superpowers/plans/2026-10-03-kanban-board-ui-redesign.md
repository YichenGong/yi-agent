# 看板四列主视图 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把桌面端看板从"叠在对话上方的单列表格"改成覆盖主区域的四列看板（queued / doing / need decision / done），并把断掉的数据链（卡片标题、分支、完成时间）接上。

**Architecture:** 纯桌面端。三层：①纯函数层 `superpowersKanbanState.ts`（`normalizeCard` 归一化原始卡 + 派生标题）与新文件 `superpowersKanbanBoard.ts`（状态→列映射 + 列内排序 + done 折叠）；②展示层新组件 `SuperpowersKanbanBoard`（四列容器）/ `SuperpowersKanbanColumn` / `SuperpowersKanbanCard`，无状态、只回调；③`App.tsx` 改主区域为"会话 / 看板"二选一的覆盖渲染，并持有 done 展开状态。插件与宿主协议零改动。

**Tech Stack:** React 19 + TypeScript + Tailwind（桌面端既有 token）；测试 vitest + jsdom + @testing-library/react。

**Spec:** `docs/superpowers/specs/2026-10-03-kanban-board-ui-redesign-design.md`

## Global Constraints

- 只改 `desktop/`。**不动** `plugins/superpowers-kanban/`、不动宿主 RPC、不动状态机。
- 状态 → 列映射（spec §3，逐字）：
  - queued ← `queued`、`launching`、`paused`
  - doing ← `running`、`merging`
  - need decision ← `needs_you`、`awaiting_merge`
  - done ← `done`、`failed`、`cancelled`
  - **未知状态 → doing**（绝不入 done）
- 卡片**只读**：允许的交互只有"打开关联会话"与"done 列展开/收起"。不新增任何写操作。
- 点卡片会话链接 = 切到该会话、离开看板（不做多层导航）。
- done 列默认显示最近 **5** 张，可展开；展开状态**不持久化**。
- 覆盖范围：看板盖住 ChatView + MessageInput，保留 TitleBar 与 StatusBar。
- `BoardCard` 接口从 `components/SuperpowersKanbanView.tsx` **迁移到** `lib/superpowersKanbanState.ts`（消除 lib→components 的反向依赖），并从 View 再导出以保持既有 import 路径可用。
- 所有新增的 `BoardCard` 字段**可选**，缺失不报错（旧卡兼容）。
- 命令均在 `desktop/` 下执行：`npx tsc --noEmit`、`npx vitest run`（单文件 `npx vitest run src/...`）。
- 只显式 `git add <path>`。

---

## File Structure

- `desktop/src/lib/superpowersKanbanState.ts`（改）— 定义 `BoardCard`（迁移 + 扩字段）；新增 `normalizeCard`；`parseBoard` 改为复用。责任：插件原始 JSON → 渲染用卡片。
- `desktop/src/lib/superpowersKanbanBoard.ts`（新）— `boardColumns` / `collapseDone` / `DONE_COLLAPSED_LIMIT` / `BoardColumns`。责任：分列与排序（纯逻辑）。
- `desktop/src/components/SuperpowersKanbanCard.tsx`（新）— 单卡展示。
- `desktop/src/components/SuperpowersKanbanColumn.tsx`（新）— 单列（列头 + 卡片 + 空占位 + done 折叠按钮）。
- `desktop/src/components/SuperpowersKanbanBoard.tsx`（新）— 四列网格 + 容器。
- `desktop/src/components/SuperpowersKanbanView.tsx`（重写）— 顶层视图：四种提示态 + 委托 `SuperpowersKanbanBoard`；保留 `BoardCard` re-export。
- `desktop/src/App.tsx`（改）— 主区域二选一覆盖；`doneExpanded` 状态；把 `board.list` 的 `cards` 经 `normalizeCard` 归一化再渲染。
- 测试：`desktop/src/lib/superpowersKanbanState.test.ts`（增）、`desktop/src/lib/superpowersKanbanBoard.test.ts`（新）、`desktop/src/components/SuperpowersKanbanView.test.tsx`（改）、`desktop/src/components/SuperpowersKanbanBoard.test.tsx`（新）。

---

### Task 1: 卡片归一化与标题派生（`superpowersKanbanState.ts`）

**Files:**
- Modify: `desktop/src/lib/superpowersKanbanState.ts`
- Test: `desktop/src/lib/superpowersKanbanState.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export interface BoardCard {
    id: string;
    state: string;            // 小写
    progress: string | null;  // 本轮恒 null
    detail: string;           // workdir → 合并卡 "source → base" → spec_path → plan_path → ""
    threadId: string | null;
    // 新增（全部可选）：
    title?: string;           // spec 文件名去扩展名；缺 spec_path 时回退 id
    specPath?: string;
    planPath?: string;
    workdir?: string;
    kind?: string;            // 缺省 "implementation"
    source?: string;
    base?: string;
    terminalAt?: string;      // 终态时刻（ISO 串）；插件提供才有
    enqueuedAt?: string;      // 入队时刻（ISO 串）；插件提供才有
    order?: number;
  }
  export function normalizeCard(raw: unknown): BoardCard | null;
  export function parseBoard(json: string): BoardCard[];  // 签名不变
  export function deriveTitle(specPath: string, id: string): string;
  ```
- `detail` 回退顺序**新增** `spec_path` 一环（旧实现只有 workdir / 合并 refs / plan_path）：因为新卡片 spec 路径更贴近"这是什么"，且计划可能缺。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/lib/superpowersKanbanState.test.ts` 末尾追加：

```ts
describe("normalizeCard", () => {
  it("derives a title from the spec file name", () => {
    const card = normalizeCard({
      id: "2026-10-03-board-smoke-spec-2026-10-03-board-smoke-plan",
      state: "Awaiting_Merge",
      spec_path: "/p/docs/superpowers/smoke/2026-10-03-board-smoke.spec.md",
      plan_path: "/p/docs/superpowers/smoke/2026-10-03-board-smoke.plan.md",
      thread_id: "thread-1",
      kind: "implementation",
      order: 2,
    });
    expect(card).not.toBeNull();
    expect(card!.state).toBe("awaiting_merge");
    expect(card!.title).toBe("2026-10-03-board-smoke");
    expect(card!.specPath).toBe("/p/docs/superpowers/smoke/2026-10-03-board-smoke.spec.md");
    expect(card!.threadId).toBe("thread-1");
    expect(card!.order).toBe(2);
  });

  it("falls back to the id when there is no spec path", () => {
    const card = normalizeCard({ id: "bare-id", state: "queued" });
    expect(card!.title).toBe("bare-id");
    expect(card!.detail).toBe("");
  });

  it("prefers spec_path over plan_path for detail", () => {
    const card = normalizeCard({
      id: "c", state: "queued", spec_path: "c.spec.md", plan_path: "c.plan.md",
    });
    expect(card!.detail).toBe("c.spec.md");
  });

  it("keeps the merge-card detail fallback (source → base)", () => {
    const card = normalizeCard({
      id: "m", state: "merging", kind: "merge", source: "kanban/a", base: "main",
    });
    expect(card!.detail).toBe("kanban/a → main");
  });

  it("carries terminal_at and enqueued_at when the plugin sends them", () => {
    const card = normalizeCard({
      id: "c", state: "done", enqueued_at: "2026-10-01T00:00:00+08:00",
      terminal_at: "2026-10-03T00:00:00+08:00",
    });
    expect(card!.enqueuedAt).toBe("2026-10-01T00:00:00+08:00");
    expect(card!.terminalAt).toBe("2026-10-03T00:00:00+08:00");
  });

  it("returns null only when id or state is missing", () => {
    expect(normalizeCard({ state: "queued" })).toBeNull();
    expect(normalizeCard({ id: "c" })).toBeNull();
    expect(normalizeCard(null)).toBeNull();
    expect(normalizeCard("nope")).toBeNull();
  });
});
```

同时把文件顶部 import 改为同时引入新函数：

```ts
import { boardJsonPath, normalizeCard, parseBoard } from "./superpowersKanbanState";
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts`
Expected: FAIL — `normalizeCard is not a function` / TS 报 `normalizeCard` 未导出。

- [ ] **Step 3: 实现**

把 `desktop/src/lib/superpowersKanbanState.ts` 整体替换为：

```ts
/**
 * 插件 `list` 原始卡（`board.json` 里的形状）→ 看板渲染用的 `BoardCard`。
 *
 * `BoardCard` 定义在这里而不是视图组件里：这是纯数据形状，`lib` 不该反过来
 * 依赖 `components`。视图为兼容旧 import 会 re-export 它。
 */

export interface BoardCard {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
  threadId: string | null;
  /** 标题：spec 文件名去扩展名；无 spec_path 时回退 id。 */
  title?: string;
  specPath?: string;
  planPath?: string;
  workdir?: string;
  kind?: string;
  /** 合并卡源分支。 */
  source?: string;
  /** 合并卡目标分支。 */
  base?: string;
  /** 进入终态的时刻（ISO 串）。插件未提供则不设。 */
  terminalAt?: string;
  /** 入队时刻（ISO 串）。插件未提供则不设。 */
  enqueuedAt?: string;
  /** 队列顺序（越小越前）。插件未提供则不设。 */
  order?: number;
}

export function boardJsonPath(stateDir: string): string {
  return `${stateDir}/board.json`;
}

/** spec 文件名去扩展名；空路径回退 id。`…/2026-10-03-foo.spec.md` → `2026-10-03-foo`。 */
export function deriveTitle(specPath: string, id: string): string {
  if (specPath === "") return id;
  const base = specPath.split("/").pop() ?? specPath;
  return base.replace(/\.(md|markdown)$/i, "").replace(/\.(spec|plan)$/i, "");
}

function optionalString(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

/**
 * 归一化一张原始卡。只有缺 `id` 或缺 `state` 时返回 `null`（沿用旧的丢弃规则）；
 * 其余字段缺失都降级，不丢卡——合并卡没有 plan_path 正是这种情况。
 */
export function normalizeCard(raw: unknown): BoardCard | null {
  if (typeof raw !== "object" || raw === null) return null;
  const record = raw as Record<string, unknown>;
  const id = typeof record.id === "string" ? record.id : "";
  const state = typeof record.state === "string" ? record.state : "";
  if (id === "" || state === "") return null;

  const planPath = typeof record.plan_path === "string" ? record.plan_path : "";
  const specPath = typeof record.spec_path === "string" ? record.spec_path : "";
  const workdir = typeof record.workdir === "string" ? record.workdir : "";
  const kind = typeof record.kind === "string" ? record.kind : "implementation";
  const source = typeof record.source === "string" ? record.source : "";
  const base = typeof record.base === "string" ? record.base : "";
  const order = typeof record.order === "number" ? record.order : undefined;
  const threadId =
    typeof record.thread_id === "string" && record.thread_id !== ""
      ? record.thread_id
      : null;

  const detail =
    workdir !== ""
      ? workdir
      : kind === "merge"
        ? `${source} → ${base}`
        : specPath !== ""
          ? specPath
          : planPath;

  return {
    id,
    state: state.toLowerCase(),
    progress: null,
    detail,
    threadId,
    title: deriveTitle(specPath, id),
    specPath: specPath !== "" ? specPath : undefined,
    planPath: planPath !== "" ? planPath : undefined,
    workdir: workdir !== "" ? workdir : undefined,
    kind,
    source: source !== "" ? source : undefined,
    base: base !== "" ? base : undefined,
    terminalAt: optionalString(record.terminal_at),
    enqueuedAt: optionalString(record.enqueued_at),
    order,
  };
}

/**
 * `board.json` 文本 → 卡片数组。复用 `normalizeCard` 保证"原始卡 → 卡片"
 * 只有一处规则；缺 `order` 的卡保持文件序。
 */
export function parseBoard(json: string): BoardCard[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json);
  } catch {
    return [];
  }
  if (typeof parsed !== "object" || parsed === null) return [];
  const cards = (parsed as { cards?: unknown }).cards;
  if (!Array.isArray(cards)) return [];

  const mapped: Array<{ card: BoardCard; order: number }> = [];
  for (const raw of cards) {
    const card = normalizeCard(raw);
    if (card === null) continue;
    mapped.push({ card, order: card.order ?? 0 });
  }
  return mapped.sort((left, right) => left.order - right.order).map((entry) => entry.card);
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanState.test.ts`
Expected: PASS（含既有用例——注意既有 `falls back to the plan path when workdir is missing` 现在走 spec_path 分支：该用例如今没有 `spec_path`，仍落到 plan_path，故不变）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/superpowersKanbanState.ts desktop/src/lib/superpowersKanbanState.test.ts
git commit -m "feat(desktop): normalize board cards and derive their title"
```

---

### Task 2: 列映射与排序（`superpowersKanbanBoard.ts`）

**Files:**
- Create: `desktop/src/lib/superpowersKanbanBoard.ts`
- Test: `desktop/src/lib/superpowersKanbanBoard.test.ts`

**Interfaces:**
- Consumes: Task 1 的 `BoardCard`。
- Produces:
  ```ts
  export const DONE_COLLAPSED_LIMIT = 5;
  export type BoardColumnKey = "queued" | "doing" | "needDecision" | "done";
  export interface BoardColumns {
    queued: BoardCard[];
    doing: BoardCard[];
    needDecision: BoardCard[];
    done: BoardCard[];
  }
  export function columnForState(state: string): BoardColumnKey;
  export function boardColumns(cards: BoardCard[]): BoardColumns;
  export function collapseDone(done: BoardCard[], expanded: boolean): BoardCard[];
  ```

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/lib/superpowersKanbanBoard.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import type { BoardCard } from "./superpowersKanbanState";
import {
  DONE_COLLAPSED_LIMIT,
  boardColumns,
  collapseDone,
  columnForState,
} from "./superpowersKanbanBoard";

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return { id, state, progress: null, detail: "", threadId: null, ...extra };
}

describe("columnForState", () => {
  it("maps every known state to its column", () => {
    expect(columnForState("queued")).toBe("queued");
    expect(columnForState("launching")).toBe("queued");
    expect(columnForState("paused")).toBe("queued");
    expect(columnForState("running")).toBe("doing");
    expect(columnForState("merging")).toBe("doing");
    expect(columnForState("needs_you")).toBe("needDecision");
    expect(columnForState("awaiting_merge")).toBe("needDecision");
    expect(columnForState("done")).toBe("done");
    expect(columnForState("failed")).toBe("done");
    expect(columnForState("cancelled")).toBe("done");
  });

  it("sends an unknown state to doing, never to done", () => {
    expect(columnForState("brand_new_state")).toBe("doing");
    expect(columnForState("")).toBe("doing");
  });
});

describe("boardColumns", () => {
  it("splits cards into the four columns", () => {
    const cols = boardColumns([
      card("q", "queued"),
      card("r", "running"),
      card("n", "needs_you"),
      card("d", "done"),
    ]);
    expect(cols.queued.map((c) => c.id)).toEqual(["q"]);
    expect(cols.doing.map((c) => c.id)).toEqual(["r"]);
    expect(cols.needDecision.map((c) => c.id)).toEqual(["n"]);
    expect(cols.done.map((c) => c.id)).toEqual(["d"]);
  });

  it("orders queued by FIFO order ascending", () => {
    const cols = boardColumns([
      card("b", "queued", { order: 5, enqueuedAt: "2026-10-01T00:00:00+08:00" }),
      card("a", "queued", { order: 1, enqueuedAt: "2026-10-02T00:00:00+08:00" }),
    ]);
    expect(cols.queued.map((c) => c.id)).toEqual(["a", "b"]);
  });

  it("orders doing by enqueued_at ascending", () => {
    const cols = boardColumns([
      card("late", "running", { enqueuedAt: "2026-10-02T00:00:00+08:00" }),
      card("early", "running", { enqueuedAt: "2026-10-01T00:00:00+08:00" }),
    ]);
    expect(cols.doing.map((c) => c.id)).toEqual(["early", "late"]);
  });

  it("pins needs_you above awaiting_merge in need decision", () => {
    const cols = boardColumns([
      card("merge", "awaiting_merge", { enqueuedAt: "2026-10-01T00:00:00+08:00" }),
      card("you", "needs_you", { enqueuedAt: "2026-10-05T00:00:00+08:00" }),
    ]);
    expect(cols.needDecision.map((c) => c.id)).toEqual(["you", "merge"]);
  });

  it("orders done by terminal_at descending, falling back to enqueued_at", () => {
    const cols = boardColumns([
      card("old", "done", { terminalAt: "2026-10-01T00:00:00+08:00" }),
      card("new", "done", { terminalAt: "2026-10-03T00:00:00+08:00" }),
      card("no-terminal", "failed", { enqueuedAt: "2026-10-02T00:00:00+08:00" }),
    ]);
    expect(cols.done.map((c) => c.id)).toEqual(["new", "no-terminal", "old"]);
  });

  it("keeps input order when timestamps are absent", () => {
    const cols = boardColumns([card("first", "running"), card("second", "running")]);
    expect(cols.doing.map((c) => c.id)).toEqual(["first", "second"]);
  });
});

describe("collapseDone", () => {
  const many = Array.from({ length: 8 }, (_, i) => card(`c${i}`, "done"));

  it("shows only the first N when collapsed", () => {
    expect(collapseDone(many, false)).toHaveLength(DONE_COLLAPSED_LIMIT);
    expect(collapseDone(many, false).map((c) => c.id)).toEqual([
      "c0", "c1", "c2", "c3", "c4",
    ]);
  });

  it("shows all when expanded", () => {
    expect(collapseDone(many, true)).toHaveLength(8);
  });

  it("shows all when there are fewer than the limit", () => {
    expect(collapseDone(many.slice(0, 2), false)).toHaveLength(2);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanBoard.test.ts`
Expected: FAIL — 模块不存在（`Failed to resolve import "./superpowersKanbanBoard"`）。

- [ ] **Step 3: 实现**

创建 `desktop/src/lib/superpowersKanbanBoard.ts`：

```ts
/**
 * 状态 → 四列的映射与列内排序。
 *
 * 纯函数、不碰 React：这些规则会被轮询反复重算（每次都换新对象），做成纯函数
 * 才既便宜又可断言。未知状态一律进 `doing`——把它放进 `done` 会让人误以为
 * 完成了而漏掉，放进 `doing` 最坏也只是"看着还在跑"。
 */

import type { BoardCard } from "./superpowersKanbanState";

export const DONE_COLLAPSED_LIMIT = 5;

export type BoardColumnKey = "queued" | "doing" | "needDecision" | "done";

export interface BoardColumns {
  queued: BoardCard[];
  doing: BoardCard[];
  needDecision: BoardCard[];
  done: BoardCard[];
}

export function columnForState(state: string): BoardColumnKey {
  switch (state) {
    case "queued":
    case "launching":
    case "paused":
      return "queued";
    case "needs_you":
    case "awaiting_merge":
      return "needDecision";
    case "done":
    case "failed":
    case "cancelled":
      return "done";
    case "running":
    case "merging":
      return "doing";
    default:
      // 未知（未来新增 / 拼错）：保守放进 doing，绝不放进 done。
      return "doing";
  }
}

/** 时刻串缺失时排到末尾；字符串 ISO 可直接按字典序比较（同带时区偏移）。 */
function ascending(left: string | undefined, right: string | undefined): number {
  if (left === undefined) return right === undefined ? 0 : 1;
  if (right === undefined) return -1;
  return left < right ? -1 : left > right ? 1 : 0;
}

export function boardColumns(cards: BoardCard[]): BoardColumns {
  const columns: BoardColumns = { queued: [], doing: [], needDecision: [], done: [] };
  for (const card of cards) columns[columnForState(card.state)].push(card);

  columns.queued.sort((l, r) => (l.order ?? 0) - (r.order ?? 0));

  columns.doing.sort((l, r) => ascending(l.enqueuedAt, r.enqueuedAt));

  // needs_you（要人回话）比 awaiting_merge（要人确认合并）更急，置顶。
  columns.needDecision.sort((l, r) => {
    const rank = (card: BoardCard) => (card.state === "needs_you" ? 0 : 1);
    return rank(l) - rank(r) || ascending(l.enqueuedAt, r.enqueuedAt);
  });

  // 完成时刻倒序：terminalAt 优先，回退 enqueuedAt（取负值实现降序）。
  columns.done.sort((l, r) =>
    ascending(r.terminalAt ?? r.enqueuedAt, l.terminalAt ?? l.enqueuedAt),
  );

  return columns;
}

export function collapseDone(done: BoardCard[], expanded: boolean): BoardCard[] {
  return expanded ? done : done.slice(0, DONE_COLLAPSED_LIMIT);
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/superpowersKanbanBoard.test.ts`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/superpowersKanbanBoard.ts desktop/src/lib/superpowersKanbanBoard.test.ts
git commit -m "feat(desktop): map board states to four columns with per-column ordering"
```

---

### Task 3: 卡片与列展示组件

**Files:**
- Create: `desktop/src/components/SuperpowersKanbanCard.tsx`
- Create: `desktop/src/components/SuperpowersKanbanColumn.tsx`
- Create: `desktop/src/components/SuperpowersKanbanBoard.tsx`
- Test: `desktop/src/components/SuperpowersKanbanBoard.test.tsx`

**Interfaces:**
- Consumes: Task 1 `BoardCard`；Task 2 `boardColumns` / `collapseDone` / `DONE_COLLAPSED_LIMIT` / `BoardColumnKey`。
- Produces:
  ```ts
  // SuperpowersKanbanCard.tsx
  export function SuperpowersKanbanCard(props: {
    card: BoardCard;
    onOpenThread?: (threadId: string) => void;
  }): JSX.Element;

  // SuperpowersKanbanBoard.tsx
  export function SuperpowersKanbanBoard(props: {
    cards: BoardCard[];
    expandedDone?: boolean;          // 默认 false
    onToggleDone?: () => void;
    onOpenThread?: (threadId: string) => void;
  }): JSX.Element;
  ```

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/SuperpowersKanbanBoard.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, expect, it, afterEach, vi } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanBoard } from "./SuperpowersKanbanBoard";

afterEach(() => cleanup());

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return { id, state, progress: null, detail: "", threadId: null, title: id, ...extra };
}

describe("SuperpowersKanbanBoard", () => {
  it("renders the four column headers with counts", () => {
    render(
      <SuperpowersKanbanBoard
        cards={[card("q", "queued"), card("r", "running"), card("n", "needs_you"), card("d", "done")]}
      />,
    );
    expect(screen.getByText(/queued/i)).toBeTruthy();
    expect(screen.getByText(/doing/i)).toBeTruthy();
    expect(screen.getByText(/need decision/i)).toBeTruthy();
    expect(screen.getByText(/done/i)).toBeTruthy();
  });

  it("shows a card's title and marks need-decision cards", () => {
    render(
      <SuperpowersKanbanBoard cards={[card("n1", "needs_you", { title: "fix the thing" })]} />,
    );
    expect(screen.getByText("fix the thing")).toBeTruthy();
    // 该列有强调标记（aria-label 稳定可断言）。
    expect(screen.getByLabelText(/需你处理/)).toBeTruthy();
  });

  it("collapses done to five cards and expands on toggle", () => {
    // 不传 expandedDone（默认 false = 折叠）；完成时刻倒序后，最新的是 d6。
    const done = Array.from({ length: 7 }, (_, i) =>
      card(`d${i}`, "done", { title: `done-${i}`, terminalAt: `2026-10-0${i + 1}T00:00:00+08:00` }),
    );
    const { rerender } = render(
      <SuperpowersKanbanBoard cards={done} onToggleDone={() => {}} />,
    );
    // 折叠只显示最近 5 张：最近的是 d6..d2，最旧的两张 d0/d1 不显示。
    expect(screen.queryByText("done-0")).toBeNull();
    expect(screen.getByText("done-6")).toBeTruthy();
    rerender(<SuperpowersKanbanBoard cards={done} expandedDone onToggleDone={() => {}} />);
    expect(screen.getByText("done-0")).toBeTruthy();
  });

  it("reports the done toggle", () => {
    const onToggleDone = vi.fn();
    // 需要超过 5 张 done 卡，展开开关才出现（不足 5 张无需折叠）。
    const done = Array.from({ length: 6 }, (_, i) => card(`d${i}`, "done", { title: `d${i}` }));
    render(<SuperpowersKanbanBoard cards={done} onToggleDone={onToggleDone} />);
    fireEvent.click(screen.getByRole("button", { name: /展开|收起/ }));
    expect(onToggleDone).toHaveBeenCalled();
  });

  it("opens a card's linked thread and renders none without one", () => {
    const onOpenThread = vi.fn();
    const { rerender } = render(
      <SuperpowersKanbanBoard
        cards={[card("c", "running", { threadId: "thread-1" })]}
        onOpenThread={onOpenThread}
      />,
    );
    fireEvent.click(screen.getByText("thread-1"));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");

    rerender(<SuperpowersKanbanBoard cards={[card("c", "running")]} onOpenThread={onOpenThread} />);
    expect(screen.queryByText("thread-1")).toBeNull();
  });

  it("keeps all four columns visible when empty", () => {
    render(<SuperpowersKanbanBoard cards={[]} />);
    expect(screen.getByText(/queued/i)).toBeTruthy();
    expect(screen.getByText(/done/i)).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanBoard.test.tsx`
Expected: FAIL — 组件不存在。

- [ ] **Step 3: 实现**

创建 `desktop/src/components/SuperpowersKanbanCard.tsx`：

```tsx
import type { BoardCard } from "../lib/superpowersKanbanState";

/** id 中间省略：头尾各留一段，够认出是哪张卡。 */
function middleEllipsis(text: string, head = 14, tail = 10): string {
  if (text.length <= head + tail + 1) return text;
  return `${text.slice(0, head)}…${text.slice(-tail)}`;
}

export function SuperpowersKanbanCard({
  card,
  onOpenThread,
}: {
  card: BoardCard;
  onOpenThread?: (threadId: string) => void;
}) {
  const needsYou = card.state === "needs_you";
  return (
    <li className="rounded border border-line bg-panel p-2 text-sm">
      <div className="flex items-center gap-2">
        <span className="truncate font-medium text-fg" title={card.title ?? card.id}>
          {card.title ?? card.id}
        </span>
        {needsYou && (
          <span aria-label="需你处理" className="shrink-0 text-red-400">
            !
          </span>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs text-fg-subtle">
        <span className="font-mono" title={card.id}>
          {middleEllipsis(card.id)}
        </span>
        <span className="text-fg-muted">{card.state}</span>
      </div>
      {card.threadId ? (
        <button
          type="button"
          onClick={() => onOpenThread?.(card.threadId as string)}
          className="mt-1 cursor-pointer font-mono text-xs text-fg-muted hover:text-fg hover:underline"
        >
          {card.threadId}
        </button>
      ) : null}
    </li>
  );
}
```

创建 `desktop/src/components/SuperpowersKanbanColumn.tsx`：

```tsx
import type { BoardCard } from "../lib/superpowersKanbanState";
import type { BoardColumnKey } from "../lib/superpowersKanbanBoard";
import { SuperpowersKanbanCard } from "./SuperpowersKanbanCard";

const COLUMN_LABEL: Record<BoardColumnKey, string> = {
  queued: "QUEUED",
  doing: "DOING",
  needDecision: "NEED DECISION",
  done: "DONE",
};

export function SuperpowersKanbanColumn({
  columnKey,
  cards,
  totalCount,
  onOpenThread,
  doneToggle,
}: {
  columnKey: BoardColumnKey;
  /** 本列要画出的卡（done 列的折叠已在调用方完成）。 */
  cards: BoardCard[];
  /** 该列卡片总数；done 列计数含被折叠掉的卡。 */
  totalCount: number;
  onOpenThread?: (threadId: string) => void;
  /** 只有 done 列会传：右侧的「展开全部 / 收起」开关。 */
  doneToggle?: { expanded: boolean; onToggle: () => void; hasMore: boolean };
}) {
  const needsDecision = columnKey === "needDecision";
  return (
    <section
      aria-label={`看板列 ${COLUMN_LABEL[columnKey]}`}
      className={`flex min-h-0 flex-1 flex-col rounded border ${
        needsDecision ? "border-red-500/40" : "border-line"
      } bg-surface`}
    >
      <header className="flex items-center justify-between px-3 py-2">
        <span
          className={`text-xs font-semibold tracking-wide ${
            needsDecision ? "text-red-400" : "text-fg-muted"
          }`}
        >
          {COLUMN_LABEL[columnKey]} · {totalCount}
        </span>
        {doneToggle && doneToggle.hasMore && (
          <button
            type="button"
            className="cursor-pointer text-xs text-fg-subtle hover:text-fg"
            onClick={doneToggle.onToggle}
          >
            {doneToggle.expanded ? "收起" : "展开全部"}
          </button>
        )}
      </header>
      <ul className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto px-2 pb-2">
        {cards.length === 0 ? (
          <li className="px-1 py-2 text-xs text-fg-subtle">空</li>
        ) : (
          cards.map((card) => (
            <SuperpowersKanbanCard key={card.id} card={card} onOpenThread={onOpenThread} />
          ))
        )}
      </ul>
    </section>
  );
}
```

> 注意：`totalCount` 是必填 prop，但上面 JSX 里写成属性名 `totalCount`——实现时把
> `SuperpowersKanbanBoard` 传 `totalCount` 即可（见下）。

创建 `desktop/src/components/SuperpowersKanbanBoard.tsx`：

```tsx
import type { BoardCard } from "../lib/superpowersKanbanState";
import {
  DONE_COLLAPSED_LIMIT,
  boardColumns,
  collapseDone,
  type BoardColumnKey,
} from "../lib/superpowersKanbanBoard";
import { SuperpowersKanbanColumn } from "./SuperpowersKanbanColumn";

export function SuperpowersKanbanBoard({
  cards,
  expandedDone = false,
  onToggleDone,
  onOpenThread,
}: {
  cards: BoardCard[];
  expandedDone?: boolean;
  onToggleDone?: () => void;
  onOpenThread?: (threadId: string) => void;
}) {
  const columns = boardColumns(cards);
  const visibleDone = collapseDone(columns.done, expandedDone);
  const order: BoardColumnKey[] = ["queued", "doing", "needDecision", "done"];

  return (
    <div className="flex min-h-0 flex-1 gap-3 overflow-x-auto p-3">
      {order.map((key) => {
        const all = columns[key];
        const shown = key === "done" ? visibleDone : all;
        return (
          <SuperpowersKanbanColumn
            key={key}
            columnKey={key}
            cards={shown}
            totalCount={all.length}
            onOpenThread={onOpenThread}
            doneToggle={
              key === "done"
                ? {
                    expanded: expandedDone,
                    onToggle: () => onToggleDone?.(),
                    hasMore: all.length > DONE_COLLAPSED_LIMIT,
                  }
                : undefined
            }
          />
        );
      })}
    </div>
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanBoard.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SuperpowersKanbanCard.tsx \
        desktop/src/components/SuperpowersKanbanColumn.tsx \
        desktop/src/components/SuperpowersKanbanBoard.tsx \
        desktop/src/components/SuperpowersKanbanBoard.test.tsx
git commit -m "feat(desktop): render the board as four columns"
```

---

### Task 4: 顶层视图改写（`SuperpowersKanbanView`）

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanView.tsx`
- Test: `desktop/src/components/SuperpowersKanbanView.test.tsx`

**Interfaces:**
- Consumes: Task 1 `BoardCard`；Task 3 `SuperpowersKanbanBoard`。
- Produces:
  ```ts
  export type { BoardCard } from "../lib/superpowersKanbanState";  // 兼容旧 import 路径
  export function SuperpowersKanbanView(props: {
    switchOn: boolean;
    source: SwitchSource;
    cards: BoardCard[];
    pluginMissing?: boolean;
    expandedDone?: boolean;
    onToggleDone?: () => void;
    onOpenThread?: (threadId: string) => void;
  }): JSX.Element;
  ```

- [ ] **Step 1: 改写测试**

把 `desktop/src/components/SuperpowersKanbanView.test.tsx` 整体替换为（**保留六条既有意图**，按新 DOM 调整；新增 done 折叠一条）：

```tsx
/** @vitest-environment jsdom */
import { describe, expect, it, afterEach, vi } from "vitest";
import { render, cleanup, screen, fireEvent } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanView } from "./SuperpowersKanbanView";

afterEach(() => cleanup());

function card(id: string, state: string, extra: Partial<BoardCard> = {}): BoardCard {
  return { id, state, progress: null, detail: "", threadId: null, title: id, ...extra };
}

describe("SuperpowersKanbanView", () => {
  it("renders a card with its title, id and state", () => {
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[card("card-1", "running", { title: "do the thing" })]}
      />,
    );
    expect(screen.getByText("do the thing")).toBeTruthy();
    expect(screen.getByText("running")).toBeTruthy();
    expect(screen.getByText("card-1")).toBeTruthy();
  });

  it("explains itself instead of looking empty when disabled", () => {
    render(<SuperpowersKanbanView switchOn={false} source="default" cards={[]} />);
    expect(screen.getByText(/disabled/i)).toBeTruthy();
  });

  it("says the board is empty when enabled with no cards", () => {
    render(<SuperpowersKanbanView switchOn source="project" cards={[]} />);
    expect(screen.getByText(/empty/i)).toBeTruthy();
  });

  it("names the missing plugin instead of showing an empty board", () => {
    render(<SuperpowersKanbanView switchOn source="project" cards={[]} pluginMissing />);
    expect(screen.getByText(/插件未安装/)).toBeTruthy();
    expect(screen.queryByText(/empty/i)).toBeNull();
  });

  it("opens a card's linked thread on click", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[card("c1", "awaiting_merge", { threadId: "thread-1" })]}
        onOpenThread={onOpenThread}
      />,
    );
    fireEvent.click(screen.getByText("thread-1"));
    expect(onOpenThread).toHaveBeenCalledWith("thread-1");
  });

  it("renders no clickable thread link for a card without a thread", () => {
    const onOpenThread = vi.fn();
    render(
      <SuperpowersKanbanView
        switchOn
        source="project"
        cards={[card("c1", "queued")]}
        onOpenThread={onOpenThread}
      />,
    );
    expect(screen.queryByRole("button")).toBeNull();
    expect(onOpenThread).not.toHaveBeenCalled();
  });

  it("collapses a long done column and forwards the toggle", () => {
    const onToggleDone = vi.fn();
    // 完成时刻倒序：最新 d6..d0。折叠默认显示最近 5 张（d6..d2）。
    const done = Array.from({ length: 7 }, (_, i) =>
      card(`d${i}`, "done", { title: `d${i}`, terminalAt: `2026-10-0${i + 1}T00:00:00+08:00` }),
    );
    const { rerender } = render(
      <SuperpowersKanbanView switchOn source="project" cards={done} onToggleDone={onToggleDone} />,
    );
    expect(screen.queryByText("d0")).toBeNull();
    expect(screen.getByText("d6")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /展开|收起/ }));
    expect(onToggleDone).toHaveBeenCalled();
    rerender(
      <SuperpowersKanbanView switchOn source="project" cards={done} expandedDone onToggleDone={onToggleDone} />,
    );
    expect(screen.getByText("d0")).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanView.test.tsx`
Expected: FAIL（`d6` 折叠/展开、DOM 与新 props 尚未实现）。

- [ ] **Step 3: 实现**

把 `desktop/src/components/SuperpowersKanbanView.tsx` 整体替换为：

```tsx
import type { SwitchSource } from "../lib/superpowersKanbanSwitch";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanBoard } from "./SuperpowersKanbanBoard";

// 兼容旧 import 路径：卡片数据形状已迁到 lib，旧代码可从 View 继续拿到类型。
export type { BoardCard };

export function SuperpowersKanbanView({
  switchOn,
  source,
  cards,
  pluginMissing = false,
  expandedDone = false,
  onToggleDone,
  onOpenThread,
}: {
  switchOn: boolean;
  source: SwitchSource;
  cards: BoardCard[];
  /** The plugin never answered, so there is no board to render at all. */
  pluginMissing?: boolean;
  expandedDone?: boolean;
  onToggleDone?: () => void;
  /** 打开某张卡关联的会话；上层接到既有的 thread/resume 入口。 */
  onOpenThread?: (threadId: string) => void;
}) {
  if (pluginMissing) {
    return (
      <div className="p-4 text-sm text-fg-muted">
        Superpowers 看板插件未安装。插件负责回答看板的所有问题，装上它这里才会显示卡片。
      </div>
    );
  }
  if (!switchOn) {
    return (
      <div className="p-4 text-sm text-fg-muted">
        Superpowers 看板 is disabled ({source}). Enable it in settings.
      </div>
    );
  }
  if (cards.length === 0) {
    return <div className="p-4 text-sm text-fg-muted">Superpowers 看板 is empty.</div>;
  }
  return (
    <SuperpowersKanbanBoard
      cards={cards}
      expandedDone={expandedDone}
      onToggleDone={onToggleDone}
      onOpenThread={onOpenThread}
    />
  );
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanView.test.tsx`
Expected: PASS（7 条）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SuperpowersKanbanView.tsx \
        desktop/src/components/SuperpowersKanbanView.test.tsx
git commit -m "feat(desktop): make the board view delegate to the four-column board"
```

---

### Task 5: App 主区域覆盖与数据接线

**Files:**
- Modify: `desktop/src/App.tsx`

**Interfaces:**
- Consumes: Task 1 `normalizeCard` / `BoardCard`；Task 3 `SuperpowersKanbanBoard`；现有 `SuperpowersKanbanSettings` / `SuperpowersKanbanEnqueue` / `SuperpowersKanbanCollapsedBar` / `BOARD_ERROR_TEXT` / `boardError` / `boardCards`。
- Produces: 主区域"会话 / 看板"二选一渲染；`doneExpanded` 状态。

- [ ] **Step 1: 改 import 与状态**

在 `App.tsx` 顶部把看板相关 import 调整为：

```ts
import { SuperpowersKanbanSettings } from "./components/SuperpowersKanbanSettings";
import { SuperpowersKanbanCollapsedBar } from "./components/SuperpowersKanbanCollapsedBar";
import { SuperpowersKanbanEnqueue } from "./components/SuperpowersKanbanEnqueue";
import { SuperpowersKanbanView } from "./components/SuperpowersKanbanView";
import { normalizeCard, type BoardCard } from "./lib/superpowersKanbanState";
```

把卡片 state 的类型从 `BoardCardDto[]` 换成 `BoardCard[]`：

```ts
const [boardCards, setBoardCards] = useState<BoardCard[]>([]);
```

在 `boardCollapsed` 附近新增（不持久化）：

```ts
// done 列是否展开。纯界面状态：每次打开/切换看板都重置为折叠。
const [doneExpanded, setDoneExpanded] = useState(false);
```

在 `refreshSelectedBoard` 里，把拿到的原始卡归一化后再落地：

```ts
const [sw, cards] = await Promise.all([
  readBoardSwitch(boardRpc, selectedBoard),
  fetchBoard(boardRpc, selectedBoard),
]);
if (seq !== boardSeq.current) return;
setBoardOn(sw.on);
setBoardSource(sw.source);
setBoardCards(cards.map(normalizeCard).filter((c): c is BoardCard => c !== null));
setBoardPluginMissing(false);
```

在 `onOpenBoard` 里，切项目时重置 done 展开：

```ts
const onOpenBoard = (path: string) => {
  if (path === selectedBoard) {
    setBoardCollapsed(false);
    return;
  }
  setBoardError(null);
  setBoardCards([]);
  setDoneExpanded(false);   // 换项目 → 新看板从折叠开始
  setSelectedBoard(path);
  setBoardCollapsed(false);
};
```

- [ ] **Step 2: 改主区域渲染（覆盖 ChatView + MessageInput）**

把 `<div className="relative flex min-w-0 flex-1 flex-col">` 里的看板段（原 `118` 行附近的
`{selectedBoard !== null && !boardCollapsed && (<section …>…)}`）替换为**完整视图覆盖**：
看板展开时，把 `StatusBar` / `ChatView` / `MessageInput` 三块包进"仅在未展开看板时渲染"的分支。
即在该 `<div>` 内改为：

```tsx
{selectedBoard !== null && boardCollapsed && (
  <SuperpowersKanbanCollapsedBar
    board={selectedBoard}
    onExpand={() => setBoardCollapsed(false)}
  />
)}
{selectedBoard !== null && !boardCollapsed ? (
  <section
    aria-label="Superpowers 看板"
    className="flex min-h-0 flex-1 flex-col overflow-hidden border-b border-neutral-800 bg-neutral-925"
  >
    {/* 顶栏：项目路径 + 开关/收起/值守 + 入队 + 错误行 */}
    <div className="flex items-center gap-3 px-4 pt-3">
      <span className="truncate font-mono text-xs text-fg-muted" title={selectedBoard}>
        {selectedBoard}
      </span>
    </div>
    <SuperpowersKanbanSettings
      switchOn={boardOn}
      source={boardSource}
      onToggle={(next) => void onToggleBoardSwitch(next)}
      onCollapse={() => setBoardCollapsed(true)}
      watchmanEnabled={watchmanEnabled}
      onToggleWatchman={(next) => void onToggleWatchman(next)}
      watchmanWarning={watchmanWarning}
    />
    {boardError !== null && (
      <div className="flex items-center gap-3 px-4 pb-3 text-xs text-red-400">
        <span>{BOARD_ERROR_TEXT[boardError]}</span>
        {boardError === "not_created" && (
          <button
            type="button"
            onClick={() => void onCreateBoard(selectedBoard)}
            className="rounded border border-neutral-700 px-2 py-0.5 text-neutral-300 hover:text-neutral-100"
          >
            创建看板
          </button>
        )}
      </div>
    )}
    <SuperpowersKanbanEnqueue
      pickFile={pickFile}
      enqueue={(spec, plan) => enqueueBoardCard(boardRpc, selectedBoard, spec, plan)}
    />
    <SuperpowersKanbanView
      switchOn={boardOn}
      source={boardSource}
      cards={boardCards}
      pluginMissing={boardPluginMissing}
      expandedDone={doneExpanded}
      onToggleDone={() => setDoneExpanded((v) => !v)}
      onOpenThread={(id) => void selectThread(id)}
    />
  </section>
) : (
  <>
    <StatusBar
      cwd={current?.info?.cwd ?? null}
      model={current?.info?.model ?? null}
      status={status}
      usage={current?.session.usage ?? null}
    />
    <ChatView
      items={current?.session.items ?? []}
      error={current?.session.lastError ?? null}
      retrying={current?.session.retrying ?? null}
    />
    <MessageInput
      turnActive={current?.session.turnActive ?? false}
      onSend={send}
      onInterrupt={interrupt}
      mode={current?.mode ?? null}
      onModeChange={setThreadMode}
      onSlashCommand={(name, args) => void onSlashCommand(name, args)}
      value={current?.draft ?? ""}
      onDraftChange={changeDraft}
      disabled={current === null}
    />
  </>
)}
```

> 关键点：`StatusBar` / `ChatView` / `MessageInput` 从原来无条件渲染，改为包进三元表达式的
> `else` 分支。`ApprovalBanner` / `SubagentTrace` / `ApprovalDialog` 保持原位（看板展开时
> 它们仍可浮在上面，不在本次覆盖范围内）。

- [ ] **Step 3: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无错误。（若 `BoardCardDto` 出现"未使用"告警，从 `superpowersKanbanSwitch` 的 import 里删掉该类型引用；`fetchBoard` 仍返回它，无需改 `superpowersKanbanSwitch.ts`。）

- [ ] **Step 4: 跑全部桌面测试**

Run: `cd desktop && npx vitest run`
Expected: 全部通过（含 `App.test.tsx` 等既有文件）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx
git commit -m "feat(desktop): make the board cover the conversation area"
```

---

### Task 6: 回归与全量确认

**Files:** 无代码改动（除失败暴露的修正）。

- [ ] **Step 1: 类型 + 全量测试**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 类型无错，全部测试通过。

- [ ] **Step 2: 确认没有触碰插件/宿主**

Run: `git diff --name-only HEAD~5 -- . | grep -v '^desktop/' || echo "only desktop changed"`
Expected: `only desktop changed`。

- [ ] **Step 3: 手动验收（可选，需要 dev 环境）**

`cd desktop && npm run dev`，点侧栏某项目的「Superpowers 看板」：
- 看板覆盖对话区、保留顶部标题栏与底部状态栏；
- 四列可见、列头带计数；`needs_you`/`awaiting_merge` 落在 need decision 列（红色强调）；
- done 列默认 5 张，列头可展开/收起；
- 点卡片会话链接切到该会话；点侧栏回看板。

- [ ] **Step 4: 提交（如有修正）**

```bash
git add -u && git commit -m "chore(desktop): fix issues found in board regression"
```

---

## Self-Review

**Spec coverage：**
- §3 列映射（10 状态 + 未知→doing）→ Task 2 `columnForState` 测试逐条。
- §4.1 覆盖范围（盖 ChatView+MessageInput，留 TitleBar/StatusBar）→ Task 5 Step 2。
- §4.2 顶栏（项目路径 + 设置 + 入队 + 错误行）→ Task 5 Step 2。
- §4.3 四列等宽 / 列头计数 / 空列不塌陷 / 窄屏横向滚动 → Task 3（`flex` + `overflow-x-auto`，空列渲染"空"）+ Task 3 测试 `keeps all four columns visible when empty`。
- §4.4 done 折叠 5 张 / 可展开 / 不持久化 → Task 2 `collapseDone` + Task 3 `doneToggle` + Task 5 `doneExpanded`（本地 state，切项目重置）。
- §4.5 need decision 强调 → Task 3（红边框 + `aria-label="需你处理"`）+ 测试。
- §4.6 卡片内容（标题 / id 中间省略 / 状态 / 会话链接 / 合并分支）→ Task 1 `deriveTitle` + Task 3 `SuperpowersKanbanCard`。
- §4.7 退出（点会话切走）→ Task 3/4 `onOpenThread` + Task 5 接 `selectThread`。
- §1.1 数据链接通（`normalizeCard` 接入 App）→ Task 1 + Task 5 Step 1。
- §6 边界（插件缺失/关闭/空/旧卡缺字段/未知状态）→ Task 1（可选字段）+ Task 2（unknown）+ Task 4（四态）。
- §7 测试（单测 + 组件 + 既有迁移 + 回归）→ Task 1–6。
- §9 遗留（写操作、持久化、进度）→ 未纳入任何任务（符合非目标）。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤都给了可粘贴代码；命令与预期输出均写明。

**Type consistency：**
- `BoardCard` 定义唯一在 `superpowersKanbanState.ts`，`View` 通过 `export type { BoardCard }` 再导出（Task 4），`App.tsx` 从 `lib/superpowersKanbanState` 导入（Task 5）——无反向依赖。
- `BoardColumnKey` 在 Task 2 定义，Task 3 的 `SuperpowersKanbanColumn`/`SuperpowersKanbanBoard` 消费，键名 `queued/doing/needDecision/done` 全一致。
- `collapseDone(cards, expanded)`、`DONE_COLLAPSED_LIMIT`、`boardColumns`、`columnForState` 的签名在定义与调用处一致。
- `normalizeCard` 返回 `BoardCard | null`，Task 5 用 `.filter((c): c is BoardCard => c !== null)` 收窄。

**已知取舍（写清以免实现者困惑）：**
- `detail` 回退链新增 `spec_path` 一环，是有意的行为变化；既有单测不含 `spec_path`，故不受影响。
- `SuperpowersKanbanColumn` 的 `totalCount` 为必填 prop；Task 3 的 `SuperpowersKanbanBoard` 已传。
- `SuperpowersKanbanView` 在"空看板"时**不画四列**（保留整块 empty 文案），与 spec §6"不画空四列"一致；此时 Task 3 的"空列不塌陷"只针对有卡时的某一空列。
