# Restore Esc Interrupt-Only Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore Esc as an interrupt-only TUI key so repeated Esc never terminates the interactive process.

**Architecture:** Keep process-exit state exclusively in the Ctrl+C branch of `handle_key`. The Esc branch first dismisses the slash popup, then non-blockingly sends the existing interrupt signal when `is_running` is true, without reading or writing `pending_quit`. Event-source and unit regression tests lock the behavioral boundary.

**Tech Stack:** Rust, Tokio bounded MPSC channel, Crossterm key events, Ratatui TestBackend, Cargo tests.

## Global Constraints

- Work only in the isolated `fix/esc-interrupt-only` worktree; do not modify `main`.
- Do not change runtime-startup, managed-process popup, Ctrl+Q, `/quit`, driver cancellation, or non-interactive behavior.
- Esc must use `interrupt_tx.try_send(())`, ensuring a full bounded channel cannot block the synchronous TUI event loop.
- Ctrl+C remains the sole two-press process-exit path.
- Run `cargo fmt --all` before committing.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
  - Owns synchronous TUI key dispatch, exit confirmation rendering, and the relevant Ratatui/Tokio regression tests.
- Create: `docs/superpowers/specs/2026-08-16-restore-esc-interrupt-only-design.md`
  - Records the merge regression, required behavior, scope boundary, and test strategy.

### Task 1: Lock the Esc regression with tests

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:3998-4038,5500-5590`

**Interfaces:**
- Consumes: `run_tui_with_backend_and_events`, `FakeEventSource`, `handle_key`, `KeyOutcome`, `AtomicBool`, and `tokio::sync::mpsc::Sender<()>` already defined in `app.rs`.
- Produces: Regression tests that fail while Esc arms `pending_quit` or returns `KeyOutcome::Quit`.

- [ ] **Step 1: Replace the end-to-end double-Esc exit test with a repeated-Esc survival test**

In `esc_same_as_ctrl_c`, rename the test to `repeated_esc_does_not_quit`. Feed events in reverse-pop order so the source yields Esc, Esc, `x`, Ctrl+Q:

```rust
let events = Rc::new(RefCell::new(vec![
    Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
    Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
    Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
    Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
]));
```

After `run_tui_with_backend_and_events` returns, read row 23 from the TestBackend and assert it contains `x`. Ctrl+Q is the explicit terminator; seeing `x` proves neither Esc ended the loop.

- [ ] **Step 2: Run the new survival test and verify the current regression fails**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent repeated_esc_does_not_quit --bin yi-agent
```

Expected: FAIL because the second Esc returns `KeyOutcome::Quit` before `x` is processed.

- [ ] **Step 3: Strengthen focused handle_key assertions**

Rename `esc_when_running_sends_interrupt` to `esc_interrupts_active_agent_without_arming_quit` and add:

```rust
assert!(!pending_quit, "Esc must not arm process exit");
```

Rename `esc_when_idle_does_not_send_interrupt` to `esc_when_idle_does_nothing` and add:

```rust
assert!(!pending_quit, "idle Esc must not arm process exit");
```

Rename `double_esc_quits` to `repeated_esc_does_not_quit_from_handle_key` and change its final assertion to:

```rust
assert_eq!(result, KeyOutcome::None);
```

- [ ] **Step 4: Run the focused unit tests and verify they fail**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent 'tui::app::tests::esc_' --bin yi-agent
cargo test -p yi-agent tui::app::tests::repeated_esc_does_not_quit_from_handle_key --bin yi-agent
```

Expected: the renamed tests compile but fail because current Esc sets `pending_quit` and the second Esc quits.

### Task 2: Restore interrupt-only Esc dispatch

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:835-860,1125-1145`

**Interfaces:**
- Consumes: `is_running: &Arc<AtomicBool>`, `interrupt_tx: &tokio::sync::mpsc::Sender<()>`, `pending_quit: &mut bool`, `popup: &mut Option<CommandPopup>`.
- Produces: `handle_key` behavior where Esc only dismisses a popup or non-blockingly signals an active task; Ctrl+C alone controls `pending_quit` and `KeyOutcome::Quit`.

- [ ] **Step 1: Implement the minimal Esc-only branch**

Replace the Esc arm of the global key match with:

```rust
KeyCode::Esc => {
    // Popup dismissal takes precedence over cancelling an agent turn.
    if popup.is_some() {
        *popup = None;
        return KeyOutcome::None;
    }
    if is_running.load(std::sync::atomic::Ordering::SeqCst) {
        // Cancellation is idempotent; coalesce repeated Esc presses.
        let _ = interrupt_tx.try_send(());
    }
    return KeyOutcome::None;
}
```

Do not modify the subsequent Ctrl+C arm: it retains the first-press interrupt plus `pending_quit` behavior and second-press `KeyOutcome::Quit` behavior.

- [ ] **Step 2: Update the pending-exit confirmation copy**

In `build_input_line`, replace:

```rust
Span::styled("再按 Ctrl+C 或 Esc 退出", Style::new().fg(Color::Yellow)),
```

with:

```rust
Span::styled("再按 Ctrl+C 退出", Style::new().fg(Color::Yellow)),
```

- [ ] **Step 3: Run all targeted behavior tests**

Run:

```bash
cd yi-agent-rs
cargo test -p yi-agent repeated_esc_does_not_quit --bin yi-agent
cargo test -p yi-agent 'tui::app::tests::esc_' --bin yi-agent
cargo test -p yi-agent tui::app::tests::two_ctrl_c_quits --bin yi-agent
```

Expected: all selected tests PASS.

- [ ] **Step 4: Format and run crate verification**

Run:

```bash
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent
```

Expected: formatter exits 0 and all `yi-agent` unit/integration tests pass. Existing unrelated compiler warnings may remain, but no test may fail.

- [ ] **Step 5: Inspect the resulting change and commit it**

Run:

```bash
git diff --check
git diff -- docs/superpowers/specs/2026-08-16-restore-esc-interrupt-only-design.md yi-agent-rs/crates/yi-agent/src/tui/app.rs
git status --short
git add docs/superpowers/specs/2026-08-16-restore-esc-interrupt-only-design.md yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "fix(tui): restore Esc interrupt-only behavior"
```

Expected: no whitespace errors; diff contains only the design record, Esc branch, confirmation copy, and matching regression tests.

## Self-Review

- Spec coverage: Task 1 covers repeated Esc survival, interrupt delivery, idle Esc, and preserves two-Ctrl+C exit. Task 2 restores the exact key dispatch and confirmation copy while excluding runtime popups, Ctrl+Q, `/quit`, and driver behavior.
- Placeholder scan: no unresolved implementation markers or unspecified test cases are present.
- Type consistency: every listed test uses existing `KeyEvent`, `KeyCode`, `KeyModifiers`, `KeyOutcome`, Tokio sender, and `AtomicBool` types from `app.rs`.
