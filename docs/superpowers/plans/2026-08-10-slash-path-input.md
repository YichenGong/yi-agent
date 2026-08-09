# Slash Path Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Submit multi-segment absolute paths from the TUI to the agent while preserving unknown-command errors for slash-prefixed non-path input.

**Architecture:** `tui/app.rs` owns input routing, so a small pure predicate will classify the first whitespace-delimited token before the existing slash-command branch executes. Path-like submissions then use the current ordinary-message path unchanged; commands and unknown slash inputs retain their current behavior and popup handling.

**Tech Stack:** Rust, Tokio MPSC, Ratatui, Cargo test, rustfmt.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs` - add the pure leading-token classifier, use it in submission routing, and add direct `handle_key` regression tests.
- Modify: `docs/project-management/yi-agent-tui.md` - record the completed TUI behavior with its verification command.
- Modify: `docs/project-management/README.md` - increase the `yi-agent-tui` completion count from 18 / 19 to 19 / 20.

### Task 1: Add Failing Input-Routing Tests

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs: around the handle_key Submit routing tests`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

- [ ] **Step 1: Add the failing multi-segment path submission test**

  Add this test beside the existing `handle_key Submit` tests. It calls the production routing path directly and asserts both the returned outcome and channel payload.

  ```rust
  #[test]
  fn submit_multi_segment_absolute_path_sends_to_agent() {
      let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
      let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
      let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
      let (decision_tx, _decision_rx) =
          mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
      let is_running = Arc::new(AtomicBool::new(false));
      let mut history = HistoryState::new();
      let mut input = InputLine::new();
      let mut queued = VecDeque::new();
      let mut pending_quit = false;
      let mut popup = None;
      let path = "/Users/name/project explain this";

      input.buffer = path.to_string();
      input.cursor = input.buffer.len();

      let result = handle_key(
          make_key(KeyCode::Enter, KeyModifiers::NONE),
          &mut input, &mut history, 1000, 80, 24, &CostTracker::default(),
          &input_tx, &interrupt_tx, &control_tx, &decision_tx, &is_running,
          &mut queued, &mut pending_quit, &mut popup,
      );

      assert_eq!(result, KeyOutcome::Submit(path.to_string()));
      assert_eq!(input_rx.try_recv().unwrap(), path);
      assert!(matches!(history.cells.as_slice(), [HistoryCell::UserMessage { text }] if text == path));
  }
  ```

- [ ] **Step 2: Run the new test and verify it fails for the expected reason**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_multi_segment_absolute_path_sends_to_agent
  ```

  Expected: FAIL because the current routing produces `KeyOutcome::None` and does not send the path to `input_tx`.

- [ ] **Step 3: Add the single-segment path regression test**

  Add this test using the same channel and `handle_key` setup. It defines the boundary requested in the design.

  ```rust
  #[test]
  fn submit_single_segment_absolute_path_shows_unknown_command() {
      let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
      let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
      let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
      let (decision_tx, _decision_rx) =
          mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
      let is_running = Arc::new(AtomicBool::new(false));
      let mut history = HistoryState::new();
      let mut input = InputLine::new();
      let mut queued = VecDeque::new();
      let mut pending_quit = false;
      let mut popup = None;

      input.buffer = "/tmp".to_string();
      input.cursor = input.buffer.len();

      let result = handle_key(
          make_key(KeyCode::Enter, KeyModifiers::NONE),
          &mut input, &mut history, 1000, 80, 24, &CostTracker::default(),
          &input_tx, &interrupt_tx, &control_tx, &decision_tx, &is_running,
          &mut queued, &mut pending_quit, &mut popup,
      );

      assert_eq!(result, KeyOutcome::None);
      assert!(input_rx.try_recv().is_err());
      assert!(matches!(
          history.cells.as_slice(),
          [HistoryCell::Separator { label: Some(label) }] if label == "未知命令: /tmp"
      ));
  }
  ```

- [ ] **Step 4: Run both focused tests and verify only the new path test fails**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_ -- --nocapture
  ```

  Expected: the multi-segment path test fails; the single-segment path test and existing `submit_` tests pass.

### Task 2: Implement the Minimal Classifier and Routing Guard

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs: immediately before handle_key`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs: InputAction::Submit branch`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

- [ ] **Step 1: Add a pure classifier for path-like leading tokens**

  Insert this helper above `handle_key`:

  ```rust
  fn starts_with_multi_segment_absolute_path(text: &str) -> bool {
      text.split_whitespace()
          .next()
          .is_some_and(|token| token.starts_with('/') && token.matches('/').count() >= 2)
  }
  ```

  This intentionally inspects only the leading token and does not access the filesystem.

- [ ] **Step 2: Guard the existing local-command branch**

  Change the current submit condition from:

  ```rust
  if text.starts_with('/') {
  ```

  to:

  ```rust
  if text.starts_with('/') && !starts_with_multi_segment_absolute_path(&text) {
  ```

  Leave the command name, argument extraction, local execution, and unknown-command separator unchanged. A classified path then falls through to the existing `UserMessage` history insertion and `input_tx.blocking_send` code.

- [ ] **Step 3: Run the focused tests and verify they pass**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_ -- --nocapture
  ```

  Expected: all matching tests pass, including the two regression tests.

- [ ] **Step 4: Run the existing unknown-command UI regression**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::app::tests::unknown_slash_command_shows_error
  ```

  Expected: PASS; `/foo` remains a local unknown-command error.

- [ ] **Step 5: Commit the tested implementation**

  ```bash
  git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
  git commit -m "fix(tui): route multi-segment paths to agent"
  ```

### Task 3: Update Project Tracking and Verify the Change

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/README.md`

- [ ] **Step 1: Record the completed behavior in the TUI module document**

  Add this completed feature entry before the remaining unchecked InlineRenderer item:

  ```markdown
  - [x] 多层绝对路径输入转发 — `tui/app.rs` 在首个空白符前的 token 含至少两个 `/` 时将完整输入发送给 agent；单层 `/tmp` 仍显示 `未知命令`；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_` 和 `cargo test -p yi-agent --bin yi-agent tui::app::tests::unknown_slash_command_shows_error`
  ```

- [ ] **Step 2: Update the module index count**

  In `docs/project-management/README.md`, change the `yi-agent-tui` table value from `18 / 19` to `19 / 20`.

- [ ] **Step 3: Format and run final verification**

  First ensure no other Cargo process is active:

  ```bash
  ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || true
  ```

  Then run:

  ```bash
  cd yi-agent-rs && cargo fmt --all && just fmt-check
  cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_
  cargo test -p yi-agent --bin yi-agent tui::app::tests::unknown_slash_command_shows_error
  ```

  Expected: formatting succeeds and every focused test passes. Do not run workspace-wide tests for this focused TUI routing change.

- [ ] **Step 4: Review the final diff**

  Run:

  ```bash
  git diff --check HEAD~1
  git status --short
  ```

  Expected: no whitespace errors; only the two project-management files remain uncommitted.

- [ ] **Step 5: Commit the tracking documentation**

  ```bash
  git add docs/project-management/yi-agent-tui.md docs/project-management/README.md
  git commit -m "docs: track slash path input routing"
  ```

- [ ] **Step 6: Verify the branch history and working tree**

  Run:

  ```bash
  git status --short --branch
  git log --oneline -3
  ```

  Expected: clean `fix/slash-path-input` worktree with the implementation and tracking commits atop the design and plan commits.
