# TUI `/cost` 命令累计 token 用量 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 TUI 的 `/cost` slash 命令显示累计 token 用量,按模型分组,字段包含 input / output / cache_creation / cache_read / 调用次数。

**Architecture:** 新建 `tui/cost.rs` 模块,内含 `CostTracker`(按模型累加 `TokenUsage`)。把 `yi-agent-core` 的 `AgentEvent::Usage` 从 `Usage(TokenUsage)` 改成结构体变体 `Usage { model, usage }`,driver 转发时带上 `config.model`。`run_loop` 持有 `CostTracker` 本地状态,`route_event` 里 `record`,`execute_slash_command` 里 `render` 推进 history。

**Tech Stack:** Rust, ratatui, tokio mpsc, `yi-agent-core` / `yi-agent` crates。

**Worktree:** `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/tui-cost` (branch `feature/tui-cost-command`)

**Design doc:** `docs/plans/2026-07-26-tui-cost-command-design.md`

---

## Task 1: 新建 `tui/cost.rs` 骨架 + 单元测试(部分 1)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/cost.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs:11` (加 `pub mod cost;`)

**Step 1: Write the failing tests (accumulator behavior)**

在 `yi-agent-rs/crates/yi-agent/src/tui/cost.rs` 写入(只有测试和最小结构定义,`record`/`render` 先用 `todo!()`):

```rust
//! Cumulative per-model token cost tracking for `/cost`.

use std::collections::BTreeMap;
use yi_agent_core::TokenUsage;

/// Per-model accumulated token counters.
#[derive(Debug, Clone, Default)]
pub struct ModelCost {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub calls: u64,
}

/// Cumulative token usage tracker, keyed by model name.
#[derive(Debug, Clone, Default)]
pub struct CostTracker {
    per_model: BTreeMap<String, ModelCost>,
}

impl CostTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, _model: &str, _usage: &TokenUsage) {
        todo!()
    }

    pub fn render(&self) -> String {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }
    }

    #[test]
    fn record_single_model_accumulates() {
        let mut t = CostTracker::new();
        t.record("claude", &usage(100, 50));
        t.record("claude", &usage(200, 30));
        let m = t.per_model.get("claude").unwrap();
        assert_eq!(m.input, 300);
        assert_eq!(m.output, 80);
    }

    #[test]
    fn record_multiple_models_separate() {
        let mut t = CostTracker::new();
        t.record("a", &usage(10, 1));
        t.record("b", &usage(20, 2));
        assert_eq!(t.per_model.len(), 2);
        assert_eq!(t.per_model.get("a").unwrap().input, 10);
        assert_eq!(t.per_model.get("b").unwrap().input, 20);
    }

    #[test]
    fn record_increments_calls() {
        let mut t = CostTracker::new();
        t.record("m", &usage(1, 1));
        t.record("m", &usage(1, 1));
        t.record("m", &usage(1, 1));
        assert_eq!(t.per_model.get("m").unwrap().calls, 3);
    }

    #[test]
    fn record_accumulates_cache_fields() {
        let mut t = CostTracker::new();
        let u1 = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(10),
        };
        let u2 = TokenUsage {
            input_tokens: 200,
            output_tokens: 30,
            cache_creation_input_tokens: Some(5),
            cache_read_input_tokens: Some(40),
        };
        t.record("m", &u1);
        t.record("m", &u2);
        let m = t.per_model.get("m").unwrap();
        assert_eq!(m.cache_creation, 25);
        assert_eq!(m.cache_read, 50);
    }
}
```

**Step 2: Register the module**

在 `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` 的 `pub mod statusbar;` 后加一行:

```rust
pub mod cost;
```

**Step 3: Run tests to verify they fail**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent tui::cost 2>&1 | tail -20`
Expected: FAIL — `record` / `render` panic with `not yet implemented` (todo! macro).

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/cost.rs yi-agent-rs/crates/yi-agent/src/tui/mod.rs
git commit -m "feat(tui): scaffold CostTracker with failing tests"
```

---

## Task 2: 实现 `CostTracker::record`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cost.rs`

**Step 1: Implement `record`**

替换 `record` 的 `todo!()`:

