# Managed Background Processes Design

## Goal

Add first-class, agent-managed background processes to yi-agent without changing the existing short-lived `bash` semantics.

The feature lets the agent start long-running commands such as dev servers, file watchers, or log-producing jobs, continue the conversation while they run, read their output later, and kill them through tools or the TUI.

## Non-Goals

- Do not turn the existing `bash` tool into a mixed foreground/background interface.
- Do not implement cross-restart process recovery in the MVP.
- Do not expose arbitrary PID kill. yi-agent may only kill processes it started and registered.
- Do not support interactive stdin in the MVP.
- Do not introduce a local daemon or SQLite state store in the MVP.

## Current Context

Today `BashTool` runs `sh -c`, streams stdout/stderr, waits for the shell child to exit, and returns a normal tool result. Shell-level backgrounding such as `sleep 30 & exit 0` can make the shell return early, but yi-agent does not own that orphaned child as a managed process.

The TUI already has a Ctrl+P bash task popup backed by `RunningTaskRegistry`. It tracks current and completed bash tool calls, but bash kill is still not wired to a real kill channel in the UI. This design keeps that bash path intact and adds a separate managed-process path.

## Chosen Approach

Implement an independent `ProcessManager` service plus new process tools. The existing `bash` tool remains for short commands. Managed background processes use a new API, but the TUI gives users a unified Ctrl+P entry point with tabs for bash tasks and managed processes.

This approach keeps tool semantics clear:

- `bash`: run one command, wait for completion, return stdout/stderr in the tool result.
- `process_start`: start a long-running managed command and return a process handle.
- `process_list`: inspect managed processes.
- `process_read`: read buffered output snapshots or cursor-based deltas.
- `process_kill`: kill a managed process.

## Architecture

Add a `ProcessManager` runtime service. It owns every child process handle and is shared by the process tools and the TUI through `Arc<ProcessManager>`.

Responsibilities:

- Spawn commands with cwd/env/on-exit policy.
- Enforce unique optional names.
- Store process metadata: process id, name, pid, command, cwd, status, start/end time, exit code, readiness, and lifecycle policy.
- Own stdout/stderr reader tasks.
- Maintain bounded output buffers and cursors.
- Kill managed process groups.
- Publish snapshots/events for TUI rendering.
- Clean up processes on yi-agent shutdown according to `on_exit`.

The manager is runtime state, not conversation history. The LLM observes it only by calling tools. The user observes it through the TUI.

## Tool API

### `process_start`

Inputs:

- `command: string` required.
- `name?: string` optional unique process name.
- `cwd?: string` optional working directory, default current yi-agent cwd.
- `env?: object<string, string>` optional environment overrides.
- `on_exit?: "kill" | "keep"`, default `"kill"`.
- `ready_pattern?: string` optional readiness substring.
- `ready_timeout_sec?: integer` optional readiness wait, default small bounded value when `ready_pattern` is present.

Behavior:

- Validates name uniqueness.
- Applies the same shell wrapping, sandbox, and shell blocklist rules used by bash.
- Spawns a managed child process and starts stdout/stderr reader tasks.
- If no `ready_pattern` is provided, returns as soon as OS spawn succeeds.
- If `ready_pattern` is provided, waits until stdout/stderr contains the substring or the readiness timeout elapses.
- On readiness timeout, the process continues running. The tool returns a warning-style result with `ready=false` and status still running, rather than killing the process automatically.

Result includes:

- `process_id`
- `name`
- `pid`
- `status`
- `ready`
- `next_cursor`
- startup output excerpt if useful

### `process_list`

Inputs:

- Optional filters may be added later. MVP can list all managed processes.

Result includes one snapshot per process:

- `process_id`
- `name`
- `pid`
- `status`
- `ready`
- `command`
- `cwd`
- `elapsed_sec`
- `on_exit`
- `exit_code`

### `process_read`

Inputs:

- `process_id?: string`
- `name?: string`
- `cursor?: u64`
- `max_bytes?: integer`

Exactly one of `process_id` or `name` is required.

Behavior:

- Without `cursor`, returns a recent snapshot from stdout and stderr plus `next_cursor`.
- With `cursor`, returns output after that cursor plus `next_cursor`.
- If the cursor is older than the retained ring buffer, returns `truncated=true` and the earliest available cursor.

Result keeps streams separate:

- `stdout`
- `stderr`
- `next_cursor`
- `truncated`
- `status`
- `ready`

The MVP does not guarantee global stdout/stderr interleaving order. A future event-ring format can add stream-tagged total ordering if needed.

### `process_kill`

Inputs:

- `process_id?: string`
- `name?: string`

Exactly one of `process_id` or `name` is required.

Behavior:

- Only kills a process registered by the current `ProcessManager`.
- Refuses arbitrary PID kill.
- Prefer killing the whole process group/session so child server processes do not survive after the shell wrapper exits.
- Marks the process as `Killed` when successful.

## Status Model

Managed processes use their own status model:

- `Starting`: spawned and still evaluating readiness.
- `Running`: running, no readiness pattern or readiness not yet proven.
- `Ready`: running and matched `ready_pattern`.
- `Exited { code }`: exited naturally.
- `Killed`: killed by user or agent.
- `FailedToStart { reason }`: spawn, cwd, name, sandbox, or validation failure.

