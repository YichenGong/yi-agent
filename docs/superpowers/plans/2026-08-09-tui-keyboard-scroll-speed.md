# TUI Keyboard Scroll Speed Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make unmodified history `Up` and `Down` events move three display lines so terminals that map trackpad scrolling to arrow keys scroll the TUI at a useful speed.

**Architecture:** Keep mouse capture disabled and retain the existing `HistoryState` clamping APIs. Introduce a named keyboard scroll-step constant in the TUI app, use it only in the unmodified `Up`/`Down` history-key route, and prove the change through the existing direct `handle_key` unit test.

**Tech Stack:** Rust, crossterm 0.28, ratatui 0.29, Cargo unit tests, Markdown project tracking.

---

## File Structure

- `yi-agent-rs/crates/yi-agent/src/tui/app.rs` owns TUI key routing and its
  direct regression test.
- `docs/bug-list.md` records the user-facing scroll-speed issue and its
  resolution.
- `docs/project-management/yi-agent-tui.md` records verifiable TUI behavior.
- `docs/project-management/README.md` remains `17 / 18`: this fixes the
  already-completed history-scrolling feature rather than adding a checklist
  feature.

### Task 1: Specify the Three-Line Key Behavior

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:3790`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:3790`

- [ ] **Step 1: Change the existing navigation expectation before implementation**

  In `normal_navigation_keys_route_to_history_without_affecting_shift_selection`,
  replace the first two expected offsets in the table with:

  ```rust
  for (key, expected_offset) in [
      (KeyCode::Up, 8),
      (KeyCode::Down, 5),
      (KeyCode::PageUp, 25),
      (KeyCode::PageDown, 5),
      (KeyCode::Home, 100),
      (KeyCode::End, 0),
  ] {
  ```

- [ ] **Step 2: Run the focused test and verify the expected failure**

  Run:

  ```bash
  cd yi-agent-rs
  cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection
  ```

  Expected: FAIL because `KeyCode::Up` still produces `scroll_offset == 6`
  instead of the new expected offset of `8`.

### Task 2: Route Keyboard History Scrolling by the Named Step

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:26`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:891`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:3790`

- [ ] **Step 1: Add the keyboard step constant beside the mouse step**

  Change the constants to:

  ```rust
  const HISTORY_WHEEL_LINES: usize = 3;
  const HISTORY_KEY_LINES: usize = 3;
  ```

- [ ] **Step 2: Replace the two unmodified history-key literals**

  Change the two existing branches to:

  ```rust
  KeyCode::Up if key.modifiers.is_empty() => {
      history.scroll_up(HISTORY_KEY_LINES, max_scroll_offset);
      return KeyOutcome::None;
  }
  KeyCode::Down if key.modifiers.is_empty() => {
      history.scroll_down(HISTORY_KEY_LINES);
      return KeyOutcome::None;
  }
  ```

  Do not alter mouse capture, `handle_mouse`, bash-popup behavior, modified
  arrow keys, or paging keys.

- [ ] **Step 3: Format and verify the focused regression test**

  Run:

  ```bash
  cd yi-agent-rs
  cargo fmt --all
  cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection
  ```

  Expected: PASS with one test passed. The test demonstrates that Up moves
  five to eight, Down restores five, and Shift+Up preserves selection routing.

- [ ] **Step 4: Run the TUI app test module**

  Run:

  ```bash
  cd yi-agent-rs
  cargo test -p yi-agent --bin yi-agent tui::app::tests
  ```

  Expected: PASS with no test failures.

### Task 3: Record the Verified Resolution

**Files:**
- Modify: `docs/bug-list.md:9`
- Modify: `docs/project-management/yi-agent-tui.md:45`
- Verify: `docs/project-management/README.md:16`

- [ ] **Step 1: Mark the resolved bug with its verification command**

  Replace the open scroll-speed entry with:

  ```markdown
  - [x] 上下scroll速度过慢 — `tui/app.rs` 让未修饰 `Up` / `Down` 每次滚动 3 行，以支持终端将触控板滚动转换为方向键的模式；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection`
  ```

- [ ] **Step 2: Extend the existing completed history-scrolling feature**

  Append this clause to the existing `对话历史滚动与滚动条` feature line in
  `docs/project-management/yi-agent-tui.md`:

  ```markdown
  ；未修饰 `Up` / `Down` 每次滚动 3 行，适配终端将触控板滚动转换为方向键的行为；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection`
  ```

- [ ] **Step 3: Confirm the module-index count remains correct**

  Keep the `yi-agent-tui` count in `docs/project-management/README.md` at
  `17 / 18`, because this task extends an existing completed feature and does
  not add a new checklist item.

- [ ] **Step 4: Validate documentation and commit the implementation group**

  Run:

  ```bash
  git diff --check
  cd yi-agent-rs
  cargo fmt --all
  git add crates/yi-agent/src/tui/app.rs ../docs/bug-list.md ../docs/project-management/yi-agent-tui.md
  git commit -m "fix(tui): increase keyboard history scroll step"
  ```

  Expected: the commit contains only the key-routing regression test and
  project-tracking updates; the separate design and implementation-plan commits
  remain unchanged.

### Task 4: Verify the Committed Branch

**Files:**
- Verify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Verify: `docs/bug-list.md`
- Verify: `docs/project-management/yi-agent-tui.md`

- [ ] **Step 1: Confirm the committed diff and working tree**

  Run:

  ```bash
  git show --check --stat HEAD
  git status --short
  ```

  Expected: no whitespace errors and a clean worktree.

- [ ] **Step 2: Re-run the full targeted TUI verification from the committed state**

  Run:

  ```bash
  cd yi-agent-rs
  cargo test -p yi-agent --bin yi-agent tui::app::tests
  ```

  Expected: PASS with no failures.
