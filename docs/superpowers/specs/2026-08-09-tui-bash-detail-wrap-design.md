# Bash Detail Content Wrapping Design

## Goal

Make every Bash task detail fully readable in the TUI. A long command or a
long physical line in stdout or stderr must wrap to the available detail-pane
width instead of being clipped at the right edge.

## Scope

- Keep the Ctrl+P task list compact; its command summary remains truncated.
- In an individual task detail, render the command under the `$ ` prefix over
  as many visual lines as needed.
- Wrap each stdout and stderr line to the detail pane width, preserving source
  newlines as separate lines.
- Keep the existing vertical scrolling and follow-at-bottom behavior. Wrapped
  lines contribute to the scrollable height.
- Do not introduce horizontal scrolling or alter task execution, output
  capture, status, or exit-code handling.

## Design

`render_detail_popup` will accept the available `Rect` and build `Line` values
whose text is already wrapped to its width. A focused helper will wrap text by
terminal display width, including Unicode-wide characters, and supports a
first-line prefix plus an indentation prefix for continuation lines.

The command uses `$ ` for its first visual line and spaces for continuations.
The stdout and stderr headings remain separate; their content wraps without a
prefix so copied output stays visually faithful. Empty content retains the
existing `(empty)` marker.

The popup layout caller supplies the same detail area used for rendering. The
existing scroll offset continues to be measured in visual lines, so no new
input handling or state is required.

## Tests

Add render-level tests that draw a detail popup into a narrow Ratatui buffer
and assert that:

1. a long Bash command appears across multiple rows without losing text;
2. long one-line stdout and stderr each appear across multiple rows; and
3. Unicode-wide command text wraps within the configured display width.

Run the focused TUI test module plus formatting and the project-required
progress-document update check after implementation.

## Project Tracking

Update `docs/project-management/yi-agent-tui.md` with the completed detail
wrapping feature and increment the matching count in
`docs/project-management/README.md`.
