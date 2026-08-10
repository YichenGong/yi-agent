# Subagent TUI MVP Design

**Date:** 2026-08-10

**Status:** approved for written specification review

## Objective

Deliver the first user-runnable vertical slice of the durable subagent runtime
inside the existing TUI. The user describes work in ordinary conversation, the
foreground application Agent may delegate it to one child, and the TUI shows
live child progress and an inline accept/rework/reject review card. The child
works in an isolated Git worktree and delivers a real commit for direct-parent
integration.

The normal development workflow must not require CLI control commands or a
`/delegate` command. CLI and Slash controls remain secondary diagnostics and
recovery affordances. This MVP changes implementation order, not final scope:
the full task tree, schedules, two-level delegation, and all remaining runtime
acceptance criteria still follow after the vertical slice works.

## Primary User Workflow

1. The user starts `yi-agent` and enters the existing TUI.
2. If the local runtime is unavailable, the TUI displays an explicit start
   confirmation. Confirming starts it without leaving the TUI; declining keeps
   the current conversation usable without delegation.
3. The user writes a normal request, for example: "Implement the login flow;
   delegate a focused part if useful."
4. The foreground root Agent decides whether to call `spawn_agent`. The user
   does not need to learn a delegation command.
5. A compact child-task card appears in the conversation and updates from
   daemon events while the user continues chatting.
6. When the child produces a valid commit delivery, the card becomes a review
   card with diff and verification evidence.
7. The user accepts, requests rework in natural language, or rejects from that
   card. A confirmed acceptance integrates only into the root integration
   worktree, never into the user checkout or `main`.

`/agents`, `/agent`, `/events`, `/diff`, `/review`, `/accept`, `/rework`, and
`/reject` remain available for advanced inspection and recovery, but they are
not the primary workflow.

## Interaction Model

### Conversation-Native Delegation

The existing foreground application Agent remains responsible for the visible
conversation and provider stream. At TUI session initialization it attaches to
a durable daemon root task and receives task-scoped `spawn_agent`,
`send_message`, and `wait_agent` proxy tools. Those tools use the daemon's typed
IPC and cannot invent a caller task, session, or capability identity.

The Agent decides to delegate from the user's natural-language request and its
system instructions. The TUI does not run a second intent-classification LLM
call and does not reinterpret arbitrary user text itself. This preserves the
normal tool-selection model and avoids a second source of truth for delegation.

If root attachment fails, the TUI reports the concrete runtime error and keeps
ordinary single-Agent chat available. It must not pretend delegation succeeded
or silently create an untracked child.

### Inline Task Cards

Runtime task state is rendered as structured history cells rather than plain
text injected into the Agent conversation. A compact running card contains:

- stable short task ID and parent relationship;
- concise objective;
- state and attempt number;
- elapsed time and current wait/resource;
- latest meaningful progress or error;
- a hint to expand details.

The card updates in place by task ID; each progress event must not append a new
chat message. Expanding it shows worktree/branch, contract, mailbox summary,
recent events, budget, and permission state. Runtime cells are visible audit
information but are not appended to the root LLM conversation context.

The first MVP uses conversation cells and an optional expanded card, not a
dedicated side panel or dashboard. This fits the current TUI history/cell model
and remains usable in narrow terminals.

### Review Cards

When a child enters `AwaitingParentReview`, its card shows:

- pinned delivery ID, base commit, and head commit;
- branch and worktree identity;
- changed-file summary and diff access;
- verification evidence and known limitations;
- `A Accept`, `R Rework`, `X Reject`, and `D Diff` actions.

`A`, `R`, `X`, and `D` apply only while the review card has focus, so normal
text entry is unaffected. Accept first shows the exact commit, target parent
branch, worktrees, and action scope. Rework opens the normal input editor in a
review-feedback mode; reject similarly requires a non-empty reason. Escape
cancels either mode without mutation.

The confirmation token is bound to the current task, delivery, parent target,
and action and expires. If task state or reviewed HEAD changes, the daemon
rejects it and the TUI refreshes the card.

