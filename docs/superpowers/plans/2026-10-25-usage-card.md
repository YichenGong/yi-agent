# `/usage` 卡片化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `/usage` 的多行文本输出改成与 `/config`、`/help` 同一套视觉语言的结构化卡片。

**Architecture:** 沿用既有的 `NoticeBody` 判别联合机制：`usageCommand.ts` 从「拼字符串」改为「产出结构化 `UsageReport`」，新增 `UsageCard.tsx` 负责摆版，`ChatView` 按 `kind` 分派，`App` 改走 `noticeBody`。

**Tech Stack:** React 19 + TypeScript、Vitest + @testing-library/react、Tailwind（既有 class 命名体系）。

## Global Constraints

- 工作目录：`/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/usage-card`（分支 `feat/usage-card`），**禁止在 `main` 上改动**。
- node 需 `export PATH="/opt/homebrew/bin:$PATH"`。
- 卡片外壳必须与 `ConfigCard` 逐字节同款：`my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm`。
- 键值行沿用 `ConfigCard` 排法：标签列 `w-24 shrink-0 text-fg-muted`，值列 `min-w-0 break-all text-fg`；分组标题 `text-xs text-fg-subtle`。
- 不新增任何 RPC、不改 Rust 侧、不碰 ts-rs 绑定。
- **不引入进度条**：上下文占用保持文字（spec §2 非目标）。
- 提交用 conventional commits，不写 `Co-Authored-By`。
- 每个 commit 前确认 `npx tsc --noEmit` 通过。

---

### Task 1: `UsageReport` 类型与 `buildUsageReport`

**Files:**
- Modify: `desktop/src/lib/usageCommand.ts`（整体改写）
- Modify: `desktop/src/lib/session.ts`（`NoticeBody` 加 `usage` 变体）
- Test: `desktop/src/lib/usageCommand.test.ts`（重写）

**Interfaces:**
- Consumes: 既有 `estimateCost(u: Usage): number | null`、`formatCost(cost: number | null): string`（`desktop/src/lib/pricing.ts`）、`Usage` / `UsageTotals` / `SubagentTotals`（`desktop/src/lib/protocol.ts`）。
- Produces:
  - `interface UsageGroupRow { label: string; value: number }`
  - `interface UsageReport { title: string; model: string; context: { used: number; limit: number | null }; session: { rows: UsageGroupRow[]; isSnapshot: boolean }; subagent: UsageGroupRow[] | null; cost: string }`
  - `function buildUsageReport(snapshot: Usage, totals: UsageTotals | null, subagent: SubagentTotals | null, opts: { compactThreshold: number | null }): UsageReport`
  - `NoticeBody` 第三变体 `{ kind: "usage"; report: UsageReport }`

- [ ] **Step 1: 写失败测试**

把 `desktop/src/lib/usageCommand.test.ts` 整体替换为：

