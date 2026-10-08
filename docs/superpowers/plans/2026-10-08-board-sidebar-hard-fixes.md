# 看板侧栏摘要与合并卡展示修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修好桌面端三处硬伤——侧栏摘要漏数合并卡、摘要不随轮询刷新、合并卡不显示合并信息。

**Architecture:** 三处都是纯前端缺陷，改动落在三个互不相干的小单元：`summarize()`（计数口径）、一个把「一次刷新同时刷选中看板与侧栏摘要」绑成一拍的纯函数、以及合并卡卡片上多渲染一行 `source → base`。全部走既有 vitest 单元测试，不碰 RPC、不碰插件。

**Tech Stack:** TypeScript、React 19、vitest、@testing-library/react、jsdom。

**依据 spec:** `docs/superpowers/specs/2026-10-08-board-merge-as-session-design.md` §1.1、§6.1、§6.2、§7。

## Global Constraints

- 包管理/工具链在 `desktop/` 下运行，且当前环境 `node`/`npm` 不在默认 PATH：每个命令前先 `export PATH="/opt/homebrew/bin:$PATH"`。
- 桌面测试：`cd desktop && npx vitest run <file>`；类型检查：`cd desktop && npx tsc --noEmit`。
- 组件测试文件首行必须是 `/** @vitest-environment jsdom */`，并在 `afterEach(() => cleanup())`。
- 注释用中文，写「为什么」而不是「是什么」；测试名用中文，说明该断言锁住的契约。
- 不改 `BoardCard` 类型、不改 RPC 形状、不改四列视图组件。
- commit 用 conventional commits，正文中文，**不写 `Co-Authored-By`**。

---

### Task 1: 摘要不再漏数合并卡与待处理卡

**Files:**
- Modify: `desktop/src/lib/boardIndex.ts`（`summarize`，现约 35-45 行）
- Test: `desktop/src/lib/boardIndex.test.ts`（现约 30-38 行的 `summarize` 分组）

**Interfaces:**
- Consumes: 无。
- Produces: `summarize(cards: { state: string }[]): string` —— 语义收紧为「分批数还没落地的卡」：
  `queued`/`launching` → 排队；`running`/`merging` → 运行中；`awaiting_merge` → 待合并；
  `needs_you` → 待处理。空数组返回 `"空"`；没有任何待办返回 `"无待办"`；
  否则用 `" · "` 连接**仅非零**的桶，顺序固定为 排队 / 运行中 / 待合并 / 待处理。

- [ ] **Step 1: 改测试（先让它失败）**

把 `desktop/src/lib/boardIndex.test.ts` 里 `describe("summarize", ...)` 中的
`it("只数排队与运行，其它状态不进摘要", ...)` 整段替换为下面三个用例
（其余 `summarize` 用例——`"2 排队 · 1 运行中"` 与大小写不敏感——保持不动，新口径下仍成立）：

```ts
describe("summarize", () => {
  it("数出还在队列里的每一类，包括合并卡", () => {
    const cards = [
      { state: "queued" },
      { state: "queued" },
      { state: "running" },
      { state: "merging" },
      { state: "awaiting_merge" },
      { state: "needs_you" },
    ];
    expect(summarize(cards)).toBe("2 排队 · 2 运行中 · 1 待合并 · 1 待处理");
  });

  it("合并卡不再被算成 0", () => {
    // 用户报的原始 case：3 张等待合并 + 2 张待处理，旧口径显示「0 排队 · 0 运行中」，
    // 于是卡住的两张合并卡在侧栏完全隐形。
    const cards = [
      { state: "awaiting_merge" },
      { state: "awaiting_merge" },
      { state: "awaiting_merge" },
      { state: "needs_you" },
      { state: "needs_you" },
    ];
    expect(summarize(cards)).toBe("3 待合并 · 2 待处理");
  });

  it("没有待办时明说，不留空", () => {
    expect(summarize([{ state: "done" }, { state: "failed" }])).toBe("无待办");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/boardIndex.test.ts`
