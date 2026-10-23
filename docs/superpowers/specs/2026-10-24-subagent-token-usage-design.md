# 会话内 subagent token 用量统计

**日期**：2026-10-24
**状态**：设计待审

## 1. 目标

一个会话运行过程中，能看到该会话里 **subagent 烧掉的 token 累计值**，与主会话自身的用
量**分开**呈现。

用户明确的三处形态：

1. **状态栏折叠态**（右下角）：显示**合计值**——主会话 + 该会话所有 subagent。
2. **状态栏展开面板**：把「主会话」与「subagent」拆成两组分别显示。
3. **`/usage` 文字输出**：主会话值与 subagent 统计值**分开**成段。

用户明确的统计口径：

- subagent 只关注 **input / output / cache read** 三项累计。
- cache write 不展示。

## 2. 非目标

- 不统计 cache write（用户明确不看）。
- 不为 subagent 提供成本估算（成本沿用现有 `pricing` 的能力，但本次只做 token）。
- 不改 `attempt_watchdogs` 的限额/监督语义，只扩展它的用量记账。
- 不引入新的 daemon 或新的存储表。

## 3. 现状与约束（代码事实）

| 事实 | 位置 |
| --- | --- |
| subagent 的 worker 跑在**独立 daemon 进程**，经 unix socket 通信 | `yi-agent-subagent/src/attach.rs:412`（`ensure_owned_runtime` → `socket_path_for`） |
| worker 的 provider 用量目前只上报 `input_tokens` / `output_tokens` | `yi-agent-subagent/src/lib.rs:962` |
| 该上报进 `WorkerWatchdogEvent::ProviderUsage`，再折算成 `token_delta = input + output` | `yi-agent-store/src/runtime.rs:2957` |
| 落到 `attempt_watchdogs.usage_json`（`WatchdogUsage`） | `yi-agent-store/src/repository.rs:1288` |
| `WatchdogUsage` 只有 `turns / tokens / cost_micros / ...`，没有分项 | `yi-agent-store/src/schedule.rs:170` |
| `ListTaskSummaries` 的 SQL 只查 `id, state_json, is_root, parent_id, thread_id` | `yi-agent-store/src/repository.rs:3396` |
| `PersistedTaskSummary` 因此不含用量 | `yi-agent-store/src/repository.rs:329` |
| 已有 children watch，**每 500ms** 轮询 `ListTaskSummaries` | `yi-agent-app-server/src/server.rs:840` |
| 子任务带 `thread_id`，可按会话过滤 | `server.rs:967` `children_for_thread` |
| `AgentChild` 协议结构当前只有身份/状态字段 | `yi-agent-app-server/src/protocol.rs:629` |
| 一个 task 可有多次 attempt，用量记在 attempt 上 | `attempt_watchdogs.attempt_id` 为 PRIMARY KEY，`task_id` 为外键 |

由此得到两条硬约束：

- **必须跨进程**：app-server 读不到 daemon 的内存，用量只能经 store 的 IPC 查询。
- **必须按 task 聚合**：同一 task 的多次 attempt 各自记账，聚合要 `SUM`，不能取最后一次。

## 4. 数据流

```
subagent worker (daemon 进程)
  AgentEvent::Usage { usage }                     ← 含四个分项
        │
        ▼  上报完整分项（本次改造点 1）
  WorkerWatchdogEvent::ProviderUsage { input, output, cache_creation, cache_read }
        │
        ▼  累加进 WatchdogUsage（本次改造点 2）
  attempt_watchdogs.usage_json
        │
        ▼  ListTaskSummaries 按 task 聚合 SUM（本次改造点 3）
  PersistedTaskSummary { …, usage: TaskUsageTotals }
        │
        ▼  已有的 500ms children watch 轮询
  AgentChild { …, usage }  →  agent/children/updated
        │
        ▼
  app-server 按 thread_id 汇总所有子任务 → 会话级 subagent 合计
        │
        ├─→ Notification::TokenUsage 新增 subagent_* 字段（累加进 translator 的累计流）
        ├─→ 状态栏折叠态：主会话 + subagent 合计
        ├─→ 状态栏展开面板：两组分开
        └─→ /usage：主会话段 + subagent 段
```

