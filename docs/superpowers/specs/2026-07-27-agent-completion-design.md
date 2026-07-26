# Agent Completion Semantics Design

## Goal

Prevent yi-agent from reporting a partially completed task as a normal
completion, especially after a file-changing tool call.

## Scope

- Preserve provider stop reasons through the agent loop and expose abnormal
  completion to CLI and TUI consumers.
- Continue after an output-token truncation using an explicit controller
  message, subject to the existing turn limit.
- Treat an idle provider stream, an EOF without a `Stop` event, and a stop
  sequence as abnormal terminal conditions rather than `EndTurn`.
- Require one bounded completion audit after a successful non-read-only tool
  call when the model tries to finish without a subsequent verification tool.
- Keep yi-agent's non-negotiable execution instructions when users supply a
  custom system prompt.
- Cover the behavior with deterministic core and headless tests.

## Non-goals

- Infer semantic completion for every natural-language task.
- Retry provider network, authentication, or tool errors automatically.
- Add a goal planner, a second LLM judge, or persistent task state.

## Architecture

### Provider stream status

`accumulate_stream` will distinguish a clean `Stop(EndTurn)` from an EOF that
arrives without a `Stop` event. The latter becomes `StopReason::Other` with a
stable reason string. The agent loop will examine every stop reason before it
considers termination.

`MaxTokens` produces a synthetic user controller message asking the model to
continue the interrupted task without repeating completed work. This preserves
the required user/assistant turn alternation across providers. `StopSequence`,
unexpected EOF, and idle timeout emit a new non-success terminal reason.

### Completion audit

The loop tracks whether a successful tool whose metadata is not read-only ran
in the current task. A subsequent successful read-only tool call clears that
pending verification state. When a model gives a normal text-only `EndTurn`
while verification is pending, yi-agent appends one synthetic user controller
message requesting inspection of the changed result and the relevant check.
It then gives the model one more turn. The audit is attempted once per task;
the second normal end remains valid so an uncooperative model cannot loop
forever.

The controller message is intentionally generic: documentation and data files
can be verified by rereading them, while source changes can be verified by a
targeted test, build, or lint command. Tool metadata, rather than tool names,
defines whether an operation changed state.

### Prompt assembly

The built-in prompt is separated into a stable base-instructions block and an
optional user instruction block. User-provided `--system-prompt` or
`YI_AGENT_SYSTEM_PROMPT` content is appended after the base instructions, and
the date and skills catalog remain appended last. The base prompt directs the
model to prefer `write` and `edit` for file changes, to verify changed files
before final text, and to use bash primarily for checks or batch mechanical
operations.

### Observable outcomes

`DoneReason` distinguishes a model-confirmed normal end from a provider
interruption. Headless mode returns a non-zero exit status for an interrupted
generation. The TUI receives the same event and can render a diagnostic rather
than a silent completion.

## Error Handling

- A malformed tool call remains a provider stream error as today.
- A `MaxTokens` continuation consumes an ordinary agent turn; `max_turns`
  remains the hard upper bound.
- A controller audit is never injected after a cancelled run or a failed tool
  result.
- On abnormal provider termination, the partial assistant content remains in
  session for visibility, but yi-agent emits an interrupted terminal state.

## Test Strategy

- Provider unit tests verify EOF without `Stop` is abnormal.
- Core loop tests verify max-token continuation, abnormal terminal mapping, and
  one write-to-verify audit sequence using scripted providers and a mutable
  test tool.
- CLI unit tests verify custom prompts retain base instructions and abnormal
  completion produces a non-zero headless exit code.
- The complex real-LLM tests require a normal `EndTurn` and evidence that a
  verification call follows the final mutating call, in addition to their
  existing artifact assertions.
