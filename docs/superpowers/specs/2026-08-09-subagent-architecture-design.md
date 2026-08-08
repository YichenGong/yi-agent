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

## Decision Record

The following decisions are accepted product requirements, not implementation
suggestions:

1. `spawn_agent`, `wait_agent`, and `send_message` are all first-class tools.
2. A root agent may delegate to depth 1; depth 1 may delegate to depth 2; depth
   2 cannot delegate further.
3. Subagents reuse the root agent's capabilities and base instructions. Their
   only extra prompt is an operational subagent layer plus a concrete contract.
4. Coding work is isolated in a dedicated worktree and is delivered as a commit.
5. A direct parent reviews and integrates a child's commit into its own branch.
   It may never integrate a descendant directly into an ancestor branch.
6. A parent must create a committed, reviewable Git base before delegating code
   that depends on its own changes.
7. The default target is high concurrency: 16 resident subagents globally, with
   resource-specific limits rather than 16 simultaneous builds or LLM calls.
8. Global coordination spans projects; project-specific build constraints use
   generic resource leases rather than Rust-specific scheduler logic.
9. Users inspect and intervene in every task through the same daemon API used
   by CLI, TUI, Web UI, and Slash commands.
10. The daemon is manually started. It may run schedules once started, but it
    is not silently auto-started and missed schedules do not run by default.
11. Mailbox delivery is hybrid: completion and blocking events wake an idle
    parent, while progress is coalesced and never creates notification storms.
12. Every attempt is bounded by system budgets and watchdogs; the Supervisor,
    not the model, terminates loops and stalled work.

## Detailed Task Protocols

### Spawn

`spawn_agent` accepts a task contract draft, not an arbitrary prompt string.
The Supervisor validates the draft before it allocates an ID or worker:

```text
1. Verify parent is depth 0 or 1 and has direct-child capacity.
2. Verify root-session and global resident-task capacity; otherwise create Queued.
3. Intersect requested authority with the parent's authority.
4. Validate all requested write paths are covered by the parent lease.
5. For coding work, resolve the parent's committed base and create a child branch/worktree.
6. Allocate a child budget from the parent's remaining allocation.
7. Persist Task, Attempt(1), Contract(v1), authority, and Spawned event atomically.
8. Return task ID and status immediately; worker admission occurs asynchronously.
```

The response includes `task_id`, `status`, `depth`, `branch`, `worktree`, and
the effective budget. A rejected spawn returns a normal tool error explaining
the limiting invariant: maximum depth, quota, authority, uncommitted base, or
invalid worktree scope.

### Wait And Message

`wait_agent` never blocks the runtime thread. It changes the caller to
`WaitingForChildren`, registers an interruptible join condition, and yields its
LLM permit. A high-priority mailbox item causes `NeedsAttention`, so a parent
cannot deadlock while a descendant waits for its decision.

`send_message` records a mailbox message and optionally sets `trigger_turn`.
It cannot mutate another task's contract, authority, worktree, or history.
Those changes have dedicated control events. Messages to a nonterminal task are
consumed at the next safe checkpoint; a terminal task only starts a new attempt
after an explicit `ReworkRequested` or user retry.

### Commit Delivery And Review

Before `AwaitingParentReview`, a coding worker must report a clean worktree,
base commit, branch head, commit range, required verification results, and any
known limitation. The Supervisor verifies the branch is a descendant of the
recorded base before it emits the delivery event.

The parent's three review decisions are:

```text
Accept:  merge --no-ff child's branch into the parent's integration branch,
         run the parent's integration validation, then mark the child Completed.
Rework:  record feedback; create a new attempt. If the parent HEAD changed,
         create a fresh child branch from it instead of rewriting old history.
Reject:  retain all evidence and mark the task terminal; do not merge.
```

Only a clean, accepted worktree is eligible for removal. Dirty, failed, rejected,
blocked, cancelled, and recovery-required worktrees remain inspectable until a
user-controlled cleanup action.

## Detailed State Transition Rules

