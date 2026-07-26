# Tool Result TUI End Time Design

## Goal

Freeze a TUI-tracked tool's elapsed time when its `ToolResult` arrives, including non-streaming tools such as `web_search`.

## Design

The TUI registers every `AgentEvent::ToolCall`, while only process-oriented tools such as `bash` emit `ToolExit` or `ToolTimeout`. `web_search` uses the default `Tool::call_stream`, which returns a result without a streaming terminal event. The registry therefore keeps it running until a later turn-end cleanup.

Add a result-finalization operation to `RunningTaskRegistry`. It changes only a `Running` task, records `Instant::now()` as its `end_time`, and sets its status to `Done` or `Failed` from `ToolResult::is_error`. It must leave tasks already finalized by `ToolExit` or `ToolTimeout` unchanged.

`route_event` invokes this operation for `AgentEvent::ToolResult`. The history behavior remains unchanged: it continues to render the tool result independently.

## Error Handling

An unknown result ID is ignored, matching the existing registry behavior. A failed tool result records `Failed`; no synthetic process exit code is assigned because non-process tools have none.

## Testing

Add a route-event regression test that sends `ToolCall(web_search)` followed by `ToolResult`, verifies a completed status and a frozen elapsed duration, then confirms a later `Done` cannot overwrite it. Existing tests cover `ToolExit`, timeout, cancellation, and error terminal paths.