```typescript
import { describe, it, expect } from "vitest";
import { buildUsageReport } from "./usageCommand";

const base = {
  model: "claude-sonnet-4-20250514",
  input: 12_345,
  output: 678,
  cacheRead: 10_000,
  cacheWrite: 0,
};

const totals = { input: 1_234_567, output: 8_900, cacheRead: 2_000_000, cacheWrite: 60 };

describe("buildUsageReport", () => {
  it("reports the session's cumulative counters and names itself as such", () => {
    const r = buildUsageReport(base, totals, null, { compactThreshold: 160_000 });
    expect(r.title).toBe("Token 用量");
    expect(r.model).toBe("claude-sonnet-4-20250514");
    expect(r.session.isSnapshot).toBe(false);
    expect(r.session.rows).toEqual([
      { label: "Input", value: 1_234_567 },
      { label: "Output", value: 8_900 },
      { label: "Cache read", value: 2_000_000 },
      { label: "Cache write", value: 60 },
    ]);
  });

  it("omits the cache write row when it is zero", () => {
    const r = buildUsageReport(base, { ...totals, cacheWrite: 0 }, null, {
      compactThreshold: null,
    });
    expect(r.session.rows.map((x) => x.label)).toEqual(["Input", "Output", "Cache read"]);
  });

  it("falls back to the snapshot and says so in the title", () => {
    const r = buildUsageReport(base, null, null, { compactThreshold: null });
    // 老服务端没有累计值：标题必须主动说明这是「一轮」，不能让人当成会话总量。
    expect(r.title).toBe("本轮用量");
    expect(r.session.isSnapshot).toBe(true);
    // cache write 为 0 时同样不出行：这条规则与口径无关，快照分支也要遵守。
    expect(r.session.rows).toEqual([
      { label: "Input", value: 12_345 },
      { label: "Output", value: 678 },
      { label: "Cache read", value: 10_000 },
    ]);
  });

  it("carries the context limit when it is known", () => {
    const r = buildUsageReport(base, totals, null, { compactThreshold: 160_000 });
    // 分子必须是快照：累计输入量与「当前上下文还剩多少」无关。
    expect(r.context).toEqual({ used: 12_345, limit: 160_000 });
  });

  it("reports a null context limit for an unknown or zero threshold", () => {
    expect(buildUsageReport(base, totals, null, { compactThreshold: null }).context).toEqual({
      used: 12_345,
      limit: null,
    });
    // 0 会让每一轮都触发压缩，不是真实上限，不能当分母。
    expect(buildUsageReport(base, totals, null, { compactThreshold: 0 }).context).toEqual({
      used: 12_345,
      limit: null,
    });
  });

  it("has no subagent group when there are none", () => {
    expect(buildUsageReport(base, totals, null, { compactThreshold: null }).subagent).toBeNull();
    expect(
      buildUsageReport(base, totals, { input: 0, output: 0, cacheRead: 0 }, {
        compactThreshold: null,
      }).subagent,
    ).toBeNull();
  });

  it("lists the three subagent counters", () => {
    const r = buildUsageReport(base, totals, { input: 500, output: 60, cacheRead: 4_000 }, {
      compactThreshold: null,
    });
    expect(r.subagent).toEqual([
      { label: "Input", value: 500 },
      { label: "Output", value: 60 },
      { label: "Cache read", value: 4_000 },
    ]);
  });

  it("estimates the cost from the cumulative counters", () => {
    const r = buildUsageReport(base, totals, null, { compactThreshold: null });
    expect(r.cost).toMatch(/^\$|^< \$/);
  });

  it("prints an em dash for an unpriced model rather than a fake zero", () => {
    const r = buildUsageReport({ ...base, model: "mystery" }, totals, null, {
      compactThreshold: null,
    });
    expect(r.cost).toBe("—");
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/lib/usageCommand.test.ts
```

Expected: FAIL —— `buildUsageReport` 未导出。

- [ ] **Step 3: 实现 `usageCommand.ts`**

整体替换文件内容：

