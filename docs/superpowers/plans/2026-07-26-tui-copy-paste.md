# TUI Copy/Paste Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the existing ratatui TUI accept bracketed paste while returning mouse selection and copy to the terminal emulator.

**Architecture:** Keep the full-screen ratatui interface and its current key, popup, permission, and history behavior. `InputLine` gains a string insertion primitive, and `run_loop` routes `Event::Paste` through one focused helper that refuses paste while a permission or bash popup owns input. The real-terminal wrapper enables bracketed paste, stops enabling mouse capture, and restores terminal modes best-effort.

**Tech Stack:** Rust, crossterm 0.28 events and terminal commands, ratatui, tokio test channels, ratatui `TestBackend`.

---

## File Structure

- `yi-agent-rs/crates/yi-agent/src/tui/input.rs` owns UTF-8-safe input-buffer mutation. Add `InputLine::insert_str` and focused unit tests here.
- `yi-agent-rs/crates/yi-agent/src/tui/app.rs` owns terminal-mode lifecycle and TUI event dispatch. Add bracketed-paste lifecycle commands, paste routing, a private paste handler, and event-loop tests here.
- `docs/superpowers/specs/2026-07-26-tui-copy-paste-design.md` is the approved design reference; it is not modified by this implementation.

### Task 1: Insert Pasted Text into the Input Editor

**Files:**

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/input.rs:85-89`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/input.rs:208-314`

- [ ] **Step 1: Add failing tests for string insertion at empty, middle, UTF-8, and multi-line positions**

  Add these tests immediately after `insert_in_middle`:

  ```rust
  #[test]
  fn insert_str_inserts_at_cursor_and_advances_by_bytes() {
      let mut inp = InputLine::new();
      inp.buffer = "ac".into();
      inp.cursor = 1;

      inp.insert_str("b");

      assert_eq!(inp.buffer, "abc");
      assert_eq!(inp.cursor, 2);
  }

  #[test]
  fn insert_str_preserves_utf8_and_newlines() {
      let mut inp = InputLine::new();
      inp.buffer = "你好".into();
      inp.cursor = "你".len();

      inp.insert_str("\nworld");

      assert_eq!(inp.buffer, "你\nworld好");
      assert_eq!(inp.cursor, "你\nworld".len());
  }
  ```

- [ ] **Step 2: Run the new tests and verify they fail because `insert_str` does not exist**

  Run: `cd yi-agent-rs && cargo test -p yi-agent tui::input::tests::insert_str -- --nocapture`

  Expected: compilation failure stating that no method named `insert_str` exists on `InputLine`.

- [ ] **Step 3: Add the minimal insertion method that preserves the existing cursor invariant**

  Add this method directly after `insert_char` in `yi-agent-rs/crates/yi-agent/src/tui/input.rs`:

  ```rust
  pub fn insert_str(&mut self, text: &str) {
      let byte_idx = self.cursor;
      self.buffer.insert_str(byte_idx, text);
      self.cursor += text.len();
  }
  ```

  Do not normalize line breaks or alter input history in this task. `cursor` is already maintained on a UTF-8 character boundary by the editor, and crossterm supplies valid UTF-8 `String` data.

- [ ] **Step 4: Run the focused editor tests and verify they pass**

  Run: `cd yi-agent-rs && cargo test -p yi-agent tui::input::tests -- --nocapture`

  Expected: all `tui::input::tests` pass, including the two new `insert_str` tests.

- [ ] **Step 5: Commit the editor change**

  ```bash
  git add yi-agent-rs/crates/yi-agent/src/tui/input.rs
  git commit -m "feat: add TUI pasted-text insertion"
  ```

### Task 2: Route Paste Events without Bypassing Input Ownership

