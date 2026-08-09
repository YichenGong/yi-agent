# Subagent Runtime Development Handoff

**Date:** 2026-08-09

**Repository:** `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent`

**Active worktree:** `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat-subagent-core`

**Branch:** `feat/subagent-core`

**Implementation HEAD before this handoff:** `7e9950b`

**Status:** implementation in progress; do not merge to `main`

## 1. Objective

Continue the implementation defined by
[`2026-08-09-subagent-architecture.md`](../plans/2026-08-09-subagent-architecture.md).
The final result is a manually started, observable, durable two-level subagent
runtime with:

- `spawn_agent`, `wait_agent`, and `send_message`;
- a supervisor-owned root/child/leaf task tree with maximum depth two;
- inherited root capabilities narrowed by a delegation contract;
- a global resource coordinator with a default limit of 16 resident
  subagents;
- isolated Git worktrees and commit-based child delivery;
- direct-parent review and integration, never automatic merge to `main`;
- a user-started local daemon, SQLite state, versioned IPC, recovery, and
  schedules;
- CLI, TUI, Slash commands, generated help, observability, and user
  intervention;
- end-to-end cancellation, review, recovery, and two-level delegation tests.

Do not redefine completion around the code that already exists. The full plan,
all supporting specifications, and their named verification criteria remain
authoritative.

## 2. Non-Negotiable Repository Rules

1. Never modify, commit, merge, revert, or otherwise operate on `main`.
2. Use only the existing `feat/subagent-core` worktree for this continuation.
3. The user must validate the completed branch and explicitly authorize the
   final merge.
4. Do not delete or recreate the worktree. It contains the complete reviewed
   branch history.
5. Do not amend existing commits unless the user explicitly requests it.
6. Commit messages must use conventional commits and must not include a
   `Co-Authored-By` line.
7. Before every commit, run `cargo fmt --all` from `yi-agent-rs/`, then run
   `git diff --check`.
8. Run Cargo commands serially. Never run Cargo tests in two shells or
   worktrees at the same time.
9. Do not run `cargo test --workspace`. Run one affected crate at a time.
10. Before each Cargo command, check for stale processes:

```bash
ps aux | rg '[c]argo|[r]ustc|yi_agent' || true
```

If a test is interrupted, also check for orphaned `yi_agent_*` test binaries
before starting another Cargo command. Follow the project-level `AGENTS.md`
instructions for lock cleanup and macOS `sample` diagnostics.

## 3. Required Superpowers Workflow

Use these skills where applicable:

- `superpowers:using-superpowers`
- `superpowers:using-git-worktrees`
- `superpowers:subagent-driven-development`
- `superpowers:test-driven-development`
- `superpowers:systematic-debugging`
- `superpowers:requesting-code-review`
- `superpowers:verification-before-completion`
- `superpowers:finishing-a-development-branch` only after the entire plan is
  complete and the user is ready to decide integration

For every implementation checkpoint:

1. Dispatch a fresh implementer Agent with the complete task text and relevant
   specification excerpts.
2. Require RED evidence before production changes, then GREEN evidence,
   formatting, self-review, and a commit.
3. Dispatch a fresh specification reviewer after the implementation commit.
4. Fix every Critical or Important specification finding and send the result
   back for re-review.
5. Only after specification approval, dispatch a different fresh code-quality
   reviewer.
6. Fix every Critical or Important quality finding and send the result back
   for re-review.
7. Run fresh verification from the parent Agent before marking the checkpoint
   complete.

Do not run multiple implementers concurrently in this shared worktree. Read-only
reviewers operate after a clean commit, not while an implementer is editing.

## 4. First Commands in a New Session

Run these before relying on this document's recorded state:

```bash
git -C .worktrees/feat-subagent-core status --short
git -C .worktrees/feat-subagent-core branch --show-current
git -C .worktrees/feat-subagent-core log --oneline -15
ps aux | rg '[c]argo|[r]ustc|yi_agent' || true
```

Expected state before the handoff commit is applied:

```text
branch: feat/subagent-core
HEAD:   7e9950b fix: drop unopened frames after IPC producer close
status: clean
```

The handoff document itself adds a later `docs:` commit. Treat the actual
worktree as authoritative if its HEAD is newer.

## 5. Authoritative Design Files

Read the relevant specification before assigning each checkpoint:

- Overall architecture:
  [`2026-08-09-subagent-architecture-design.md`](../specs/2026-08-09-subagent-architecture-design.md)
- Core task, contract, authority, mailbox, and state model:
  [`2026-08-09-subagent-core-design.md`](../specs/2026-08-09-subagent-core-design.md)
- Worktree ownership and commit delivery:
  [`2026-08-09-subagent-worktree-design.md`](../specs/2026-08-09-subagent-worktree-design.md)
