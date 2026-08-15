# Compact TUI Completion Status Design

## Goal

Make a manual `/compact` operation in the TUI transition from its existing
"正在压缩对话..." pending state to an explicit success or failure result. Also
make auto-compact completion visible in the TUI history.

## Scope

- Add explicit `AgentEvent` variants for the result of a manually requested
  compaction.
- Emit one of those events from the TUI driver after `compact_session` returns.
- Replace the pending manual-compaction separator in the history with a result
  separator.
- Append a result separator for `AutoCompacting` events.
- Cover successful manual compaction, failed manual compaction, and visible
  auto-compaction completion with unit tests.

The compaction algorithm, thresholds, session rebuilding behavior, and slash
command syntax remain unchanged.

## Event Contract

`yi-agent-core::AgentEvent` gains two variants:

- `ManualCompacted { old_msg_count, new_msg_count }`
- `ManualCompactFailed { message }`

The TUI driver records the message count before calling `compact_session`. On a
successful result it rebuilds the agent as today, then emits `ManualCompacted`.
On failure it logs the error and emits `ManualCompactFailed` instead of the
generic `Error` event. This lets the TUI distinguish a `/compact` failure from
an unrelated agent error.

## TUI History Behavior

When `/compact` is selected, the TUI continues to append exactly one pending
separator labeled `正在压缩对话...`.

When a manual result event arrives, `HistoryState::push_event` finds the most
recent separator bearing that pending label and changes its label in place:

- success: `压缩完成（<old> → <new> 条消息）`
- failure: `压缩失败：<message>`

Changing the existing cell preserves the chronological meaning of the pending
status and avoids a stale "正在" line. History's existing scroll-delta logic
will preserve a scrolled user's viewport if the replacement changes wrapping.

`AutoCompacting { old_msg_count, new_msg_count }` appends a separate separator:
`已自动压缩（<old> → <new> 条消息）`. It has no pending manual separator to
resolve and must never modify a manual `/compact` status.

If a manual result arrives without a pending separator, it is ignored by
history. This is defensive for a TUI shutdown or a future non-TUI caller and
prevents an out-of-context result from adding misleading output.

## Tests

Add focused `HistoryState` tests that first insert the pending separator and
then feed result events. They assert:

1. Manual success replaces the pending label and displays both counts.
2. Manual failure replaces the pending label and displays the error message.
3. Auto-compaction appends its completed label without changing an existing
   manual pending separator.

Run the focused TUI tests and the full `yi-agent` crate tests. Run `cargo fmt
--all` before committing. Update the TUI project-management entry and README
module count in the same implementation commit.
