# TUI `/cost` 命令累计 token 用量设计

日期: 2026-07-26

## 背景与目标

当前 TUI 的 `/cost` slash 命令是占位实现(`tui/app.rs:782-787`),只在 history 里
push 一条 `"Token 用量: (暂未实现)"`。`tui/slash-commands-design.md` 也标注为
"初版占位,实际数据从 agent 获取"。

`StatusBarState`(`tui/statusbar.rs`)只跟踪**单次调用**的 input/output token,
每次 `AgentEvent::Start` 都会 `reset_for_new_call()` 清零,无法跨调用累计。

目标: 让 `/cost` 显示**累计** token 用量,并**按模型分组**,字段包含:
input / output / cache_creation / cache_read / 调用次数。

## 数据来源缺口

`AgentEvent::Usage(TokenUsage)` 当前的 `TokenUsage` **不带模型名**:
```rust
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}
```

TUI 端无法可靠知道一次 `Usage` 事件来自哪个模型(`run_loop` 的 `model: &str`
是配置值,`/model` 切换后未必同步)。因此需要从事件源头带模型名。

## 架构

### 新模块: `tui/cost.rs`

独立的累计器,按模型累加 `TokenUsage`,不依赖 TUI 其他状态。

```rust
use std::collections::BTreeMap;
use yi_agent_core::TokenUsage;

#[derive(Debug, Clone, Default)]
pub struct ModelCost {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub calls: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CostTracker {
    per_model: BTreeMap<String, ModelCost>,
}

impl CostTracker {
    pub fn record(&mut self, model: &str, usage: &TokenUsage) { ... }
    pub fn render(&self) -> String { ... }
}
```

- `ModelCost`: 单模型累计值(input/output/cache_creation/cache_read/calls)
- `CostTracker`: `BTreeMap<模型名, ModelCost>`,用 `BTreeMap` 保证输出按模型名
  字典序稳定
- `record(model, usage)`: 累加一次 `Usage` 事件的 token 到对应模型,`calls += 1`
- `render()`: 返回多行文本,用于 `/cost` 推进 history

### `AgentEvent::Usage` 变体改造

`yi-agent-core/src/agent.rs`: 把 `Usage(TokenUsage)` 改成结构体变体:

```rust
pub enum AgentEvent {
    // ...
    Usage {
        model: String,
        usage: TokenUsage,
    },
    // ...
}
```

`ProviderEvent::Usage` **不变** —— provider 层不知道调用方用的哪个模型是合理的,
模型名在 driver 层(`agent.rs` 的 `forward_provider_events`)拼装。driver 已有
`let model = config.model.clone();`(`agent.rs:294`),发送时带上即可:

```rust
ProviderEvent::Usage(u) => {
    let _ = tx.try_send(AgentEvent::Usage {
        model: model.clone(),
        usage: u,
    });
}
```

### 匹配点机械更新(5 处)

- `yi-agent-core/src/agent.rs:735` — 发送点(如上)
- `yi-agent-core/src/agent.rs:1070` — 测试解构
- `yi-agent/src/tui/app.rs:483` — `route_event` 处理,调 `cost.record`
- `yi-agent/src/tui/app.rs:1061` — 测试构造
- `yi-agent/src/tui/history.rs:177` — `AgentEvent::Usage(_)` → `Usage { .. }`

### 接入 `run_loop`(`tui/app.rs`)

1. 新增本地状态:
   ```rust
   let mut cost_tracker = CostTracker::default();
   ```
   与 `statusbar_state`、`task_registry` 并列。

2. `route_event` 签名加参数:
   ```rust
   fn route_event(
       registry: &mut RunningTaskRegistry,
       statusbar: &mut StatusBarState,
       cost: &mut CostTracker,
       event: &AgentEvent,
   ) { ... }
   ```
   `AgentEvent::Usage { model, usage }` 分支调 `cost.record(model, &usage)`。

