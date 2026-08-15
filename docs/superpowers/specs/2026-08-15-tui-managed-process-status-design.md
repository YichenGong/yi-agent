# TUI Managed Process Status Design

## Goal

Show the count of active agent-managed background processes in the TUI status bar. The indicator is textual only and does not display elapsed time.

## Scope

- Count managed-process snapshots whose status is `Starting`, `Running`, or `Ready`.
- Render the indicator as `N proc running`, including when `N` is one.
- Omit the indicator when no managed process is active.
- Preserve the existing foreground Bash task activity indicator, including its tool name and elapsed time.
- Do not count foreground Bash tool calls in the managed-process indicator.
- Do not render elapsed time for managed processes.

## Data Flow

`app.rs` already refreshes `process_snapshots` from the shared `ProcessManager`. Before each status-bar render, it derives the active managed-process count from that snapshot collection. Terminal process statuses (`Exited`, `Killed`, and `FailedToStart`) are excluded.

`render_statusbar` receives the derived count as a value. It appends `N proc running` after any foreground Bash task indicator and before the prefill/decode/model fields. The renderer does not need process metadata or timing information.

Example with both kinds of activity:

```text
● bash 4.2s  2 proc running  prefill 1,024  decode 64  model
```

## Error Handling

The status bar consumes snapshots already supplied by `ProcessManager`; it performs no process I/O and introduces no new failure paths. If no snapshot is available yet, the count is zero and the indicator is omitted. Stale snapshots are handled consistently with the existing TUI refresh cycle.

## Testing

Add focused status-bar rendering tests that verify:

- zero active managed processes omit `proc running`;
- one and multiple active managed processes render `1 proc running` and `N proc running` exactly;
- the process indicator has no duration text;
- the existing foreground Bash task indicator remains visible alongside the process count.

Run `cargo fmt --all` and `cargo test -p yi-agent --bin yi-agent tui::statusbar::tests` from `yi-agent-rs/`. Update the TUI project-management record and its README count when implementation is complete.