Expected: FAIL —— 新用例得到旧的 `"0 排队 · 0 运行中"`，与期望不符。

- [ ] **Step 3: 改实现**

把 `desktop/src/lib/boardIndex.ts` 的 `summarize` 整段替换为：

```ts
export function summarize(cards: { state: string }[]): string {
  if (cards.length === 0) return "空";
  let queued = 0;
  let active = 0;
  let awaitingMerge = 0;
  let needsYou = 0;
  for (const card of cards) {
    const state = card.state.toLowerCase();
    if (state === "queued" || state === "launching") queued += 1;
    else if (state === "running" || state === "merging") active += 1;
    else if (state === "awaiting_merge") awaitingMerge += 1;
    else if (state === "needs_you") needsYou += 1;
  }
  const parts: string[] = [];
  if (queued > 0) parts.push(`${queued} 排队`);
  if (active > 0) parts.push(`${active} 运行中`);
  if (awaitingMerge > 0) parts.push(`${awaitingMerge} 待合并`);
  if (needsYou > 0) parts.push(`${needsYou} 待处理`);
  return parts.length > 0 ? parts.join(" · ") : "无待办";
}
```

同时把该函数上方文档注释替换为说明新口径：

```ts
/**
 * 侧栏条目上的摘要。
 *
 * 分批数，且**只数还没落地的卡**：排队（含刚认领）、运行中（含正在合并的一轮）、
 * 待合并（等用户发话）、待处理（要用户回话）。合并卡曾整类落在计数之外——五张卡
 * 卡住时摘要仍显示「0 排队 · 0 运行中」，侧栏看起来什么都没发生。完成/失败/取消
 * 是历史，不进摘要。没有待办时明说「无待办」，而不是显示一排 0。
 */
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/boardIndex.test.ts`
Expected: PASS（该文件全部用例通过）。

- [ ] **Step 5: 类型检查**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit`
Expected: 无错误。

- [ ] **Step 6: Commit**

```bash
git add desktop/src/lib/boardIndex.ts desktop/src/lib/boardIndex.test.ts
git commit -m "fix(desktop): 侧栏摘要不再漏数合并卡与待处理卡

summarize 原先只数 queued/running，awaiting_merge/merging/needs_you
全被忽略，导致卡住的合并卡在侧栏显示为「0 排队 · 0 运行中」。改为分批
统计未落地状态，并区分待合并/待处理。"
```

---

### Task 2: 侧栏摘要随 2 秒轮询刷新

**Files:**
- Create: `desktop/src/lib/boardRefresh.ts`
- Test: `desktop/src/lib/boardRefresh.test.ts`
- Modify: `desktop/src/App.tsx`（导入区；`boardTick.current` 赋值处的 `useEffect`，现约 316-321 行）

**Interfaces:**
- Consumes: 无（纯函数，注入两个刷新器）。
- Produces: `makeBoardTick(refreshSelectedBoard: () => Promise<unknown>, refreshBoards: () => Promise<unknown>): () => void`
  —— 返回一个「一拍」函数，调用它即同时触发两处刷新、不等结果。

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/lib/boardRefresh.test.ts`：

```ts
import { describe, expect, it, vi } from "vitest";
import { makeBoardTick } from "./boardRefresh";

describe("makeBoardTick", () => {
  it("一拍同时刷新选中看板与侧栏摘要", () => {
    // 两套数据必须同拍：摘要只在握手时读过一次，不并入轮询就会长期停在旧值，
    // 侧栏因此一直显示过期的「0 排队」。
    const selected = vi.fn(async () => {});
    const boards = vi.fn(async () => {});
    makeBoardTick(selected, boards)();
    expect(selected).toHaveBeenCalledTimes(1);
    expect(boards).toHaveBeenCalledTimes(1);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/boardRefresh.test.ts`
Expected: FAIL —— `Failed to resolve import "./boardRefresh"`。

