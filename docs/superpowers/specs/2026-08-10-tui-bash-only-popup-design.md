# Ctrl+P Bash-Only Popup Design

## Goal

Keep the `Ctrl+P` popup focused on Bash tasks. Other tools, such as `edit`,
`grep`, and `read`, must not appear in its list or have an incomplete detail
view.

## Scope

- Register a task in `RunningTaskRegistry` only when an `AgentEvent::ToolCall`
  has the tool name `bash`.
- Preserve the existing Bash list, detail view, real-time stdout/stderr,
  lifecycle status, timeout indicator, scrolling, and kill action.
- Keep the popup's Bash-specific copy and API names; do not add a generic
  tool-detail renderer or retain parameters/results for non-Bash tools.
- Update the registry test suite with a regression test proving that the
  event-routing layer ignores a non-Bash tool call.

## Data Flow

`route_event` receives every `ToolCall`, but filters on `name == "bash"`
before extracting `command` and registering the task. Later output, exit,
timeout, and result events use their tool-call ID; when the ID was not
registered, the registry already treats those events as no-ops. Therefore, no
additional event filtering or state is necessary.

## Error Handling

The change does not alter tool execution or error handling. Bash lifecycle
events retain their current behavior. Events from non-Bash tools are harmlessly
ignored by the popup registry, while the normal conversation history continues
to render their calls and results.

## Verification

- A focused `route_event` test asserts that a `grep` tool call does not add a
  popup task while a `bash` tool call still does.
- Run `cargo test -p yi-agent --bin yi-agent tui::app::tests` and
  `cargo fmt --all` from `yi-agent-rs/`.
- Record the completed behavior in `docs/project-management/yi-agent-tui.md`
  and increment its count in `docs/project-management/README.md` when the
  implementation is complete.
