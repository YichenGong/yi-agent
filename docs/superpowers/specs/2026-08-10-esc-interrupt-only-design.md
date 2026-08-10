# Esc Interrupt-Only Design

## Goal

Make `Esc` cancel only the active agent turn or command. It must never exit the
interactive agent process, including after repeated presses. Keep process exit
on two consecutive `Ctrl+C` presses.

## Key Handling

- `Esc` keeps its existing higher-priority popup behavior: it dismisses an open
  popup without changing the pending-exit state.
- Outside a popup, `Esc` sends an interrupt when an agent turn is active. When
  idle, it does nothing.
- `Esc` never sets or confirms `pending_quit`, so it cannot participate in the
  double-press exit sequence.
- The first `Ctrl+C` still signals an active turn to cancel and sets
  `pending_quit`, displaying the existing confirmation message.
- A second consecutive `Ctrl+C` exits the TUI. Any other key, including `Esc`,
  clears the pending-exit state as the existing generic key handling does.
- `Ctrl+Q` and the `/quit` slash command retain their existing behavior.

## Implementation Boundary

The change is limited to `yi-agent-rs/crates/yi-agent/src/tui/app.rs`:

- Separate `Esc` from the shared quit-key predicate.
- Retain its interrupt sender wiring.
- Update the exit confirmation help text to name only `Ctrl+C`.

No driver, cancellation-token, or process-exit behavior changes are required.

## Tests

Update the TUI event-source regression tests to prove:

1. Repeated `Esc` events do not end the TUI loop; a following `Ctrl+Q` is still
   processed to terminate the test.
2. An `Esc` during an active turn sends an interrupt signal.
3. Two `Ctrl+C` events still exit cleanly.

Run the focused TUI unit tests, then `cargo fmt --all` and the crate test suite
for `yi-agent`.

## Scope

This deliberately does not alter popup dismissal, `Ctrl+Q`, slash commands, or
non-interactive `yi-agent run` behavior.
