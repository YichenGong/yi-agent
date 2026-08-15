# Compact TUI Completion Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Change compact statuses from a permanent pending line to visible completion or failure feedback.

**Architecture:** Driver code emits manual compact result events. History owns all TUI cell transitions: manual results replace the pending separator, while auto compaction appends an independent completed separator.

**Tech Stack:** Rust, Tokio, Ratatui, Cargo.

---

## Files

- `yi-agent-rs/crates/yi-agent-core/src/agent.rs`: outcome event variants.
- `yi-agent-rs/crates/yi-agent/src/main.rs`: driver event emission.
- `yi-agent-rs/crates/yi-agent/src/tui/history.rs`: history mapping and unit tests.
- `yi-agent-rs/crates/yi-agent/src/tui/app.rs`: outcome logging.
- `docs/project-management/yi-agent-tui.md`, `docs/project-management/README.md`: delivered-feature record and `17 / 18` TUI count.

### Task 1: Model manual compaction completion

**Files:** `agent.rs`, `tui/history.rs` and the latter's test module.

- [ ] **Step 1: Write a failing success test**

```rust
#[test]
fn manual_compaction_success_replaces_pending_status() {
    let mut history = HistoryState::new();
    history.push(HistoryCell::Separator { label: Some("正在压缩对话...".into()) }, 80);
    history.push_event(AgentEvent::ManualCompacted {
        old_msg_count: 12,
        new_msg_count: 5,
    }, 80);
    assert_eq!(history.cells.len(), 1);
    assert!(matches!(&history.cells[0],
        HistoryCell::Separator { label: Some(label) }
        if label == "压缩完成（12 → 5 条消息）"));
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::history::tests::manual_compaction_success_replaces_pending_status
```

Expected: it does not compile because `ManualCompacted` does not exist.

- [ ] **Step 3: Implement the smallest change**

Add after `AutoCompacting` in `AgentEvent`:

```rust
ManualCompacted {
    old_msg_count: usize,
    new_msg_count: usize,
},
```

Add a private `HistoryState::replace_pending_compaction(&mut self, label: String)` that finds the newest `Separator` labeled `正在压缩对话...` and replaces its label, otherwise does nothing. Map the new event in `push_event`:

```rust
AgentEvent::ManualCompacted { old_msg_count, new_msg_count } => {
    self.replace_pending_compaction(format!(
        "压缩完成（{old_msg_count} → {new_msg_count} 条消息）"
    ));
}
```

- [ ] **Step 4: Verify GREEN and commit**