## 5. 数据结构

### 5.1 `WatchdogUsage`（store）

新增三个分项字段，`tokens` 保留为**四者之和**以兼容现有 watchdog / 看板 / 调度判断：

```rust
pub struct WatchdogUsage {
    pub turns: u32,
    pub tokens: u64,              // 保持原语义：所有 provider 调用的 input+output 之和
    pub cost_micros: u64,
    pub provider_retries: u16,
    pub tool_retries: u16,
    pub rework_cycles: u16,
    // 新增：供会话级展示的三项累计
    #[serde(default)] pub input_tokens: u64,
    #[serde(default)] pub output_tokens: u64,
    #[serde(default)] pub cache_read_tokens: u64,
}
```

- `#[serde(default)]` 保证老 `usage_json`（无这些键）能反序列化，读出来是 0。
- 不需要 DB 迁移：`usage_json` 是自由格式的 JSON 文本列。
- 写入点是 `record_watchdog_progress`：把三个分项按 delta 累加，同时维持 `tokens` 不变
  （仍为 input+output）。

### 5.2 `WorkerWatchdogEvent::ProviderUsage`（core）

```rust
ProviderUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,  // 新增（本次不展示，但记账以保持 tokens 语义完整）
    cache_read_tokens: u64,      // 新增
}
```

上报点 `yi-agent-subagent/src/lib.rs:962` 从 `TokenUsage` 取全部四项
（`cache_*` 是 `Option<u32>`，`None` 按 0）。

### 5.3 `TaskUsageTotals`（store IPC 边界）

```rust
pub struct IpcTaskUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
```

`IpcTaskSummary` 新增 `usage: IpcTaskUsage`（老 daemon 不回传时 `#[serde(default)]` 为全零）。

### 5.4 聚合 SQL

三个分项存在 `attempt_watchdogs.usage_json` 里（**不是独立列**），所以 SQL 无法直接
`SUM` 它们。两个选项：

- **(a)** 额外按 task 查出 attempt 行，在 Rust 侧解析 `usage_json` 求和。
- **(b)** 迁移出三列真实列，让 SQL 直接 `SUM`。

选 **(a)**：不做 schema 迁移，也不把 JSON 解析塞进热路径 SQL。任务数是一个会话的子任务
量级（个位数到几十），一次批量查询足够。

`task_summaries` 保持原 SQL 不变，聚合在 Rust 侧追加一步：

```rust
// 1) task_summaries 照旧产出身份/状态
let mut summaries = /* 现有查询 */;

// 2) 一次批量取这些 task 的全部 attempt 用量（避免 N+1）
let ids: Vec<&str> = summaries.iter().map(|s| s.task_id.as_str()).collect();
let usages: HashMap<String, IpcTaskUsage> = self.attempt_usages_for_tasks(&ids)?;

// 3) 挂到 summary 上
for summary in &mut summaries {
    summary.usage = usages.remove(&summary.task_id).unwrap_or_default();
}
```

`attempt_usages_for_tasks` 的 SQL（一次查询，按 task_id 分组）：

```sql
SELECT task_id, usage_json FROM attempt_watchdogs WHERE task_id IN (?1, ?2, ...)
```

在 Rust 侧对每个 `usage_json` 反序列化成 `WatchdogUsage`，按 `task_id` 累加三个分项
（多个 attempt 求和，`COALESCE` 语义由 `unwrap_or_default()` 承担）。

### 5.5 协议（app-server → 桌面端）

`AgentChild` 新增：

```rust
#[serde(default)]
pub usage: AgentChildUsage,   // { inputTokens, outputTokens, cacheReadTokens }

pub struct AgentChildUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}
```

`Notification::TokenUsage` 新增三个字段（与会话累计同款 `u64` + `#[ts(type = "number")]`）：

```rust
subagent_input_tokens: u64,
subagent_output_tokens: u64,
subagent_cache_read_tokens: u64,
```

