# 子任务文本完成报告解码收敛 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `text_completion_report` 的三份副本收敛为 `yi-agent-store::terminal` 里的一份，并统一「空白报告视为无报告」的语义。

**Architecture:** 新建 `crates/yi-agent-store/src/terminal.rs` 拥有该解码函数（终态 JSON 形状由 store 自己的 `runtime.rs` 写出，故解码归 store）。`repository.rs` 与 `runtime.rs` 删除本地副本改为引用同 crate 的模块；`yi-agent-subagent` 依赖 store，改用 `yi_agent_store::terminal::text_completion_report`。

**Tech Stack:** Rust（`yi-agent-store`：`serde_json`；`yi-agent-subagent` 已依赖 `yi-agent-store`）。

**Spec:** `docs/superpowers/specs/2026-10-17-text-completion-report-single-source-design.md`

## Global Constraints

- 行号以基线 `f5dd3376` 为准；改动前用 `grep -n` 复核锚点。
- 环境：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`，所有 cargo 命令加 `--offline`。
- 只显式 `git add <path>`；每个 Task 结束前 `cargo fmt -p <crate>`。
- **不改**终态 JSON 的写入形状（`text_completion_terminal_json` /
  `budget_exhausted_terminal_json` 保持原位、输出不变）。
- **不改**任何 IPC 协议、不改 `delivery_json` 解码。
- 语义定夺：**空白报告 → `None`**（采用 `repository.rs` 的现行行为；这是本设计的刻意变更）。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-store/src/terminal.rs` — **新建**。`text_completion_report` 及其单元测试。
- `yi-agent-rs/crates/yi-agent-store/src/lib.rs` — 增加 `pub mod terminal;`。
- `yi-agent-rs/crates/yi-agent-store/src/repository.rs` — 删本地副本，改引用。
- `yi-agent-rs/crates/yi-agent-store/src/runtime.rs` — 删本地副本，改引用。
- `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` — 删本地副本，改跨 crate 引用。

---

### Task 1: 新建 `terminal` 模块（含语义锁定测试）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/src/terminal.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/lib.rs`

**Interfaces:**
- Produces: `pub fn text_completion_report(terminal_json: Option<&str>) -> Option<String>`

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent-store/src/terminal.rs`：

```rust
//! 子任务终态 JSON 的公共解码。
//!
//! 终态形状由本 crate 的 `runtime` 模块写出（`text_completion_terminal_json` /
//! `budget_exhausted_terminal_json`），因此解码也归本模块所有，避免回收路径与
//! 读取路径各自再实现一遍而漂移。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_text_completion_report() {
        let json = r#"{"kind":"text_completion","report":"I got halfway through the refactor"}"#;
        assert_eq!(
            text_completion_report(Some(json)).as_deref(),
            Some("I got halfway through the refactor")
        );
    }

    #[test]
    fn extracts_a_report_from_a_budget_exhausted_payload() {
        let json =
            r#"{"reason":"turn_budget_exhausted","kind":"text_completion","report":"partial"}"#;
        assert_eq!(text_completion_report(Some(json)).as_deref(), Some("partial"));
    }

    #[test]
    fn returns_none_for_a_non_text_completion_kind() {
        let json = r#"{"kind":"delivery","report":"ignored"}"#;
        assert_eq!(text_completion_report(Some(json)), None);
    }

    #[test]
    fn returns_none_for_an_empty_report() {
        let json = r#"{"kind":"text_completion","report":""}"#;
        assert_eq!(text_completion_report(Some(json)), None);
    }

    #[test]
    fn returns_none_for_a_whitespace_only_report() {
        // 本设计刻意的行为：只有空白的报告视为「没有报告」。
        let json = r#"{"kind":"text_completion","report":"   "}"#;
        assert_eq!(text_completion_report(Some(json)), None);
    }

    #[test]
    fn returns_none_when_there_is_no_terminal_json() {
        assert_eq!(text_completion_report(None), None);
    }

    #[test]
    fn returns_none_for_invalid_json() {
        assert_eq!(text_completion_report(Some("not json")), None);
    }
}
```

- [ ] **Step 2: 运行测试确认编译失败**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-store --offline terminal:: 2>&1 | tail -20`

Expected: 编译失败，报 `cannot find function text_completion_report`（模块尚未声明）。

- [ ] **Step 3: 实现函数**

在 `terminal.rs` 中、`#[cfg(test)] mod tests` 之前插入：

```rust
/// 从终态 JSON 中取出 `text_completion` 形状的文本报告。
///
/// 返回 `None` 的四种情况：入参为 `None`、JSON 非法、`kind` 不是
/// `text_completion`、或 `report` 为空/纯空白。
///
/// 空白报告视为「无报告」：一份只有空白的报告对父级没有信息量，而 writer 侧
/// 用 `report.unwrap_or("")` 表示「没有内容」，两者语义一致。
pub fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<serde_json::Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(serde_json::Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(serde_json::Value::as_str))
        .flatten()
        .filter(|report| !report.trim().is_empty())
        .map(str::to_owned)
}
```

- [ ] **Step 4: 在 `lib.rs` 声明模块**

