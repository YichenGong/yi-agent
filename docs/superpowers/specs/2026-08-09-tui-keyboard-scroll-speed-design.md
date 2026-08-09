# TUI Keyboard Scroll Speed Design

Date: 2026-08-09

## Goal

Make TUI conversation-history scrolling move three display lines for each
unmodified `Up` or `Down` key event. This improves trackpad scrolling in
terminals that translate trackpad movement into arrow-key events while keeping
ordinary keyboard navigation predictable.

## Background

The real TUI deliberately does not enable crossterm mouse capture so terminal
native text selection and copy continue to work. Consequently, the existing
`HISTORY_WHEEL_LINES` constant does not control normal real-terminal trackpad
input. Some terminals translate alternate-screen trackpad scrolling into arrow
keys, which currently reach `handle_key` and move history by one line.

## Design

Define one named keyboard history-scroll step of three lines in
`crates/yi-agent/src/tui/app.rs`. Apply it only to unmodified `KeyCode::Up`
and `KeyCode::Down` events in `handle_key`.

The key handler will continue to call the existing `HistoryState` methods, so
their maximum-offset clamping and bottom clamping semantics remain unchanged.

## Scope

Included:

- Conversation-history `Up` and `Down` key scrolling changes from one to three
  display lines per event.
- Regression coverage for both directions.
- Project tracking updates: mark the matching bug-list entry complete and
  record the verification command in the TUI module status document.

Excluded:

- Mouse capture, including `EnableMouseCapture` and `DisableMouseCapture`.
- The existing mouse-event route and its three-line constant.
- Bash-popup navigation and scrolling.
- `Ctrl+U`, `Ctrl+D`, `PageUp`, `PageDown`, `Home`, and `End` behavior.

## Testing

Extend the existing `normal_navigation_keys_route_to_history_without_affecting_shift_selection`
test. With an initial history offset of five, an unmodified `Up` must produce
an offset of eight, and the subsequent `Down` must restore five. The test
continues to assert that Shift+Up uses selection behavior rather than history
scrolling.

Run the focused test before and after the implementation, then run the TUI app
test module. Format Rust sources with `cargo fmt --all` before committing.