```rust
pub fn record(&mut self, model: &str, usage: &TokenUsage) {
    let m = self.per_model.entry(model.to_string()).or_default();
    m.input += usage.input_tokens as u64;
    m.output += usage.output_tokens as u64;
    m.cache_creation += usage.cache_creation_input_tokens.unwrap_or(0) as u64;
    m.cache_read += usage.cache_read_input_tokens.unwrap_or(0) as u64;
    m.calls += 1;
}
```

**Step 2: Run tests to verify Task 1 tests pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent tui::cost 2>&1 | tail -20`
Expected: PASS — 4 tests pass.

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/cost.rs
git commit -m "feat(tui): implement CostTracker::record"
```

---

## Task 3: 实现 `CostTracker::render` + 测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cost.rs`

**Step 1: Write the failing render tests**

在 `mod tests` 末尾追加:

```rust
    #[test]
    fn render_empty_shows_no_data() {
        let t = CostTracker::new();
        let s = t.render();
        assert!(s.contains("Token 用量统计"), "should have title: {s}");
        assert!(s.contains("尚无数据"), "empty should show no-data: {s}");
    }

    #[test]
    fn render_single_model_has_header_data_total() {
        let mut t = CostTracker::new();
        t.record("claude-sonnet-4-5", &usage(12345, 6789));
        let s = t.render();
        assert!(s.contains("input"), "should have header: {s}");
        assert!(s.contains("output"), "should have header: {s}");
        assert!(s.contains("claude-sonnet-4-5"), "should have model row: {s}");
        assert!(s.contains("12,345"), "should format input with thousands: {s}");
        assert!(s.contains("6,789"), "should format output with thousands: {s}");
        assert!(s.contains("总计"), "should have total row: {s}");
    }

    #[test]
    fn render_multiple_models_sorted() {
        let mut t = CostTracker::new();
        t.record("zeta", &usage(1, 1));
        t.record("alpha", &usage(2, 2));
        t.record("mid", &usage(3, 3));
        let s = t.render();
        let ai = s.find("alpha").unwrap();
        let mi = s.find("mid").unwrap();
        let zi = s.find("zeta").unwrap();
        assert!(ai < mi && mi < zi, "models should be sorted alphabetically");
    }

    #[test]
    fn render_total_row_sums_all_models() {
        let mut t = CostTracker::new();
        t.record("a", &usage(100, 10));
        t.record("b", &usage(200, 20));
        let s = t.render();
        assert!(s.contains("300"), "total input should be 300: {s}");
        assert!(s.contains("30"), "total output should be 30: {s}");
    }

    #[test]
    fn render_shows_calls_column() {
        let mut t = CostTracker::new();
        t.record("m", &usage(1, 1));
        t.record("m", &usage(1, 1));
        let s = t.render();
        assert!(s.contains("calls"), "should have calls header: {s}");
        assert!(s.contains("2"), "should show call count 2: {s}");
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent tui::cost 2>&1 | tail -20`
Expected: FAIL — `render` panics with `todo!()`.

**Step 3: Implement `render`**

Add `use super::statusbar::format_thousands;` at top of `cost.rs` (reuse existing thousands formatter), then replace `render`'s `todo!()`:

