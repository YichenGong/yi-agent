# Subagent Orchestration Capabilities Design

## Goal

Give a parent agent the minimum capability surface it needs to supervise its
children, so that orchestration is expressible in prompts rather than baked
into the runtime.

## Scope

Extend or add agent-facing tools and their IPC wiring:

- `spawn_agent` — add an optional per-child `model`.
- `wait_agent` — include each child's delivery summary in its return value.
- `inspect_agent` — new; read one child's state, delivery, diff, and report.
- `cancel_agent` — new; stop a descendant.
- Authorization for the two new read/control tools.

## Non-Goals

- No workflow is encoded in the runtime. No fix-loop, no round counting, no
  reviewer/implementer distinction. The model composes those from the tools.
- No `keep_alive` / resident-idle children.
- No session-history persistence and no general child resumption.
- No change to the sandbox least-privilege or parent-merge work.

## The single real gap: a parent cannot see a child's result

A parent already has three tools. Delegation starts fine. What is missing is
sight: after `wait_agent` reports that a child delivered, the parent receives
`status`, `children`, and text `reports` only. It never learns the delivered
`commit`, and `wait_agent(mode = all)` returns `needs_attention` with an empty
child list on delivery.

A child becomes terminal only after its commit is an ancestor of the parent
`HEAD` (`reconcile_integrated_deliveries`). So a parent that only waits can
never satisfy the condition it is waiting for. This one gap is the root cause
of both the observed deadlock and the unmerged-delivery failure.

The fix does not need new machinery: `InspectTask` and `ReadTaskDiff` already
expose `delivery_json`, and the TUI already uses both. They are simply not
exposed to the agent. `delivery_json` carries `commit` via `DeliveryReport`.

## Capability surface

### spawn_agent: optional model

Add `model` to the tool schema and carry it to `SpawnChild` /
`SpawnApplicationChild`. The child's `AgentConfig.model` overrides the
factory's. The model is already a per-request field (`AgentConfig.model`,
read on each call at `agent.rs:503`), so no provider rebuild is needed.
Omitted means inherit the parent's model.

### wait_agent: delivery summary

Keep the existing return shape and add a delivery summary per child, derived
from `delivery_json` when present. This lets a parent act immediately on a
delivery instead of having to poll. `status` semantics do not change.

### inspect_agent: read one child

New tool. Input: `task_id`, `include_diff` (default false). Output: the
child's state, delivery summary, text report, and — only when
`include_diff` is true — the diff. Diff defaults off because a large diff must
not enter the parent's context unrequested; the parent can request it when it
needs to hand a review package to another agent.

The tool must accept a completed child. The existing `send_message` refuses
terminal recipients, so it cannot serve this purpose.

### cancel_agent: stop a descendant

New tool. Wraps the existing two-step `PreviewCancel` / `ConfirmCancel` path,
which is fully implemented and already used by the TUI. Constrained to the
caller's own descendants.

### send_message: unchanged

Its behavior does not change. That it refuses to wake a terminal child is
intentional and stays.

## Authorization (required, security-relevant)

`InspectTask`, `ReadTaskDiff`, and the cancel path perform **no caller
authorization**. They accept a `task_id` and act on it. The daemon socket is
protected by filesystem permissions (runtime dir `0o700`, socket `0o600`), so
this is not remotely reachable, but any task inside a runtime can read another
project's delivery or cancel another project's running child.

Spawning is different: `SpawnApplicationChild` carries a capability that the
coordinator verifies.

Therefore exposing `inspect_agent` and `cancel_agent` must not be plain
wiring. Each must authorize the caller.

Chosen rule: a task may only inspect or cancel tasks in **its own descendant
subtree**. This matches the adjacency semantics `send_message` already uses.
The subtree is computable with the existing recursive query
(`Repository::task_tree_ids`) and the existing capability primitive
(`can_use_worker_capability` / `authorize_application_root`).

Constraint discovered during design: the TUI calls `InspectTask` and
`ReadTaskDiff` as the local human operator, with no task identity, from
`daemon_agent_detail_at` and `daemon_task_session_at`. Authorization must
therefore live on the agent-facing path — either new authorized requests used
by the tools, or an optional caller identity on the existing requests that is
required when present.

## Child reports

The structured return channel already exists and is durable: a text-completing
child stores its report in `attempts.terminal_json` as
`{"kind":"text_completion","report":...}`, and it survives a daemon restart
(tested). No new report store is needed.

Handoff rules:

- Default: structured return values. This works for every child, including
  read-only children.
- Optional: a `coding` child may write a report file. It naturally lands in
  that child's worktree, where a later reviewer can read it.
- Read-only children get no file-write exception. A read-only session has no
  `write`/`edit` tool and its shell is denied file writes by the process
  sandbox; this design does not weaken that.

## Deferred

Recorded so the design is not re-litigated later, but not implemented:

- `keep_alive` / resident-idle children.
- General child resumption. No table stores conversation history — all
  nineteen existing tables were checked; `sessions.config_json` is always
  `'{}'`. A new attempt is a successor attempt with a fresh in-memory agent
  and only the original objective, so "resume the same implementer" is not
  currently expressible. The report file is the intended stand-in, which is
  why report handoff above is in scope and resumption is not.

## Verification

Deterministic, before any real-provider run:

- A parent inspects a delivered child, obtains the commit, merges it, and the
  child then reaches a terminal state. This directly locks the deadlock shut.
- A parent can inspect a child that has already completed.
- `include_diff` false returns no diff; true returns one.
- A caller cannot inspect or cancel a task outside its descendant subtree.
- `spawn_agent` with `model` runs the child on that model.
- `cancel_agent` cancels through the two-step confirmation path.
- Existing suites stay green, including the TUI paths that use the
  unauthorized `InspectTask` / `ReadTaskDiff` directly.

## Assumptions Requiring Confirmation

1. Authorization is limited to a caller's own descendant subtree.
2. `include_diff` defaults to false.
3. Deferred items stay deferred in this design.
