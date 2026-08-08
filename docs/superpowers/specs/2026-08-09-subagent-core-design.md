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

## Canonical Rust Types

The initial implementation uses opaque UUID-backed newtypes rendered as strings
at the API boundary. Do not use a branch name, tool-use ID, or provider request
ID as a task identity.

```rust
pub struct TaskId(pub Uuid);
pub struct AttemptId(pub Uuid);
pub struct RootSessionId(pub Uuid);
pub struct MessageId(pub Uuid);
pub struct ContractVersion(pub u32);

pub enum TaskDepth { Root, Child, Leaf }

pub enum TaskState {
    Queued,
    Running,
    WaitingForResource(ResourceWait),
    WaitingForPermission(PermissionRequestId),
    WaitingForChildren(WaitSelector),
    Paused(PauseReason),
    AwaitingParentReview(DeliveryId),
    Completed,
    CompletedNoChanges,
    Blocked(BlockReason),
    Stalled(WatchdogEvidence),
    TimedOut(TimeoutKind),
    BudgetExhausted(BudgetKind),
    Failed(TaskFailure),
    Cancelled(CancelReason),
    RecoveryRequired(RecoveryEvidence),
}

pub enum DeliveryState {
    None,
    ReadyForReview(DeliveryId),
    Accepted { delivery: DeliveryId, integration: IntegrationId },
    ReworkRequested { previous: DeliveryId, feedback: MessageId },
    Rejected { delivery: DeliveryId, reason: MessageId },
}
```

`CompletedNoChanges` is valid only for a contract whose delivery policy allows
no-change completion. A coding task requiring a commit cannot enter it. All
states after `AwaitingParentReview` describe task delivery, not whether a
provider stream happened to return normally.

The durable task record is intentionally split from mutable execution data:

```rust
pub struct AgentTask {
    pub id: TaskId,
    pub root_session_id: RootSessionId,
    pub parent_id: Option<TaskId>,
    pub depth: TaskDepth,
    pub created_at: DateTime<Utc>,
    pub current_contract: ContractVersion,
    pub authority_id: AuthorityId,
    pub active_attempt: AttemptId,
    pub state: TaskState,
    pub delivery: DeliveryState,
    pub workspace: Option<WorkspaceLeaseId>,
}

pub struct TaskAttempt {
    pub id: AttemptId,
    pub task_id: TaskId,
    pub number: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub checkpoint: Option<StableCheckpoint>,
    pub budget: EffectiveBudget,
    pub usage: AttemptUsage,
    pub terminal_reason: Option<TerminalReason>,
}
```

An attempt owns a fresh cancellation token. `Agent::run()` already resets its
token for sequential runs; the Supervisor must additionally ensure an old token
is never reused for a retry, rework, or recovery attempt.

## Event-Sourced Transition Contract

Only `TaskEvent` changes `AgentTask.state`; workers, UI clients, and tools emit
events through the Supervisor. Every accepted event creates exactly one durable
event-journal row and one state snapshot update.

```rust
pub enum TaskEvent {
    AdmissionGranted { permits: Vec<PermitId> },
    ResourceUnavailable { wait: ResourceWait },
    PermissionRequested { request: PermissionRequestId },
    PermissionResolved { request: PermissionRequestId, decision: Decision },
    ChildJoinRequested { selector: WaitSelector },
    ChildJoinResolved { result: WaitResult },
    WorkerDelivered { delivery: DeliveryReport },
    ReviewAccepted { delivery: DeliveryId, integration: IntegrationId },
    ReviewRework { delivery: DeliveryId, feedback: MessageId },
    ReviewRejected { delivery: DeliveryId, reason: MessageId },
    PauseRequested { reason: PauseReason },
    ResumeRequested,
    CancelRequested { reason: CancelReason, recursive: bool },
    WatchdogExpired { evidence: WatchdogEvidence },
    BudgetExceeded { kind: BudgetKind },
    RuntimeInterrupted { evidence: RecoveryEvidence },
    RetryRequested { actor: ActorId },
}
```

Transition guards are normative:

| Event | Source states | Guard | Result |
|---|---|---|---|
| `AdmissionGranted` | `Queued` | attempt is current; all required permits held | `Running` |
| `ResourceUnavailable` | `Running` | no incompatible permit retained | `WaitingForResource` |
| `PermissionRequested` | `Running` | request authority is valid | `WaitingForPermission` |
| `PermissionResolved(allow)` | waiting permission | request ID matches active wait | `Queued` |
| `PermissionResolved(deny)` | waiting permission | request ID matches active wait | `Blocked(PermissionDenied)` |
| `WorkerDelivered` | `Running` | report validates against contract and workspace | `AwaitingParentReview` |
| `ReviewAccepted` | awaiting review | actor is direct parent; integration succeeded | `Completed` |
| `ReviewRework` | awaiting review | actor is direct parent; limit not exceeded | `Queued` with new attempt |
| `CancelRequested` | any nonterminal state | actor has cancellation authority | `Cancelled` |
| `RuntimeInterrupted` | running/waiting | daemon lost worker ownership | `RecoveryRequired` |

