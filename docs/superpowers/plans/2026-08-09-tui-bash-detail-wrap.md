# Bash Detail Content Wrapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render complete Bash commands, stdout, and stderr in a Ctrl+P task detail by wrapping all long physical lines to the available pane width.

**Architecture:** Keep `bash_popup.rs` responsible for transforming a `TaskState` into display lines. Add a Unicode display-width-aware line wrapper there and use it for every content section. Pass the detail `Rect` from `app.rs` into the renderer and use the same wrapped line count as the keyboard and mouse scroll limit.

**Tech Stack:** Rust 2024, Ratatui 0.29, `unicode-width`, cargo test, cargo fmt.

---

### Task 1: Cover wrapped detail rendering

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs:151-212`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs:230-306`

- [ ] **Step 1: Write failing render tests for command, stdout, stderr, and CJK wrapping**

  Add a test helper that creates a `RunningTaskRegistry` task, renders its detail into a narrow `TestBackend`, and reads the buffer text. Add these tests:

  ```rust
  #[test]
  fn detail_wraps_long_command_stdout_and_stderr() {
      let task = task_with_output(
          "printf 'this command must remain fully visible'",
          "stdout-with-a-single-very-long-line",
          "stderr-with-a-single-very-long-line",
      );
      let buffer = render_detail_to_buffer(&task, Rect::new(0, 0, 16, 20));
      assert_contains_in_order(&buffer, "printf", "visible");
      assert_contains_in_order(&buffer, "stdout", "long-line");
      assert_contains_in_order(&buffer, "stderr", "long-line");
  }

  #[test]
  fn detail_wraps_cjk_at_display_width() {
      let task = task_with_output("echo 一二三四五六七", "", "");
      let lines = detail_lines(&task, 10);
      assert!(lines.iter().all(|line| display_width(line) <= 10));
  }
  ```

- [ ] **Step 2: Run the focused tests and verify they fail**

  Run: `cargo test -p yi-agent --bin yi-agent tui::bash_popup::tests::detail_`

  Expected: FAIL because the detail renderer still accepts no width and emits each long command/output line as one `Line`.

### Task 2: Wrap all detail content and synchronize scrolling

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs:151-212`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:340-360`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:468-550`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:556-610`

- [ ] **Step 1: Add a Unicode display-width wrapping helper and a shared detail-line builder**

  In `bash_popup.rs`, add `detail_lines(task: &TaskState, width: u16) -> Vec<Line<'static>>`. Move the existing header, warning, section labels, empty markers, and footer construction into it. Add a helper that splits on explicit newlines and breaks long words character-by-character using `UnicodeWidthChar::width`, so every line is at most `width` display columns. Invoke it for command, stdout, and stderr; command continuations use a two-column indentation after the first `$ ` line.

  ```rust
  pub fn detail_line_count(task: &TaskState, width: u16) -> usize {
      detail_lines(task, width).len()
  }

  pub fn render_detail_popup(
      popup: &DetailPopup,
      task: &TaskState,
      area: Rect,
  ) -> Paragraph<'static> {
      Paragraph::new(detail_lines(task, area.width))
          .alignment(Alignment::Left)
          .scroll((popup.scroll as u16, 0))
  }
  ```

- [ ] **Step 2: Pass the detail area into both renderer call sites**

  In `app.rs`, change both calls so the renderer receives `detail_area`:

  ```rust
  super::bash_popup::render_detail_popup(p, task, detail_area)
  ```

- [ ] **Step 3: Use wrapped visual lines as the keyboard and mouse scroll maximum**

  Replace both `stdout.lines().count() + stderr.lines().count() + 6` approximations with the shared count:

  ```rust
  super::bash_popup::detail_line_count(t, history_area.width)
  ```

  In the key handler, pass the history-pane width into `handle_bash_popup_key`, so Down uses that width. Keep `f` as follow-to-bottom and preserve existing upward scrolling behavior.

- [ ] **Step 4: Run focused tests and verify they pass**

  Run: `cargo test -p yi-agent --bin yi-agent tui::bash_popup::tests::detail_`

  Expected: PASS, including the narrow-buffer and CJK wrapping cases.

- [ ] **Step 5: Run surrounding TUI tests**

  Run: `cargo test -p yi-agent --bin yi-agent tui::bash_popup::tests`

  Expected: PASS with all Bash popup state and rendering tests green.

### Task 3: Record the delivered TUI feature

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md:39-45`
- Modify: `docs/project-management/README.md:16`

- [ ] **Step 1: Update the TUI feature list and total**

  Add a completed feature stating that `tui/bash_popup.rs` wraps command, stdout, and stderr by display width in the Ctrl+P detail view, with the focused test command as its verification criterion. Change the TUI index count from `17 / 18` to `18 / 19`.

- [ ] **Step 2: Format and run final verification**

  Run:

  ```bash
  cargo fmt --all
  just fmt-check
  cargo test -p yi-agent --bin yi-agent tui::bash_popup::tests
  git diff --check
  ```

  Expected: all commands exit 0.

- [ ] **Step 3: Commit the implementation**

  ```bash
  git add yi-agent-rs/crates/yi-agent/src/tui/bash_popup.rs \
          yi-agent-rs/crates/yi-agent/src/tui/app.rs \
          docs/project-management/yi-agent-tui.md \
          docs/project-management/README.md
  git commit -m "fix(tui): wrap bash detail content"
  ```
