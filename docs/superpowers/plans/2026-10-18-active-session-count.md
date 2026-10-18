# 侧栏「运行中会话数」指示器 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在桌面 App 侧栏 footer 的设置齿轮旁,显示当前全局正在运行的会话数(形如 `3 个会话运行中`)。

**Architecture:** 纯前端。`ThreadSidebar` 已经收到覆盖全部 workspace 会话的
`statuses: Map<thread_id, ThreadStatus>`(由 `App.tsx:1888` 每轮渲染从 `ThreadStore` 重建,
底层是 `Delivery::List` 层的 `thread/status/updated` 恒推)。新增一个无依赖纯函数数出
`status === "running"` 的条目数,再在 footer 里渲染一颗复用既有 `useSpinPhase` 的胶囊。
不动协议、不动 Rust、不新增 prop。

**Tech Stack:** TypeScript、React 19、Tailwind v4、Vitest(jsdom)+ @testing-library/react。

## Global Constraints

- **分支**:所有改动在 worktree `.worktrees/active-session-count`(分支
  `feat/active-session-count`)内进行。**严禁**在 `main` 上直接改代码或提交。
- **commit message**:conventional commits(`feat:` / `test:` / `docs:` 等)。**不要**写
  `Co-Authored-By: Claude ...` 行。首行 <= 72 字符。
- **语义(不得漂移)**:「运行中」严格等于 `status === "running"`。`awaiting_approval`
  与 `idle` **都不计入**。
- **文案(逐字,不得改写)**:`${n} 个会话运行中`。计数 0 时**整个节点不渲染**。
- **位置**:footer 内、设置齿轮按钮的**紧右侧**(`desktop/src/components/ThreadSidebar.tsx`
  的 footer `div` 内,齿轮 `</button>` 之后)。**不加** `ml-auto`,**不改**齿轮的位置/样式。
- **测试环境**:组件测试文件顶部必须有 `/** @vitest-environment jsdom */`;纯逻辑测试
  (`lib/*.test.ts`)默认 node 环境即可(仓库既有约定)。
- **不新增 prop、不改 `App.tsx`、不改协议、不改 Rust**。

---

### Task 1: 计数纯函数 `runningSessions.ts`

**Files:**
- Create: `desktop/src/lib/runningSessions.ts`
- Test: `desktop/src/lib/runningSessions.test.ts`

**Interfaces:**
- Consumes: `ThreadStatus`(`desktop/src/lib/protocol.ts:44` 再导出的
  `generated/ThreadStatus.ts` 类型,值为 `"idle" | "running" | "awaiting_approval"`)。
- Produces:
  - `countRunningSessions(statuses: ReadonlyMap<string, ThreadStatus>): number`
  - `runningSessionsLabel(n: number): string` — 返回 `${n} 个会话运行中`

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src/lib/runningSessions.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { countRunningSessions, runningSessionsLabel } from "./runningSessions";
import type { ThreadStatus } from "./protocol";

/** 按 [id, status] 对构造入参,避免测试里手写一大堆 Map.set。 */
const map = (...entries: [string, ThreadStatus][]) => new Map(entries);

describe("countRunningSessions", () => {
  it("returns 0 for an empty map", () => {
    expect(countRunningSessions(new Map())).toBe(0);
  });

  it("returns 0 when every thread is idle", () => {
    expect(countRunningSessions(map(["a", "idle"], ["b", "idle"]))).toBe(0);
  });

  // 本设计的核心语义:停在审批窗口的会话**不算**在跑(它在等人,没在干活)。
  it("does not count awaiting_approval", () => {
    expect(countRunningSessions(map(["a", "awaiting_approval"], ["b", "awaiting_approval"]))).toBe(0);
  });

  it("counts only running when statuses are mixed", () => {
    const s = map(
      ["a", "idle"],
      ["b", "running"],
      ["c", "awaiting_approval"],
      ["d", "running"],
      ["e", "idle"],
    );
    expect(countRunningSessions(s)).toBe(2);
  });

  it("counts every entry when all are running", () => {
    expect(countRunningSessions(map(["a", "running"], ["b", "running"], ["c", "running"]))).toBe(3);
  });
});

