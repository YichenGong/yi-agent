# Compact History Redesign

Date: 2026-08-15
Status: approved for planning

## Problem

The current auto-compaction logic preserves the most recent N user turns. When one user request causes a long tool loop, there may be only one user message. The current splitter cannot find the required user-turn boundary, returns the original session unchanged, and can repeatedly emit a misleading no-op auto-compaction event while the context continues to grow.

The trigger token count is also local to one `Agent::run()` invocation. A session that ends near the threshold is not reliably compacted before its next user turn.

## Goals

- Support effective compaction in a long, single-user, multi-tool loop.
- Trigger compaction both before a new user turn and between tool-loop THINK requests.
- Preserve the user task and enough recent, raw tool context for reliable continuation.
- Keep compacted histories valid for both Anthropic and OpenAI message protocols.
- Never replace a session or emit a successful compaction event when compaction is a no-op.

## Non-goals

- Match Codex remote compaction or token-budget-window implementations.
- Add an exact local tokenizer.
- Truncate retained raw tool calls or tool results.
- Change normal non-compaction history construction.

## Reference model

The design follows the relevant local Codex pattern:

- Compact before a new turn and in the middle of a continuing tool loop.
- Ask the model for a handoff-oriented checkpoint summary.
- Rebuild history around preserved user intent, a rolling summary, and safe retained context.

Unlike Codex's summary-only replacement, yi-agent also retains a bounded raw suffix of complete tool interactions.

## Compacted history

The replacement history is constructed as:

```text
User: merged preserved real user messages
Assistant: [conversation summary] + first retained tool-use message, if any
Tool: matching retained tool results
Assistant/Tool: remaining retained complete tool-interaction suffix
```

If there is no retained tool suffix, it is:

```text
User: merged preserved real user messages
Assistant: [conversation summary]
```

This shape avoids adjacent user or assistant messages after Anthropic maps `Role::Tool` to `user`, while preserving the OpenAI assistant-tool-call/tool-result relationship.

### Preserved real user messages

- A real user message is a `Role::User` message that is not a previously generated conversation summary.
- Select real user messages newest-first up to `20_000` approximate tokens, then restore chronological order.
- A user message is atomic: if the next older message does not fit, omit it rather than truncate it.
- Merge selected messages into one User message with stable `[用户消息]` section labels. This prevents consecutive user messages on Anthropic while retaining the original text.
- The user budget must be positive. A session with no selected user messages is not compacted, because task intent would have no durable anchor.

### Retained raw tool suffix

A complete tool interaction unit is:

```text
Assistant(text and one or more ToolUse blocks)
+ all subsequent ToolResult blocks for exactly those ToolUse IDs
```

- Select complete units newest-first, limited by `12_000` approximate tokens, and restore chronological order.
- Assistant text, tool names, inputs, and all result content count toward the budget and are retained verbatim.
- Do not retain a partial unit, truncate a unit, orphan a result, or retain an unmatched tool use.
- If the newest complete unit alone exceeds `12_000` tokens, retain that complete unit as the sole budget exception; do not retain older units.
- Plain assistant text with no ToolUse is summarized rather than included in the raw suffix.

### Summary placement

The new summary is an Assistant text block prefixed with `[对话摘要]`.

- If the tool suffix is empty, it is one Assistant message after the merged User anchor.
- If the suffix is non-empty, prepend the summary text to the first retained Assistant message. That message already contains ToolUse blocks, so the result is a valid `User -> Assistant(tool_use) -> Tool` start without adjacent Assistant messages.

## Planning and validation

`compact.rs` gains a pure planning phase before any provider call:

```rust
plan_compaction(messages, user_budget, tool_budget) -> Option<CompactionPlan>
```

The plan contains the full pre-compaction history for summary input, retained user material, retained complete tool suffix, and predicted replacement shape.

No provider request is made if the plan cannot remove at least one original history message.

A provider-neutral validator runs on the replacement history before it replaces the session. It must verify:

1. The first message is User.
2. There are no adjacent User or adjacent Assistant messages.
3. Each retained ToolResult references a preceding retained ToolUse.
4. Each retained ToolUse has exactly one retained ToolResult.
5. The summary merge leaves tool IDs, names, inputs, and results unchanged.

Validation failure leaves the original session untouched and surfaces a compaction error. The implementation must not silently discard tool history to make validation pass.