```typescript
/**
 * `/usage` 卡片的数据来源。
 *
 * 纯函数：不碰 React、不碰 RPC。它只把用量数字排成**结构**（分组 → 键值行），
 * 千分位与百分比留给渲染层 —— `/config`、`/help` 也是这么分工的。
 *
 * 口径分两套，**不能混用**：
 * - `totals`（累计）：本会话所有 provider 调用之和，是主体；
 * - `snapshot`（快照）：最后一次调用的用量，只用来算上下文占用。
 *
 * 无累计值时（老服务端）整份报告退回快照，并把标题改成「本轮用量」——卡片有标题
 * 位置，正好用来主动说清这是一轮还是一辈子；文本版没标题，只能靠「不写累计二字」
 * 这种消极方式表达。
 */
import { estimateCost, formatCost } from "./pricing";
import type { SubagentTotals, Usage, UsageTotals } from "./protocol";

/** 一组「标签 + token 数」。`value` 保持原始数字，格式化属于呈现。 */
export interface UsageGroupRow {
  label: string;
  value: number;
}

export interface UsageReport {
  /** `Token 用量`（有累计值）或 `本轮用量`（只有快照）。 */
  title: string;
  model: string;
  /** 上下文占用。`limit` 为 null 时不显示分母与百分比。 */
  context: { used: number; limit: number | null };
  /** 主会话用量。`isSnapshot` 为真表示这份数字是最后一轮而非整个会话。 */
  session: { rows: UsageGroupRow[]; isSnapshot: boolean };
  /** subagent 三项。`null` 表示没有 subagent 用量，卡片不渲染该组。 */
  subagent: UsageGroupRow[] | null;
  /** 估算成本，只含钱数（`"$1.23"` / `"< $0.01"` / `"—"`）。 */
  cost: string;
}

export function buildUsageReport(
  snapshot: Usage,
  totals: UsageTotals | null,
  subagent: SubagentTotals | null,
  opts: { compactThreshold: number | null },
): UsageReport {
  const limit = opts.compactThreshold;
  // 全零与 `null` 同义：都没有 subagent 用量可报。
  const hasSubagents =
    subagent !== null && subagent.input + subagent.output + subagent.cacheRead > 0;

  // 上限未知（老服务端 / 清单没声明 context_length）就不谈占用比例：分母是猜的，
  // 百分比只会误导。0 同理 —— 那会让每一轮都触发压缩，不该被当成真实上限。
  // 分子必须是**快照**：累计值代表整个会话的输入量，与「当前上下文还剩多少」无关。
  const contextLimit = limit !== null && limit > 0 ? limit : null;

  // 主会话四行。cache write 是「写进缓存」的一次性开销，累计后通常远小于
  // cache read；为零时不留死行。
  const sessionRowsFrom = (src: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
  }): UsageGroupRow[] => {
    const rows: UsageGroupRow[] = [
      { label: "Input", value: src.input },
      { label: "Output", value: src.output },
      { label: "Cache read", value: src.cacheRead },
    ];
    if (src.cacheWrite > 0) rows.push({ label: "Cache write", value: src.cacheWrite });
    return rows;
  };

  const session = totals
    ? { rows: sessionRowsFrom(totals), isSnapshot: false }
    : { rows: sessionRowsFrom(snapshot), isSnapshot: true };

  // subagent 只报三项：cache write 在子 agent 侧不采集，不要凭空补一个 0 行。
  const subagentRows: UsageGroupRow[] | null = hasSubagents
    ? [
        { label: "Input", value: subagent.input },
        { label: "Output", value: subagent.output },
        { label: "Cache read", value: subagent.cacheRead },
      ]
    : null;

  // 成本按累计口径估：快照只是一次调用的钱，用户关心的是这一整段对话花了多少。
  // 有 subagent 时这笔钱只算了主会话（子任务跑在独立进程里，模型名没随用量上报），
  // 限定语由渲染层按 `subagent !== null` 补 —— 数据层不替渲染层拼句子。
  const billable: Usage = totals ? { ...snapshot, ...totals } : snapshot;

  return {
    title: totals ? "Token 用量" : "本轮用量",
    model: snapshot.model,
    context: { used: snapshot.input, limit: contextLimit },
    session,
    subagent: subagentRows,
    cost: formatCost(estimateCost(billable)),
  };
}
```

- [ ] **Step 4: 给 `NoticeBody` 加 `usage` 变体**

在 `desktop/src/lib/session.ts` 中，`NoticeBody` 的联合类型加一项，并补文档：

```typescript
/**
 * notice 携带的卡片载荷。
 *
 * - `config`：`/config` 的 `config/read` 原始结果（或读取错误）。
 * - `help`：`/help` 的目录/详情；`error` 非空时先显示错误行（未知目标、未知命令）。
 * - `usage`：`/usage` 的用量报告。数据由 `buildUsageReport` 算好，卡片只摆版。
 */
export type NoticeBody =
  | { kind: "config"; data: Record<string, unknown> | null; error: string | null }
  | { kind: "help"; target: string | null; error: string | null }
  | { kind: "usage"; report: UsageReport };
```

