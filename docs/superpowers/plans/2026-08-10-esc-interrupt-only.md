# Esc Interrupt-Only Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Esc` interrupt an active TUI agent turn without ever exiting the process, while retaining two consecutive `Ctrl+C` presses as the exit gesture.

**Architecture:** Keep all behavior in the TUI key-routing state machine. Split the `Esc` branch from the exit state transition: dismiss an active popup first, otherwise signal the existing interrupt channel only while the agent is running. Retain `pending_quit` exclusively for Ctrl+C and update its user-facing hint.

**Tech Stack:** Rust, crossterm key events, ratatui test backend, Tokio MPSC channels, Cargo.

---

### Task 1: Establish Esc Regression Coverage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:2349-2384`

- [ ] **Step 1: Replace the legacy Esc-exits regression with a failing no-exit test**

  Rename `esc_same_as_ctrl_c` to `repeated_esc_does_not_quit` and use this event sequence so that direct `Ctrl+Q` is the only terminating event:

  ```rust
  let events = Rc::new(RefCell::new(vec![
      Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
      Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
      Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
  ]));
  ```

  Keep the existing `run_tui_with_backend_and_events` call, then assert its result is `Ok` with the message `repeated Esc must not quit the TUI`.

- [ ] **Step 2: Run the test and verify it fails**

  Run from `yi-agent-rs/`:

  ```bash
  ps aux | rg '[c]argo|[r]ustc|[y]i_agent' || true
  cargo test -p yi-agent --bin yi-agent tui::app::tests::repeated_esc_does_not_quit
  ```

  Expected: FAIL because the second `Esc` returns `KeyOutcome::Quit` before the scripted `Ctrl+Q` event is consumed.

### Task 2: Prove Esc Still Interrupts Active Work

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:tests module`

- [ ] **Step 1: Add a failing direct key-handler test for active work**

  Build the same `InputLine`, `HistoryState`, channel, and `AtomicBool::new(true)` fixtures used by nearby `handle_key` tests. Call:

  ```rust
  let result = handle_key(
      KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
      &mut input,
      &mut history,
      0,
      80,
      20,
      &cost_tracker,
      &input_tx,
      &interrupt_tx,
      &control_tx,
      &decision_tx,
      &is_running,
      &mut VecDeque::new(),
      &mut pending_quit,
      &mut None,
  );
  assert_eq!(result, KeyOutcome::None);
  assert!(interrupt_rx.try_recv().is_ok());
  assert!(!pending_quit);
  ```

  Name it `esc_interrupts_active_agent_without_arming_quit`.

- [ ] **Step 2: Run the test and verify it fails**

  ```bash
  ps aux | rg '[c]argo|[r]ustc|[y]i_agent' || true
  cargo test -p yi-agent --bin yi-agent tui::app::tests::esc_interrupts_active_agent_without_arming_quit
  ```

  Expected: FAIL because current `Esc` sets `pending_quit` to `true`.

### Task 3: Separate Esc From Process Exit

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:740-790`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:1133-1140`

- [ ] **Step 1: Implement the minimal Esc branch**

  Keep the permission pass-through predicate recognizing `Ctrl+Q`, `Ctrl+C`, and `Esc` so Esc reaches global cancellation handling; then replace the global Esc arm/quit branch with:

  ```rust
  KeyCode::Esc => {
      // Popup dismissal takes precedence over cancelling an agent turn.
      if popup.is_some() {
          *popup = None;
      } else if is_running.load(std::sync::atomic::Ordering::SeqCst) {
          let _ = interrupt_tx.blocking_send(());
      }
      return KeyOutcome::None;
  }
  ```

  Keep the Ctrl+C branch as the sole branch that sets or confirms `pending_quit`.

- [ ] **Step 2: Update the confirmation text**

  Replace the input-row text with:

  ```rust
  Span::styled("再按 Ctrl+C 退出", Style::new().fg(Color::Yellow)),
  ```

- [ ] **Step 3: Re-run both focused tests and verify they pass**

  ```bash
  ps aux | rg '[c]argo|[r]ustc|[y]i_agent' || true
  cargo test -p yi-agent --bin yi-agent tui::app::tests::repeated_esc_does_not_quit tui::app::tests::esc_interrupts_active_agent_without_arming_quit tui::app::tests::two_ctrl_c_quits
  ```

  Expected: PASS; Esc neither exits nor arms quit, while two Ctrl+C presses still exit.

### Task 4: Record the Completed TUI Behavior

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md:16,34`
- Modify: `docs/bug-list.md:17`

- [ ] **Step 1: Replace stale two-key wording in the TUI module description**

  Change the scope bullet to `两步退出确认（仅 Ctrl+C 两次退出）` and change the completed feature entry to state `Ctrl+C 两次才退出；Esc 只打断运行中的 agent 或命令，不退出进程`, including this verification command:

  ```text
  cargo test -p yi-agent --bin yi-agent tui::app::tests::repeated_esc_does_not_quit
  ```

- [ ] **Step 2: Mark the matching bug-list item complete**

  Replace its unchecked entry with:

  ```markdown
  - [x] 两次ESC不应该直接退出Agent的进程。ESC可以打断命令执行，可以打断对话，但是不应该退出整体进程。— `yi-agent-rs/crates/yi-agent/src/tui/app.rs` 将 Esc 与 `pending_quit` 分离；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::repeated_esc_does_not_quit`
  ```

### Task 5: Validate and Commit

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/bug-list.md`

- [ ] **Step 1: Format and run the complete crate test suite serially**

  ```bash
  cd yi-agent-rs
  ps aux | rg '[c]argo|[r]ustc|[y]i_agent' || true
  cargo fmt --all
  just fmt-check
  ps aux | rg '[c]argo|[r]ustc|[y]i_agent' || true
  cargo test -p yi-agent
  ```

  Expected: formatting check and all `yi-agent` tests pass.

- [ ] **Step 2: Check the patch and create the implementation commit**

  ```bash
  git diff --check
  git add yi-agent-rs/crates/yi-agent/src/tui/app.rs docs/project-management/yi-agent-tui.md docs/bug-list.md
  git commit -m "fix(tui): keep Esc from exiting the agent"
  ```

  Expected: a conventional commit containing the key-routing regression tests, behavior change, and progress documentation.
