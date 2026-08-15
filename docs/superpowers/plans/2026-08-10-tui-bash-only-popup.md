# Ctrl+P Bash-Only Popup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep the Ctrl+P popup limited to Bash tasks so other tools cannot appear with empty details.

**Architecture:** Filter `AgentEvent::ToolCall` in the existing TUI event router before it creates a `RunningTaskRegistry` entry. The registry already ignores subsequent events for unknown IDs, so the existing Bash detail and lifecycle code needs no structural change. Record the new behavior in the TUI progress tracker.

**Tech Stack:** Rust 2024, `yi-agent`, Ratatui, Cargo tests, Markdown.

---

### Task 1: Filter Popup Tasks at the Event Router

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:631-646`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1340-1380`

- [ ] **Step 1: Write the failing regression test**

  Add this test beside `test_route_event_full_flow`:

  ```rust
  #[test]
  fn test_route_event_tracks_only_bash_tool_calls() {
      let mut registry = RunningTaskRegistry::new();
      let mut statusbar = StatusBarState::default();
      let mut cost = CostTracker::default();

      route_event(
          &mut registry,
          &mut statusbar,
          &mut cost,
          &AgentEvent::ToolCall {
              id: "grep".into(),
              name: "grep".into(),
              input: serde_json::json!({"pattern": "TODO"}),
          },
      );
      assert!(registry.list().is_empty());

      route_event(
          &mut registry,
          &mut statusbar,
          &mut cost,
          &AgentEvent::ToolCall {
              id: "bash".into(),
              name: "bash".into(),
              input: serde_json::json!({"command": "echo hi", "expected_timeout_sec": 30}),
          },
      );
      assert_eq!(registry.list().len(), 1);
      assert_eq!(registry.get("bash").unwrap().command, "echo hi");
  }
  ```

- [ ] **Step 2: Run the regression test and verify it fails**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests::test_route_event_tracks_only_bash_tool_calls
  ```

  Expected: FAIL because the current router registers the `grep` call.

- [ ] **Step 3: Add the minimal Bash-name guard**

  In the `AgentEvent::ToolCall` arm of `route_event`, keep
  `statusbar.on_tool_call_phase()` before the guard, then replace the
  unconditional registration with:

  ```rust
  if name == "bash" {
      let cmd = input
          .get("command")
          .and_then(|v| v.as_str())
          .unwrap_or("");
      let exp = input
          .get("expected_timeout_sec")
          .and_then(|v| v.as_u64())
          .unwrap_or(120) as u32;
      registry.on_tool_call(id, name, cmd, exp);
  }
  ```

- [ ] **Step 4: Run the focused regression test and verify it passes**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests::test_route_event_tracks_only_bash_tool_calls
  ```

  Expected: PASS with one test passed.

- [ ] **Step 5: Run the TUI application tests**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests
  ```

  Expected: PASS with no failed tests.

### Task 2: Update Project Tracking and Commit

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md:39-40`
- Modify: `docs/project-management/README.md:16`

- [ ] **Step 1: Record the verified Bash-only popup behavior**

  Add this completed feature to `yi-agent-tui.md` after the Bash popup entry:

  ```markdown
  - [x] Ctrl+P 仅显示 Bash 任务 — `tui/app.rs::route_event` 只注册 `bash` 工具调用，避免其他工具显示空详情；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::test_route_event_tracks_only_bash_tool_calls`
  ```

  Update the `yi-agent-tui` total in `docs/project-management/README.md` from
  `19 / 20` to `20 / 21`.

- [ ] **Step 2: Format the Rust workspace**

  Run:

  ```bash
  cargo fmt --all
  ```

  Expected: exit code 0.

- [ ] **Step 3: Re-run the focused TUI test suite**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests
  ```

  Expected: PASS with no failed tests.

- [ ] **Step 4: Commit the implementation**

  Run:

  ```bash
  git add yi-agent-rs/crates/yi-agent/src/tui/app.rs docs/project-management/yi-agent-tui.md docs/project-management/README.md
  git commit -m "fix(tui): limit Ctrl+P popup to bash tasks"
  ```

  Expected: a conventional-commit implementation commit with no co-author trailer.
