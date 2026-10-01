# Spec 3：桌面端建卡入口 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面端看板面板里补一个「加入看板」按钮：连选 spec + plan 两个文件，投递一张卡。

**Architecture:** 沿用 Spec 5 的组件分法——新建一个单一职责的展示+交互组件 `SuperpowersKanbanEnqueue`，把「选文件」与「发 RPC」都以 props 注入，组件自己不碰 Tauri、不碰 RPC 客户端。`App.tsx` 负责把 `@tauri-apps/plugin-dialog` 的 `open` 与既有的 `enqueueBoardCard()` 接进去。这样组件可用假的选择器测试，不必 mock Tauri。

**Tech Stack:** React + TypeScript、vitest + jsdom + @testing-library/react、既有 `superpowers-kanban/enqueue` RPC。

## Global Constraints

- 提交前 `./node_modules/.bin/tsc --noEmit` 必须干净。
- vitest 需 `TMPDIR="$PWD/.tmpverify"`（worktree 无 node_modules，先 `ln -sfn <主仓库>/desktop/node_modules node_modules`，跑完删除，不要提交）。
- 只**投递**，不在此入口生成 spec/plan（与 Spec 2 的 skill 职责一致）。
- 只选**文件**，不选目录；不做拖拽、不做路径手输。
- 取消选择必须**不产生**任何 RPC 调用。
- 本 spec 按概述的实施顺序落在 Spec 4 **之前**，因此调用既有的 `superpowers-kanban/enqueue`；Spec 4 时改道 `plugin/query`。

---

### Task 1: 抽出错误信息格式化（消除重复）

**Files:**
- Create: `desktop/src/lib/errorMessage.ts`
- Create: `desktop/src/lib/errorMessage.test.ts`
- Modify: `desktop/src/App.tsx`（删本地 `formatError`，改 import）

**Interfaces:**
- Produces: `export function formatError(error: unknown): string`——与 `App.tsx` 原实现**逐字同行为**：对象且 `message` 为字符串 → 该字符串；否则 `String(error)`。

- [ ] **Step 1: 写失败测试**

```ts
import { describe, expect, it } from "vitest";
import { formatError } from "./errorMessage";

describe("formatError", () => {
  it("prefers the message field of an object", () => {
    expect(formatError({ message: "unknown thread" })).toBe("unknown thread");
    expect(formatError({ code: -32011, message: "unknown thread" })).toBe("unknown thread");
  });

  it("falls back to String() for anything else", () => {
    expect(formatError("plain")).toBe("plain");
    expect(formatError(42)).toBe("42");
    expect(formatError({ message: 7 })).toBe("[object Object]");
    expect(formatError(null)).toBe("null");
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" ./node_modules/.bin/vitest run src/lib/errorMessage.test.ts`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**（照搬 `App.tsx` 原 5 行）

- [ ] **Step 4: 改 `App.tsx` 用它**

删掉 App.tsx 里的 `function formatError(e: unknown)`，改成
`import { formatError } from "./lib/errorMessage";`。**行为不得改变**。

- [ ] **Step 5: 运行确认通过（含既有 App 测试，锁住行为不变）**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" ./node_modules/.bin/vitest run src/App.test.tsx src/lib/errorMessage.test.ts`
Expected: PASS（App 测试里有断言错误文案的用例，是这次重构的回归网）

- [ ] **Step 6: Commit**

```bash
git commit -m "refactor(desktop): move formatError into lib"
```

---

### Task 2: `SuperpowersKanbanEnqueue` 组件

**Files:**
- Create: `desktop/src/components/SuperpowersKanbanEnqueue.tsx`
- Create: `desktop/src/components/SuperpowersKanbanEnqueue.test.tsx`

**Interfaces:**
- Consumes: Task 1 的 `formatError`。
- Produces: `SuperpowersKanbanEnqueue({ pickFile, enqueue })`：
  - `pickFile: () => Promise<string | null>`（`null` = 用户取消）
  - `enqueue: (specPath: string, planPath: string) => Promise<void>`

- [ ] **Step 1: 写失败测试**（对应 spec §5 的三条重点）

```tsx
// 1. 点按钮 → 选两个文件 → 恰好一次 enqueue(spec, plan)
// 2. spec 步取消 → 不调用；plan 步取消 → 不调用
// 3. enqueue 抛错 → 面板显示 formatError 的文案，且按钮可再点
```

- [ ] **Step 2: 运行确认失败** → FAIL

- [ ] **Step 3: 实现**

要点：`busy` 期间禁用按钮（避免连点产生两次投递）；`error` 与 `notice` 互斥；
成功提示区分「已投递，等待插件校验」——因为投递后要等 runner tick 才入队
（Spec 2 的 `list` 把它显示为 `pending`），不能说成"已加入队列"。

- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(desktop): add the enqueue control for the kanban panel"
```

---

### Task 3: 接进 App

**Files:**
- Modify: `desktop/src/App.tsx`
- Modify: `desktop/src/App.test.tsx`

- [ ] **Step 1: 写失败测试**（mock `@tauri-apps/plugin-dialog`，用可变队列控制两次选择）

```tsx
// 选择两个文件 → 发出一次 superpowers-kanban/enqueue，参数为两个路径
// 取消 → 不发出
// RPC 失败 → 面板出现错误文案
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**：`App.tsx` 加 `pickFile`（动态 import `open({directory:false})`，
  与 `pickDirectory` 同款）与 `enqueue={(spec, plan) => enqueueBoardCard(boardRpc, spec, plan)}`，
  在看板列内、设置之下渲染 `SuperpowersKanbanEnqueue`。
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(desktop): wire the kanban enqueue control to the board panel"
```

---

### Task 4: 回归 + 文档

- [ ] **Step 1:** `tsc --noEmit` 干净 + 全量 vitest 通过。
- [ ] **Step 2:** 更新 spec 状态为「已实施」，并修正其过时的文件名引用
  （`desktop/src/lib/boardSwitch.ts` → `superpowersKanbanSwitch.ts`；`board/enqueue` → 实际方法名）。
- [ ] **Step 3:** Commit。