**语义**：这是「本会话所有 subagent 的累计」，由 driver 在每次 provider 用量事件与每次
children 更新时重算，**不并入** `total_*`。

## 6. 会话级汇总

`translator` 已有 `committed + turn + snapshot` 三段结构。subagent 的量不来自 provider 流，
因此单独放一个字段，由 driver 在收到 children 汇总时调用：

```rust
fn set_subagent_totals(&mut self, totals: SubagentTotals) -> Notification
```

它只更新 `self.subagent` 并回一条 `TokenUsage` 通知（`total_*` 不变，`subagent_*` 变化）。

**为什么必须经 driver 中转**：`watch_children` 是独立 task，拿不到 `translator` 里的
`total_*`。若由它自己发 `TokenUsage`，会把客户端已正确的主会话累计覆盖成零。因此它只用
`tokio::sync::watch::Sender<SubagentTotals>` 把汇总值送回 driver，由 driver 用自己那份
translator 生成完整通知。选 `watch` 而非 `mpsc`：只保留最新值，`send_replace` 不阻塞、
不积压。

**`/clear` 的基线语义（重要）**：`/clear` 只截断会话日志（`thread_store.rs:862`），
**不会删除 daemon 里的 task 记录**。所以清空后 children 列表仍带着旧 usage，若不处理，
下一次 children 推送会把旧数字带回来。做法是：driver 维护一个基线，`/clear` 时把
**当前收到的原始汇总**记为基线，此后上报 `原始 − 基线`（逐项饱和相减）。

`reset_totals()` 仍必须把 `self.subagent` 归零并推一条通知，让客户端立刻清零，不必等
下一次轮询；基线的存在保证那之后也不会被旧值污染。

**空值语义（必须明确，避免两处实现分歧）**：`subagent_*` 恒为**具体数字**，不是
`Optional`。没有 subagent 时就是三个 `0`。桌面端据此判断「是否展示 subagent 段」的条件
是 `> 0`，而不是 `null`。理由：与 `total_*` 保持同一种形状，避免客户端出现第三种状态
（缺失 / null / 有值）。老服务端完全不发这三个字段，此时桌面端按缺字段处理（见 §8）。

汇总逻辑（server 侧）：

```rust
fn subagent_totals(children: &[AgentChild]) -> SubagentTotals {
    children.iter().map(|c| c.usage).fold(default, |mut acc, u| { acc.add(u); acc })
}
```

**为什么用 children 列表而不是另开一路查询**：children watch 每 500ms 已经在拉这份数据，
复用它不增加任何轮询与 IPC 调用。代价是最多 500ms 的展示延迟。

## 7. 展示

### 7.1 状态栏折叠态

现在是三组：`in / cache / out`。改为**合计值**（主会话 + subagent）：

- `1.2M in / 3.4M cache / 56k out` —— 这三个数各自是 `主会话 + subagent` 的和。
- 折叠态**不加**任何视觉标记（不新增第四组、不加角标），保持现有宽度与手机端单行。

读屏句子与可见数字必须表达**同一件事**（都是合计），否则读屏用户与视觉用户看到的是两个
数。措辞：

```
"{合计 input} cumulative input / {合计 cache read} cumulative cache read / {合计 output}
 cumulative output, including subagents"
```

有 subagent 时在句尾追加 `", N subagents"`（N 为该会话子任务数）；没有 subagent 时保持
现有句子逐字不变，避免无谓的文案改动影响既有测试。

### 7.2 状态栏展开面板（`UsagePanel`）

标题分两组：

```
Usage (this session)
  Session      input 1,234,567   output 8,900   cache read 2,000,000
  Subagents    input   123,456   output   789   cache read   456,789
  Model        claude-sonnet-4
  Est. cost    $1.23        ← 仅主会话（subagent 成本本次不估）
```

- subagent 全零时**不显示** Subagents 段（无 subagent 的会话保持现状，不出现死行）。
- 成本行明确标注只含主会话，避免误读。

### 7.3 `/usage`