同文件顶部补导入（放在既有 import 之后）：

```typescript
import type { UsageReport } from "./usageCommand";
```

> 注意：`session.ts` 与 `usageCommand.ts` 之间是**类型**依赖，用 `import type`，
> 不产生运行时环（`usageCommand` 只依赖 `pricing` 与 `protocol`）。

- [ ] **Step 5: 运行测试确认通过**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/lib/usageCommand.test.ts
npx tsc --noEmit
```

Expected: 9 passed；tsc 无错误。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/lib/usageCommand.ts desktop/src/lib/usageCommand.test.ts desktop/src/lib/session.ts
git commit -m "refactor(desktop): /usage 产出结构化报告而非多行文本"
```

---

### Task 2: `UsageCard` 组件

**Files:**
- Create: `desktop/src/components/UsageCard.tsx`
- Test: `desktop/src/components/UsageCard.test.tsx`

**Interfaces:**
- Consumes: `UsageReport`、`UsageGroupRow`（Task 1）。
- Produces: `function UsageCard({ report }: { report: UsageReport })`，根节点带 `data-testid="usage-card"`。

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/UsageCard.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach } from "vitest";
import { render, screen, cleanup } from "@testing-library/react";
import { UsageCard } from "./UsageCard";
import type { UsageReport } from "../lib/usageCommand";

afterEach(cleanup);

const sessionRows = [
  { label: "Input", value: 1_234_567 },
  { label: "Output", value: 8_900 },
  { label: "Cache read", value: 2_000_000 },
];

function report(over: Partial<UsageReport> = {}): UsageReport {
  return {
    title: "Token 用量",
    model: "claude-sonnet-4-20250514",
    context: { used: 12_345, limit: 160_000 },
    session: { rows: sessionRows, isSnapshot: false },
    subagent: null,
    cost: "$1.23",
    ...over,
  };
}

