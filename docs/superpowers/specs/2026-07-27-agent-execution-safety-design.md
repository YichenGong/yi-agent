# Agent Execution Safety Design

## Goal

Make tool cancellation, headless permission handling, provider failures, and
shell working-directory state consistent with the agent's visible state.

## Decisions

- Add a cancellation token to the streaming tool API. Tools that do not need
  cancellation keep the default behavior; `BashTool` uses it to terminate the
  running process when the agent is cancelled.
- In headless mode, do not attach a confirmation receiver. A blacklisted
  command therefore becomes an immediate error result rather than waiting for
  user input that can never arrive.
- Every assistant tool call receives exactly one model-visible tool result,
  including unknown tools.
- Treat provider stream failures as `AgentError::Provider` and do not emit a
  successful end-turn event for a partial or failed response.
- Preserve shell cwd only after a standalone, successful `cd` command. Other
  command strings run in their own shell and cannot reliably update parent
  process state; this avoids false updates from conditionals and parallel
  calls.

## Scope Boundary

The command blocklist remains a best-effort UX safeguard, not a security
sandbox. This change does not attempt to parse arbitrary shell syntax or add
platform-specific sandboxing.

## Validation

Regression tests cover cancellation, headless blacklisting, unknown tools,
provider stream failures, and false cwd updates. The Rust workspace is then
formatted, linted, and tested.