- [ ] **Step 3: 写实现**

创建 `desktop/src/lib/boardRefresh.ts`：

```ts
/**
 * 看板刷新：一「拍」同时刷两处。
 *
 * 主区域的选中看板（`refreshSelectedBoard`）与侧栏各项目的摘要（`refreshBoards`）
 * 是两套数据。摘要原先只在握手时读一次，此后除非用户点进看板否则永远停在旧值——
 * 侧栏因此长期显示过期的「0 排队」。把两者绑进同一拍，侧栏才随队列真实变化。
 *
 * 两个刷新器各自吞掉自己的失败（它们内部的 try/catch 负责），这里不 await、
 * 不 catch：一拍绝不能被某处的读失败拖住。
 */
export function makeBoardTick(
  refreshSelectedBoard: () => Promise<unknown>,
  refreshBoards: () => Promise<unknown>,
): () => void {
  return () => {
    void refreshSelectedBoard();
    void refreshBoards();
  };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/lib/boardRefresh.test.ts`
Expected: PASS。

- [ ] **Step 5: 接到 App 的轮询上**

在 `desktop/src/App.tsx` 顶部导入区（与其它 `./lib/...` 导入同处）加入：

```ts
import { makeBoardTick } from "./lib/boardRefresh";
```

把现约 316-321 行的 `useEffect`：

```ts
  useEffect(() => {
    // 计时器只建一次，每次响都走「当前项目」的读法：重建计时器会漏掉切换
    // 瞬间在途的那一拍。
    boardTick.current = () => void refreshSelectedBoard();
    // 选中项目一落地就读一次，不等下一拍轮询：否则点开看板会有最多 2 秒的空面板。
    void refreshSelectedBoard();
  }, [refreshSelectedBoard]);
```

替换为：

```ts
  useEffect(() => {
    // 计时器只建一次，每次响都走「当前项目」的读法：重建计时器会漏掉切换
    // 瞬间在途的那一拍。
    // 一拍要同时刷选中看板与侧栏摘要：摘要若只在握手时读一次，侧栏会一直
    // 停在旧值，看起来像「什么都没在跑」。
    boardTick.current = makeBoardTick(refreshSelectedBoard, refreshBoards);
    // 选中项目一落地就读一次，不等下一拍轮询：否则点开看板会有最多 2 秒的空面板。
    void refreshSelectedBoard();
  }, [refreshSelectedBoard, refreshBoards]);
```

- [ ] **Step 6: 类型检查 + 全量桌面测试**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 无类型错误；全部测试通过（含既有 `App` 相关与侧栏测试，确认没回归）。

- [ ] **Step 7: Commit**

```bash
git add desktop/src/lib/boardRefresh.ts desktop/src/lib/boardRefresh.test.ts desktop/src/App.tsx
git commit -m "fix(desktop): 侧栏摘要并入 2 秒轮询

refreshBoards 原先只在握手时调用，摘要此后不再更新，侧栏长期停在
过期的「0 排队」。把刷新选中看板与刷新侧栏摘要绑进同一拍。"
```

---