```rust
pub fn render(&self) -> String {
    if self.per_model.is_empty() {
        return "Token 用量统计:\n(尚无数据)".to_string();
    }

    let mut rows: Vec<[String; 6]> = Vec::new();
    for (model, cost) in &self.per_model {
        rows.push([
            model.clone(),
            format_thousands(cost.input),
            format_thousands(cost.output),
            format_thousands(cost.cache_creation),
            format_thousands(cost.cache_read),
            format_thousands(cost.calls),
        ]);
    }

    let mut total = ModelCost::default();
    for c in self.per_model.values() {
        total.input += c.input;
        total.output += c.output;
        total.cache_creation += c.cache_creation;
        total.cache_read += c.cache_read;
        total.calls += c.calls;
    }

    let header = [
        "模型".to_string(),
        "input".to_string(),
        "output".to_string(),
        "cache_create".to_string(),
        "cache_read".to_string(),
        "calls".to_string(),
    ];
    let total_row = [
        "总计".to_string(),
        format_thousands(total.input),
        format_thousands(total.output),
        format_thousands(total.cache_creation),
        format_thousands(total.cache_read),
        format_thousands(total.calls),
    ];

    // Compute column widths from header + all rows + total row.
    let mut widths = [0usize; 6];
    let all_rows: Vec<[String; 6]> = std::iter::once(header.clone())
        .chain(rows.iter().cloned())
        .chain(std::iter::once(total_row.clone()))
        .collect();
    for r in &all_rows {
        for (i, cell) in r.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut out = String::from("Token 用量统计:\n");
    let mut first = true;
    for r in &all_rows {
        for (i, cell) in r.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("{:<width$}", cell, width = widths[i]));
            } else {
                out.push_str(&format!("  {:>width$}", cell, width = widths[i]));
            }
        }
        out.push('\n');
        if first {
            let sep: String = std::iter::repeat_n('─', widths.iter().sum::<usize>() + 2 * 5)
                .collect();
            out.push_str(&sep);
            out.push('\n');
        }
        first = false;
    }
    // Trailing newline already added by last row; trim if desired.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent tui::cost 2>&1 | tail -20`
Expected: PASS — all 9 cost tests pass.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/cost.rs
git commit -m "feat(tui): implement CostTracker::render with per-model totals"
```

---

## Task 4: 改造 `AgentEvent::Usage` 变体

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:141` (枚举)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:724-744` (发送点 `accumulate_provider_stream`)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:366` (调用 `accumulate_provider_stream` 处,传 `model`)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:1070` (测试解构)

**Step 1: 改造枚举变体**

把 `agent.rs:141` 的 `Usage(TokenUsage),` 改成:

```rust
    Usage {
        model: String,
        usage: TokenUsage,
    },
```

**Step 2: 改造发送点**

把 `accumulate_provider_stream` 函数签名加上 `model: &str` 参数,并在 `ProviderEvent::Usage` 分支带上:

```rust
async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
) -> Result<(Vec<ContentBlock>, StopReason), AgentError> {
    let tx = tx.clone();
    let model = model.to_string();
    let (content, stop_reason) =
        crate::provider::accumulate_stream(stream, move |event| match event {
            ProviderEvent::TextDelta(s) => {
                let _ = tx.try_send(AgentEvent::AssistantText(s));
            }
            ProviderEvent::Usage(u) => {
                let _ = tx.try_send(AgentEvent::Usage {
                    model: model.clone(),
                    usage: u,
                });
            }
            ProviderEvent::ToolUseDelta { partial_json, .. } => {
                let _ = tx.try_send(AgentEvent::DecodeDelta(partial_json));
            }
            _ => {}
        })
        .await?;
    Ok((content, stop_reason))
}
```

**Step 3: 更新调用处传 `model`**

在 `agent.rs:366` 附近,把 `accumulate_provider_stream(stream, &tx)` 改成 `accumulate_provider_stream(stream, &tx, &model)`(`model` 在 `run` 作用域的 `agent.rs:294` 已有)。

**Step 4: 更新现有测试解构**

`agent.rs:1070` 附近:

```rust
AgentEvent::Usage(u) => Some(u.clone()),
```

改成:

```rust
AgentEvent::Usage { usage, .. } => Some(usage.clone()),
```

**Step 5: 编译验证**

Run: `cargo build --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core 2>&1 | tail -30`
Expected: 编译通过(可能有下游 crate 编译错误,后续 task 修复)。

**Step 6: 运行 yi-agent-core 测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core --lib 2>&1 | tail -20`
Expected: 之前失败的 `agent_emits_debug_events_for_request_delta_and_response` 仍失败(pre-existing),其他测试通过。

**Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): add model field to AgentEvent::Usage"
```

---

## Task 5: 更新 TUI 对 `AgentEvent::Usage` 的匹配

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs:177`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:483` (先只改匹配形状,不动 `route_event` 签名,Task 6 再接 `CostTracker`)
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1061` (测试)

**Step 1: 改 `history.rs:177`**

```rust
AgentEvent::Usage(_) => {}
```

改成:

```rust
AgentEvent::Usage { .. } => {}
```

**Step 2: 改 `app.rs:483` 的匹配形状**

```rust
AgentEvent::Usage(u) => {
    statusbar.set_token_target(u.input_tokens as u64, u.output_tokens as u64);
}
```

改成(暂时不接 `CostTracker`,只改形状,保持编译通过):

```rust
AgentEvent::Usage { usage, .. } => {
    statusbar.set_token_target(usage.input_tokens as u64, usage.output_tokens as u64);
}
```

**Step 3: 改 `app.rs:1061` 的测试构造**

把测试里的:

```rust
&AgentEvent::Usage(yi_agent_core::TokenUsage {
    input_tokens: 100,
    output_tokens: 50,
    ..Default::default()
}),
```

改成:

```rust
&AgentEvent::Usage {
    model: "test".to_string(),
    usage: yi_agent_core::TokenUsage {
        input_tokens: 100,
        output_tokens: 50,
        ..Default::default()
    },
},
```

**Step 4: 编译 + 运行 yi-agent 测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent 2>&1 | tail -30`
Expected: 编译通过;除 5 个 pre-existing `config::tests` 失败外,其他测试通过(包括 `tui::cost` 9 个测试)。

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/history.rs yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "refactor(tui): match new AgentEvent::Usage variant shape"
```

---

## Task 6: 把 `CostTracker` 接入 `route_event`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (route_event 签名 + Usage 分支 + 调用处 + 测试调用处)

**Step 1: 加 import**

`app.rs` 顶部 `use super::statusbar::{...}` 附近加:

```rust
use super::cost::CostTracker;
```

**Step 2: 改 `route_event` 签名 + Usage 分支**

```rust
fn route_event(
    registry: &mut RunningTaskRegistry,
    statusbar: &mut StatusBarState,
    cost: &mut CostTracker,
    event: &AgentEvent,
) {
    match event {
        // ... 其他分支不变 ...
        AgentEvent::Usage { model, usage } => {
            statusbar.set_token_target(usage.input_tokens as u64, usage.output_tokens as u64);
            cost.record(model, usage);
        }
        // ... 其他分支不变 ...
    }
}
```

**Step 3: 改 `run_loop` 里 `route_event` 调用处**

在 `run_loop` 本地状态区(`let mut statusbar_state = ...; let mut task_registry = ...;`)后加:

```rust
let mut cost_tracker = CostTracker::default();
```

把 `route_event(&mut task_registry, &mut statusbar_state, &event);` 改成:

```rust
route_event(&mut task_registry, &mut statusbar_state, &mut cost_tracker, &event);
```

**Step 4: 更新所有 `route_event` 测试调用处**

搜索 `app.rs` 里所有测试中的 `route_event(` 调用,补上 `&mut CostTracker::default()` 参数。示例:

```rust
route_event(&mut registry, &mut sb, &AgentEvent::Start);
```

改成:

```rust
route_event(&mut registry, &mut sb, &mut CostTracker::default(), &AgentEvent::Start);
```

对每个测试中的每个 `route_event(` 调用都加这个参数。

**Step 5: 编译 + 运行测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent 2>&1 | tail -30`
Expected: 编译通过;除 pre-existing 5 个 config 失败外其他通过。

**Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): wire CostTracker into route_event"
```

---

## Task 7: 把 `CostTracker` 接入 `execute_slash_command` 的 `/cost`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` (execute_slash_command 签名 + Cost 分支 + 调用处 + 集成测试)

**Step 1: 改 `execute_slash_command` 签名 + Cost 分支**

签名加 `cost: &CostTracker`:

```rust
fn execute_slash_command(
    cmd: SlashCommand,
    args: Option<String>,
    history: &mut HistoryState,
    cost: &CostTracker,
    _input_tx: &tokio::sync::mpsc::Sender<String>,
    _interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
) -> KeyOutcome {
    match cmd {
        // ... 其他分支不变 ...
        SlashCommand::Cost => {
            let text = cost.render();
            history.push(HistoryCell::UserMessage { text });
            KeyOutcome::None
        }
        // ... 其他分支不变 ...
    }
}
```

**Step 2: 改 `execute_slash_command` 调用处**