Run the Step 2 command; expect `1 passed; 0 failed`. Then:

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/tui/history.rs
git commit -m "feat(tui): show manual compaction completion"
```

### Task 2: Model manual compaction failure

**Files:** `agent.rs`, `tui/history.rs` and the latter's test module.

- [ ] **Step 1: Write a failing failure test**

```rust
#[test]
fn manual_compaction_failure_replaces_pending_status() {
    let mut history = HistoryState::new();
    history.push(HistoryCell::Separator { label: Some("正在压缩对话...".into()) }, 80);
    history.push_event(AgentEvent::ManualCompactFailed {
        message: "provider unavailable".into(),
    }, 80);
    assert!(matches!(&history.cells[0],
        HistoryCell::Separator { label: Some(label) }
        if label == "压缩失败：provider unavailable"));
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::history::tests::manual_compaction_failure_replaces_pending_status
```

Expected: it does not compile because `ManualCompactFailed` does not exist.

- [ ] **Step 3: Implement and verify GREEN**

Add this event variant and mapping:

```rust
ManualCompactFailed { message: String },

AgentEvent::ManualCompactFailed { message } => {
    self.replace_pending_compaction(format!("压缩失败：{message}"));
}
```

Run the Step 2 command; expect `1 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/tui/history.rs
git commit -m "fix(tui): report manual compaction failure"
```

### Task 3: Display completed automatic compaction

**Files:** `tui/history.rs` and its test module.

- [ ] **Step 1: Write a failing auto-status test**

```rust
#[test]
fn auto_compaction_appends_completed_status() {
    let mut history = HistoryState::new();
    history.push_event(AgentEvent::AutoCompacting {
        old_msg_count: 10,
        new_msg_count: 4,
    }, 80);
    assert!(matches!(&history.cells[0],
        HistoryCell::Separator { label: Some(label) }
        if label == "已自动压缩（10 → 4 条消息）"));
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::history::tests::auto_compaction_appends_completed_status
```

Expected: assertion fails because auto compaction is ignored.

- [ ] **Step 3: Implement and verify GREEN**

Replace the ignored `AutoCompacting` pattern with:

```rust
AgentEvent::AutoCompacting { old_msg_count, new_msg_count } => {
    self.cells.push(HistoryCell::Separator {
        label: Some(format!("已自动压缩（{old_msg_count} → {new_msg_count} 条消息）")),
    });
}
```

Run the Step 2 command; expect `1 passed; 0 failed`.

- [ ] **Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/history.rs
git commit -m "feat(tui): show automatic compaction completion"
```

### Task 4: Send manual outcomes from the driver

**Files:** `main.rs`, `tui/app.rs`, and the `main.rs` test module.

- [ ] **Step 1: Write the failing helper test**

```rust
#[test]
fn manual_compaction_outcome_events_preserve_counts_and_errors() {
    assert!(matches!(manual_compaction_outcome_event(9, Ok(3)),
        yi_agent_core::AgentEvent::ManualCompacted { old_msg_count: 9, new_msg_count: 3 }));
    assert!(matches!(manual_compaction_outcome_event(9, Err("request failed".into())),
        yi_agent_core::AgentEvent::ManualCompactFailed { message }
        if message == "request failed"));
}
```

- [ ] **Step 2: Verify RED**

Run `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent manual_compaction_outcome_events_preserve_counts_and_errors`.

Expected: it does not compile because the helper does not exist.

- [ ] **Step 3: Implement and wire the helper**

Add near `ControlCommand`:

```rust
fn manual_compaction_outcome_event(
    old_msg_count: usize,
    result: Result<usize, String>,
) -> yi_agent_core::AgentEvent {
    match result {
        Ok(new_msg_count) => yi_agent_core::AgentEvent::ManualCompacted {
            old_msg_count,
            new_msg_count,
        },
        Err(message) => yi_agent_core::AgentEvent::ManualCompactFailed { message },
    }
}
```

Capture `old_msg_count` before `compact_session`. On success make an event with `Ok(new_session.messages().len())`, rebuild the agent as it does now, and send the event through `agent_tx`. On failure retain the warning log but send `Err(e.to_string())` through this helper rather than generic `AgentEvent::Error`. Add logging-only `route_event` arms for both manual result variants.

- [ ] **Step 4: Verify GREEN, run crate tests, and commit**

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent manual_compaction_outcome_events_preserve_counts_and_errors
cargo test -p yi-agent --bin yi-agent
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "fix(tui): resolve compact status from driver events"
```

Expected: focused and crate test suites pass before commit.

### Task 5: Update progress records and final verification

**Files:** `docs/project-management/yi-agent-tui.md`, `docs/project-management/README.md`.

- [ ] **Step 1: Record the completed feature**

Add this line after the slash-command feature:

```markdown
- [x] 压缩状态闭环 — `/compact` 的 pending 行由 `ManualCompacted` / `ManualCompactFailed` 原地更新，`AutoCompacting` 追加完成行；验证：`cargo test -p yi-agent --bin yi-agent tui::history::tests::manual_compaction_` 和 `cargo test -p yi-agent --bin yi-agent tui::history::tests::auto_compaction_appends_completed_status`
```

Set the `yi-agent-tui` README row to `| yi-agent-tui | 17 / 18 | [详情](./yi-agent-tui.md) |`.

- [ ] **Step 2: Format, test, and commit**

```bash
ps aux | grep -v grep | grep -E 'cargo|rustc|yi_agent' || true
cd yi-agent-rs
cargo fmt --all
cargo fmt --all -- --check
cargo test -p yi-agent --bin yi-agent
git add docs/project-management/yi-agent-tui.md docs/project-management/README.md
git commit -m "docs: record compact TUI status feedback"
```

Expected: no live Cargo process before testing, formatting has no diff, and all binary unit tests pass.