### Runtime Connection State

The status area displays `runtime: connected`, `starting`, `disconnected`, or
`resyncing`, plus resident/queued counts when connected. Starting the runtime
from the TUI is always an explicit user action. The TUI never silently starts a
daemon merely because an Agent attempted delegation.

The TUI holds a versioned event subscription with its last durable cursor.
Events update a local read-only projection. On reconnect or `ResyncRequired`,
the TUI discards that projection and replaces it with a fresh daemon snapshot;
it never infers task transitions locally. Subscription work runs outside the
render/input loop so a slow or unavailable daemon cannot freeze typing or Agent
stream rendering.

## Runtime And Application Boundaries

### Root Attachment

One typed attach/start request creates the root task, records the natural
language objective when the first turn begins, and returns an opaque
task-scoped capability bundle for the foreground Agent tools. It is correlated
with a TUI-generated idempotency key so retrying a failed transport does not
create a duplicate root session.

The application-side root adapter reports lifecycle, usage, permission waits,
safe checkpoints, child waits, and completion to the daemon. SQLite and the
daemon supervisor remain authoritative for task state; the TUI history model is
only a projection. Closing or disconnecting the TUI triggers the existing
pause/drain protocol rather than marking an active root successfully complete.

### Permission Channel

The foreground root continues using the existing TUI permission UX.
Daemon-owned children send permission requests through typed IPC to the same visible
interaction queue. Each card includes task lineage and requested tool/input so
the user can distinguish root and child requests.

The TUI sends decisions through `ResolvePermission`; the daemon persists the
decision before waking the waiting worker. Security-reserved actions cannot be
approved by a parent Agent. `--yolo` retains its current explicit semantics and
is never inferred from a detached daemon or child task.

## Git Worktree Ownership

The user checkout is a read-only source of the selected committed base. A
coding session requires a clean Git checkout. The runtime creates:

```text
user checkout (never written by the runtime)
  root integration worktree and branch
    child delivery worktree and branch
```

The root worktree is created from the user checkout's recorded HEAD. A child
worktree is created from the direct parent's recorded clean HEAD. Branch names
and paths derive only from daemon-generated session/task IDs, never from raw
objectives. The daemon persists repository root, worktree path, branch, direct
parent branch, and base commit before starting the corresponding worker.

Every Agent receives its own persisted workspace path. Built-in filesystem and
shell tools, recovery inspection, Git evidence collection, and nested
delegation all use that path. The global daemon configuration directory must
not be reused as every worker's workspace.

Dirty or non-Git source checkouts disable coding delegation with an actionable
TUI explanation before worker/provider side effects. Ordinary read-only chat
remains usable. The runtime never stashes, copies, resets, force-removes, or
writes the user checkout.

## Commit Delivery

When a non-root coding worker finishes a successful Agent turn, the worker
factory inspects its assigned worktree through the trusted worktree service. A
valid delivery requires:

- a clean worktree;
- the expected child branch;
- a HEAD different from the recorded base;
- the recorded base to be an ancestor of HEAD;
- a commit still reachable at the exact inspected HEAD;
- non-empty verification evidence collected from the task contract/runtime.

The factory reports a real `DeliveryReport` containing the pinned commit, base,
workspace lease, and evidence. It must not report
`CompletedWithoutDelivery` for a coding child expected to deliver a commit. A
dirty tree, missing commit, branch mismatch, or invalid ancestry is a durable
task failure with retained worktree evidence, not a successful empty delivery.

Root completion is different: its integration branch is the session result and
is not automatically merged into the user checkout. The TUI displays its
branch/worktree for later explicit user validation.

## Review And Integration

TUI card actions and Slash fallbacks encode the same typed `Review` IPC
requests. They operate only on the delivery currently named by
`AwaitingParentReview`; a stale delivery or changed child HEAD is rejected.

Acceptance has two distinct durable facts:

1. the local user approves the pinned delivery;
2. the trusted daemon integration service merges that exact commit with
   `merge --no-ff` into the direct parent's worktree and records successful
   validation evidence.

The task becomes accepted only after both facts succeed. Approval alone wakes
or notifies the direct parent but cannot claim integration. Immediately before
merging, the integration service verifies parent ownership, recorded base,
parent cleanliness, child cleanliness, and pinned child HEAD. It then runs the
contract's MVP validation command, at minimum `git diff --check`.

A merge or validation failure is recorded with Git evidence and leaves the
relevant worktrees intact. It never silently resets a successful merge. Rework
persists feedback, starts a successor attempt through the controlled admission
path, and delivers the durable feedback exactly once after admission becomes
unambiguous. If the parent HEAD moved, the successor uses a fresh worktree from
the new parent base and retains old history. Reject records the reason and
retains the unmerged child worktree.

No review action merges the feature branch or a runtime integration branch
into `main`.

## Recovery And Failure Semantics

The existing controlled recovery gate remains authoritative. On daemon restart,
workspace identity, worktree identity, checkpoint state, registered tools, and
Git HEAD/status must attest before a recovered worker receives tools or invokes
the provider.

Before building the vertical slice, the current review checkpoint must close
two known correctness gaps:

- ambiguous rework fallback must release or durably invalidate the SQLite
  `resident:*` lease even when persisting `RecoveryRequired` also fails;
- a post-admission worker factory failure must retain the concrete factory
  error in terminal evidence while closing the attempt as failed.

These fixes are prerequisites because the MVP exercises the same admission,
rework, and restart paths.

## Secondary Controls

Slash commands mirror every MVP control and remain useful when a card is no
longer visible. Existing CLI commands remain supported for automation,
diagnostics, and tests, but the user does not need them for ordinary TUI
development. Generated help describes both card shortcuts and Slash fallbacks
from the same command metadata.

## End-To-End Verification

The deterministic MVP test uses a temporary Git repository, scripted providers,
the real daemon/IPC stack, and the TUI application reducer; it never calls a
real LLM API. It proves this sequence:

1. initialize and commit a clean user checkout;
2. open the TUI and explicitly start/attach the local runtime;
3. submit a normal natural-language coding request;
4. the root provider selects `spawn_agent` and then waits;
5. an inline child card appears and updates without polluting LLM context;
6. the child writes only its assigned worktree, commits, and completes;
7. the card becomes a review card with a real pinned delivery;
8. accepting the focused card merges that exact commit into the root
   integration worktree with a merge commit;
9. the user checkout and its branch/HEAD remain unchanged;
10. SQLite records approval, integration evidence, terminal state, and released
    resident ownership;
11. restarting and reattaching replaces the TUI projection from durable state
    without replaying the accepted delivery.

Separate deterministic paths prove natural-language rework reaches the
successor attempt and reject retains an unmerged worktree. Failure tests cover
runtime-start refusal, daemon disconnect/resync, dirty source checkout, missing
child commit, changed reviewed HEAD, merge conflict, validation failure,
permission denial, and recovery conflict. Rendering tests cover narrow and
normal terminal widths, focused review actions, and input isolation.

## MVP Completion Boundary

The MVP is complete only when the normal TUI conversation workflow runs against
the real daemon and real Git operations, the deterministic end-to-end tests
pass, and focused core/store/tools/application suites pass serially. Unit tests
that invoke the coordinator directly or Slash-only workflows are supporting
evidence but cannot substitute for the TUI-to-Agent-to-daemon-to-child-to-Git
end-to-end test.

The following remain required for the full subagent-runtime milestone but do
not block this first runnable slice:

- a dedicated full task-tree side panel and richer navigation;
- root-to-child-to-leaf end-to-end delegation and direct-parent integration;
- natural-language schedules executing the same worker pipeline;
- all remaining checkpoints 1-20 audit items;
- strict Clippy and project-management updates;
- final user validation and an explicitly authorized merge decision.

No MVP operation merges the current feature branch or any runtime integration
branch into `main`.