**Files:**

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:313-370`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:837-860`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1619-3070`

- [ ] **Step 1: Add failing unit tests for the private paste-routing helper**

  Add these tests near the existing permission-handling tests in `app.rs`. They use state-local assertions, so they do not depend on exact terminal rows:

  ```rust
  #[test]
  fn paste_inserts_text_clears_pending_quit_and_syncs_slash_popup() {
      let mut input = InputLine::new();
      let history = HistoryState::new();
      let bash_popup = BashPopup::None;
      let mut pending_quit = true;
      let mut popup = None;

      handle_paste(
          "/cl".into(),
          &mut input,
          &history,
          &bash_popup,
          &mut pending_quit,
          &mut popup,
      );

      assert_eq!(input.buffer, "/cl");
      assert_eq!(input.cursor, 3);
      assert!(!pending_quit);
      assert_eq!(popup.unwrap().filtered(), &[SlashCommand::Clear]);
  }

  #[test]
  fn paste_is_ignored_while_permission_or_bash_popup_is_active() {
      let mut input = InputLine::new();
      let mut history = HistoryState::new();
      history.push_event(make_permission_request_normal(7), 80);
      let mut pending_quit = true;
      let mut popup = None;

      handle_paste(
          "blocked".into(),
          &mut input,
          &history,
          &BashPopup::None,
          &mut pending_quit,
          &mut popup,
      );
      assert!(input.buffer.is_empty());
      assert!(pending_quit);

      let history = HistoryState::new();
      let bash_popup = BashPopup::List(ListPopup::new(vec!["task-1".into()]));
      handle_paste(
          "still blocked".into(),
          &mut input,
          &history,
          &bash_popup,
          &mut pending_quit,
          &mut popup,
      );
      assert!(input.buffer.is_empty());
      assert!(pending_quit);
  }
  ```

- [ ] **Step 2: Run the new tests and verify they fail because `handle_paste` does not exist**

  Run: `cd yi-agent-rs && cargo test -p yi-agent tui::app::tests::paste_ -- --nocapture`

  Expected: compilation failure stating that `handle_paste` is not found.

- [ ] **Step 3: Implement the private paste handler next to `sync_popup`**

  Add this function immediately before `sync_popup` in `yi-agent-rs/crates/yi-agent/src/tui/app.rs`:

  ```rust
  fn handle_paste(
      text: String,
      input: &mut InputLine,
      history: &HistoryState,
      bash_popup: &BashPopup,
      pending_quit: &mut bool,
      popup: &mut Option<CommandPopup>,
  ) {
      if !matches!(bash_popup, BashPopup::None) || history.pending_permission_info().is_some() {
          return;
      }

      input.insert_str(&text);
      *pending_quit = false;
      sync_popup(popup, &input.buffer);
  }
  ```

  This deliberately has no submit path: pasted newlines remain input text and cannot select a permission option or act on a bash popup.

- [ ] **Step 4: Add the `Event::Paste` dispatch branch to the loop**

  In the `match events.poll(...)` block, insert this branch after the existing `Some(Event::Key(key))` branch and before `Some(Event::Mouse(mouse))`:

  ```rust
  Some(Event::Paste(text)) => {
      handle_paste(
          text,
          input,
          history,
          &bash_popup,
          &mut pending_quit,
          &mut popup,
      );
  }
  ```

  Do not remove `Event::Mouse` or `handle_mouse` in this task. Mouse capture will no longer be requested on a real terminal, but retaining the branch preserves existing test-backend behavior and avoids unrelated churn.

- [ ] **Step 5: Add an event-loop test proving `Event::Paste` reaches the input renderer**

  Add this test beside the slash-popup event-loop tests. `ScriptedEvents` consumes its `Vec` with `pop`, so list the quit event before the paste event:

  ```rust
  #[test]
  fn bracketed_paste_renders_input_and_opens_slash_popup() {
      let backend = TestBackend::new(80, 24);
      let mut terminal = Terminal::new(backend).unwrap();
      let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
      let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
      let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
      let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
      let (decision_tx, _decision_rx) =
          tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
      let is_running = Arc::new(AtomicBool::new(false));
      let source = ScriptedEvents {
          events: Rc::new(RefCell::new(vec![
              Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
              Event::Paste("/cl".into()),
          ])),
      };

      run_tui_with_backend_and_events(
          &mut terminal,
          &mut agent_rx,
          &input_tx,
          &interrupt_tx,
          &control_tx,
          &decision_tx,
          &is_running,
          &source,
      )
      .unwrap();

      let text = collect_all_text(&terminal);
      assert!(text.contains("/cl"), "expected pasted input, got: {text:?}");
      assert!(text.contains("clear"), "expected slash popup, got: {text:?}");
  }
  ```

- [ ] **Step 6: Run paste routing tests and verify they pass**

  Run: `cd yi-agent-rs && cargo test -p yi-agent tui::app::tests::paste_ -- --nocapture && cargo test -p yi-agent tui::app::tests::bracketed_paste -- --nocapture`

  Expected: all three targeted tests pass.

- [ ] **Step 7: Commit paste event handling**

  ```bash
  git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
  git commit -m "feat: handle bracketed paste in TUI"
  ```

### Task 3: Restore Native Mouse Selection and Manage Bracketed-Paste Mode

**Files:**

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1-76`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1619-3070`

- [ ] **Step 1: Add a failing compile-time lifecycle test by replacing the terminal commands in `run_tui`**

  Change the crossterm imports to the target command names before adding their cleanup implementation:

  ```rust
  use std::io::{self, stdout};

  use crossterm::event::{
      self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent,
      KeyModifiers, MouseEvent, MouseEventKind,
  };
  ```

  Change startup to this incomplete target form:

  ```rust
  execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
  ```

  Then temporarily leave the old `DisableMouseCapture` teardown reference in place and run the command in Step 2. It must fail because that command is no longer imported. This confirms the old mouse-capture teardown cannot survive the change.

- [ ] **Step 2: Run the targeted build and verify the old teardown reference fails**

  Run: `cd yi-agent-rs && cargo test -p yi-agent tui::app::tests::bracketed_paste_renders_input_and_opens_slash_popup --no-run`

  Expected: compilation failure mentioning `DisableMouseCapture` is not in scope.

- [ ] **Step 3: Replace teardown with ordered, best-effort restoration**

  Replace the current cleanup block from `// Always restore terminal state, even on error` through `result` with:

  ```rust
  // Try every cleanup step so a failed write cannot leave the terminal in another mode.
  let mut cleanup_error: Option<io::Error> = None;
  if let Err(error) = execute!(terminal.backend_mut(), DisableBracketedPaste) {
      cleanup_error.get_or_insert(error);
  }
  if let Err(error) = disable_raw_mode() {
      cleanup_error.get_or_insert(error);
  }
  if let Err(error) = execute!(terminal.backend_mut(), LeaveAlternateScreen) {
      cleanup_error.get_or_insert(error);
  }

  match (result, cleanup_error) {
      (Err(error), _) => Err(error),
      (Ok(()), Some(error)) => Err(error),
      (Ok(()), None) => Ok(()),
  }
  ```

  This disables bracketed paste, then raw mode, then leaves the alternate screen. It intentionally removes both `EnableMouseCapture` and `DisableMouseCapture`; no DECSET 1007 alternate-scroll command is introduced.

