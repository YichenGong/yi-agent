# Subagent Runtime Architecture Design

## Goal

Add a supervised, observable multi-agent runtime to yi-agent. A root agent can
delegate independent work to child agents, and a child agent can delegate once
more to leaf agents. Every coding agent works in an isolated Git worktree and
delivers a verified commit for its parent to review and integrate.

## Status And Scope

This is a design specification. The current codebase has one `Agent`, one
in-memory `Session`, and parallel *tool* calls, but no subagent task model,
supervisor, daemon, or cross-project resource scheduler.

In scope:

- Two levels of delegation below a root agent.
- `spawn_agent`, `wait_agent`, and `send_message` as agent tools.
- A session-level `AgentSupervisor` and a user-level `RuntimeCoordinator`.
- Per-agent sessions, budgets, mailboxes, cancellation, Git worktrees, and
  commit-based delivery.
- A manually started local daemon with durable state, local IPC, schedules,
  and CLI/TUI/Slash-command control.

Out of scope:

- Delegation deeper than root -> child -> leaf.
- Automatic final merge into a target branch without review.
- Network-accessible or multi-machine runtime control in the first release.
- Automatic replay of interrupted tool, Git, or provider calls.

## Terms

- **Root**: the user-facing agent session. It has depth 0.
- **Child**: an agent spawned by a root or another child. Children have depth
  1; leaf agents have depth 2 and cannot spawn further agents.
- **Task**: the durable identity, contract, lineage, worktree, and delivery
  state of a unit of delegated work.
- **Attempt**: one execution of a task. Retries, rework, and recovery create
  new attempts without erasing previous evidence.
- **Supervisor**: owns one root session's task tree and mailbox routing.
- **Coordinator**: owns global resource permits and fairness across projects.

## Architecture

```text
CLI / TUI / Web UI
        | local IPC
        v
Runtime daemon
  RuntimeCoordinator
    Project session A -> AgentSupervisor -> root / children / leaves
    Project session B -> AgentSupervisor -> root / children / leaves
  Scheduler, persistence, audit event stream
```

The daemon owns workers and state after the user explicitly starts it. Clients
only observe and control it. A disconnected TUI must not become the source of
truth for a running task.

## Delegation And Prompt Assembly

Subagents inherit the root's base system prompt, project instructions, skills,
provider, registered tools, and permission policy. They do not receive the
root's complete chat history. The runtime assembles their context as:

```text
base system prompt
+ project/developer instructions and skills
+ subagent runtime instructions
+ versioned DelegationContract
```

The subagent runtime instructions require the agent to stay within the contract,
use only its assigned worktree, verify changes, commit completed coding work,
report evidence to its parent, and never merge into an ancestor branch. A child
may review and merge a direct leaf's commit only into its own integration branch.

`DelegationContract` is durable and versioned. It contains:

```text
identity: task, parent, root-session, depth, attempt
objective: title, outcome, non-goals
context: verified facts with provenance, relevant paths, commits, summaries
scope: project root, read/write/forbidden paths, dependencies, preconditions
workspace: worktree, branch, base commit, parent integration branch
acceptance: functional criteria, required checks, documentation requirements
authority: delegated capability, tool/sandbox/Git limits, escalation policy
collaboration: delivery policy, progress policy, child allocation
budget: turns, tokens, deadline, retry limits, priority
delivery: commit requirement, report schema, review owner
```

Contract amendments are append-only audit events. An amendment may narrow or
extend a task within the parent's authority. A materially new outcome requires
a new task instead of silently changing the original task.

## Task State And Delivery

Task execution and delivery acceptance are distinct. A coding attempt that has
produced a commit is not complete until its direct parent reviews it.

```text
Queued -> Running -> AwaitingParentReview -> Completed
              |             |
              |             -> ReworkRequested -> Queued (new attempt)
              -> WaitingForResource / WaitingForPermission / WaitingForChildren
              -> Paused / Blocked / Stalled / TimedOut / BudgetExhausted
              -> Failed / Cancelled / RecoveryRequired
```

