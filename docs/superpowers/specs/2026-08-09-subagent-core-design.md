# Subagent Core Runtime Design

## Purpose

Define the pure, process-local semantics that belong in `yi-agent-core`: task
identity, attempts, contracts, authority, mailbox routing, and Supervisor
control. This specification deliberately does not choose SQLite, IPC, or Git
commands; those are covered by companion specifications.

## References

- [Architecture decision index](2026-08-09-subagent-architecture-design.md)
- [Codex delegation research](../../research/2026-07-26-codex-long-running-task.md#8-任务分解子-agent-委派)
- Existing single-agent loop: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Existing permission model: `yi-agent-rs/crates/yi-agent-core/src/permission.rs`

## Data Model

```rust
pub struct AgentTask {
    pub id: TaskId,
    pub parent_id: Option<TaskId>,
    pub root_id: RootSessionId,
    pub depth: TaskDepth, // Root=0, Child=1, Leaf=2
    pub contract: ContractVersion,
    pub authority: DelegatedAuthority,
    pub state: TaskState,
    pub active_attempt: AttemptId,
    pub children: Vec<TaskId>,
    pub delivery: DeliveryState,
}

pub struct TaskAttempt {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub checkpoint: Option<StableCheckpoint>,
    pub budget: EffectiveBudget,
    pub started_at: Timestamp,
    pub terminal_reason: Option<TerminalReason>,
}
```

`TaskId` never changes. Every retry, rework, and recovery creates a new
`TaskAttempt`; historical attempts and their evidence remain queryable.

## State Guards

`Queued -> Running` requires a live parent (unless root), an active contract,
all admission permits, and no cancellation. `Running -> AwaitingParentReview`
requires a validated delivery report. `AwaitingParentReview -> Completed`
requires a direct-parent accept event and a successful integration validation.

`WaitingForPermission`, `WaitingForResource`, and `WaitingForChildren` store a
typed wait reason. Only that reason's resolver may return the task to `Queued`.
User pause wins over a resolved wait; cancellation wins over every nonterminal
state. A terminal task may re-enter `Queued` only through an explicit retry or
rework event that creates a new attempt.

## Delegation Contract And Authority

The runtime, not the model, constructs `DelegationContract` from a draft and
parent state. Required fields are objective/non-goals, fact packets with source,
path scope, workspace/base ref, acceptance checks, delivery policy, budget, and
review owner. A contract is immutable; amendments form `v2`, `v3`, and so on.

Authority derivation is monotonic:

```text
child tools      subset(parent tools)
child paths      subset(parent paths)
child budget     <= parent unallocated budget
child deadline   <= parent deadline
child depth      = parent depth + 1 <= 2
```

An attempted widening is rejected before any worker or worktree exists.
High-risk security permission remains a user-policy decision even if task scope
permits the operation.

## Mailbox Contract

`MailboxMessage` has immutable `id`, sender/recipient IDs, kind, priority,
causation ID, correlation ID, timestamp, payload, and delivery policy. Messages
are direct-parent/direct-child by default. The Supervisor creates explicit,
audited user-override messages for cross-level intervention.

`Progress` coalesces per `(sender, correlation_id)` and does not wake a parent.
`Completed`, `Blocked`, `Failed`, `ScopeChangeRequest`, and permission requests
set `pending_input`; an idle parent becomes runnable. Identical terminal reports
on the same correlation chain are idempotent. `wait_agent` yields the parent's
LLM permit and returns `NeedsAttention` for a high-priority message.

## Supervisor API

```rust
async fn spawn(parent: TaskId, draft: ContractDraft) -> SpawnResult;
async fn wait(caller: TaskId, selector: WaitSelector) -> WaitResult;
async fn send(message: MailboxMessageDraft) -> DeliveryReceipt;
async fn transition(task: TaskId, event: TaskEvent) -> TransitionResult;
async fn cancel(task: TaskId, mode: CancelMode) -> CancellationReport;
```

`spawn` returns after persistence and admission enqueue; it never waits for a
child result. Supervisor events are the only source of task state for clients.
Worker output is converted to events; a worker cannot mutate task state directly.

## Required Tests

- Legal/illegal transition matrix, including cancellation precedence.
- Depth-two rejection and direct-child quota rejection.
- Authority narrowing over tools, paths, budgets, and deadlines.
- Contract amendment provenance and material-scope rejection.
- Progress coalescing, terminal-message idempotency, and parent wake behavior.
- `wait(all)`/`wait(any)` interruption by permission or blocking messages.