```
模型: claude-sonnet-4
主会话 input: 1,234,567  output: 8,900
主会话 cache read: 2,000,000
subagent input: 123,456  output: 789
subagent cache read: 456,789
上下文: 12,345 / 160,000 (7.7%)
估算成本: $1.23（主会话，不含 subagent）
```

- 用「主会话」而不是「本会话」：「本会话」在用户语境里是整段对话（含 subagent），
  而这里要表达的是「主 Agent 自己」。
- 有 subagent 时输出两段；没有时保持现有单段格式（**不输出空的 subagent 段**），
  既有文案与测试逐字不变。
- `formatUsage` 增加 `subagent: UsageTotals | null` 参数；`null` 或三项全零 = 无 subagent。
- 估算成本只在有 subagent 时追加「（主会话，不含 subagent）」限定语；无 subagent 时
  保持现有 `估算成本: $X` 不变。

## 8. 错误处理与边界

| 情形 | 行为 |
| --- | --- |
| daemon 不回传 `usage`（老版本） | 桌面端视为 0，不报错 |
| `usage_json` 缺新字段（老数据） | `serde(default)` 读出 0 |
| children 轮询失败 | 沿用现有行为：跳过本轮，保留上次值 |
| subagent 中途被杀 | 丢失最多一次调用的用量，可接受 |
| daemon 重启后任务被清理 | 当前汇总可能小于基线，饱和相减报 0，不回绕 |
| `/clear` | subagent 累计一并归零；基线钉住 clear 前的量，旧值不会被下次推送带回 |
| 会话切换 | 由 `usageId` 变化触发直接跳终值，不滚动 |

**精度口径**（已确认）：subagent 在独立进程，经 500ms 轮询可见，因此累计值有最多约
0.5 秒延迟；进程被杀可能丢失最后一次上报。与主会话「落盘可接受小幅偏差」同标准。

## 9. 测试

### 9.1 store

- `WatchdogUsage` 老 JSON（无新字段）能反序列化，分项为 0。
- `record_watchdog_progress` 累加三个分项，`tokens` 语义不变。
- 同一 task 的**多次 attempt** 求和（不取最后一次、不重复计）。
- 聚合结果按 `thread_id` 正确归属。

### 9.2 app-server

- `AgentChild` 携带 usage 并能序列化。
- children 更新时 `TokenUsage` 通知带正确的 `subagent_*`。
- `/clear` 后 `subagent_*` 归零。
- 老 daemon 不回 `usage` 时全零且不报错。

### 9.3 桌面端

- 折叠态显示合计（主+子）。
- 展开面板两组分开；subagent 为零时不显示该段。
- `/usage` 两段输出；无 subagent 时保持原格式。
- 读屏句子含 subagent 数量。
- 通知缺 `subagent_*`（老服务端）时行为不变。

### 9.4 真实链路

一个 subagent 实际跑起来后，桌面端 500ms 内看到数字增长（手工验证，记录在验收项）。

## 10. 实施顺序

1. store：`WatchdogUsage` 分项 + 上报点传四项 + 老数据兼容测试。
2. store IPC：`IpcTaskSummary.usage` + 按 task 聚合（Rust 侧求和）。
3. app-server：`AgentChildUsage` + children 汇总 + `TokenUsage.subagent_*`。
4. 桌面端：协议类型 + `Session.subagentTotals` + 通知/children 两路写入。
5. 展示：折叠态合计、展开面板分组、`/usage` 分段。
6. 文档：`docs/project-management/desktop.md` 验收项。

## 11. 风险

1. **500ms 轮询延迟**：折叠态数字最多晚 0.5 秒。若用户觉得不够实时，后续可让 daemon 主动
   推送（需要新的 IPC 订阅通道），本次不做。
2. **`tokens` 语义**：现有 watchdog 判断依赖 `tokens`。新增分项时保持 `tokens = input +
   output` 不变，避免影响监督阈值。
3. **多 attempt 求和**：若同一 task 重试会重复花 token，求和正是想要的语义；但需确认不会
   把「被取消的 attempt」也算进去——按当前设计会算，这是真实消耗，符合预期。
