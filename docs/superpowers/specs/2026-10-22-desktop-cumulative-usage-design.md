# 桌面端 `/usage` 与右下角用量改为「会话累计」

**日期：** 2026-10-22
**状态：** 设计已确认，待实现

## 1. 问题

桌面端现在展示的是**最后一轮、最后一次 provider 调用**的用量快照：

- `desktop/src/lib/session.ts` 的 `usage` 只由 `thread/tokenUsage/updated` 覆盖，
  而该通知的正文来自 `Translator::on_event` 里 `UsageSnapshot::merge`
  （`yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:52`）——`input_tokens > 0`
  判定为「新的一次调用」并**整体替换**，故每条通知都是「本轮最后一次调用」的快照。
- 落盘同理：`server.rs` 的 driver 用最后一条通知覆盖 `last_usage`，写进 jsonl 的
  `usage` 也只代表那次调用（`persist_and_finish_turn` → `TurnLine::Turn.usage`）。

于是：

1. 一个含 10 次工具调用的 turn，输入 token 实际被请求了 11 次，而界面只显示最后一次
   ——用户看到的 `in` 不是「这个会话花了多少输入」，而是「最后一次请求的上下文长度」。
2. 跑完一轮数字就停，下一轮开始又整体跳变，没有「一直在涨」的累计观感。
3. 缓存命中量（`cache_read_input_tokens`）恰恰是省钱的部分，却只反映最后一次调用。

TUI 的 `/cost` 早已是累计值（`tui/cost.rs` 按每个 `AgentEvent::Usage` 累加），桌面端
与它口径不一致。

## 2. 目标

- `/usage` 与右下角状态栏统计的是**该会话生命周期内的累计用量**：
  累计 input、累计 cache read、累计 output（cache write 仅计入成本与总量，默认不展示）。
- **实时**：每发生一次 provider 调用，数字就增长一次——Anthropic 的
  `message_start`（input/cache）与 `message_delta`（output）到达时各自增长。
- 冷会话（进程重启后）打开时，累计值从已落盘的历史恢复；只有改造前的老轮次会沿用
  「最后一次调用」近似值（可接受，见 §7）。
- 上下文占用继续按**最后一次调用**的 `input` 计算，不受累计值影响。

## 3. 语义定义

累计值分三段拼成，任何时刻展示都是三段之和：

| 段 | 含义 | 生命周期 |
| --- | --- | --- |
| `committed` | 本次 driver 启动前已落盘轮次之和 | 进程内不变（`/clear` 除外） |
| `turn` | 本轮已结束的调用之和 | 每个 turn 清零 |
| `snapshot` | 当前正在进行的这一次调用 | 每次调用整体替换 |

**调用边界**沿用既有判据（`UsageSnapshot::merge`）：`input_tokens > 0` 表示新调用开始。
新调用开始时把 `snapshot` 结算进 `turn`，再以新值覆盖 `snapshot`；`input_tokens == 0`
的事件（Anthropic `message_delta` 只带 output）按字段合并进 `snapshot`。

展示值恒为 `committed + turn + snapshot`，因此：

- Anthropic：`message_start` 到达 → 累计 input / cache read 各涨一次；`message_delta`
  到达 → 累计 output 涨一次。
- OpenAI（单条合并事件）：调用结束时三段合一，累计值一次性涨到位。
- Turn 收尾时 `turn += snapshot`，并把 `turn` 落盘为 `usage_total`、并入 `committed`。

`/clear` 截断日志 ⇒ 三段全部清零，并推一条全零的 `thread/tokenUsage/updated`，保证
客户端与服务端、重启前后一致。

`/compact` 内部生成摘要的那次 provider 调用**不计入**（与 TUI `/cost` 一致）。

## 4. 后端改动（`yi-agent-app-server`）

### 4.1 落盘格式

`TurnLine::Turn` 新增字段（`#[serde(default, skip_serializing_if = "Option::is_none")]`）：

```rust
usage_total: Option<TurnUsage>,   // 本轮所有调用之和；model 取本轮最后一次调用的模型
```

`usage` 保持原义（最后一次调用快照，供上下文占用与 auto-compact 恢复），**不改**。
老日志缺 `usage_total` 时，累加退化为该行的 `usage`。

