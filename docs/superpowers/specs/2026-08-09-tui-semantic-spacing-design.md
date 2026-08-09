# TUI Semantic Spacing Design

## Goal

Make the TUI conversation history easier to scan by using vertical space to
separate semantic groups rather than by inserting a blank line after every
low-level event. In particular, a model response that follows completed tool
work must begin after one blank line.

## Scope

- Keep each user prompt visually separated from the following activity.
- Keep an assistant's pre-tool explanation, tool call, and tool result compact
  as one work group.
- Insert exactly one blank line before assistant text that resumes after one or
  more tool results in the same agent run.
- Preserve a compact layout for consecutive tool calls and their results.
- Preserve independent visibility for existing system separator cells, such as
  compaction, interruption, and error statuses.
- Preserve scroll anchoring, folding, Markdown rendering, and event contracts.

The agent loop, tool execution order, status bar, and non-interactive `run`
output are unchanged.

## Layout Rules

History layout is derived from cell boundaries plus a small amount of
presentation state in `HistoryState`:

1. A `UserMessage` has a blank spacer after it when later history exists.
2. Assistant text remains adjacent to the immediately preceding content unless
   tool work has completed since the last assistant text block.
3. A `ToolCall` and its `ToolResult` have no spacer between them. Multiple tool
   calls and results also remain compact.
4. The first `AssistantText` received after one or more `ToolResult` events
   starts a new assistant cell, preceded by one blank spacer. Later streamed
   chunks append to that same cell and do not add spacers.
5. A completed `Done` separator is not followed by an additional blank spacer.
   Existing labeled system separators retain their current visual treatment.

This gives the transcript the following rhythm:

```text
> User prompt

Assistant explanation
  tool invocation
  tool result

Assistant follow-up / final answer
```

## State and Rendering

`HistoryState` will track whether tool results have created a pending
assistant-response boundary. `ToolResult` sets that boundary. The next
`AssistantText` consumes it by inserting a dedicated blank spacer cell (or an
equivalent layout marker) before creating a new assistant message. The marker
is only emitted when there is visible earlier content, preventing a leading
blank line.

The existing flattened-line and viewport-anchor logic will recognize this
spacer as belonging to the preceding semantic group. Its height therefore
participates consistently in total line counts, scroll limits, scrollbar
calculations, and reflow anchoring.

## Error Handling

- A tool failure follows the same grouping rule as a successful tool result:
  the next assistant response is visually separated from the failed work.
- Cancellation, errors, maximum-turn completion, and compaction retain their
  current separator labels and do not leave an accidental second blank line.
- An assistant message that streams without prior tool results retains its
  current compact behavior.

## Tests

Add focused `HistoryState` and rendering tests that assert:

1. User prompts retain their one-line boundary before subsequent activity.
2. A tool call and matching result stay adjacent.
3. The first assistant text after one or more tool results has exactly one
   blank line before it; subsequent chunks have none.
4. Consecutive tool work creates only one boundary before the resumed assistant
   message.
5. A failed tool result follows the same boundary rule.
6. Spacer-aware flattened line counts and scroll anchoring remain correct.

Run the focused `yi-agent` TUI tests, `cargo fmt --all`, and the project
format check. Update `docs/project-management/yi-agent-tui.md` and the README
module count in the implementation commit.