`PauseRequested` records an intent while a worker is in a noninterruptible tool
step; the state becomes `Paused` only at the next safe checkpoint. Cancellation
is stronger: it signals the token immediately, rolls back incomplete model
tool-use history to the last stable checkpoint, and recursively signals children
when requested.

## Contract Wire Format

The draft submitted by `spawn_agent` is JSON-compatible and validated before it
becomes `DelegationContract`:

```json
{
  "title": "Implement task transitions",
  "objective": "Add legal transition checks and tests.",
  "non_goals": ["Do not modify TUI rendering."],
  "context": [{"fact": "Root uses a two-level limit.", "source": "user-decision"}],
  "scope": {"read_paths": ["crates/yi-agent-core/**"], "write_paths": ["crates/yi-agent-core/src/subagent/**"]},
  "acceptance": {"commands": ["cargo test -p yi-agent-core subagent::task::tests"], "require_commit": true},
  "delivery_policy": "immediate",
  "budget": {"max_turns": 50, "max_wall_time_secs": 1800}
}
```

The persisted version adds derived `task_id`, parent/root IDs, effective
authority, effective budget, worktree/base commit, creation time, and a digest
of the exact prompt fact packet. `context` is capped by a configured byte/token
budget. A reference to a file/commit is preferred to copying large content.

An amendment is one of `AddContext`, `NarrowScope`, `ExtendScope`,
`ChangeAcceptance`, `ChangeBudget`, or `ChangeDeliveryPolicy`. It stores the
parent event ID and reason. `ExtendScope` is rejected if it exceeds the parent
authority; every amendment is presented to the worker as a new controller input
at a safe checkpoint.

## Authority Matrix

| Operation | Contract authority | Additional decision |
|---|---|---|
| Read assigned paths | inherited read capability | none |
| Write assigned worktree paths | worktree/path lease | none if session policy permits |
| Run approved formatter/test | tool and resource lease | none |
| Commit assigned branch | Git commit capability | required checks must pass |
| Merge direct child branch | parent integration lease | direct-parent review decision |
| Read ancestor/peer worktree | never inherited | lease-owner approval |
| External network/secrets/privilege | never silently delegated | user-security approval |
| Cross-project resource/configuration | never child delegated | user/coordinator approval |

`DelegatedAuthority::derive_child` returns a typed error naming the attempted
widening: `DepthExceeded`, `ToolNotDelegable`, `PathOutsideLease`,
`BudgetOverallocated`, `DeadlineAfterParent`, or `GitOperationNotDelegable`.

## Mailbox Payloads And Priority

```rust
pub enum MessageKind {
    Progress(ProgressReport),
    Completed(DeliveryReport),
    Blocked(BlockReason),
    Failed(TaskFailure),
    PermissionRequest(PermissionRequestId),
    ScopeChange(ScopeChangeDraft),
    Rework(ReworkInstruction),
    UserInstruction(UserInstruction),
}

pub enum MessagePriority { Critical, High, Normal, Background }
```

`PermissionRequest` is `Critical`; `Blocked`, `Failed`, `ScopeChange`, and
`Completed` are `High`; `Progress` is `Normal` and is coalesced; background
schedule notices are `Background`. Coalescing retains the newest payload and
increments a count, so the parent can see that updates were compressed. A
delivery message is never coalesced or dropped.

## Supervisor Worker Boundary

The Supervisor owns task state and worker handles. A worker receives immutable
attempt input plus channels; it cannot receive an `&mut AgentTask`.

```rust
pub struct WorkerStart {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub prompt: AssembledPrompt,
    pub workdir: PathBuf,
    pub cancellation: CancellationToken,
}

pub enum WorkerEvent {
    Agent(AgentEvent),
    SafeCheckpoint(StableCheckpoint),
    Delivery(DeliveryReport),
    Failure(TaskFailure),
}
```

The worker forwards normal `AgentEvent` values for UI visibility, but the
Supervisor interprets only `SafeCheckpoint`, `Delivery`, and `Failure` as task
lifecycle input. A provider's `Done(EndTurn)` means the worker finished a model
turn; it is not automatically a task delivery.

## Core Test Matrix

| Test name | Proves |
|---|---|
| `leaf_cannot_spawn_descendant` | depth 2 is a hard cap |
| `retry_creates_new_attempt_without_erasing_previous` | attempt history remains auditable |
| `cancel_wins_over_permission_resolution` | cancellation precedence |
| `delivery_requires_validated_commit_report` | no false completed coding task |
| `review_accept_requires_direct_parent_and_integration` | branch ownership chain |
| `progress_messages_coalesce_without_waking_parent` | no notification storm |
| `wait_is_interrupted_by_permission_request` | no parent/child wait deadlock |
| `authority_derivation_is_monotonic` | children cannot escalate scope |