### 4.2 读取侧

- `ThreadStore::read_log` 除 `(items, messages, usage)` 外再返回 `totals: UsageTotals`
  （逐轮 `usage_total`，缺失回退 `usage`，跨轮相加）。
- `LoadedThread` 新增 `totals: UsageTotals`，`load` 的 partial 合流分支同样把
  `partial.usage_total` 计入（缺失回退 `partial.usage`）。
- `thread/readItems` 响应新增 `"totals"` 字段（与 `usage` 并列，老客户端忽略）。
- `thread/resume` 回放的 `TokenUsage` 通知携带持久化 totals（见 4.3）。

`UsageTotals` 为 `u64` 计数（`input / output / cache_creation / cache_read`），避免
`u32` 在长会话里溢出。

### 4.3 通知

`Notification::TokenUsage` 在既有「本轮快照」字段之外新增：

```rust
total_input_tokens: u64,
total_output_tokens: u64,
total_cache_creation_input_tokens: u64,
total_cache_read_input_tokens: u64,
```

既有四个字段（`input_tokens` 等）语义不变。改完后必须重新生成 ts-rs 绑定
（`just bindings-check` 会校验）。

### 4.4 Translator

`Translator` 新增：

```rust
totals: UsageTotals,       // committed + turn（已结束的调用）
snapshot: UsageSnapshot,   // 当前调用（复用现有结构）
```

- `Translator::new(thread_id, baseline: UsageTotals)`：driver 启动时用
  `store.load()` 的 `totals` 播种。`thread/start` 的新会话为全零。
- `set_turn` 把 `turn` 并入 `committed`，并清空 `snapshot`（保留既有「本轮重置」语义）。
  实现上可让 `totals` 表示 committed，另用 `turn_totals` 表示本轮——见 4.5 的落盘取值。
- `on_event(Usage)`：按 §3 结算后发通知，四类总量取 `committed + turn + snapshot`。
- 新增 `reset_totals()`：`/clear` 后清零并发一条全零通知。

### 4.5 落盘

`persist_and_finish_turn` 新增入参 `usage_total: Option<TurnUsage>`（本轮 `turn` 之和，
含最后一次调用的 `snapshot` 结算），与 `last_usage` 一并写入本轮记录。
`usage_total` 的 `model` 取本轮最后一次调用所用模型。

### 4.6 清理

`apply_session_command` 处理 `SessionCommand::Clear` 后，driver 需调用
`translator.reset_totals()`。实现方式：调用前用 `matches!(&command, ..Clear{..})`
记下类型，调用后按需重置——`apply_session_command` 消费了 command，不能在内部回传。

## 5. 桌面端改动

### 5.1 状态与类型

- `desktop/src/lib/protocol.ts` 新增 `UsageTotals`（camelCase：`input / output /
  cacheRead / cacheWrite`）与 `WireTotals`（snake_case）+ `wireTotalsToTotals`
  纯映射；`Usage` 类型保持不变。
- `Session` 新增 `usageTotals: UsageTotals | null`：
  - `thread/tokenUsage/updated` 同时更新 `usage`（快照）与 `usageTotals`；
  - `reset()` 置 `null`；
  - 新增 `adoptPersistedTotals(t)`，规则同 `adoptPersistedUsage`：本地已有 live 值
    时不覆盖；
  - `App.tsx` 两处 `thread/readItems` 调用点接 `r.adoptPersistedTotals(wireTotalsToTotals(r.totals))`。
- 老服务端不带 `totals` 时 `usageTotals` 为 `null`，状态栏回退用 `usage` 快照显示
  （与今日行为一致），不做第二种文案。

### 5.2 `/usage` 正文（`desktop/src/lib/usageCommand.ts`）

```
模型: claude-sonnet-4
累计 input: 1,234,567  output: 45,678
累计 cache read: 1,200,000
累计 tokens: 2,500,245
上下文: 142,330 / 160,000 (89.0%)
估算成本: $3.2901
```

