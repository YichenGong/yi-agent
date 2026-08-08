# Subagent Control Surface Design

## References

- [Architecture decision index](2026-08-09-subagent-architecture-design.md)
- Existing Slash metadata: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Existing TUI event loop: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

## Canonical API

CLI, TUI, Web UI, and Slash commands invoke daemon controls; none maintains task
truth locally. Core actions are inspect, subscribe, message, pause, resume,
cancel, retry, priority/budget update, approve/deny, and accept/rework/reject.
Every mutation writes an audit event with actor, time, reason, subject, and
before/after state.

## Slash Commands

`/agents`, `/agent <id>`, `/events <id>`, `/diff <id>`, `/mailbox <id>`,
`/message <id> <text>`, `/pause <id>`, `/resume <id>`, `/cancel <id>
[--recursive]`, `/retry <id>`, `/priority <id> <level>`, `/approve <request>`,
`/deny <request>`, `/review <id>`, `/accept <id>`, `/rework <id> <text>`,
`/reject <id> <reason>`, `/budget <id> ...`, and `/daemon status` are first
release commands. `/help`, `/help <command>`, and `/?` render the same schema
used by CLI help and completion.

Destructive commands show affected descendants, worktrees, unmerged commits,
and confirmation scope. `/accept` records review approval and wakes the direct
parent; it does not merge around branch ownership. Daemon-unavailable commands
show `yi-agent daemon start` and perform no implicit start.

## Required Tests

- Shared command schema has unique names, valid argument parsing, and help.
- TUI reflects daemon snapshot/events and reconnect state.
- Cancel confirmation reports descendants; recursive cancellation is audited.
- Direct user leaf intervention notifies its parent.
- Accept routes integration to parent and cannot merge a leaf into root.
