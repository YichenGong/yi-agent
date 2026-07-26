# Tool Result TUI End Time Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finalize TUI task timing when a non-streaming tool returns its `ToolResult`.

**Architecture:** Extend `RunningTaskRegistry` with an idempotent result-finalization method. Route `AgentEvent::ToolResult` through that method before history sees the event, so all tools share a terminal path while bash retains its `ToolExit`/timeout-specific states.

**Tech Stack:** Rust, Tokio, yi-agent TUI unit tests.

---

### Task 1: Cover non-streaming tool completion

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1155`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_route_event_tool_result_finalizes_non_streaming_tool() {
    let mut registry = RunningTaskRegistry::new();
    let mut statusbar = StatusBarState::default();
    let mut cost = CostTracker::default();
    route_event(&mut registry, &mut statusbar, &mut cost, &AgentEvent::ToolCall {
        id: "search".into(),
        name: "web_search".into(),
        input: serde_json::json!({"query": "rust"}),
    });
    route_event(&mut registry, &mut statusbar, &mut cost, &AgentEvent::ToolResult {
        id: "search".into(),
        result: yi_agent_core::ToolResult::text("result"),
    });
    let elapsed = registry.get("search").unwrap().elapsed();
    assert_eq!(registry.get("search").unwrap().status, TaskStatus::Done);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(registry.get("search").unwrap().elapsed(), elapsed);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p yi-agent test_route_event_tool_result_finalizes_non_streaming_tool`

Expected: FAIL because `ToolResult` is not handled by `route_event`, leaving the task `Running`.

### Task 2: Finalize a running registry entry from a tool result

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/state.rs:100`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:562`

- [ ] **Step 1: Add the minimal registry operation**

```rust
pub fn on_result(&mut self, id: &str, is_error: bool) {
    if let Some(t) = self.tasks.get_mut(id) {
        if t.status == TaskStatus::Running {
            t.end_time = Some(Instant::now());
            t.exit_code = None;
            t.status = if is_error { TaskStatus::Failed } else { TaskStatus::Done };
        }
    }
}
```

- [ ] **Step 2: Route result events**

```rust
AgentEvent::ToolResult { id, result } => {
    registry.on_result(id, result.is_error);
}
```

- [ ] **Step 3: Run the new test to verify it passes**

Run: `cargo test -p yi-agent test_route_event_tool_result_finalizes_non_streaming_tool`

Expected: PASS.

### Task 3: Verify regression coverage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1155`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/state.rs:100`

- [ ] **Step 1: Run the affected test suite**

Run: `cargo test -p yi-agent test_route_event`

Expected: PASS, including existing timeout and turn-end cleanup behavior.

- [ ] **Step 2: Run formatting and the workspace test suite**

Run: `cargo fmt --check && cargo test --workspace`

Expected: PASS with no formatter differences and no test failures.

- [ ] **Step 3: Commit the implementation**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs yi-agent-rs/crates/yi-agent/src/tui/state.rs
git commit -m "fix(tui): finalize non-streaming tool timers"
```
