# TUI History Scrolling Design

## Goal

Make conversation history reliably reviewable while an agent is streaming. The
history pane shows a visible scrollbar and supports mouse-wheel and keyboard
navigation without changing the user's reading position when new output
arrives.

## Scope

The change applies only to the TUI history pane. Input editing, command
popups, and bash task popups retain their existing keyboard and mouse routing.

## Interaction

- A one-column vertical scrollbar is rendered on the right side of the history
  pane whenever its content exceeds the viewport. Its thumb represents the
  visible range and its position represents the current history offset.
- The history content area reserves that column, so text does not render below
  the scrollbar.
- With focus in the normal TUI state, `Up` and `Down` move history by one line;
  `PageUp` and `PageDown` move by one viewport; `Home` jumps to the oldest
  content; `End` jumps to the newest content.
- Mouse-wheel events over the history pane move several lines per wheel event.
  Events outside that pane keep their current behavior.
- A zero history offset means the view is at the newest content and follows
  future output automatically. Any upward movement creates a nonzero offset.
- While the offset is nonzero, newly appended or expanded content increases
  the offset by the number of newly added display lines. The same historical
  text therefore remains on screen rather than jumping to newer output.
- Scrolling down to offset zero, or pressing `End`, restores automatic
  following. `Home` remains clamped to the oldest meaningful viewport.

## Architecture

`HistoryState` owns positioning and exposes helpers for line/page/top/bottom
navigation plus methods that capture and restore a viewport anchor. An anchor
identifies the history cell and rendered line at the top of a non-bottom
viewport; it is content-relative rather than a bottom-origin offset.
`HistoryView` renders content in the reduced text rectangle and renders the
scrollbar in its reserved column. `app::handle_key` and `app::handle_mouse`
map events to the state helpers and calculate a viewport height from the
history layout rectangle.

The width used for wrapping and line counts is the text width, excluding the
scrollbar column when a scrollbar is needed. The same effective width is used
for rendering, maximum-offset calculations, anchor capture, and anchor
restoration. Before any mutation that can change history geometry--including
streaming output, a terminal resize, a queued-preview height change, or the
scrollbar appearing--the app captures the current anchor from the old history
rectangle. After rendering geometry and history content are final, it restores
the anchor using the new rectangle. At the bottom, no anchor is retained and
the offset remains zero.

## Error Handling And Edge Cases

- Content that fits in the viewport has no active scrollbar, an offset of zero,
  and all scroll actions safely clamp to zero.
- Very narrow history panes reserve no scrollbar column when doing so would
  leave no text width.
- Anchor restoration uses saturating arithmetic and clamps after terminal
  resizing or content removal. If the anchored cell is removed, it falls back
  to the nearest valid line while preserving bottom follow when appropriate.
- Existing modal popups continue to consume their own navigation before normal
  history key handling runs.

## Tests

- State tests prove new content preserves a non-bottom reading position and
  still auto-follows at the bottom.
- Rendering tests prove long history reserves a scrollbar column and renders a
  thumb at the expected location.
- Input-routing tests prove the new keyboard shortcuts move the history by the
  correct line or page amount, and mouse-wheel scrolling uses the configured
  multi-line step.
- Anchor tests cover resize reflow, queued-preview growth and promotion,
  first-overflow scrollbar activation, and streaming output below the reader.
- Existing TUI tests continue to cover input, popups, and prior scroll routing.