- Daemon, SQLite, IPC, stop, and recovery:
  [`2026-08-09-runtime-daemon-design.md`](../specs/2026-08-09-runtime-daemon-design.md)
- Global scheduling and limits:
  [`2026-08-09-runtime-scheduling-design.md`](../specs/2026-08-09-runtime-scheduling-design.md)
- CLI, TUI, Slash commands, help, and confirmations:
  [`2026-08-09-subagent-control-surface-design.md`](../specs/2026-08-09-subagent-control-surface-design.md)

The component plans provide more focused task boundaries:

- [`2026-08-09-subagent-core.md`](../plans/2026-08-09-subagent-core.md)
- [`2026-08-09-subagent-worktree.md`](../plans/2026-08-09-subagent-worktree.md)
- [`2026-08-09-runtime-daemon.md`](../plans/2026-08-09-runtime-daemon.md)
- [`2026-08-09-runtime-scheduling.md`](../plans/2026-08-09-runtime-scheduling.md)
- [`2026-08-09-subagent-control-surface.md`](../plans/2026-08-09-subagent-control-surface.md)

## 6. Implemented Foundation

The branch contains commits for the following foundations. This list records
implemented work; it is not proof that the entire parent milestone is complete.

- Task IDs, attempts, depth, state reduction, terminal and review transitions.
- Delegation contracts, narrowed authority, mailbox semantics, and progress
  coalescing.
- Supervisor task indexes, cancellation trees, worker ownership, and built-in
  `spawn_agent`, `wait_agent`, and `send_message` schemas and routing.
- Default resource capacities, fairness primitives, resident-subagent
  admission, and exclusive named leases.
- Child worktree creation from committed bases, direct-parent merge direction,
  delivery commit pinning, dirty-worktree retention, and accepted-clean
  cleanup.
- SQLite runtime schema, atomic task/event persistence, attempts, mailbox
  records, leases, schedules, and append-only events.
- User-private single-instance daemon files, local Unix socket, stale-lock
  recovery, daemon start/status/stop clients, and worker-factory ownership.
- Runtime coordinator integration for spawning, waiting, messaging,
  cancellation, retry, pause, and worker lifecycle reconciliation.
- TUI/CLI task snapshots, inspect details, cancellation, retry, user messages,
  pause/resume controls, Slash command usage metadata, and contextual help
  foundations.
- Safe-checkpoint pause acknowledgement and durable external-message consumption
  acknowledgement.
- Versioned IPC envelopes, typed errors, bounded frames, event subscriptions,
  cursor replay, filters, and slow-subscriber backpressure.

Inspect the complete branch history when exact provenance is required:

```bash
git log --reverse --oneline main..HEAD
```

Do not mark any project-management item complete merely because related commits
exist. Re-run its named acceptance command and confirm the complete feature
criterion first.

## 7. Current Gate: IPC Subscription Review

The immediate task is not checkpoint 17. First close the two-stage review for
the IPC envelope, cursor, filter, and subscription-backpressure work.

### 7.1 Review Range

```text
Base: 0f3be6b fix: preserve IPC not-found error semantics
Head: 7e9950b fix: drop unopened frames after IPC producer close
```

Relevant commits:

```text
d36299c feat: add IPC subscription backpressure
796ae45 fix: preserve append-only IPC event history
7ba7756 fix: release runtime store before async confirmations
2e80cba fix: preserve IPC resync under backpressure
6de4f73 fix: close IPC subscription writer races
7e9950b fix: drop unopened frames after IPC producer close
```

### 7.2 Required Specification Review

Create a fresh read-only specification reviewer. It must compare the range
against the runtime daemon specification and report findings by severity with
file and line references. At minimum, verify:

1. Every request, response, error, and event frame is versioned and correlated
   with the correct request or event identity.
2. The public protocol uses stable tagged names rather than Rust `Debug`
   formatting.
3. The nine specified error categories are preserved, including `NotFound` and
   validation behavior for malformed or oversized frames.
4. A fresh cursor receives a current replacement snapshot without historical
   replay.
5. A replayable cursor receives ordered history followed by ordered live events
   across one transactional high-water boundary.
6. An expired cursor receives a replacement snapshot, and the replay floor does
   not delete or rewrite append-only audit events.
7. Task and event-kind filters apply identically to replay and live events.
8. Every subscriber has an independent buffer of exactly 1,024 pending event
   frames.
9. Overflow discards all event frames that have not started writing, emits
   exactly one versioned `ResyncRequired` correlated to the subscription's
   `request_id`, then produces EOF.
10. A partially written event frame is completed before `ResyncRequired`, so
    the client never observes malformed JSON framing.
11. The overflow check and the first nonblocking write are atomic with respect
    to the queue state.
12. A producer close or failure terminates a writer blocked on `WouldBlock` and
    cannot leave a client-handler thread spinning forever.
