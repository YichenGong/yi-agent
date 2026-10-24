# `/usage` 输出改为卡片格式

日期：2026-10-25
状态：已确认，待实施

## 1. 问题

用户反馈：「现在 `/usage` 出来的内容实在是有点难看，应该用卡片格式去格式化。」

### 1.1 根因不是「不够好看」，是排版坏了

`formatUsage()`（`desktop/src/lib/usageCommand.ts`）返回一段用 `"\n"` 连接的多行
文本（6–7 行）。调用点把它交给 `session.notice(...)`，而 notice 的渲染容器
（`desktop/src/components/ChatView.tsx` 的 `notice` 分支）只有
`my-1 self-center rounded-md bg-raised/60 px-3 py-1 text-xs text-fg-muted`，
**没有 `whitespace-pre-wrap`**。

HTML 折叠连续空白（含换行），于是：

- 六行被压成一行；
- `self-center` 让这行灰字居中；
- 每行内部的 `input: 1,234  output: 56` 双空格同样被折叠成单空格。

用户看到的是一条又长又居中的灰色文本 —— 信息全在，但完全无法扫读。

### 1.2 同一层早有正确做法

`/config` 与 `/help` 都通过 `NoticeBody` 判别联合走结构化卡片
（`desktop/src/components/ConfigCard.tsx`、`HelpCard.tsx`），外壳统一是
`my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm`。
`/usage` 是三个多行命令里唯一没跟上的一条。

## 2. 目标与非目标

**目标**

1. `/usage` 输出渲染成与 `/config`、`/help` 同一套视觉语言的卡片。
2. 不改变 `/usage` 承载的**信息**：模型、上下文占用、input/output/cache
   read/cache write、subagent 三项、估算成本，一项不少。
3. 保持「无累计值时明确表达这是一轮而不是整个会话」的既有语义，并且比文本版
   表达得更主动。

**非目标**

- 不改数据来源、不改 RPC、不改 `Session.usageTotals` / `subagentTotals` 的维护。
- 不改状态栏与展开面板（那是另一条展示路径，刚在
  `2026-10-24-subagent-token-usage-design.md` 里定稿）。
- 不加进度条。上下文占用保持文字表达：`/usage` 是按需查看而非常驻监控，
  项目里也还没有进度条组件，为一个卡片新增配色逻辑不划算。

## 3. 设计

### 3.1 数据：`NoticeBody` 增加 `usage` 变体

```ts
// desktop/src/lib/session.ts
export type NoticeBody =
  | { kind: "config"; data: Record<string, unknown> | null; error: string | null }
  | { kind: "help"; target: string | null; error: string | null }
  | { kind: "usage"; report: UsageReport };
```

与 `config` / `help` 一样，是**纯数据**载荷：渲染层不重新算钱、不重新拼字符串，
只摆版。判别联合而非并列可选字段，理由同既有注释 —— 一张卡片只有一种载荷。

### 3.2 `UsageReport`：`usageCommand.ts` 产出的结构化报告

```ts
export interface UsageGroupRow {
  label: string;   // "Input" / "Output" / "Cache read" / "Cache write"
  value: number;   // 原始 token 数，千分位由渲染层负责
}

export interface UsageReport {
  /** "Token 用量"（有累计值）或 "本轮用量"（只有快照）。 */
  title: string;
  model: string;
  /** 上下文占用。`limit` 为 null 时不显示分母与百分比。 */
  context: { used: number; limit: number | null };
  /**
   * 主会话用量。`isSnapshot` 为真表示 `totals` 缺失（老服务端），
   * 这份数字是最后一轮而非整个会话。
   */
  session: { rows: UsageGroupRow[]; isSnapshot: boolean };
  /** subagent 三项。`null` 表示没有 subagent 用量，卡片不渲染该组。 */
  subagent: UsageGroupRow[] | null;
  /**
   * 估算成本，只含钱数本身（`"$1.23"`），模型未知时是 `"—"`。
   * 「（主会话，不含 subagent）」这句限定语**不在这里**：它是呈现层对
   * `subagent !== null` 的读法，数据层不该替渲染层拼句子。
   */
  cost: string;
}
```

**为什么 `rows` 是数组而不是四个具名字段**：`cache write` 在为零时不出行（现有
行为），数组天然表达「这一行今天不存在」，无需渲染层再判空。**为什么值保持
number**：千分位格式化属于呈现，放在渲染层，纯数据层不掺表示。

### 3.3 卡片布局

`UsageCard.tsx` 外壳与 `ConfigCard` 逐字节同款：

```
┌────────────────────────────────────────────┐
│ Token 用量                                  │
│   模型       claude-sonnet-4-20250514       │
│   上下文     12,345 / 160,000 (7.7%)        │
│                                             │
│   主会话                                     │
│     Input          1,234,567                │
│     Output             8,900                │
│     Cache read     2,000,000                │
│                                             │
│   Subagents                                 │
│     Input                500                │
│     Output                60                │
│     Cache read         4,000                │
│                                             │
│   估算成本   $1.23（主会话，不含 subagent）   │
└────────────────────────────────────────────┘
```

规则（全部来自既有语义，不新增判断）：

