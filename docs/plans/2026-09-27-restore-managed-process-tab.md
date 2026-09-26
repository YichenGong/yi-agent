# Fix `main` Clippy Errors + Restore Ctrl+P Managed-Process Tab

**Goal:** Make `cargo clippy --all-targets --all-features -- -D warnings` pass on `main`
by (A) fixing mechanical lints and (B) restoring the `RuntimePopup` cluster that a
merge regression dropped, which is the sole cause of the `dead_code` warnings.

**Architecture:** `app.rs` already carries the process plumbing (`process_manager`,
`process_events`, `process_snapshots`, `is_active_managed_process`, status-bar count).
The tab system (`RuntimePopup` + `process_popup.rs`) is what's missing: `process_popup.rs`
and `bash_popup::detail_line_count` exist but nothing references them. Restoring the
`RuntimePopup` enum from commit `48be050` re-wires them.

**Tech Stack:** Rust, ratatui/crossterm, tokio (`spawn_blocking` TUI thread), clippy 1.97.

**Baseline (authoritative, `cargo clippy --all-targets --all-features`):** 22 warnings across
4 files + the `yi-agent` bin dead-code cluster. See task list.

---

## Task A: Mechanical lints

### A1. `crates/yi-agent/src/tui/cell.rs:253-267` — `if_same_then_else`
Both arms of `if i == 0 { push } else { push }` are identical. Drop the `enumerate`
and the branch:

```rust
let mut lines: Vec<Line<'static>> = {
    let mut header: Vec<Line<'static>> = Vec::new();
    let header_text = format!("? Permission needed: {tool_name}");
    for chunk in wrap_by_display_width(&header_text, w, "", "  ") {
        header.push(Line::from(Span::styled(chunk, warn_style)));
    }
    header
};
```

### A2. `crates/yi-agent-store/src/runtime.rs:3761` — `cloned_ref_to_slice_refs`
`test_repository(&root, &[valid_task.clone()])` → `test_repository(&root, std::slice::from_ref(&valid_task))`.

### A3. `crates/yi-agent-store/tests/subagent_ipc_protocol.rs:109,135` — `needless_borrows_for_generic_args`
`Daemon::start` takes `impl AsRef<Path>`; drop the `&`:
`&directory.path().join("runtime.sqlite")` → `directory.path().join("runtime.sqlite")`.

### A4. `crates/yi-agent-store/tests/runtime_coordinator.rs`
- `2217-2218` `needless_question_mark`: remove the `Ok(...?)` wrapper:
  ```rust
  String::from_utf8(output.stdout)
      .map_err(|error| WorkerError::Startup(format!("Git workspace error: {error}")))
  ```
- `2116` / `3410` `await_holding_lock`: clippy 1.97 does not honour `drop(guard)`, so the
  guard is treated as held to end of function. Scope the guard in a block so its lexical
  scope ends before the awaits (keep the asserts inside the block).

### A5. `crates/yi-agent-core/tests/subagent_supervisor.rs`
- `64` `cloned_ref_to_slice_refs`: `&[child.clone()]` → `std::slice::from_ref(&child)`.
- `689` `bool_assert_comparison`: `assert_eq!(..., false)` → `assert!(!...is_terminal())`.
- `155` `await_holding_lock`: wrap the two asserts in a block (guard scope ends before
  the awaits at 170/177).
- `449` `await_holding_lock`: `subscribe_messages(&self) -> WorkerMailbox` returns owned;
  bind it, then `.recv().await` (guard drops at end of the `let` statement).
- `598` `await_holding_lock`: `start_worker(&mut self)` must hold the guard across the
  await. Add `#[allow(clippy::await_holding_lock)]` on the test fn with a one-line
  justification (matches repo convention at `runtime.rs:670`).

---

## Task B: Restore `RuntimePopup`

Reference: `48be050:yi-agent-rs/crates/yi-agent/src/tui/app.rs` (saved at `/tmp/app_48be050.rs`).

### B1. Imports
```rust
use super::process_popup::{
    ConfirmProcessKill as ConfirmProcessKillPopup, ProcessDetailPopup, ProcessListPopup,
    ProcessPopup, RuntimeTab,
};
```

### B2. State
Replace `let mut bash_popup: BashPopup = BashPopup::None;` with
```rust
let mut runtime_popup = RuntimePopup::None;
let mut process_outputs: std::collections::HashMap<String, yi_agent_tools::ProcessReadResult> =
    std::collections::HashMap::new();
```
Keep `process_snapshots`; refresh via `refresh_process_snapshots(...)`.

### B3. `RuntimePopup` enum + helpers (`is_none`, `bash`, `blocks_text_input`, `switch_tab`, `tab`),
`switch_runtime_tab`, `switch_runtime_tab_for_test`, `refresh_process_snapshots`.

### B4. Render helpers: `render_runtime_popup`, `render_existing_bash_popup` (extract the inline
bash-popup render from the draw closure), `render_kill_confirmation_overlay` (extract the inline
confirm box).

### B5. Key handling: `handle_runtime_popup_key` (returns `Option<String>` kill id),
`handle_runtime_popup_key_for_test`, `process_detail_max_scroll`.

### B6. `handle_bash_popup_key` gains a `detail_width: u16` param; Down/mouse use
`super::bash_popup::detail_line_count(t, detail_width)` instead of the inline `so + se + 6`.

### B7. `handle_mouse` takes `&mut RuntimePopup` + `processes` + `process_outputs`; adds the
process-detail scroll branch. Update 7 test call sites.

### B8. `handle_paste` takes `&RuntimePopup` and uses `blocks_text_input()`. Update 3 test call sites.

### B9. Event loop: Ctrl+P opens `RuntimePopup::Bash(BashPopup::List(...))`; Tab switches tab;
`handle_runtime_popup_key` result drives `process_manager.kill(...)`; render via
`render_runtime_popup`.

### B10. Restore 3 tests: `ctrl_p_process_tab_kill_confirmation_sends_process_id`,
`runtime_popup_tab_switches_between_bash_and_processes`, `process_runtime_popup_blocks_text_input`.

---

## Task C: Docs

`docs/project-management/yi-agent-tui.md`: line 40 (`Ctrl+P managed process tab`) — fix the
verification command to the restored test name; line 42 (`Ctrl+P 仅显示 Bash 任务`) — update to
reflect `route_event` tracks all tools (test `test_route_event_tracks_all_tool_calls`).

---

## Verification

1. `cargo clippy --all-targets --all-features -- -D warnings` → clean.
2. `cargo test -p yi-agent --bin yi-agent` (TUI tests) → pass.
3. `cargo test -p yi-agent-core`, `cargo test -p yi-agent-store` → pass.
4. `cargo fmt --all` → no diff.
5. Commit + `git merge --no-ff` back to `main`.