### Task 3: 合并卡显示 source → base

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanCard.tsx`（约 34-35 行之后插入一行）
- Test: `desktop/src/components/SuperpowersKanbanCard.test.tsx`（新建）

**Interfaces:**
- Consumes: `BoardCard`（既有类型；`kind`、`detail` 字段由 `superpowersKanbanState.ts::normalizeCard` 保证：
  合并卡的 `detail` 恒为 `"<source> → <base>"`）。
- Produces: 无对外接口；仅渲染行为。

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/SuperpowersKanbanCard.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, expect, it, afterEach } from "vitest";
import { render, cleanup, screen } from "@testing-library/react";
import type { BoardCard } from "../lib/superpowersKanbanState";
import { SuperpowersKanbanCard } from "./SuperpowersKanbanCard";

afterEach(() => cleanup());

function card(extra: Partial<BoardCard> = {}): BoardCard {
  return { id: "c1", state: "needs_you", progress: null, detail: "", threadId: null, ...extra };
}

describe("SuperpowersKanbanCard", () => {
  it("合并卡显示 source → base", () => {
    // 合并卡没有 thread_id，卡片是用户唯一的落点：不显示 source→base 就
    // 只剩一串 id + 红色感叹号，看不出它要合什么、卡在哪。
    render(
      <SuperpowersKanbanCard
        card={card({ kind: "merge", source: "kanban/a", base: "main", detail: "kanban/a → main" })}
      />,
    );
    expect(screen.getByText("kanban/a → main")).toBeTruthy();
  });

  it("实现卡不显示合并行", () => {
    render(<SuperpowersKanbanCard card={card({ kind: "implementation", detail: "/work/tree" })} />);
    expect(screen.queryByText("/work/tree")).toBeNull();
  });

  it("合并卡没有会话时不渲染跳转按钮", () => {
    render(<SuperpowersKanbanCard card={card({ kind: "merge", detail: "kanban/a → main" })} />);
    expect(screen.queryByRole("button")).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/SuperpowersKanbanCard.test.tsx`
Expected: FAIL —— 找不到文本 `kanban/a → main`。

- [ ] **Step 3: 写实现**

在 `desktop/src/components/SuperpowersKanbanCard.tsx` 中，把 id/state 那一行
`</div>`（现第 34 行，即 `<span className="text-fg-muted">{card.state}</span>` 所在 div 的闭合）
之后、`{card.threadId ? (` 之前，插入：

```tsx
      {card.kind === "merge" && card.detail ? (
        <div className="mt-1 truncate font-mono text-xs text-fg-subtle" title={card.detail}>
          {card.detail}
        </div>
      ) : null}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run src/components/SuperpowersKanbanCard.test.tsx`
Expected: PASS。

- [ ] **Step 5: 类型检查 + 全量桌面测试**

Run: `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 无类型错误；全部测试通过。

- [ ] **Step 6: Commit**

```bash
git add desktop/src/components/SuperpowersKanbanCard.tsx desktop/src/components/SuperpowersKanbanCard.test.tsx
git commit -m "fix(desktop): 合并卡展示 source → base

卡片原先只画 title/state/threadId，而合并卡没有 thread_id，用户只剩一串
id 可看。补上 normalizeCard 已算好的 detail（source → base）。"
```

---

## 验收（人可验证）

1. `export PATH="/opt/homebrew/bin:$PATH" && cd desktop && npx vitest run` 全绿。
2. 侧栏「看板」摘要对当前 5 张卡（3 `awaiting_merge` + 2 `needs_you`）显示
   `"3 待合并 · 2 待处理"`，不再是 `"0 排队 · 0 运行中"`。
3. 不点进看板，摘要也会随队列变化更新（≤2 秒）。
4. 两张合并卡的卡片上能读到 `kanban/<slug> → main`。

## Self-Review

- **Spec 覆盖**：spec §6.1 三处硬伤 → Task 1（摘要口径）、Task 2（刷新）、Task 3（卡片信息）；
  §7 桌面测试项逐条对应；§6.2 的 `merging → DOING` 列映射**已存在于** `superpowersKanbanBoard.ts`，
  无需改动（`columnForState` 现有 `case "merging": return "doing"`），故不设任务。
- **口径细化**：spec §6.1 原文把 `merging` 与 `awaiting_merge` 归为同一桶。本计划把 `merging`
  归入「运行中」（一轮合并 turn 是进行中的工作），并保留「排队」桶（否则队列积压会从摘要消失）。
  这是对 spec 的细化，已在 Commit 与测试名中写明依据。
- **占位符**：无。每个代码步骤都给了可直接粘贴的完整代码。
- **类型一致**：`makeBoardTick` 在两处（测试与 App）签名一致；`summarize` 签名未变；
  `BoardCard.kind`/`detail` 均为既有字段。
