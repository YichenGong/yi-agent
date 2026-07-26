# TUI Copy/Paste Compatibility Design

Date: 2026-07-26

## Goal

Support practical copy and paste in the current `main` branch's ratatui TUI without rewriting the interface into a line-oriented CLI.

The target behavior is Codex-like in spirit:

- Do not capture mouse input unless the application truly needs it.
- Let the terminal emulator keep native text selection and copy behavior.
- Use bracketed paste so pasted text is delivered as a single paste event instead of a stream of typed keys.
- Optionally provide an app-level copy path via OSC 52 for remote-terminal friendliness.

## Current State

The current TUI setup in `yi-agent-rs/crates/yi-agent/src/tui/app.rs` does the following:

- Enables raw mode before entering the TUI.
- Enters the alternate screen.
- Enables mouse capture.
- Handles `Event::Mouse` for history and bash-popup scrolling.
- Does not handle `Event::Paste`.

This blocks the terminal's native mouse selection path because mouse events are routed to the application instead of the terminal emulator. It also means paste arrives as ordinary key events unless the terminal/application path emits a structured paste event.

## Recommended Approach

Use an incremental Codex-style compatibility layer while keeping the existing ratatui TUI.

This avoids the larger line-CLI rewrite and preserves existing work in:

- structured history rendering,
- queued input preview,
- status bar,
- slash command popup,
- bash task popup,
- permission prompts.

## Terminal Mode Changes

### Remove Mouse Capture

In the real terminal setup path:

- Remove `EnableMouseCapture` from startup.
- Remove `DisableMouseCapture` from teardown.
- Keep `EnterAlternateScreen` and `LeaveAlternateScreen` for now.

This should restore native mouse drag selection in terminals that allow selection in alternate-screen applications when mouse reporting is disabled.

The existing `Event::Mouse` branch and `handle_mouse` can stay temporarily. Without mouse capture, most terminals will not send application mouse events. Keeping the code reduces initial churn. A later cleanup can remove mouse-scrolling behavior and its tests if we decide terminal-native selection is more important than in-app wheel scrolling.

### Do Not Add Alternate Scroll Initially

Do not enable DECSET 1007 in the first patch.

Rationale:

- The current app still uses ratatui's full-screen history region.
- Wheel-to-arrow translation varies by terminal.
- The immediate priority is reliable text selection and paste.

If users miss wheel scrolling after mouse capture is disabled, add a follow-up experiment with alternate scroll and verify behavior in iTerm2, Terminal.app, VS Code terminal, and common SSH flows.

## Paste Handling

### Enable Bracketed Paste

In TUI startup:

- Execute `EnableBracketedPaste` after raw mode is enabled.

In teardown:

- Execute `DisableBracketedPaste` even if the loop returns an error.

This lets crossterm surface paste as `Event::Paste(String)` instead of pretending that pasted text was typed one key at a time.

### Insert Pasted Text Into Input

Add `InputLine::insert_str(&str)` to `yi-agent-rs/crates/yi-agent/src/tui/input.rs`.

Behavior:

- Insert the pasted string at the current cursor byte offset.
- Advance the cursor by the pasted string's byte length.
- Preserve UTF-8 correctness by relying on the existing invariant that `cursor` is always on a char boundary.
- Preserve newlines in the buffer for the first version.

The current renderer already wraps the input buffer visually. If multi-line pasted content exposes rendering issues, handle those as a follow-up by either normalizing newlines to spaces or teaching the input renderer to display hard line breaks.

### Event Loop Routing

In `run_loop`:

- Add a `Some(Event::Paste(text))` branch near the `Event::Key` branch.
- If a bash popup is active, ignore paste for the first version.
- If a permission prompt is pending, ignore paste for the first version.
- Otherwise, insert the pasted text into `InputLine` and call `sync_popup(&mut popup, &input.buffer)`.
- Clear `pending_quit` after a paste, because paste indicates the user is continuing input rather than confirming exit.

This avoids accidental permission decisions or popup interactions caused by pasted text.

## Copy Handling

### Native Copy Is Primary

The primary copy path is terminal-native selection:

- User drags/selects visible text in the terminal.
- User invokes the terminal's copy shortcut, such as Cmd+C on macOS.
- The app does not participate.

This path is available because mouse capture is not enabled.

### OSC 52 Is Optional App-Level Copy

Add a small clipboard helper only if app-level copy is desired in the same implementation pass.

Suggested file:

- `yi-agent-rs/crates/yi-agent/src/tui/clipboard.rs`

Suggested function:

```rust
pub fn copy_to_clipboard_osc52(text: &str) -> std::io::Result<()> {
    // Emit OSC 52: ESC ] 52 ; c ; base64 BEL
}
```

Use OSC 52 as the first implementation instead of adding `arboard` immediately.

Rationale:

- OSC 52 works across many local and SSH terminal setups.
- It avoids GUI clipboard dependencies in headless or CI environments.
- It keeps the first patch focused.

A later enhancement can add `arboard` as a local-first path and fall back to OSC 52, matching Codex more closely.

### Copy Target

If app-level copy is included, bind it to a shortcut that does not conflict with interrupt semantics.

Recommended first shortcut:

- `Alt+C`: copy the currently selected history cell as plain text.

Avoid using `Ctrl+C`; it already means interrupt/quit in this TUI.

The selected-cell machinery already exists for `Ctrl+O` folding, so copying the selected cell is a natural first target.

If no selected cell exists, the command should do nothing or add a small separator/status message such as `已复制当前消息` only on success.

## Error Handling

Terminal teardown must be best-effort and must restore modes in this order:

1. Disable bracketed paste.
2. Disable raw mode.
3. Leave alternate screen.

If one cleanup step fails, still attempt the remaining steps.

Paste insertion should not fail for normal UTF-8 strings because crossterm delivers a `String`.

OSC 52 copy should surface I/O failures as non-fatal UI feedback, not crash the TUI.

## Testing

### Unit Tests

Add tests for `InputLine::insert_str`:

- Inserts into an empty buffer.
- Inserts at the cursor in the middle of a buffer.
- Preserves UTF-8 text.
- Preserves multi-line pasted text.
- Advances cursor correctly by byte length.

### Event Loop Tests

Add or adapt tests around the fake event source:

- `Event::Paste("hello")` updates the input buffer.
- Paste clears `pending_quit`.
- Paste updates slash popup state when pasted text begins with `/`.
- Paste is ignored while a permission prompt is pending.
- Paste is ignored while a bash popup is active.

### Manual Verification

Run the TUI in a real terminal and verify:

- Mouse drag selection works for visible TUI text.
- Terminal copy shortcut copies selected text.
- Pasting short text inserts it into the input field.
- Pasting multi-line text does not submit automatically.
- Slash commands pasted into the input still trigger slash popup behavior.
- Exiting the app restores terminal state.

Recommended terminals for manual coverage:

- iTerm2 on macOS.
- Terminal.app on macOS.
- VS Code integrated terminal.
- SSH session in at least one terminal that supports OSC 52, if app-level copy is implemented.

## Non-Goals

This design does not rewrite the TUI into a line-oriented CLI.

This design does not guarantee terminal scrollback for the alternate screen. Native scrollback is part of the larger line-CLI redesign and remains out of scope.

This design does not initially preserve in-app mouse wheel scrolling. If mouse capture is removed, wheel events are no longer guaranteed to reach the app.

This design does not add image paste or clipboard file detection. Those are separate enhancements.

## Implementation Order

1. Add `InputLine::insert_str` with unit tests.
2. Enable bracketed paste in setup and disable it in teardown.
3. Add `Event::Paste` routing in the TUI loop.
4. Remove mouse capture from startup and teardown.
5. Run unit tests and targeted TUI tests.
6. Manually verify copy and paste in a real terminal.
7. Optionally add OSC 52 app-level copy as a follow-up patch.

## Open Follow-Ups

- Decide whether OSC 52 copy belongs in the first implementation or a second small patch.
- Decide whether to remove `handle_mouse` and mouse-scroll tests immediately or keep them as dormant code until line-CLI work resumes.
- Evaluate DECSET 1007 only after confirming the behavior loss from removing mouse capture.