3. `execute_slash_command` 签名加参数:
   ```rust
   fn execute_slash_command(
       cmd: SlashCommand,
       args: Option<String>,
       history: &mut HistoryState,
       cost: &CostTracker,
       input_tx: ...,
       interrupt_tx: ...,
       control_tx: ...,
   ) -> KeyOutcome { ... }
   ```
   `SlashCommand::Cost` 分支:
   ```rust
   SlashCommand::Cost => {
       let text = cost.render();
       history.push(HistoryCell::UserMessage { text });
       KeyOutcome::None
   }
   ```

4. `run_loop` 调用处同步更新签名。

5. 所有 `route_event` 测试调用点机械补上 `&mut CostTracker::default()` 参数。

## `/cost` 输出格式

`CostTracker::render()` 返回的多行文本(作为 `HistoryCell::UserMessage` 推进
history):

```
Token 用量统计:
模型                    input     output   cache_create  cache_read  calls
claude-sonnet-4-5      12,345    6,789    1,200        3,400       8
gpt-4o                 5,000     2,100    0            0           3
─────────────────────────────────────────────────────────────────────
总计                   17,345    8,889    1,200        3,400       11
```

- 第 1 行标题 `Token 用量统计:`
- 第 2 行表头,列用空格对齐(不画 ASCII 表框,简洁)
- 每个模型一行,数字用 `format_thousands` 加千分位(`tui/statusbar.rs` 已有)
- 末行分隔线 + 总计行(所有模型汇总)
- 列宽在 `render()` 里按最长模型名/数字动态计算对齐
- 空状态(无任何 `Usage` 事件)显示 `Token 用量统计: (尚无数据)`

## 测试策略

按 TDD 原则,先写测试再实现。

### `tui/cost.rs` 单元测试

- `record_single_model_accumulates` — 单模型多次记录,字段累加正确
- `record_multiple_models_separate` — 多模型各自独立累计
- `record_increments_calls` — 每次 `record` 让 `calls += 1`
- `render_empty_shows_no_data` — 空状态显示 `(尚无数据)`
- `render_single_model_format` — 单模型输出格式含表头/数据行/总计
- `render_multiple_models_sorted` — 多模型按模型名字典序输出
- `render_total_row_sums_all` — 总计行汇总所有模型
- `render_uses_thousands_separators` — 数字带千分位

### `yi-agent-core` 测试更新

- `agent.rs` 现有 `AgentEvent::Usage` 测试解构更新为新变体形状
- 新增测试:driver 发送 `AgentEvent::Usage` 时 `model` 字段等于 `config.model`

### `tui/app.rs` 集成测试

- `cost_command_renders_tracker` — 预先 `record` 一些用量,触发 `/cost`,
  history 里出现 `UserMessage` 含模型名和数字
- `cost_command_empty_shows_no_data` — 无任何 `Usage` 事件时 `/cost` 显示
  `(尚无数据)`
- `route_event_usage_records_to_tracker` — `route_event` 收到 `Usage` 后,
  `CostTracker` 对应模型计数增加
- 现有 `route_event` 测试补 `&mut CostTracker::default()` 参数后仍通过

## 改动文件清单

| 文件 | 改动 |
|------|------|
| `yi-agent-core/src/agent.rs` | `AgentEvent::Usage` 改结构体变体;发送点带 `model`;测试更新 |
| `yi-agent/src/tui/cost.rs` | **新建** `CostTracker` + `ModelCost` + `render` |
| `yi-agent/src/tui/mod.rs` | `pub mod cost;` |
| `yi-agent/src/tui/app.rs` | `run_loop` 加 `cost_tracker` 本地;`route_event` / `execute_slash_command` 加参数;`/cost` 分支调 `render`;测试补参数 |
| `yi-agent/src/tui/history.rs` | `AgentEvent::Usage(_)` → `Usage { .. }` |

## 与现有 `UserCommand` 的关系

不改 `crate::input::UserCommand`(InlineRenderer 路径),与 TUI 的 `SlashCommand`
独立,沿用既有设计文档约定。

## 不做的事(YAGNI)

- 不显示单价/费用金额(只显示 token 计数)
- 不持久化累计值到磁盘(进程重启清零)
- 不按时间范围过滤(如"今天的用量")
- 不显示 cache 命中率/百分比(只显示原始计数)