修改 `yi-agent-rs/crates/yi-agent-store/src/lib.rs`，在 `pub mod schedule;` 后加：

```rust
pub mod terminal;
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-store --offline terminal:: 2>&1 | tail -15`

Expected: 7 个测试全部 PASS。

- [ ] **Step 6: fmt + commit**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt -p yi-agent-store
cd ..
git add yi-agent-rs/crates/yi-agent-store/src/terminal.rs yi-agent-rs/crates/yi-agent-store/src/lib.rs
git commit -m "refactor(store): add a shared decoder for the text_completion terminal payload"
```

---

### Task 2: store 内部两处调用点改引用共享模块

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs:4140-4147`（删除）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:3161-3167`（删除）

**Interfaces:**
- Consumes: `crate::terminal::text_completion_report`（Task 1）。

- [ ] **Step 1: 记录基线测试状态**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-store --offline 2>&1 | grep -E "test result" | tail -10`

Expected: 全绿。记下各测试二进制的通过数量（尤其含 `reclaimed`/`budget_exhausted` 的用例）。

- [ ] **Step 2: 改 `repository.rs`**

删除本地函数（基线 4136-4147，含其上方 doc 注释）：

```rust
/// The partial report a text-only worker left in its attempt's terminal JSON.
///
/// Mirrors the store's own decoder; kept local so the reclaim path does not
/// reach across module boundaries for one field.
fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<serde_json::Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(serde_json::Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(serde_json::Value::as_str))
        .flatten()
        .filter(|report| !report.trim().is_empty())
        .map(str::to_owned)
}
```

在该文件的 `use` 区加入：

```rust
use crate::terminal::text_completion_report;
```

调用点（基线 2304 `let terminal_json = match text_completion_report(report.as_deref())`）保持不变。

- [ ] **Step 3: 改 `runtime.rs`**

删除本地函数（基线 3161-3167）：

```rust
fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<serde_json::Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(serde_json::Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(serde_json::Value::as_str))
        .flatten()
        .map(str::to_owned)
}
```

在该文件的 `use` 区加入：

```rust
use crate::terminal::text_completion_report;
```

调用点（基线 3269 `let completion_report = text_completion_report(terminal_json);`）保持不变。

- [ ] **Step 4: 编译并清理 import**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-store --offline 2>&1 | grep -E "^(warning|error)" | head -20`

Expected: 无输出。

- [ ] **Step 5: 运行 store 测试确认行为不变**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-store --offline 2>&1 | grep -E "test result" | tail -10`

Expected: 与 Step 1 相同数量的通过（既有正常报告路径用例不受空白过滤影响）。

- [ ] **Step 6: fmt + commit**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt -p yi-agent-store
cd ..
git add yi-agent-rs/crates/yi-agent-store/src/repository.rs yi-agent-rs/crates/yi-agent-store/src/runtime.rs
git commit -m "refactor(store): route both decoders through terminal::text_completion_report"
```

---

### Task 3: subagent 改用 store 的共享解码并全量验证

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1832-1838`（删除）

**Interfaces:**
- Consumes: `yi_agent_store::terminal::text_completion_report`（Task 1、Task 2）。

- [ ] **Step 1: 记录基线测试状态**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-subagent --offline 2>&1 | grep -E "test result" | tail -10`

Expected: 全绿（含 `daemon_worker_reports_text_completion_without_a_workspace_delivery` 等）。

- [ ] **Step 2: 删除本地副本并改引用**

删除本地函数（基线 1830-1838，含其上方 doc 注释）：

```rust
/// Extract the child's text report from its stored terminal payload, using the
/// same `kind` marker the store writes.
fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned)
}
```

在该文件的 `use` 区加入：

```rust
use yi_agent_store::terminal::text_completion_report;
```

调用点（基线 2009 `let report = text_completion_report(detail.terminal_json.as_deref());`）保持不变。

- [ ] **Step 3: 编译并确认 `Value` import 仍被使用**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-subagent --offline 2>&1 | grep -E "^(warning|error)" | head -20`

Expected: 无输出。若报 `unused import: serde_json::Value`，按提示删除该 import（`Value` 在本文件其它处可能仍在使用，仅当报错时才删）。

- [ ] **Step 4: 运行 subagent 测试确认行为不变**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-subagent --offline 2>&1 | grep -E "test result" | tail -10`

Expected: 与 Step 1 相同数量的通过。

- [ ] **Step 5: 全量验收（本 P0 的最终门槛）**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp

# 1) 只剩一份定义：应只命中 terminal.rs
grep -rn "fn text_completion_report" crates/

# 2) 全量测试与 lint
cargo test --workspace --offline 2>&1 | tail -15
cargo clippy --workspace --all-targets --offline -- -D warnings 2>&1 | tail -3
```

Expected:
- 命令 1 只打印 `crates/yi-agent-store/src/terminal.rs` 一行；
- `cargo test` 全绿；
- clippy 无警告。

- [ ] **Step 6: fmt + commit**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt -p yi-agent-subagent
cd ..
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "refactor(subagent): reuse the store's text_completion_report decoder"
```