搜索 `run_loop` 里 `execute_slash_command(` 的调用,补上 `&cost_tracker` 参数。

**Step 3: 写集成测试**

在 `app.rs` 的 `mod tests` 末尾加:

```rust
    #[test]
    fn cost_command_renders_tracker() {
        use yi_agent_core::TokenUsage;
        let mut history = HistoryState::new();
        let mut cost = CostTracker::default();
        cost.record(
            "claude-sonnet-4-5",
            &TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        );
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            &mut history,
            &cost,
            &input_tx,
            &interrupt_tx,
            &control_tx,
        );
        assert_eq!(outcome, KeyOutcome::None);
        // History 应含一条 UserMessage,文本里有模型名和数字
        let cell = history.cells.last().unwrap();
        match cell {
            crate::tui::cell::HistoryCell::UserMessage { text } => {
                assert!(text.contains("claude-sonnet-4-5"), "cost text should include model: {text}");
                assert!(text.contains("100"), "cost text should include input tokens: {text}");
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[test]
    fn cost_command_empty_shows_no_data() {
        let mut history = HistoryState::new();
        let cost = CostTracker::default();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) =
            tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            &mut history,
            &cost,
            &input_tx,
            &interrupt_tx,
            &control_tx,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let cell = history.cells.last().unwrap();
        match cell {
            crate::tui::cell::HistoryCell::UserMessage { text } => {
                assert!(text.contains("尚无数据"), "empty cost should show no-data: {text}");
            }
            other => panic!("expected UserMessage, got {other:?}"),
        }
    }

    #[test]
    fn route_event_usage_records_to_tracker() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        let mut cost = CostTracker::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::Usage {
                model: "claude".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            },
        );
        let rendered = cost.render();
        assert!(rendered.contains("claude"), "tracker should contain model after route_event: {rendered}");
        assert!(rendered.contains("100"), "tracker should contain input tokens: {rendered}");
    }
```

**Step 4: 编译 + 运行测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent 2>&1 | tail -30`
Expected: 编译通过;除 5 个 pre-existing config 失败外其他通过(含新增 3 个集成测试)。

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "feat(tui): wire CostTracker into /cost slash command"
```

---

## Task 8: 格式化 + 全量测试 + 验证

**Step 1: cargo fmt**

Run: `cd yi-agent-rs && cargo fmt --all`
Expected: 格式化代码。

**Step 2: fmt-check**

Run: `cd yi-agent-rs && cargo fmt --all -- --check 2>&1 | tail -10`
Expected: 无输出(格式通过)。

**Step 3: yi-agent 测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent --bin yi-agent 2>&1 | tail -20`
Expected: 除 5 个 pre-existing config 失败外,其他全部通过(含 `tui::cost` 9 个 + 3 个新集成测试)。

**Step 4: yi-agent-core 测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-core --lib 2>&1 | tail -20`
Expected: 除 1 个 pre-existing `agent_emits_debug_events_for_request_delta_and_response` 失败外,其他通过。

**Step 5: 提交格式化**

```bash
git add -A
git commit -m "style: cargo fmt"
```

(如有格式化改动)

---

## 改动文件清单

| 文件 | Task | 改动 |
|------|------|------|
| `yi-agent-rs/crates/yi-agent/src/tui/cost.rs` | 1,2,3 | **新建** `CostTracker` |
| `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` | 1 | `pub mod cost;` |
| `yi-agent-rs/crates/yi-agent-core/src/agent.rs` | 4 | `AgentEvent::Usage` 加 `model` 字段 + 发送点 + 测试 |
| `yi-agent-rs/crates/yi-agent/src/tui/history.rs` | 5 | `Usage(_)` → `Usage { .. }` |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | 5,6,7 | 匹配形状 + 接入 `CostTracker` + `/cost` 分支 + 测试 |

## 不做的事(YAGNI)

- 不显示单价/费用金额(只显示 token 计数)
- 不持久化累计值到磁盘(进程重启清零)
- 不按时间范围过滤
- 不显示 cache 命中率/百分比
- 不改 `ProviderEvent::Usage`(只在 `AgentEvent` 层加模型)
