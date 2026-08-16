# Project-Local Runtime Directory Design

## Problem

The subagent Runtime currently defaults to one user-global directory, `~/.yi-agent/runtime`. The first `yi-agent` process that creates its daemon binds the daemon worker factory to that process's `Config.workdir`. Later clients launched from different projects detect the existing daemon and attach to its socket. `AttachApplicationRoot` does not carry a workspace, so those later clients receive worktrees created beneath the first daemon's repository.

For example, a TUI launched from `quest-pop-swift` can receive a root workspace beneath `yi-agent/.worktrees/`, because the global daemon was first started from `yi-agent`.

## Goal

Default Runtime state must be isolated by effective project workdir so independently launched projects cannot share a daemon, SQLite state, root worktree factory, or task list accidentally.

## Runtime Directory Resolution

All Runtime entry points resolve a directory with this priority:

1. A non-empty `YI_AGENT_RUNTIME_DIR` environment variable, interpreted as a path exactly as supplied.
2. The effective workdir joined with `.yi-agent/runtime`.

The effective workdir remains the existing configuration result:

1. CLI `--workdir`.
2. Non-empty `YI_AGENT_WORKDIR`.
3. Process current directory.

The project default is therefore:

```text
<effective-workdir>/.yi-agent/runtime
```

The runtime directory is a state location only. It stores `runtime.sock`, lock files, and SQLite database files; root and child Git worktrees continue to be created by the daemon's workspace service beneath the effective Git repository's `.worktrees/` directory.

## Behavior

- A TUI or `run --subagents` launched in project A starts or attaches only to project A's default runtime.
- A separately launched project B uses its own default runtime and constructs its daemon factory with project B's `Config.workdir`.
- `YI_AGENT_RUNTIME_DIR` remains an explicit opt-in for users who intentionally want a shared custom daemon directory. No automatic cross-project protection is added for an explicit override.
- `yi-agent daemon start`, `status`, and `stop` resolve the same project-local runtime directory from the command's effective workdir, unless overridden by `YI_AGENT_RUNTIME_DIR`.
- Existing `~/.yi-agent/runtime` state is neither migrated nor deleted. After upgrade, normal launches do not attach to that legacy global daemon unless the user explicitly exports `YI_AGENT_RUNTIME_DIR=~/.yi-agent/runtime`.

## Implementation Boundary

Change the CLI-level runtime-directory resolver in `crates/yi-agent/src/main.rs` so each caller provides the resolved effective workdir. Update all call sites, including daemon control, TUI attachment, headless attachment, and task-control commands that communicate with a runtime socket.

The IPC request shape, SQLite schema, `AgentWorkspaceService` trait, and daemon workspace factory stay unchanged. Project isolation is achieved by choosing a different socket/database directory before a daemon can be reused.

## Error Handling

Runtime directory resolution is fallible only when no effective workdir can be determined. In ordinary CLI operation config loading already resolves or errors for the workdir; callers should surface the resolver error with existing command context. Empty `YI_AGENT_RUNTIME_DIR` is ignored and uses the project-local default.

## Tests

Add focused unit tests for the runtime-directory resolver:

- explicit non-empty runtime override wins over workdir;
- empty override falls back to `<workdir>/.yi-agent/runtime`;
- different workdirs produce different default runtime directories;
- project-local directory is used without an override.

Run CLI unit tests, runtime IPC integration tests, formatting, and a workspace build/test check after implementation.