## Summary request

The summary call uses the configured provider and model, no tool schemas, and `system: None`. It formats the complete pre-compaction history as text and asks for a checkpoint handoff, not a user-facing answer.

The prompt must require:

- current goal, progress, and key decisions;
- user constraints and preferences;
- changed/read files, commands, tests, and important results;
- failures, unresolved work, and concrete next steps;
- information from discarded tool history that the next model needs;
- concise structured output targeting at most `8_000` tokens.

The raw suffix is retained separately, so the prompt says not to reproduce its large outputs unnecessarily. The 8,000-token requirement is prompt-only: there is no local hard truncation of the generated summary.

Existing summaries remain in the summary input so repeated compaction produces a rolling replacement summary, but are excluded from preserved real-user selection.

## Triggering and session token state

Move the most recent provider-reported input token count from the local run loop into `Session`:

```rust
last_input_tokens: Option<u32>
```

Each completed provider stream with `Usage` updates this state.

Run the shared auto-compaction check at:

1. pre-turn: after the new user message is added to the session and before the first THINK request;
2. mid-turn: after tools have completed and before the next THINK request.

An auto-compaction attempt requires an enabled positive threshold and `Session.last_input_tokens >= compact_threshold`. Planning must also show a real reduction.

After a successful replacement, clear `last_input_tokens`. This prevents repeated compaction before a new model request reports fresh usage. A failed compaction changes neither the history nor token state and may retry at the next check.

## Configuration

Replace turn-count retention with explicit token budgets:

```text
compact_user_budget_tokens = 20000
compact_tool_budget_tokens = 12000
```

Expose them as:

```text
--compact-user-budget-tokens
YI_AGENT_COMPACT_USER_BUDGET_TOKENS

--compact-tool-budget-tokens
YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS
```

`compact_user_budget_tokens` must be positive. `compact_tool_budget_tokens` may be zero, which retains no raw tool suffix.

Remove `compact_keep_turns` from core configuration. For one compatibility release, the CLI/environment reader accepts the old option but ignores it and emits a single deprecation warning; new configuration and documentation use only the two budgets.

## Events and UI

Keep the existing user-facing event shape:

```rust
AgentEvent::AutoCompacting {
    old_msg_count: usize,
    new_msg_count: usize,
}
```

Emit it only after a validated replacement with a genuinely shorter history. The TUI continues displaying:

```text
已自动压缩（old -> new 条消息）
```

Record retained user/tool token estimates in tracing only. Manual `/compact` uses the same planner, summary call, reconstruction, and validation path.

## Tests

### Core planner and reconstruction tests

1. One User plus a long tool loop compacts; preserves the user task, a summary, and the valid raw suffix.
2. Tool retention selects newest complete units until the 12,000-token budget, with no fixed unit count.
3. A newest unit exceeding 12,000 tokens remains whole and excludes older units.
4. Parallel ToolUse blocks and all matching results remain together.
5. User selection uses newest-first 20,000-token budget and never truncates a User message.
6. Existing summaries are input to the next summary but are not preserved as real user input.
7. Summary merges into the first retained tool-use Assistant message without adjacent Assistant roles.
8. Empty tool suffix reconstructs `User -> Assistant(summary)`.
9. Validator rejects consecutive User/Assistant roles, orphan results, and unmatched ToolUse blocks.
10. No real reduction produces no plan and no provider request.

### Agent-loop tests

1. A single-user mid-turn tool loop crosses the threshold, compacts, and completes the same task.
2. A completed run near threshold compacts before the next user turn's first THINK request.
3. Successful compaction clears session token state and does not immediately repeat.
4. Summary-provider failure leaves session/token state intact and the loop continues.
5. Auto-compaction event is absent for a no-op plan and present only for a real reduction.

### Provider serialization tests

For a compacted history with a retained tool suffix:

- Anthropic serialization alternates user/assistant after mapping Tool to user and preserves tool-use/result pairing.
- OpenAI serialization preserves assistant `tool_calls` and matching `tool_call_id` messages.

## Documentation and verification

Update `docs/project-management/yi-agent-core.md` and the README module-count table in the implementation commit. Required verification includes `cargo fmt --all`, focused `yi-agent-core` and provider serialization tests, then relevant `yi-agent` tests.