| Current state | Allowed transition | Trigger |
|---|---|---|
| `Queued` | `Running` | Coordinator grants all required admission permits. |
| `Queued` | `Cancelled` | User or ancestor cancels before admission. |
| `Running` | waiting state | Agent requests resource, permission, or child join. |
| `Running` | `AwaitingParentReview` | Delivery is complete and evidence is persisted. |
| `Running` | terminal failure state | Budget, watchdog, cancellation, or unrecoverable error. |
| waiting state | `Running` | Named condition resolves and a permit is available. |
| `AwaitingParentReview` | `Completed` | Direct parent accepts and integration validates. |
| `AwaitingParentReview` | `Queued` | Direct parent requests rework; new attempt is created. |
| `Paused` | `Queued` | User or parent resumes. |
| `RecoveryRequired` | `Queued` | Explicit resume creates a recovery attempt. |
| terminal state | `Queued` | Explicit retry creates a new attempt, never rewrites history. |

No transition implicitly reuses a cancelled cancellation token, copies an
unfinished assistant/tool-use pair into a recovery session, or removes evidence.
Attempt checkpoints occur only after a stable model turn and persisted tool
result. A crash during a side-effecting tool call always requires reconciliation.

## Watchdogs And Loop Prevention

The runtime evaluates these independent conditions at every state transition:

```text
turn budget             -> BudgetExhausted
token/cost budget       -> BudgetExhausted
wall-clock deadline     -> TimedOut
no meaningful progress  -> Stalled
resource queue deadline -> Blocked or TimedOut
retry/rework limit      -> Failed
```

Meaningful progress is a completed tool terminal event, new verified fact,
commit, acquired lease, accepted/rejected review, satisfied dependency, or legal
state transition. Model text, repeating stdout, identical progress updates, and
repeated `send_message` calls are not progress. Mailbox messages carry
correlation and causation IDs; an identical terminal report on the same causal
chain is stored once and does not wake a parent again.

## Coordinator Admission Algorithm

The coordinator maintains separate queues for resident-task admission, provider
LLM permits, coding permits, host build permits, and named resource leases.
For each queue it:

```text
1. Discards cancelled, expired, or no-longer-runnable requests.
2. Selects a root session using weighted fair rotation plus wait-time aging.
3. Selects a runnable subtree within that session using the same rotation.
4. Reserves one LLM permit for root/parent coordination when any parent has
   a high-priority mailbox event; unused reserved capacity can be borrowed.
5. Grants only the permits requested by the next task; a task waits rather than
   holding unrelated permits.
```

Interactive root sessions have a higher weight than scheduled background roots.
An idle session lends unused capacity. A project can set a stricter session
ceiling, but no project can increase the user's global capacity.

## IPC And Persistence Contract

Every API command is a versioned envelope with a request ID and caller identity.
Every durable state change appends an event in the same database transaction as
its snapshot update. Client subscription has this shape:

```text
SubscribeEvents { after_event_id, filters }
  -> Snapshot { event_id, sessions, tasks }
  -> Event { event_id, timestamp, actor, subject, kind, payload }
```

If a client reconnects with a retained cursor, the daemon replays later events;
if retention no longer covers the cursor, it returns a fresh snapshot. Socket
permissions and OS-user checks protect local access. Database and log records
redact tool inputs classified as secret and never contain provider credentials.

## Human Intervention Semantics

The following controls have defined effects:

| Action | Effect |
|---|---|
| Pause | Stop admission after the next safe checkpoint; retain state and worktree. |
| Resume | Move `Paused` to `Queued`; do not discard prior evidence. |
| Cancel | Cancel the target; default recursive mode cancels descendants and preserves worktrees. |
| Retry | Create a new attempt with the same immutable contract version. |
| Message | Append a mailbox item; may request a next turn. |
| Priority/budget | Apply within inherited ceilings and log the actor/reason. |
| Approve/deny | Resolve a named permission request with the requested scope. |
| Accept/rework/reject | Apply the direct-parent delivery workflow described above. |

All destructive actions show affected descendants, leases, worktrees, and
unmerged commits before confirmation. A user may target a leaf directly, but
the direct parent receives a visible override notification.
