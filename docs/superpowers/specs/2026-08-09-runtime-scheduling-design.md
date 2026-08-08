# Runtime Scheduling And Budget Design

## References

- [Architecture decision index](2026-08-09-subagent-architecture-design.md)
- Current Cargo concurrency constraints in `CLAUDE.md`
- [Codex budget and execution research](../../research/2026-07-26-codex-long-running-task.md#2-多层-token-预算系统)

## Defaults

```text
resident subagents: 16 global, 16 maximum per root session
queued tasks:       64
depth/direct child: 2 / 4
LLM permits:        8 per provider/API-key; reserve 1 for coordination
coding permits:     6
host builds:        2
Cargo workspace:    1 exclusive lease for this project
scheduled default:  read-only, background, 4 residents, 30 turns, 15 minutes
```

Limits compose by minimum; path/tool authority composes by intersection. User
runtime ceilings > project ceilings > root-session selections > child contracts.
Only a user can raise a root selection within the user ceiling.

## Admission

Each resource has a queue. The Coordinator selects a root session by weighted
fair rotation with age boost, then selects a runnable subtree by the same rule.
Interactive roots outweigh background schedules; unused shares are borrowed and
reclaimed only on future admission, never by killing a running task. A task
holds only the permits it actively needs.

Resource requests are generic: `(scope, key, mode, units)`. Cargo is one
project adapter rule, not coordinator special logic. A parent waiting for child
results yields its LLM permit; one coordination permit protects completion,
permission, and review processing from leaf saturation.

## Watchdogs

Every attempt has turn/token/cost, wall-clock, idle-progress, retry, rework,
and resource-wait bounds. Meaningful progress excludes generated text and
repeated stdout. Exceeded limits create visible terminal states and release all
permits. Scheduled overlap defaults to skip; missed daemon-offline runs default
to skip.

## Required Tests

- Equal projects share capacity; an idle project lends capacity.
- Aged background work eventually admits.
- Depth-two descendants cannot evade root/global limits.
- Coordination permit admits an eligible parent under leaf saturation.
- Resource deadline and every watchdog release permits exactly once.

## Configuration Schema And Effective Policy

User runtime configuration is TOML; project configuration may only narrow it.

```toml
[runtime]
max_resident_subagents = 16
max_queued_subagents = 64
max_depth = 2
max_direct_children_per_agent = 4

[resources]
max_llm_requests_per_provider_key = 8
reserved_coordination_llm_requests = 1
max_coding_agents = 6
max_host_build_jobs = 2

[attempt_defaults]
max_turns = 100
max_wall_time_secs = 2700
max_idle_time_secs = 300
max_provider_retries = 3
max_tool_retries = 2
max_rework_cycles = 2

[schedule_defaults]
max_resident_subagents = 4
max_turns = 30
max_wall_time_secs = 900
priority = "background"
read_only = true
overlap_policy = "skip"
missed_run_policy = "skip"
```

The resolver computes limits with `min(user, project, root, parent, child)` and
capabilities with set intersection. Project configuration can declare a named
resource constraint, for example `cargo:<canonical-workspace-root> = exclusive
1`; it cannot create extra host build or provider permits. Reducing a limit
affects future admissions immediately. Increasing it requires a user-level
configuration change and never automatically retries a terminal task.

## Resource Keys And Lease Lifecycle

```rust
pub struct ResourceRequest {
    pub scope: ResourceScope, // Global, ProviderKey, Project, Workspace, Branch, Path
    pub key: String,
    pub mode: LeaseMode,      // Shared or Exclusive
    pub units: u16,
    pub deadline: DateTime<Utc>,
}
```

Canonical keys are:

```text
resident:global
llm:<provider-profile-id>
coding:global
build:host
cargo:<canonical-cargo-workspace-root>
git-integrate:<canonical-repo-root>:<target-branch>
worktree:<canonical-worktree-path>
```

Admission acquires only the lease needed by the next step. A task releases an
LLM lease when it waits for a child, permission, tool, or resource. A workspace
lease lives through task review because it protects the delivery worktree. A
lease is released exactly once by a terminal transition, explicit integration
cleanup, or daemon crash reconciliation; release is idempotent by lease ID.

## Fair Queue Algorithm

Each resource owns a queue of `AdmissionRequest { task_id, root_id, parent_id,
priority, enqueued_at, sequence }`. Choose a request as follows:

```text
1. Remove requests whose task is cancelled, terminal, expired, or no longer needs the resource.
2. Partition remaining requests by root session.
3. Compute root score = priority_weight + min(wait_seconds / 30, 20) - recent_grants_penalty.
4. Select the highest score; ties rotate by the persisted round-robin cursor.
5. Within that root, repeat the same score/rotation by direct-parent subtree.
6. Grant only if all requested units and compatibility checks succeed; otherwise continue scanning.
7. Persist the grant and cursor before notifying the worker.
```

Weights are `Critical=40`, `High=20`, `Normal=10`, `Background=0`. A root with
no runnable request does not reserve capacity. `recent_grants_penalty` is one
point per outstanding permit of the same resource, which produces soft fairness
without preempting running work. The age cap prevents an old background schedule
from permanently outranking interactive work.

One LLM permit is logically reserved for a root/parent with a high-priority
mailbox event. It may be borrowed only when no eligible coordination request is
queued; the borrowed worker releases it at its next provider-turn boundary when
coordination becomes eligible.

## Task Admission And Queue Semantics

`max_resident_subagents` counts nonterminal child/leaf tasks except `Queued`.
Tasks that cannot become resident remain `Queued` and count against the 64 task
queue cap. On resident admission the Coordinator reserves a resident slot before
creating a worker. A task waiting for permission or children remains resident;
it has state/mailbox/workspace ownership but no LLM permit.

If a parent attempts to spawn while global resident capacity is full, the child
is persisted as `Queued`. If the queue cap is full, `spawn_agent` returns
`QueueCapacityExceeded` and creates no task. This makes high fan-out visible and
bounded rather than silently allocating unbounded state.

## Watchdog Evidence And Retry Policy

```rust
pub struct WatchdogEvidence {
    pub last_meaningful_event_id: Option<i64>,
    pub last_meaningful_at: DateTime<Utc>,
    pub elapsed_secs: u64,
    pub current_wait: Option<ResourceWait>,
}
```

Provider network/429/5xx errors retry at most three times with exponential
backoff capped at 30 seconds and jitter. Tool retries are contract-controlled
and default to two only for explicitly retryable tool failures. Permission
denial, Git conflicts, test failures, authority errors, and scope ambiguity do
not auto-retry; they become `Blocked` or `Failed` with evidence for a parent.

Idle time resets only for a persisted meaningful event defined by core. Resource
wait timeout begins when the request is queued, not when the worker last ran.
All watchdog outcomes emit one terminal event and invoke idempotent lease release.

## Scheduled Root Instances

A schedule stores an immutable `ScheduleDefinition` and creates a new root
session/contract snapshot per fire. It never inherits an interactive session's
mailbox or chat history. `skip` overlap emits `ScheduleSkippedOverlap`; `queue`
would enqueue one pending run only and is not a default. Missed time while the
daemon is stopped emits `ScheduleMissed`; `catch_up_once` is an explicit opt-in.
Scheduled roots cannot use a provider profile or permission policy unavailable
to the daemon and cannot elevate read-only defaults without user approval.

## Scheduler Test Matrix

| Test | Assertion |
|---|---|
| `effective_policy_only_narrows` | every child/project value is bounded by parent/user |
| `exclusive_workspace_lease_serializes_cargo` | same workspace never has two active Cargo leases |
| `roots_share_idle_capacity_then_rebalance` | borrowing happens until another root becomes runnable |
| `age_prevents_background_starvation` | aged background task gains admission within bounded grants |
| `coordination_reserve_unblocks_parent` | leaf saturation cannot block review/permission handling |
| `queue_limit_rejects_without_persisting_task` | no hidden unbounded task allocation |
| `watchdog_releases_each_lease_once` | terminal outcomes are idempotent |