`ReworkRequested` is a control event, not a terminal state. A successful leaf
reports a `DeliveryReport` with branch, base/head commit, changed files,
verification evidence, limitations, and follow-ups. A research task reports
claims with evidence rather than a commit.

Every terminal or waiting state releases execution permits. `Queued` does not
consume a resident-agent slot. `Running`, paused, and waiting tasks do consume
one of the configured resident task slots because they retain durable state,
mailbox ownership, and possibly a worktree lease.

## Mailbox And Wake-Up Policy

Messages are structured records with sender, recipient, type, priority,
causation/correlation IDs, payload, and delivery policy. Messages normally move
only between direct parent and child; user overrides are recorded and notify the
direct parent.

| Message | Default behavior |
|---|---|
| `Progress` | Coalesce by sender and do not wake the parent. |
| `Completed`, `Blocked`, `Failed`, `ScopeChangeRequest` | Wake an idle parent; otherwise mark it pending. |
| `PermissionRequest` | Highest priority and wakes the responsible parent/user. |
| `ReworkRequest` | Creates a new child attempt at a safe checkpoint. |

Delivery policy is `immediate` by default, with `batch` for compare-and-decide
work and `on_wait` for low-priority background work. `wait_agent` supports one
task plus `all` and `any` group joins. Waiting is interruptible by a higher
priority mailbox event, preventing parent/child deadlocks.

## Concurrency And Resource Leases

The default global capacity is 16 resident subagents, maximum depth 2, maximum
four direct children per agent, and a queue of 64 not-yet-resident tasks. This
is task capacity, not a promise that 16 model requests or builds execute at
once.

```text
per provider/API-key LLM permits: 8 (reserve one for root/parent coordination)
coding permits:                   6
host build permits:               2
workspace build lease:            project-defined; Cargo workspace default 1
Git integration lease:            one per target branch
```

The coordinator schedules fairly across root sessions, then across subtrees of
one session. A session can borrow unused capacity but gives it back as other
sessions become runnable. Critical, interactive, normal, and background
priorities affect admission only; aging prevents background schedules from
starving forever. Recursive children count against the same root-session and
global quotas, so a child cannot bypass limits by spawning leaves.

Resource requests are generic (`scope`, `key`, shared/exclusive mode, units).
Cargo serialization is a project-specific workspace lease rule, not a Rust-only
global queue. This repository's documented Cargo constraints require its
workspace lease capacity to be one.

## Git Worktree And Commit Protocol

The original user worktree is read-only to delegated coding work. The root and
each coding task receive an integration branch/worktree. A child branch is
created from its parent's committed integration HEAD, never from uncommitted
changes.

```text
leaf branch -> merge --no-ff -> child integration branch
child branch -> merge --no-ff -> root integration branch
root branch  -> reviewed merge --no-ff -> configured target branch
```

Before delegating code dependent on its own changes, a parent must either have a
clean committed base, create a self-contained verified baseline commit, or keep
the dependent work local. A parent owns conflict resolution into its branch; it
may request a child rework attempt based on a new parent commit but must not let
the child write the parent worktree.

Rejected and recovery-required attempts keep their worktree and Git evidence.
Accepted, clean child worktrees may be removed after their parent has merged
them. Project policy can require immediate cleanup, as this repository does.

## Permissions

Tools and policies are inherited, but authority narrows down the tree.
`DelegatedAuthority` binds an issuer, holder, task, allowed worktree/path scope,
allowed tools/Git operations, budget, and expiry. A child cannot grant more than
it received.

Read-only actions and pre-authorized editing/testing/committing inside the
assigned worktree can proceed without a new parent decision. Scope expansion,
ancestor worktree access, and parent-branch integration route to the lease owner.
Network access, secrets, privilege escalation, destructive operations, and
cross-project effects require user-level confirmation unless the user's session
policy explicitly allows them. Permission timeouts become visible `Blocked`
states; they do not wait indefinitely.

