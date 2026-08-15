# Slash Path Input Design

## Goal

Allow a TUI prompt that starts with a multi-segment absolute path to reach the
agent instead of being rejected as an unknown slash command.

## Classification Rule

On submission, inspect only the first whitespace-delimited token.

- If it does not start with `/`, submit it as an ordinary user message.
- If it starts with `/` and contains two or more `/` characters, submit the
  complete original input as an ordinary user message. For example,
  `/Users/name/project` and `/foo/bar explain this` are agent prompts.
- Otherwise, continue through the existing local slash-command parser. Known
  commands run locally; unknown names keep the existing `未知命令: <input>`
  history separator. Thus `/tmp` remains an explicit unknown-command error.

The slash-command completion popup remains unchanged. This feature only changes
how submitted text is routed.

## Implementation

Add a small, focused predicate near the TUI input submission handling that
identifies path-like leading tokens. Guard the existing slash-command branch
with that predicate so that path-like text falls through to the existing normal
message enqueue/send logic. Do not query the filesystem: paths need not already
exist, and routing should be deterministic.

## Error Handling

Non-path text that starts with `/` preserves current behavior. Unknown local
commands are not sent to the agent and appear in history as `未知命令: <input>`.

## Tests

Add focused TUI submission tests that verify:

1. A multi-segment absolute path is submitted to `input_tx` and appears as a
   user message rather than an unknown-command separator.
2. A single-segment absolute path such as `/tmp` is not submitted and produces
   the existing unknown-command separator.
3. An existing local command is still routed locally.

Run the focused TUI test target and the formatting check before integration.

## Project Tracking

Record the completed regression behavior in `docs/project-management/yi-agent-tui.md`
and increase the TUI module count in `docs/project-management/README.md`.