- `formatUsage` 签名改为 `(snapshot: Usage, totals: UsageTotals, opts)`：累计行取
  totals，上下文行取 snapshot（`snapshot.input`，不能用累计值——历史请求量之和会
  远超上下文上限，百分比没有意义）。
- `累计 tokens` = input + cacheRead + cacheWrite + output（cache write 计入总量与
  成本，只是不单独展示）。
- 成本用累计 totals 估算，沿用最新模型定价；未知模型显示 `—`。
- `cache write` 若累计非零，在 `累计 cache read` 之后补一行
  `累计 cache write: N`；为零则整行不出现（用户只关注 cache read）。
- 阈值未知或为 0 时上下文行只显示 `上下文: 142,330`（既有行为）。

### 5.3 右下角状态栏

- `StatusBar` 的 `usage` prop 改为 `totals: UsageTotals | null`，折叠态三组数字：

```
12.3k in / 120.0k cache / 678 out
```

- 新增纯函数 `desktop/src/lib/compactNumber.ts`：
  `compact(n) -> { digits: number, fractionDigits: number, unit: "" | "k" | "M" | "G" }`
  —— `< 1000` 原值；`< 1e6` 用 k、1 位小数；`< 1e9` 用 M、2 位小数；否则 G、2 位小数。
  四舍五入后若跨过下一个阈值（如 999,950 → `1000.0k`），改用上一级单位。
- `RollingNumber` 新增可选 `fractionDigits`（默认 0）与 `unit`（默认空）：
  实际滚动的是放大 `10^fractionDigits` 后的整数，渲染时在末 `fractionDigits` 位前
  插入小数点，单位紧随其后且不参与滚动。默认参数下 DOM 与今日逐字节一致。
- `sr-only` 文案改为完整精确值：
  `1,234,567 input / 1,200,000 cache read / 45,678 output tokens`（用 `Intl.NumberFormat`）。
- 展开面板 `UsagePanel` 标题改为 `Usage (this session)`，行改为累计
  Input / Output / Cache read（+ 非零时 Cache write）/ Total / Model / Est. cost。

### 5.4 切换会话

沿用 `usageId`：会话切换时数字直接跳到终值，不从上一个会话滚过来。

## 6. 测试

**Rust**

- `thread_store`：`usage_total` 序列化往返；`read_log` 跨轮求和；老行（无
  `usage_total`）回退到 `usage`；空 usage 行不改变 totals。
- `translate`：一次调用拆成两个事件时 totals 在 `message_start` 后已含 input；
  第二次调用开始时上一次快照被结算（不重复、不丢失）；OpenAI 单事件路径 totals
  立即包含该次调用；`set_turn` 后 totals 保留已结束轮次；`reset_totals` 归零。
- `rpc_tests`：多步 turn 落盘后 `readItems.totals.input` 大于单次调用的
  `usage.input`（证明是累计而非最后快照）；`thread/clear` 后 totals 归零。

**TypeScript**

- `compactNumber.test.ts`：阈值、四舍五入进位、单位选择。
- `RollingNumber.test.tsx`：`fractionDigits`/`unit` 的小数点位置与默认零回归。
- `session.test.ts`：通知更新 totals；`adoptPersistedTotals` 不覆盖 live 值。
- `usageCommand.test.ts`：累计行、cache write 为零时隐藏、上下文行用快照、
  未知模型成本 `—`。
- `StatusBar.test.tsx`：三组数字与 `sr-only` 全文。

## 7. 已知取舍

- **老轮次精度**：改造前的 jsonl 每轮只存最后一次调用，历史累计会偏小（可能数倍）。
  无法回溯，接受。新轮次起落盘 `usage_total`，精度恢复。
- **重试**：provider 重试会重新计费，多次失败尝试的用量都会累加。这是真实消耗，
  不做去重。
- **跨模型**：totals 只记一个 model（最后一次调用所用），成本估算按它套算；会话中途
  换模型时成本是近似值（面板文案即为「估算成本」）。
- **TUI 不在本次范围**：它的 `/cost` 已经是按事件累计的正确值。

## 8. 文档

- 新增本 spec。
- 更新 `docs/project-management/desktop.md`：`/usage` 与状态栏两条验收项改为累计口径，
  并给出可执行判据。