## Daemon, Persistence, And Scheduling

The user manually starts and stops the daemon. It does not auto-start or install
an OS login service. When it is running, it persists task trees, attempts,
mailboxes, leases, schedules, approval and review decisions, worktree metadata,
and append-only events in a user-private SQLite store. Raw API keys and secret
values are never stored in task records or event logs.

Clients communicate through a versioned local IPC protocol over a user-private
Unix socket (or platform equivalent). Snapshots plus monotonically increasing
event IDs let clients reconnect and replay missed state transitions.

Scheduled work creates a separate root session rather than inheriting old chat
history. Defaults are read-only, background priority, no overlap, no missed-run
catch-up, strict budgets, and user confirmation for privileged effects.

On graceful stop, tasks stop at safe checkpoints and become resumable. After an
unexpected stop/restart, running attempts become `RecoveryRequired`; the daemon
does not replay an unknown LLM, shell, Git, or external call. Resume starts a
new attempt that first inspects Git status, latest commits, and recorded command
state.

## User Control Surface

CLI, TUI, Web UI, and Slash commands use the same daemon API. Users can inspect
the full task tree, contracts, attempts, mailbox, event stream, resource waits,
worktree, branch, diff, commit, and verification evidence. They can message,
pause, resume, cancel, retry, change priority/budget, approve/deny permissions,
and accept/rework/reject deliveries.

The first Slash-command set includes `/agents`, `/agent`, `/events`, `/diff`,
`/mailbox`, `/message`, `/pause`, `/resume`, `/cancel`, `/retry`, `/priority`,
`/approve`, `/deny`, `/review`, `/accept`, `/rework`, `/reject`, `/budget`, and
`/daemon status`. `/help`, `/help <command>`, and `/?` are generated from the
same command schema as completion metadata and CLI help.

An accept decision records user approval and wakes the direct parent to perform
normal integration; it does not bypass ownership by directly merging a leaf into
an ancestor branch. Destructive operations show their descendant/worktree impact
before execution and are audit events.

## Configuration

Configuration precedence is built-in safety defaults, user runtime settings,
project settings and instructions, root-session choices, then task contract.
The effective value is the most restrictive compatible value for limits and the
intersection for authorities. Only the user can raise a root session within the
user-level hard ceiling; children can only narrow or allocate their inherited
budget.

User runtime configuration owns daemon and global resource ceilings. Project
configuration declares resource leases, Git/worktree rules, validation defaults,
and project limits. Existing `AGENTS.md` instructions remain authoritative
human-readable constraints and cannot be bypassed by structured configuration.

## Failure Controls

Each attempt has bounded turns, tokens/cost, wall-clock time, idle time, retry
count, and rework cycles. Meaningful progress is a state-changing tool result,
new verified fact, commit, lease acquisition, review decision, or satisfied
dependency; streamed text alone does not reset the watchdog. Mailbox correlation
and duplicate suppression prevent completion/rework ping-pong. Resource waits
have deadlines and report `Blocked` or `TimedOut` with evidence.

## Test Strategy

- Unit-test task transitions, authority narrowing, mailbox coalescing, loop
  detection, task/attempt persistence, and resource permit accounting.
- Use deterministic fake clocks and providers to test deadlines, retries,
  scheduling fairness, and daemon restart recovery.
- Use temporary Git repositories/worktrees to test clean-base enforcement,
  branch ownership, merge direction, rework attempts, and cleanup refusal for
  dirty worktrees.
- Test local IPC reconnect/replay and ensure a second client observes the same
  task state and audit history.
- Add TUI/Slash/CLI tests for command-schema help, invalid state actions,
  confirmations, and daemon-unavailable diagnostics.