- [ ] **Step 4: Run formatting and the complete crate test suite**

  Run: `cd yi-agent-rs && cargo fmt --check && cargo test -p yi-agent`

  Expected: formatting check succeeds and all `yi-agent` crate tests pass.

- [ ] **Step 5: Commit terminal-mode compatibility changes**

  ```bash
  git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
  git commit -m "fix: preserve terminal mouse selection in TUI"
  ```

### Task 4: Verify in Real Terminal Emulators

**Files:**

- Modify: none
- Test: real terminal execution of `yi-agent-rs` binary

- [ ] **Step 1: Build the binary used for manual verification**

  Run: `cd yi-agent-rs && cargo build -p yi-agent`

  Expected: build completes successfully and produces the debug executable.

- [ ] **Step 2: Verify native selection and copy in a macOS terminal**

  Run the normal TUI command used by this repository in iTerm2 or Terminal.app. Drag-select visible history text, then use the emulator copy shortcut (`Cmd+C`). Paste into another application.

  Expected: the selected text is copied by the terminal emulator; mouse dragging does not trigger TUI history scrolling.

- [ ] **Step 3: Verify bracketed paste behavior**

  Paste `hello`, then paste this two-line text into the composer without pressing Enter:

  ```text
  first line
  second line
  ```

  Expected: each paste appears in the input editor, no submit occurs automatically, and pressing Enter once submits the accumulated buffer.

- [ ] **Step 4: Verify slash command and teardown behavior**

  Paste `/cl` into an empty composer, confirm the `clear` slash-command popup appears, then exit with `Ctrl+Q`. Return to the shell and type a normal command.

  Expected: the popup opens after paste, and the shell has normal input and terminal behavior after exit.

## Out of Scope for This Plan

- OSC 52 application-level copying and a selected-history-cell copy shortcut.
- `arboard` local clipboard integration.
- DECSET 1007 alternate-scroll mode.
- Removing dormant `handle_mouse` code and mouse-scroll tests.
- Replacing the full-screen TUI with the separate line-CLI redesign.