describe("UsageCard", () => {
  it("renders the model, the context occupancy and the session rows", () => {
    const { container } = render(<UsageCard report={report()} />);
    expect(container.querySelector('[data-testid="usage-card"]')).toBeTruthy();
    expect(screen.getByText("Token 用量")).toBeTruthy();
    expect(screen.getByText("claude-sonnet-4-20250514")).toBeTruthy();
    // 千分位与百分比是渲染层的职责：数据层只给数字。
    expect(screen.getByText("12,345 / 160,000 (7.7%)")).toBeTruthy();
    expect(screen.getByText("1,234,567")).toBeTruthy();
    expect(screen.getByText("2,000,000")).toBeTruthy();
    expect(screen.getByText("$1.23")).toBeTruthy();
  });

  it("shows no group headings when there are no subagents", () => {
    render(<UsageCard report={report()} />);
    // 无对照对象时「主会话」是空指涉，只会让人问「子会话在哪」。
    expect(screen.queryByText("主会话")).toBeNull();
    expect(screen.queryByText("Subagents")).toBeNull();
  });

  it("splits into session and subagent groups when subagents exist", () => {
    render(
      <UsageCard
        report={report({
          subagent: [
            { label: "Input", value: 500 },
            { label: "Output", value: 60 },
            { label: "Cache read", value: 4_000 },
          ],
        })}
      />,
    );
    // `getByText` 默认精确匹配，「主会话」与成本行的「（仅主会话）」是两个节点，不会撞。
    expect(screen.getByText("主会话")).toBeTruthy();
    expect(screen.getByText("Subagents")).toBeTruthy();
    expect(screen.getByText("500")).toBeTruthy();
    expect(screen.getByText("4,000")).toBeTruthy();
  });

  it("scopes the cost line to the main session when subagents exist", () => {
    render(
      <UsageCard report={report({ subagent: [{ label: "Input", value: 500 }] })} />,
    );
    // 子任务跑在独立进程里、模型名没随用量上报，不标注就会被读成合计。
    expect(screen.getByText("（仅主会话）")).toBeTruthy();
  });

  it("keeps the whole cost line when there are no subagents", () => {
    render(<UsageCard report={report()} />);
    expect(screen.queryByText("（仅主会话）")).toBeNull();
    expect(screen.getByText("$1.23")).toBeTruthy();
  });

  it("says the numbers are one turn when there are no cumulative totals", () => {
    render(
      <UsageCard
        report={report({ title: "本轮用量", session: { rows: sessionRows, isSnapshot: true } })}
      />,
    );
    expect(screen.getByText("本轮用量")).toBeTruthy();
  });

  it("omits the denominator when the context limit is unknown", () => {
    render(<UsageCard report={report({ context: { used: 12_345, limit: null } })} />);
    expect(screen.getByText("12,345")).toBeTruthy();
    // 用函数匹配器：字符串参数是字面量，`/%/` 永远匹配不到任何节点。
    expect(screen.queryByText((t) => t.includes("%"))).toBeNull();
  });

  it("renders a cache write row when the data carries one", () => {
    render(
      <UsageCard
        report={report({
          session: {
            rows: [...sessionRows, { label: "Cache write", value: 60 }],
            isSnapshot: false,
          },
        })}
      />,
    );
    expect(screen.getByText("Cache write")).toBeTruthy();
    expect(screen.getByText("60")).toBeTruthy();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/components/UsageCard.test.tsx
```

Expected: FAIL —— 模块不存在。

- [ ] **Step 3: 实现 `UsageCard.tsx`**

```tsx
/**
 * `/usage` 的输出卡片。
 *
 * 取代此前把 `formatUsage()` 的六行等宽文本塞进 notice 胶囊的做法：那个容器
 * 没有 `whitespace-pre-wrap`，换行被 CSS 折叠，六行压成一行居中灰字。外壳与
 * `ConfigCard` / `HelpCard` 同一套视觉语言，三个多行命令的输出因此长得一样。
 *
 * 分两组：主会话是「主 Agent 自己」，subagent 是它派出去的子任务。只有存在
 * subagent 时才给两个分组加标题 —— 没有对照对象时「主会话」是空指涉。
 */
import type { UsageGroupRow, UsageReport } from "../lib/usageCommand";

/** 千分位。卡片里数字要能一眼扫读，`12,345` 比 `12345` 容易。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

/** 上下文占用一行：上限未知时只给已用值，不编分母。 */
function contextText(c: UsageReport["context"]): string {
  if (c.limit === null) return thousands(c.used);
  return `${thousands(c.used)} / ${thousands(c.limit)} (${((c.used / c.limit) * 100).toFixed(1)}%)`;
}

function Rows({ rows }: { rows: UsageGroupRow[] }) {
  return (
    <>
      {rows.map((row) => (
        <div key={row.label} className="flex gap-3">
          <span className="w-24 shrink-0 text-fg-muted">{row.label}</span>
          <span className="min-w-0 break-all text-fg">{thousands(row.value)}</span>
        </div>
      ))}
    </>
  );
}

export function UsageCard({ report }: { report: UsageReport }) {
  const grouped = report.subagent !== null;
  return (
    <div
      data-testid="usage-card"
      className="my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm"
    >
      <div className="mb-1 font-medium text-fg">{report.title}</div>
      <div className="flex flex-col gap-2">
        <div className="flex flex-col gap-0.5">
          <div className="flex gap-3">
            <span className="w-24 shrink-0 text-fg-muted">模型</span>
            <span className="min-w-0 break-all text-fg">{report.model}</span>
          </div>
          <div className="flex gap-3">
            <span className="w-24 shrink-0 text-fg-muted">上下文</span>
            <span className="min-w-0 break-all text-fg">{contextText(report.context)}</span>
          </div>
        </div>

        <div className="flex flex-col gap-0.5">
          {grouped && <div className="text-xs text-fg-subtle">主会话</div>}
          <Rows rows={report.session.rows} />
        </div>

        {report.subagent !== null && (
          <div className="flex flex-col gap-0.5">
            <div className="text-xs text-fg-subtle">Subagents</div>
            <Rows rows={report.subagent} />
          </div>
        )}

        <div className="flex gap-3">
          <span className="w-24 shrink-0 text-fg-muted">估算成本</span>
          <span className="min-w-0 break-all text-fg">
            {report.cost}
            {/*
             * 成本只按主会话估：subagent 用的是哪个模型这次没有采集（子任务在
             * 独立 daemon 进程里，模型名没随用量上报）。不加限定语会被读成合计。
             */}
            {grouped && <span className="text-fg-muted">（仅主会话）</span>}
          </span>
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/components/UsageCard.test.tsx
npx tsc --noEmit
```

Expected: 8 passed；tsc 无错误。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/UsageCard.tsx desktop/src/components/UsageCard.test.tsx
git commit -m "feat(desktop): /usage 输出改为卡片"
```

---

### Task 3: 接线（`ChatView` 分派 + `App` 调用点）

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`（notice 分支）
- Modify: `desktop/src/App.tsx`（`case "usage"`）
- Test: `desktop/src/components/ChatView.test.tsx`（补一条）、`desktop/src/App.test.tsx`（改既有 `/usage` 用例）

**Interfaces:**
- Consumes: `UsageCard`（Task 2）、`buildUsageReport`（Task 1）、`NoticeBody` 的 `usage` 变体（Task 1）。
- Produces: 用户可见的 `/usage` 卡片；无新增导出。

- [ ] **Step 1: 写失败测试（ChatView 分派）**

在 `desktop/src/components/ChatView.test.tsx` 里补一条用例（放在既有 config/help 卡片用例旁边，沿用该文件已有的渲染辅助函数 —— 先读文件确认辅助函数名，例如 `renderChat(...)` 或直接 `render(<ChatView .../>)`，**照抄同文件里现有卡片用例的写法**）：

```tsx
it("renders a usage card for a usage notice body", () => {
  // 结构化载荷才是 /usage 的呈现；普通胶囊会把卡片内容压成一行灰字。
  renderNotice({
    type: "notice",
    id: "n-usage",
    text: "",
    body: {
      kind: "usage",
      report: {
        title: "Token 用量",
        model: "claude-sonnet-4-20250514",
        context: { used: 100, limit: null },
        session: { rows: [{ label: "Input", value: 100 }], isSnapshot: false },
        subagent: null,
        cost: "$0.01",
      },
    },
  });
  expect(screen.getByTestId("usage-card")).toBeTruthy();
  expect(screen.queryByTestId("notice")).toBeNull();
});
```

> 若该文件没有 `renderNotice` 这类辅助，就用文件里现成的 items 注入方式
> （构造 `ChatView` 所需的 `items` prop）。**以文件实际写法为准，不要新造辅助。**

- [ ] **Step 2: 运行测试确认失败**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/components/ChatView.test.tsx
```

Expected: FAIL —— `usage-card` 未渲染（落到普通胶囊分支）。

- [ ] **Step 3: 在 `ChatView` 加分派**

把 `desktop/src/components/ChatView.tsx` 里 notice 的三元链改成带 `usage` 的版本：

```tsx
        case "notice":
        // 结构化载荷按 `kind` 分派；没有载荷的就是普通文本提示（胶囊）。
        return item.body?.kind === "config" ? (
          <ConfigCard config={item.body.data} error={item.body.error} />
        ) : item.body?.kind === "help" ? (
          <HelpCard target={item.body.target} error={item.body.error} />
        ) : item.body?.kind === "usage" ? (
          <UsageCard report={item.body.report} />
        ) : (
```

并补导入（与 `ConfigCard` / `HelpCard` 同级，按字母序放到 `UsagePanel` 之前即可）：

```tsx
import { UsageCard } from "./UsageCard";
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/components/ChatView.test.tsx
```

Expected: PASS（含新用例）。

- [ ] **Step 5: 写失败测试（App 调用点）**

在 `desktop/src/App.test.tsx` 找到既有断言 `/usage` 文本输出的用例（搜索 `累计 input` 或 `formatUsage` 或 `"模型: "`），把它改为断言卡片：

```tsx
    // /usage 的输出是卡片，不是一段灰字：notice 容器没有 pre-wrap，
    // 多行文本会被折叠成一行。
    expect(document.querySelector('[data-testid="usage-card"]')).toBeTruthy();
    expect(screen.getByText("Token 用量")).toBeTruthy();
```

**先读该用例的原有断言再改**：只替换与「文本内容」相关的部分，保留它原有的 RPC mock 与等待逻辑。

- [ ] **Step 6: 改 `App` 的 `/usage` 分支**

`desktop/src/App.tsx` 里 `case "usage"` 的末尾：

```tsx
        // 累计值是 `/usage` 的主体；没有（老服务端）时报告内部退回快照并改标题。
        // subagent 用量单独成组：它与主会话是两笔账，混在一起看不出谁花得多。
        session.noticeBody({
          kind: "usage",
          report: buildUsageReport(u, session.usageTotals, session.subagentTotals, {
            compactThreshold: threshold,
          }),
        });
        break;
```

导入改为：

```tsx
import { buildUsageReport } from "./lib/usageCommand";
```

（原来导入的是 `formatUsage`，**删掉它**，不要留未使用的导入。）

- [ ] **Step 7: 运行测试确认通过**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx vitest run src/App.test.tsx src/lib/usageCommand.test.ts src/components/UsageCard.test.tsx src/components/ChatView.test.tsx
npx tsc --noEmit
```

Expected: 全绿；tsc 无错误。

- [ ] **Step 8: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "feat(desktop): /usage 走 noticeBody 渲染卡片"
```

---

### Task 4: 全量校验与文档登记

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`（计数）

- [ ] **Step 1: 全量测试**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
npx tsc --noEmit
npx vitest run
```

Expected: 全绿，无失败文件。

- [ ] **Step 2: 确认没有残留的 `formatUsage`**

```bash
grep -rn "formatUsage" desktop/src || echo "NO_LEFTOVER"
```

Expected: `NO_LEFTOVER`（文本版本已随 Task 1 删除，调用点随 Task 3 迁移）。

- [ ] **Step 3: 登记到 `desktop.md`**

在 `docs/project-management/desktop.md` 的 Features 列表里，"`/usage` 取代 `/cost`" 那条之后，追加一条 `[x]`：

```markdown
- [x] `/usage` 输出改为卡片格式 — 用户报「/usage 出来的内容实在是有点难看」。根因是**排版坏了而非审美问题**：`formatUsage` 产出用 `\n` 连接的 6–7 行文本，而 notice 胶囊容器（`desktop/src/components/ChatView.tsx` 的 `notice` 分支）只有 `my-1 self-center rounded-md bg-raised/60 px-3 py-1 text-xs text-fg-muted`、**没有 `whitespace-pre-wrap`**，HTML 折叠换行后六行压成一行居中灰字（行内双空格同样被折叠）；`/config` 与 `/help` 早已通过 `NoticeBody` 走结构化卡片，`/usage` 是三个多行命令里唯一漏掉的。修复沿用同一机制：`NoticeBody` 增加第三变体 `{ kind: "usage"; report: UsageReport }`（`desktop/src/lib/session.ts`）；`usageCommand.ts` 的 `formatUsage`（拼字符串）改为 `buildUsageReport`（产出结构：title / model / context / session.rows / subagent / cost，千分位与百分比交给渲染层，数据层不替渲染层拼句子）；新增 `desktop/src/components/UsageCard.tsx`（外壳与 `ConfigCard` 逐字节同款 `my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm`，标签列 `w-24` / 值列 `min-w-0 break-all`）；`ChatView` 按 `kind` 分派；`App` 的 `case "usage"` 改走 `session.noticeBody`。呈现规则：有累计值标题为 `Token 用量`、无累计值（老服务端）为 `本轮用量`（卡片有标题位正好主动说明是一轮而非总账）；`cacheWrite === 0` 不出该行；有 subagent 时出现 `主会话` / `Subagents` 两个分组标题且成本行追加「（仅主会话）」（子任务跑在独立 daemon 进程里、模型名没随用量上报，不标注会被读成合计），subagent 全零或缺失时两个标题都不出（无对照对象时「主会话」是空指涉）；`compactThreshold` 未知或为 0 时上下文只显示已用值，无分母无百分比。**不做进度条**：`/usage` 是按需查看而非常驻监控，项目里也还没有进度条组件。文本版本删除、只留结构化报告（唯一调用点是 App，留两套渲染必然漂移）。判据：`cd desktop && npx tsc --noEmit && npx vitest run`（全绿，含 `UsageCard.test.tsx` 8 例与 `buildUsageReport` 9 例）；`grep -rn formatUsage desktop/src` 无输出 — [设计](../superpowers/specs/2026-10-25-usage-card-design.md)、[计划](../superpowers/plans/2026-10-25-usage-card.md)
```

- [ ] **Step 4: 更新 README 计数**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/usage-card
grep -c '^- \[x\]' docs/project-management/desktop.md
grep -c '^- \[' docs/project-management/desktop.md
```

用这两个实测值更新 `docs/project-management/README.md` 的 desktop 行（格式 `| desktop | <x> / <total> | [详情](./desktop.md) |`）。

- [ ] **Step 5: 勾选本计划的复选框并提交**

```bash
sed -i '' 's/^- \[ \] \*\*Step/- [x] **Step/' docs/superpowers/plans/2026-10-25-usage-card.md
git add docs
git commit -m "docs: 登记 /usage 卡片化"
```

---

## Self-Review

**Spec 覆盖检查：**

| Spec 要求 | 任务 |
| --- | --- |
| §3.1 `NoticeBody` 加 `usage` 变体 | Task 1 Step 4 |
| §3.2 `UsageReport` 结构与 `buildUsageReport` | Task 1 Step 3 |
| §3.3 卡片布局与条件规则 | Task 2 Step 3 |
| §3.4 调用点改走 `noticeBody` | Task 3 Step 6 |
| §3.5 删除文本版本 | Task 1 Step 3 + Task 3 Step 6 |
| §4 影响面（6 个文件 + 3 个测试） | Task 1–3 |
| §5 测试（纯函数 / 组件 / 分派 / App） | Task 1 Step 1、Task 2 Step 1、Task 3 Step 1/5 |
| §6 验证命令 | Task 4 Step 1 |

**占位符扫描：** 无 TBD / TODO。Task 3 Step 1 与 Step 5 要求「先读文件确认辅助函数名」——
这是因为该文件的既有测试辅助不在本计划可预先得知的范围内，但**已给出判定标准**
（照抄同文件现有卡片用例的写法、只替换文本断言部分），不是「自己看着办」。

**类型一致性：**
- `UsageReport` 字段名在 Task 1 定义、Task 2 消费、Task 3 的测试字面量里三处一致
  （`title` / `model` / `context` / `session` / `subagent` / `cost`）。
- `UsageGroupRow` 的 `label` / `value` 两处一致。
- 函数名只有一个：`buildUsageReport`（Task 1 定义，Task 3 调用）。全计划无 `formatUsage` 残留引用。

**已知取舍（来自 spec §7，此处不重复论证）：** 删文本版不预置 `formatUsageText`；
不做进度条；分组标题条件渲染。
