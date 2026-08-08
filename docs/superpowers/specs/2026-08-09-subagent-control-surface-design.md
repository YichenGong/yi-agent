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

## Daemon Control API

Every UI command maps one-to-one to a typed daemon command. Command responses
contain either a snapshot/event receipt or a typed error; clients never infer a
state transition from rendered text.

```rust
pub enum RuntimeCommand {
    ListTasks { project: Option<PathBuf>, include_terminal: bool },
    InspectTask { task_id: TaskId },
    ReadEvents { task_id: TaskId, after_event_id: Option<i64>, follow: bool },
    ReadMailbox { task_id: TaskId },
    ReadDiff { task_id: TaskId },
    SendMessage { task_id: TaskId, text: String, trigger_turn: Option<bool> },
    Pause { task_id: TaskId }, Resume { task_id: TaskId },
    Cancel { task_id: TaskId, recursive: bool, confirmation: ConfirmationToken },
    Retry { task_id: TaskId }, SetPriority { task_id: TaskId, priority: Priority },
    SetBudget { task_id: TaskId, patch: BudgetPatch },
    ResolvePermission { request_id: PermissionRequestId, decision: Decision },
    Review { task_id: TaskId, decision: ReviewDecision },
    DaemonStatus,
}
```

`ConfirmationToken` is obtained from a preview command that contains the exact
affected task IDs, worktrees, leases, unmerged commit ranges, and expiry. It is
single-use and bound to the action digest, preventing a stale TUI confirmation
from cancelling a changed task tree.

## CLI Grammar

```text
yi-agent daemon start|status|stop|logs
yi-agent agents [--project PATH] [--all]
yi-agent agent show ID | events ID [--follow] | diff ID | mailbox ID
yi-agent agent message ID TEXT [--trigger|--no-trigger]
yi-agent agent pause|resume|retry ID
yi-agent agent cancel ID [--recursive] [--yes]
yi-agent agent priority ID critical|high|normal|background
yi-agent agent budget ID [--turns N] [--tokens N] [--deadline SECS]
yi-agent permission approve|deny REQUEST_ID [--scope once|task]
yi-agent agent accept|rework|reject ID [TEXT]
```

Without `--yes`, destructive CLI commands print the preview and require a
second explicit invocation with the returned confirmation token. JSON output
uses the same response schema as IPC. Daemon-unavailable commands exit nonzero
with a diagnostic that instructs `yi-agent daemon start`; they never start it.

## Slash Grammar And Help Schema

Slash commands mirror CLI nouns but retain concise forms:

```text
/agents [--project PATH]
/agent ID | /events ID | /diff ID | /mailbox ID
/message ID TEXT [--trigger]
/pause ID | /resume ID | /cancel ID [--recursive] | /retry ID
/priority ID LEVEL | /budget ID --turns N --tokens N --deadline SECS
/approve REQUEST [once|task] | /deny REQUEST
/review ID | /accept ID | /rework ID TEXT | /reject ID TEXT
/daemon status
/help [COMMAND] | /?
```

One `CommandSpec` defines name, aliases, positional/option grammar, description,
examples, availability predicate, confirmation predicate, and daemon command
encoder. TUI completion filters `CommandSpec` names after `/`; contextual help
renders the same examples as CLI help. Availability is explanatory: for example,
`/accept a01` displays `a01 is not AwaitingParentReview` rather than silently
doing nothing.

## TUI State Projection

The TUI receives `RuntimeSnapshot` and ordered runtime events. It maintains an
event cursor, not an independent mutable task model. The agent panel renders:

```text
header: resident/queued/LLM/Cargo counts and daemon connection state
tree:   parent-child indentation, task ID, state, priority, current wait
detail: contract vN, attempt N, mailbox, event timeline, lease/worktree,
        delivery report, diff/test evidence, pending permission/review actions
```

On `ResyncRequired` or reconnect, discard local projection and request a fresh
snapshot. Selecting a task does not pause its worker. UI action results append a
system history cell with request ID and result, preserving an audit trail without
polluting the root agent's conversational context.

## Permission And Review Flow

Permission cards display task lineage, requested tool/input summary, parent
recommendation, user-security classification, and scope choices. A direct
parent can resolve delegation-scope questions only within its authority; the
user resolves security confirmations. Timeout is visible and maps to the core
permission transition, not a TUI-only expiry.

Review cards show child branch/base/head, diff summary, verification evidence,
and limitations. `Accept` sends `ReviewAccepted` to the direct parent; the
parent's worker or integration service performs merge/validation. `Rework`
requires feedback text. `Reject` requires a reason. A user targeting a leaf
produces an override event addressed to both leaf and parent.

## Control Surface Test Matrix

| Test | Assertion |
|---|---|
| `command_specs_match_cli_and_slash_grammar` | one schema drives all parsers/help |
| `unsafe_control_requires_fresh_preview_token` | stale/changed action is rejected |
| `daemon_absence_never_autostarts` | control command exits with start guidance |
| `task_tree_resync_replaces_stale_projection` | ordered events and snapshot converge |
| `help_explains_unavailable_action` | state predicate appears in help/result |
| `permission_card_routes_security_to_user` | parent cannot approve reserved action |
| `accept_wakes_parent_without_bypassing_integration` | no direct ancestor merge |