13. A frame already dequeued but not yet written is dropped if the producer
    closes before its first write.
14. One slow subscriber cannot block or close another healthy subscriber.
15. The v1-to-v2 migration preserves real event IDs and gaps, initializes the
    replay floor correctly, and leaves existing cursors replayable when valid.
16. Synchronous repository mutexes are not held across async confirmation
    awaits.

Do not start code-quality review until this reviewer explicitly approves the
specification. If it reports a Critical or Important issue, use an implementer
to add a deterministic failing test, fix it, commit it, and request re-review.

### 7.3 Required Code-Quality Review

After specification approval, create a different fresh read-only reviewer for
the same range. It should inspect:

- lock ordering and mutex scope;
- nonblocking socket correctness and CPU-spin behavior;
- partial-write state and shutdown paths;
- producer/writer thread termination and joins;
- error propagation and framed-error behavior;
- queue invariants and race-test determinism;
- migration safety and append-only guarantees;
- test flakiness, timeouts, process leakage, and cleanup;
- public protocol compatibility and unnecessary API changes.

Fix and re-review all Critical and Important findings before proceeding.

### 7.4 Recorded Verification Evidence

The latest evidence after `7e9950b` is:

```text
cargo test -p yi-agent-store
  library subscription queue tests: 6 passed
  runtime_coordinator:               7 passed
  runtime_ipc:                      33 passed
  scheduler:                         2 passed
  doc tests:                         0 failed

cargo fmt --all: passed
just fmt-check:  passed
git diff --check: passed
```

`cargo clippy -p yi-agent-store --no-deps -- -D warnings` passed after
`6de4f73`, before the final small test/fix in `7e9950b`. Re-run strict clippy
after review; do not treat the earlier run as final evidence.

The downstream TUI test suite passed earlier in the IPC sequence, but it has
not been rerun at `7e9950b`. After both reviews approve, run fresh serial
verification:

```bash
cd yi-agent-rs
ps aux | rg '[c]argo|[r]ustc|yi_agent' || true
cargo test -p yi-agent-store
ps aux | rg '[c]argo|[r]ustc|yi_agent' || true
cargo test -p yi-agent --bin yi-agent
ps aux | rg '[c]argo|[r]ustc|yi_agent' || true
cargo clippy -p yi-agent-core -p yi-agent-store --no-deps -- -D warnings
cargo fmt --all
just fmt-check
git diff --check
```

## 8. Next Checkpoint: Graceful Stop and Recovery

Only begin this section after the IPC specification and quality reviews approve
and the fresh verification commands pass.

Checkpoint 17 requires a fresh implementer Agent using strict TDD. The complete
stop algorithm is:

```text
daemon stop:
  reject new sessions and admissions
  publish Draining event
  request worker safe checkpoints
  wait until the configured grace deadline
  cancel workers that did not acknowledge a checkpoint
  persist Paused or Interrupted outcomes and release process-local permits
  close subscriptions and client handlers
  close the socket and release the daemon lock
```

The complete startup algorithm is:

```text
daemon startup:
  run migrations
  mark attempts recorded as Running or Waiting as RecoveryRequired
  release process-local leases
  retain workspace leases for explicit reconciliation
  publish RuntimeRecovered event
  never auto-retry or replay provider, tool, command, or Git actions
```

Resume after recovery must create a fresh attempt. Its first controller action
must inspect the recorded worktree, `git status`, latest commit, required tool
state, and prior checkpoint. If that inspection cannot prove a safe base, the
task becomes `Blocked(RecoveryConflict)`.

### 8.1 Minimum RED Tests

The implementer should add focused tests proving at least:

1. Once draining starts, `CreateSession`, child spawn, retry, resume, and any
   other admission path are rejected with a typed state error.
2. `Draining` is persisted and published before checkpoint requests.
3. A cooperative worker reports `Paused` only after its safe checkpoint; the
   daemon persists the paused snapshot and releases its resident/process-local
   permit.
4. A non-cooperative worker is cancelled after the grace deadline and is
   persisted as interrupted/recovery-required according to the state model.
5. Stop waits for and joins listener and client-handler threads rather than
   leaving detached handlers.
6. The socket and lock disappear only after state persistence and worker
   reconciliation finish.
7. Restart converts every Running/Waiting task and attempt to
   `RecoveryRequired` atomically with audit events.
8. Process-local leases are released while worktree/workspace leases remain.
9. `RuntimeRecovered` is published once with recovery counts or equivalent
   inspectable evidence.
10. An injected recording worker factory proves restart does not start a worker
    and does not replay a provider/tool/Git action.
11. Resume creates a new attempt containing the mandatory inspection
    instruction; unsafe inspection produces `Blocked(RecoveryConflict)`.