Readiness timeout does not become a terminal state. It leaves the process running with `ready=false` and returns a warning to the caller.

## Output Buffer And Cursors

Each process keeps bounded stdout and stderr buffers. MVP default: 256 KiB per stream.

Every output chunk advances a monotonically increasing cursor. A cursor identifies a position in the process output history. `process_read` can use this cursor for incremental reads. If the caller asks for a cursor older than retained output, the manager returns the earliest available cursor and `truncated=true`.

This lets the agent poll logs without re-reading the same content, while preventing unbounded memory growth.

## Permissions And Safety

Process tools must not bypass existing shell permissions.

- `process_start`: mutating, requires confirmation, and applies command prefix checking plus shell blocklist.
- `process_list`: read-only, no confirmation.
- `process_read`: read-only, no confirmation.
- `process_kill`: mutating, requires confirmation, and can only target managed process ids or unique names.

Additional safeguards:

- Optional process `name` must be unique. Name conflicts fail fast.
- Limit managed processes per session, with 16 as a reasonable default.
- Bound output memory per stream.
- Default `on_exit="kill"` so yi-agent does not accidentally leave orphaned servers.
- `on_exit="keep"` is allowed, but yi-agent shutdown must clearly prompt or report retained processes.
- Stdin is not exposed in the MVP. The internal spawn path may keep design room for a future stdin pipe and `process_write_stdin` tool.

## Process Group Handling

On Unix-like systems, start managed commands in a new process group/session where practical. `process_kill` should kill the group, not just the shell wrapper, so commands like `npm run dev` do not leave grandchildren behind.

Windows support can initially be documented as best-effort if the project does not yet target Windows process groups. A later implementation can use Job Objects.

## TUI Design

Ctrl+P remains the single entry point for runtime task visibility. The popup becomes a tabbed view:

- `Bash Tasks`: current behavior, backed by the existing bash task registry.
- `Processes`: managed background processes, backed by `ProcessManager` snapshots/events.

Keyboard behavior:

- `Tab`: switch tabs.
- `Up` / `Down`: move selection or scroll detail.
- `Enter`: open detail.
- `q` / `Esc`: back or close.
- `f`: follow detail output bottom.
- `k`, then `y`: kill a running managed process.

The MVP should implement real kill for managed processes. The existing unwired bash task kill path can remain out of scope unless the same control channel naturally makes it cheap to wire later.

Process detail view shows:

- process id and optional name
- pid
- command
- cwd
- status and readiness
- elapsed time
- `on_exit` policy
- exit code if terminal
- stdout/stderr recent output

## Event Flow

`ProcessManager` sends process events or exposes snapshot subscriptions for TUI refresh:

- process started
- output delta
- readiness matched
- process exited
- process killed
- process failed

The TUI should not own child handles. It sends management commands, such as kill, back to the manager. Tool calls and TUI actions share the same manager instance, so both see the same processes.

Agent turn completion does not stop managed background processes. Only explicit kill or yi-agent shutdown applies lifecycle policy.

## Shutdown Behavior

On TUI or run-mode shutdown:

1. Enumerate managed processes.
2. Kill all with `on_exit="kill"`.
3. For `on_exit="keep"`, preserve the process and clearly report its process id/name/pid to the user.
4. If an interactive TUI prompt is available, ask for confirmation before leaving keep-processes alive. If not interactive, report the retained processes in stderr/log output.

The MVP does not reattach kept processes after yi-agent restarts.

## Testing Strategy

Unit tests should cover:

- `process_start` spawns and returns a process id.
- `process_start` rejects duplicate names.
- `process_start` applies shell blocklist.
- `ready_pattern` success transitions to `Ready`.
- `ready_pattern` timeout leaves the process running with `ready=false`.
- `process_list` returns expected metadata.
- `process_read` returns snapshot output without a cursor.
- `process_read` returns deltas with a cursor.
- old cursors return `truncated=true`.
- `process_kill` terminates the process group and marks `Killed`.
- shutdown kills `on_exit="kill"` processes and preserves `on_exit="keep"` processes.
- TUI tab state switches between bash tasks and processes.
- TUI process kill confirmation sends the correct manager command.

Follow project testing guidance: avoid full workspace tests by default. Prefer targeted crate tests such as `cargo test -p yi-agent-tools process_` and `cargo test -p yi-agent --bin yi-agent tui::app::tests::process_`.

## Documentation And Project Tracking

Implementation should update project-management docs in the same PR:

- Add an unchecked feature to `docs/project-management/yi-agent-tools.md` for managed process tools and mark it complete only once implemented with tests.
- Add an unchecked feature to `docs/project-management/yi-agent-tui.md` for the Ctrl+P process tab and mark it complete only once implemented with tests.
- Update `docs/project-management/README.md` counts when those features are completed.

No new crate is required for the MVP unless the implementation discovers that `ProcessManager` needs a cleaner crate boundary.

## Open Implementation Notes

- Prefer reusing existing sandbox and shell blocklist code rather than duplicating validation paths.
- Keep `ProcessManager` isolated enough that a future local daemon can replace or wrap it.
- The future persistent design can store process metadata, but MVP should not promise reattachment after restart.
- Use a conservative default process limit and output buffer size; expose config later only if needed.