describe("runningSessionsLabel", () => {
  it("renders the exact copy", () => {
    expect(runningSessionsLabel(3)).toBe("3 个会话运行中");
    expect(runningSessionsLabel(1)).toBe("1 个会话运行中");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/runningSessions.test.ts`
Expected: FAIL — 报错找不到模块 `./runningSessions`(文件尚不存在)。

- [ ] **Step 3: 写最小实现**

创建 `desktop/src/lib/runningSessions.ts`:

```ts
import type { ThreadStatus } from "./protocol";

/**
 * 「此刻有几个会话真的在跑」——严格 `running`。
 *
 * `awaiting_approval` 不计:那种会话在**等人**,没在干活(与侧栏徽标的语义刻意不同,
 * 徽标对两者都显示)。`idle` 自然不计。
 *
 * 入参是侧栏徽标用的同一张表(`App.tsx:1888` 每轮渲染由 `ThreadStore` 重建),故不存在
 * 第二条数据路径、也不会与徽标漂移。
 */
export function countRunningSessions(statuses: ReadonlyMap<string, ThreadStatus>): number {
  let n = 0;
  for (const s of statuses.values()) if (s === "running") n += 1;
  return n;
}

/**
 * 指示器文案。与 `countRunningSessions` 同处一个模块:文案是这条语义的一部分,
 * 两者一起改才不容易失配。
 */
export function runningSessionsLabel(n: number): string {
  return `${n} 个会话运行中`;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/runningSessions.test.ts`
Expected: PASS — 6 个用例全绿(0 空表 / 0 全 idle / 0 全审批 / 混合=2 / 全 running=3 / 文案两条断言)。

- [ ] **Step 5: 提交**

```bash
cd desktop && npx vitest run src/lib/runningSessions.test.ts && npx tsc --noEmit
git add desktop/src/lib/runningSessions.ts desktop/src/lib/runningSessions.test.ts
git commit -m "feat(desktop): count running sessions from the sidebar status map"
```

---

### Task 2: footer 里的指示器胶囊

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`(footer 在 `:1100-1115`;新增内部组件紧邻 `StatusBadge` 定义处 `:49`)。
- Test: `desktop/src/components/ThreadSidebar.test.tsx`(加 3 个用例;`sidebarProps` 在 `:42`)。

**Interfaces:**
- Consumes: `countRunningSessions` / `runningSessionsLabel`(Task 1);既有的
  `useSpinPhase`(`desktop/src/lib/useSpinPhase.ts`)、`supportsWaapi`
  (`desktop/src/lib/spinPhase.ts:57`)。
- Produces: 内部组件 `ActiveSessionCount({ count, stale })`,仅在 `ThreadSidebar` 内部使用
  (不导出)。

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/components/ThreadSidebar.test.tsx` 的 `describe("ThreadSidebar", ...)` 内、
紧随现有 `offers a settings button in the footer` 用例(`:950`)之后,追加:

```tsx
  it("shows the running-session count beside the settings button", () => {
    const statuses = new Map<string, ThreadStatus>([
      ["1", "running"],
      ["2", "awaiting_approval"],
      ["3", "running"],
    ]);
    renderSidebar({ statuses });

    // 只数 running(2 条),审批中的那条不计。
    expect(screen.getByRole("status")).toBeTruthy();
    expect(screen.getByText("2 个会话运行中")).toBeTruthy();
    // 齿轮仍在(新胶囊没有把它挤掉)。
    expect(screen.getByRole("button", { name: "设置" })).toBeTruthy();
  });

  it("renders no running-session indicator when nothing is running", () => {
    renderSidebar({ statuses: new Map([["1", "idle"]]) });

    expect(screen.queryByRole("status")).toBeNull();
    expect(screen.getByRole("button", { name: "设置" })).toBeTruthy();
  });

  it("marks the running-session count as possibly stale when offline", () => {
    renderSidebar({
      statuses: new Map([["1", "running"]]),
      statusStale: true,
    });

    expect(screen.getByTitle("1 个会话运行中（可能过期）")).toBeTruthy();
  });
```

`ThreadStatus` 已在该文件顶部导入(`:6` 的 `import type { ThreadSummary, Workspace, WorkspaceGroup } from "../lib/protocol";` **不含**它,需补)。若缺失,把该行改为:

```tsx
import type { ThreadStatus, ThreadSummary, Workspace, WorkspaceGroup } from "../lib/protocol";
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL — 3 个新用例失败(找不到 `role="status"` 元素 / 找不到文案;`statusStale` 用例找不到 title)。

- [ ] **Step 3: 写最小实现**

在 `desktop/src/components/ThreadSidebar.tsx` 顶部导入区加上:

```tsx
import { countRunningSessions, runningSessionsLabel } from "../lib/runningSessions";
```

在 `StatusBadge` 定义之后(约 `:78`)新增内部组件:

```tsx
/**
 * footer 里的「此刻有几个会话在跑」读数,紧挨设置齿轮。
 *
 * 与 `StatusBadge` 共用 `useSpinPhase`(相位锚定墙钟),避免 CSS 动画在节点重排时
 * 从头播放的视觉回退。恒为 running 形态:它只在计数 > 0 时出现,不存在
 * `awaiting_approval` 形态(那种会话根本不计入)。
 *
 * 计数为 0 时整个节点不渲染,footer 与改动前逐像素一致。
 */
function ActiveSessionCount({ count, stale = false }: { count: number; stale?: boolean }) {
  const ref = useRef<HTMLSpanElement>(null);
  useSpinPhase(ref, count > 0);
  if (count === 0) return null;
  const label = runningSessionsLabel(count);
  const cssFallback = !supportsWaapi();
  return (
    <span
      role="status"
      aria-live="polite"
      aria-label={label}
      title={stale ? `${label}（可能过期）` : label}
      className={`ml-2 flex min-w-0 items-center gap-1.5 text-xs text-fg-subtle ${
        stale ? "opacity-40" : ""
      }`}
    >
      <span
        ref={ref}
        aria-hidden="true"
        className={`shrink-0 rounded-full ${cssFallback ? "animate-spin" : ""} size-3 border-2 border-line-strong border-t-fg`}
      />
      <span className="truncate">{label}</span>
    </span>
  );
}
```

在 `ThreadSidebar` 函数体内、`return` 之前算出计数(放在已有的 `useState` 之后):

```tsx
  const runningCount = countRunningSessions(statuses);
```

在 footer 内、齿轮 `</button>` 之后追加:

```tsx
        <ActiveSessionCount count={runningCount} stale={statusStale} />
```

改完 footer 应为:

```tsx
      <div className="mt-auto flex items-center border-t border-line p-2">
        <button
          type="button"
          aria-label="设置"
          title="设置"
          onClick={onOpenSettings}
          className="rounded p-1 text-fg-subtle hover:bg-raised/50 hover:text-fg"
        >
          {/* ...既有 svg 原样不动... */}
        </button>
        <ActiveSessionCount count={runningCount} stale={statusStale} />
      </div>
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS — 3 个新用例通过,且既有用例(含 `:950` 的「设置」按钮用例)全绿。

- [ ] **Step 5: 跑全套 + 类型检查**

Run: `cd desktop && npx vitest run src/lib/runningSessions.test.ts src/components/ThreadSidebar.test.tsx && npx tsc --noEmit`
Expected: 全 PASS,`tsc` 无错误输出。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): show running-session count beside the settings gear"
```

---

### Task 3: 回归验证 + 项目进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`(追加一条 `[x]`)
- Modify: `README.md:268`(desktop 计数)与 `docs/project-management/README.md`(desktop 行)

**Interfaces:**
- Consumes: Task 1/2 的实现与测试。
- Produces: 无代码接口。

- [ ] **Step 1: 跑全量前端测试与类型检查**

Run: `cd desktop && npm test && npx tsc --noEmit`
Expected: 全部 PASS,0 失败。**这是回归门禁**:本功能改了 `ThreadSidebar`,必须确认
没有别的组件测试被打回(`App.test.tsx` 的侧栏相关用例尤其要看)。

- [ ] **Step 2: 确认生产构建通过**

Run: `cd desktop && npm run build`
Expected: `tsc && vite build` 成功,产出 `dist/`。

- [ ] **Step 3: 更新模块文档**

在 `docs/project-management/desktop.md` 的 feature 清单末尾追加一条(照既有格式:一条
`[x]` + 代码位置 + 判据命令 + 设计链接):

```markdown
- [x] 侧栏「运行中会话数」指示器 — footer 里、设置齿轮右侧显示全局正在运行的会话数（`3 个会话运行中`），严格只数 `status === "running"`（`awaiting_approval` 不计：会话在等人，没在干活）；计数 0 时整个节点不渲染，footer 与改动前逐像素一致；复用 `useSpinPhase` 相位锚定墙钟，手机端离线经 `statusStale` 降透明并标「可能过期」。纯函数 `desktop/src/lib/runningSessions.ts:12`（`countRunningSessions` 只认 `running`）/ `:22`（`runningSessionsLabel` 单点持有文案）；组件 `desktop/src/components/ThreadSidebar.tsx`（内部 `ActiveSessionCount`，`role="status"` + `aria-live="polite"`，恒 running 形态）/ footer 接线（齿轮 `</button>` 之后，不加 `ml-auto`、不动齿轮位置）；**零协议、零 Rust、零新增 prop**。判据：`cd desktop && npx vitest run src/lib/runningSessions.test.ts src/components/ThreadSidebar.test.tsx && npx tsc --noEmit` — [设计](../superpowers/specs/2026-10-18-active-session-count-design.md)
```

- [ ] **Step 4: 修正并更新两份索引的 desktop 计数**

先核对模块文件里的**实际**条数(不要照抄旧数字):

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/active-session-count && awk '/^- \[x\]/{x++} /^- \[ \]/{u++} END{print "done="x" undone="u" total="x+u}' docs/project-management/desktop.md
```

把算出的 `<done>` / `<total>` 写进两处索引,并修掉既有的漂移
(`README.md:268` 原写 `55 / 67`、`docs/project-management/README.md` 原写 `54 / 66`,
都与模块文件当时的真实值不符):

```markdown
| desktop | <done> / <total> | [详情](docs/project-management/desktop.md) |
```

(`README.md` 与 `docs/project-management/README.md` 两处都要改,且数字必须一致。)

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/desktop.md README.md docs/project-management/README.md
git commit -m "docs: 记录侧栏运行中会话数指示器与修正模块计数"
```

---

## Self-Review

**1. Spec coverage** — 逐条对 spec:

| spec 章节 | 落在哪个 Task |
|---|---|
| §2 目标 1(侧栏 footer、齿轮旁、全局计数) | Task 2 Step 3 |
| §2 目标 2(严格 `running`) | Task 1(纯函数)+ 其「does not count awaiting_approval」用例 |
| §2 目标 3(0 时不渲染) | Task 2 Step 3(`if (count === 0) return null`)+ 对应用例 |
| §2 目标 4(即时跟随推送) | 无需代码:沿用既有 `statuses` 表(已是实时)。spec §3.2 |
| §2 目标 5(`statusStale` 标注过期) | Task 2 Step 3 + 对应用例 |
| §2 目标 6(纯函数可单测) | Task 1 |
| §3.3 新模块 `runningSessions.ts` | Task 1 Step 3 |
| §3.3 内部组件 + 位置 | Task 2 Step 3 |
| §3.4 手机端不特殊对待 | Task 2:未加 `isMobile` 分支(符合 spec) |
| §3.5 边界(空表/无 view/崩溃重连) | 空表由 Task 2「0 时不渲染」用例覆盖;另两条沿用既有机制,spec 已说明无需新代码 |
| §3.6 测试 | Task 1 Step 1 + Task 2 Step 1 |
| §3.7 验证命令 | Task 2 Step 5、Task 3 Step 1-2 |
| §4 文档 | Task 3 |

无遗漏。

**2. Placeholder scan** — 无 TBD/TODO;所有代码步骤都带完整可粘贴代码;文案与
CSS 类逐字给出。

**3. Type consistency** — `countRunningSessions` / `runningSessionsLabel` 在 Task 1 定义,
Task 2 按同签名消费;`ActiveSessionCount` 的 props(`count: number`, `stale?: boolean`)
在 Task 2 定义与使用一致;`ThreadStatus` 从 `lib/protocol` 再导出(Task 1 与 Task 2
测试都从那里导入)。

**已修正的一处 spec 内部矛盾**:spec §3.3 原写「胶囊在齿轮**左侧**」而 §3.5 又写
「齿轮在右侧独立定位」,同一 flex 行里两者不能同时成立。已改为「紧右侧、不加
`ml-auto`」(`ml-auto` 会与 footer 既有布局冲突,且需求只要求「旁边」),并同步修了
§3.5 的布局段。本计划的 Task 2 与该修正后的 spec 一致。

**已知的既有文档漂移(本计划顺带修)**:`README.md:268` 的 desktop 计数 `55 / 67` 与
`docs/project-management/README.md` 的 `54 / 66` 都落后于模块文件的真实值(脚本核过:
56 / 68),且两处本身不一致。Task 3 Step 4 一并修正。