The current `Daemon::stop()` only sets an atomic flag, wakes the listener, and
joins the listener thread. The IPC `Stop` request only sets the same flag and
returns `Stopping`. Startup already calls `recover_inflight_tasks()`, but the
full draining order, safe-checkpoint deadline, event publication, handler joins,
lease classification, no-replay proof, and recovery-resume controller remain
to be implemented or proven.

Likely files:

- `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`
- `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`
- state/reducer or worker files in `yi-agent-core` only when the existing model
  cannot express the required transition

Keep crate dependencies one-way:

```text
yi-agent -> yi-agent-store -> yi-agent-core
```

`yi-agent-store` must not depend on the binary crate.

## 9. Remaining Scope After Checkpoint 17

Do not claim the overall runtime complete after graceful stop/recovery. Audit
and finish the following ordered checkpoints from the architecture plan:

### Checkpoint 18: Scheduling and Effective Configuration

- schedule overlap and missed-run behavior;
- read-only/background/no-overlap/no-catch-up defaults;
- user, project, root-session, and task policy narrowing;
- wall-clock, idle, turn, token/cost, retry, and resource-wait limits;
- visible terminal classification for exhausted limits;
- cross-project fairness through the global coordinator.

Existing schedule-policy tests cover part of this scope, not necessarily the
complete checkpoint. Audit every specification row before marking it complete.

### Checkpoint 19: Shared Command Schema and User Controls

- one schema shared by clap, Slash completion, contextual help, confirmation,
  and daemon routing;
- `/agents`, `/agent`, `/events`, `/diff`, `/mailbox`, `/message`, `/priority`,
  `/pause`, `/resume`, `/cancel`, `/retry`, `/approve`, `/deny`, `/review`,
  `/accept`, `/rework`, `/reject`, `/budget`, `/daemon status`,
  `/help <command>`, and `/?`;
- daemon-unavailable behavior;
- task tree/detail views backed only by daemon snapshots and event streams;
- resource waits, contracts, logs, diffs, commits, delivery evidence, pending
  permissions, and reviews;
- preview/confirmation tokens for destructive actions.

The branch contains foundations for several controls, but the complete shared
schema and every named command are not yet proven.

### Checkpoint 20: End-to-End Runtime

- real root -> child -> leaf two-level delegation;
- parent-only review and integration at both levels;
- commit delivery reports with base/head, diff, clean status, and verification;
- rework on a new base with old evidence retained;
- recursive cancellation and safe cleanup refusal;
- permission timeout and mailbox ping-pong suppression;
- daemon stop/restart and explicit recovery;
- user inspection and intervention throughout the lifecycle.

Run the final crate suites serially and perform a requirement-by-requirement
completion audit against all plans and specifications. A narrow green test does
not prove a broad milestone.

## 10. Project-Management Documentation

[`docs/project-management/subagent-runtime.md`](../../project-management/subagent-runtime.md)
currently records `0 / 9` complete features. Do not update it merely to reflect
partial commits.

Update a feature from `[ ]` to `[x]` only when its complete named acceptance
criterion passes. In the same commit:

1. include a verifiable file location or executable command;
2. update the completed/total count in
   [`docs/project-management/README.md`](../../project-management/README.md);
3. use only `[x]`, `[ ]`, or `[-]`; never use `[~]`.

## 11. Harness Issue Versus Project Runtime

The previous Codex thread could call `list_agents` after a CLI restart but
could not reliably route `spawn_agent` calls into the collaboration tool. This
was a Codex session/harness issue, not evidence that the `yi-agent` global
scheduler or its 16-resident limit was broken.

In a fresh Codex thread, validate collaboration before resuming development:

1. call `list_agents`;
2. spawn one read-only probe Agent;
3. have it report this worktree's branch and HEAD without editing files or
   running Cargo;
4. wait for its result;
5. only then dispatch the real specification reviewer.

If `list_agents` works but `spawn_agent` does not create a live Agent, start a
new thread rather than changing project code or resource limits.

## 12. Definition of Done

The branch is ready for user validation only when all of the following are
proven from current evidence:

- every checkpoint 1-20 satisfies its complete plan and specification;
- every implementation checkpoint has specification and quality approval;
- all named mock test suites pass serially;
- strict clippy passes for `yi-agent-core`, `yi-agent-tools`,
  `yi-agent-store`, and `yi-agent`;
- formatting and `git diff --check` pass;
- real-LLM tests remain opt-in and are not required without API keys;
- project-management feature status and index counts match verified reality;
- no secret, raw API key, or secret-bearing tool input is persisted or exposed;
- every coding delivery is a committed child worktree reviewed by its direct
  parent;
- the final root branch remains unmerged until the user explicitly approves
  integration into `main`.

When all work is complete, use `superpowers:finishing-a-development-branch` to
present integration options. Do not select or execute the merge option on the
user's behalf.