| 条件 | 表现 |
| --- | --- |
| 有累计值 | 标题 `Token 用量`；无分组标题，直接 `Input / Output / Cache read` |
| 无累计值（老服务端） | 标题 `本轮用量` —— 主动说明这是一轮 |
| `cacheWrite === 0` | 不出 `Cache write` 行（现有行为） |
| 有 subagent | 出现 `主会话` 与 `Subagents` 两个分组标题；成本行追加上限定语 |
| subagent 全零或 `null` | 不出现 `Subagents` 分组，也不出现 `主会话` 标题（无对照即无区分意义） |
| `limit === null` 或 `<= 0` | 上下文只显示 `12,345`，无分母、无百分比 |
| 模型未知（`estimateCost` 返回 null） | `cost` 为 `—`，照常显示（不隐藏这一行） |

键值行沿用 `ConfigCard` 的排法：标签列 `w-24 shrink-0 text-fg-muted`，
值列 `min-w-0 break-all text-fg`；分组标题 `text-xs text-fg-subtle`。

**分组标题的条件性**是本次唯一新增的呈现判断，理由写进代码注释：`主会话` 这个
限定只有在**同时**列出 subagent 时才有对照对象；无 subagent 时它是个空指涉，
只会让人问「那子会话在哪」。

### 3.4 调用点

`desktop/src/App.tsx` 的 `case "usage"`：

```ts
session.noticeBody({ kind: "usage", report: buildUsageReport(u, session.usageTotals, session.subagentTotals, { compactThreshold: threshold }) });
```

`session.notice(...)` 的那条调用被替换。App 的「本会话还没有用量记录」分支不变。

### 3.5 删除文本版本

`formatUsage` 删除，只保留 `buildUsageReport`。

理由：唯一调用点是 App；留两套渲染（文本 + 卡片）必然漂移，而文本版没有第二个
消费者（TUI 有自己的 `HistoryCell`）。测试全部迁到 `UsageReport` 结构断言。

## 4. 影响面

| 文件 | 变更 |
| --- | --- |
| `desktop/src/lib/session.ts` | `NoticeBody` 加 `usage` 变体；`UsageReport` 类型 |
| `desktop/src/lib/usageCommand.ts` | `formatUsage` → `buildUsageReport`，返回结构而非文本 |
| `desktop/src/components/UsageCard.tsx` | 新增 |
| `desktop/src/components/ChatView.tsx` | `notice` 分支加 `usage` 分派 |
| `desktop/src/App.tsx` | `/usage` 分支改走 `noticeBody` |
| `desktop/src/lib/usageCommand.test.ts` | 改为结构断言 |
| `desktop/src/components/UsageCard.test.tsx` | 新增 |
| `desktop/src/components/ChatView.test.tsx` | 补分派用例 |

**不受影响**：Rust 侧全部、ts-rs 绑定、状态栏与 `UsagePanel`。

## 5. 测试

**`usageCommand.test.ts`**（纯函数，无 DOM）— 四类：

1. 有累计值：`session.isSnapshot === false`、title 为 `Token 用量`、rows 含
   Input/Output/Cache read、cacheWrite 为 0 时无该行。
2. 无累计值：`isSnapshot === true`、title 为 `本轮用量`、rows 取快照值。
3. 上下文：`limit` 已知时 `{used, limit}` 原样透出；`limit` 为 `null`/`0` 时为 null。
4. subagent：`null` 与全零都给 `subagent: null`；非零时三行。
   （成本文案里的限定语属于渲染层，不在这一层断言。）

百分比与千分位**不在这一层断言**（它们是渲染层的职责）。

**`UsageCard.test.tsx`**（DOM）— 五类：

1. 有累计值无 subagent：`data-testid="usage-card"`、标题 `Token 用量`、无 `主会话`
   / `Subagents` 标题、有四行键值（模型/上下文/Input/Output/Cache read 等）。
2. 有 subagent：出现两个分组标题，subagent 三个数字都在，成本行带限定语。
3. 无累计值：标题 `本轮用量`。
4. `limit === null`：上下文行只有数字，不含 `%`、不含 `/`。
5. `cacheWrite > 0`：出现 `Cache write` 行。

**`ChatView.test.tsx`**：带 `body.kind === "usage"` 的 notice 渲染出
`usage-card`，而不是普通胶囊。

**`App.test.tsx`**：`/usage` 跑完后会话里出现 `usage-card`（替换原先断言文本的
用例）。

## 6. 验证命令

```bash
cd desktop
npx tsc --noEmit
npx vitest run src/lib/usageCommand.test.ts src/components/UsageCard.test.tsx \
               src/components/ChatView.test.tsx src/App.test.tsx
npx vitest run   # 全量
```

## 7. 已知取舍

- **删掉文本版**：若将来有「复制用量为文本」的需求，需要重新加一个
  `formatUsageText`。当前没有任何消费者，不预置。
- **不做进度条**：上下文占用保持文字。若日后要转成常驻监控，再单独立项。
- **分组标题条件渲染**：无 subagent 时卡片少两个标题，与 `/config` 那种「固定
  分组」不完全一致。这是刻意的 —— 空分组比少一个标题更让人困惑。
