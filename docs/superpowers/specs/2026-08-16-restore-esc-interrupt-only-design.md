# Restore Esc Interrupt-Only Design

## Goal

Restore the TUI behavior accidentally reverted by the attached-runtime merge:
`Esc` cancels an active agent turn but never exits the interactive process,
even after repeated presses.

## Root Cause

Commit `036cd2b` separated `Esc` from the quit-confirmation state machine. It
made Esc dismiss a slash popup first and otherwise send a non-blocking interrupt
only if `is_running` is true. Commit `64fd813` subsequently restored the older
combined Esc/Ctrl+C implementation in `tui/app.rs`: Esc sets `pending_quit` and
a second Esc returns `KeyOutcome::Quit`.

## Behavior

- Outside a popup, Esc sends one interrupt signal while an agent task is
  active. Repeated Esc presses may coalesce into the bounded interrupt channel;
  none can arm or confirm process exit.
- Outside a popup, Esc does nothing while idle.
- Esc keeps its existing higher-priority popup behavior: it dismisses an open
  slash-command popup without interrupting or changing `pending_quit`.
- Ctrl+C continues to arm `pending_quit` on its first press and exits on a
  second consecutive Ctrl+C. When a task is active, the first Ctrl+C also
  signals its interrupt.
- Ctrl+Q and `/quit` are unchanged.
- The exit confirmation copy names Ctrl+C only.

The runtime-startup prompt and managed-process popups are intentionally outside
this change because they intercept keys before `handle_key`; their explicit Esc
cancel/dismiss behavior is unchanged.

## Implementation Boundary

Modify only `yi-agent-rs/crates/yi-agent/src/tui/app.rs`:

1. Replace the Esc branch in `handle_key` with the interrupt-only branch from
   the prior fix. Use `try_send(())`, not `blocking_send(())`, so repeated
   presses cannot block the synchronous TUI loop when the channel already holds
   an interrupt.
2. Keep the Ctrl+C branch as the sole owner of `pending_quit` and process exit.
3. Restore the confirmation text to `再按 Ctrl+C 退出`.
4. Replace stale tests encoding the old double-Esc exit semantics with tests for
   repeat Esc survival and active-turn interrupt delivery.

## Tests

The TUI event-source test will feed Esc, Esc, a normal character, and Ctrl+Q.
It proves the loop still processes the normal character after repeated Esc and
only terminates at Ctrl+Q. Unit tests will assert that an active-turn Esc sends
an interrupt without arming `pending_quit`, that idle Esc does nothing, and
that two Ctrl+C presses still exit.

Run `cargo fmt --all`, the focused Esc/Ctrl+C unit tests, and `cargo test -p
yi-agent` from `yi-agent-rs/`.
