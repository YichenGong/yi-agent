# Subagent Core Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the pure two-level task runtime, contracts, authority,
mailboxes, and Supervisor APIs in `yi-agent-core` without daemon persistence or
Git worktree side effects.

**Architecture:** Create a focused `subagent/` module under core. State changes
flow through a Supervisor event reducer; workers receive immutable attempt input
and emit lifecycle events. The companion design is
`docs/superpowers/specs/2026-08-09-subagent-core-design.md`.

**Tech Stack:** Rust 2024, Tokio, serde, uuid, chrono, `CancellationToken`,
scripted `Provider` tests.

---

### Task 1: Add IDs, states, attempts, and transition reducer

**Files:**
- Create: `crates/yi-agent-core/src/subagent/{mod.rs,task.rs}`
- Modify: `crates/yi-agent-core/src/lib.rs`

- [ ] Write `task.rs` tests for every legal transition in the specification and
  rejection of `Completed -> Running`, leaf spawning, and stale-attempt events.
- [ ] Add serializable `TaskId`, `AttemptId`, `RootSessionId`, `TaskDepth`,
  `TaskState`, `DeliveryState`, `AgentTask`, `TaskAttempt`, and `TaskEvent`.
- [ ] Implement `reduce(task, event, now) -> Result<Transition, TransitionError>`;
  reducer must not spawn Tokio tasks or write storage.
- [ ] Re-export public types from `lib.rs`.
- [ ] Run `cargo test -p yi-agent-core subagent::task::tests`.
- [ ] Commit `feat: add subagent task state reducer`.

### Task 2: Add contracts and monotonic delegated authority

**Files:**
- Create: `crates/yi-agent-core/src/subagent/{contract.rs,authority.rs}`
- Modify: `crates/yi-agent-core/src/permission.rs`

- [ ] Write tests that deserialize a contract draft, reject a child path/tool/
  budget/deadline widening, and preserve amendment provenance.
- [ ] Define `ContractDraft`, persisted `DelegationContract`, amendment types,
  `DelegatedAuthority`, `EffectiveBudget`, and typed derivation errors from the
  core specification.
- [ ] Implement `validate_draft` and `derive_child`; intersect capabilities and
  compute minimum limits before any worktree/worker is requested.
- [ ] Add conversion from inherited permission policy to authority constraints;
  reserve security approvals for the existing permission checker.
- [ ] Run `cargo test -p yi-agent-core subagent::contract::tests subagent::authority::tests`.
- [ ] Commit `feat: add subagent contracts and authority`.

### Task 3: Add mailbox storage semantics and wait selectors

**Files:**
- Create: `crates/yi-agent-core/src/subagent/mailbox.rs`
- Test: `crates/yi-agent-core/src/subagent/mailbox.rs`

- [ ] Write tests for progress coalescing, terminal-message idempotency,
  priority ordering, and `all`/`any` wait selection.
- [ ] Implement `MailboxMessage`, message kind/payload/priority, correlation
  keys, coalescing receipt, and `WaitSelector`.
- [ ] Implement `Mailbox::push`, `take_next`, and `resolve_wait`; delivery must
  not wake a busy parent until its safe checkpoint.
- [ ] Run `cargo test -p yi-agent-core subagent::mailbox::tests`.
- [ ] Commit `feat: add subagent mailbox semantics`.

### Task 4: Add supervisor ownership and worker boundary

**Files:**
- Create: `crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `crates/yi-agent-core/src/agent.rs`
- Test: `crates/yi-agent-core/tests/subagent_supervisor.rs`

- [ ] Add scripted-provider tests for immediate spawn receipt, depth/direct-child
  limits, cancellation precedence, and interruptible `wait_agent`.
- [ ] Define `AgentWorkerFactory`, `WorkerStart`, `WorkerHandle`, and
  `WorkerEvent`; adapt `Agent` so a factory can create an isolated child session
  without sharing the parent `Session` mutex.
- [ ] Implement `AgentSupervisor` task index, parent-child index, attempt token
  tree, event broadcaster, `spawn`, `wait`, `send`, `pause`, `resume`, and
  recursive cancellation.
- [ ] Ensure a worker cannot mutate `AgentTask`; it reports events to the
  supervisor reducer only.
- [ ] Run `cargo test -p yi-agent-core --test subagent_supervisor`.
- [ ] Commit `feat: add subagent supervisor`.

### Task 5: Expose model-facing tools and core integration tests

**Files:**
- Modify: `crates/yi-agent-core/src/tool.rs`, `crates/yi-agent-core/src/agent.rs`
- Create: `crates/yi-agent-core/tests/subagent_tools.rs`

- [ ] Write scripted tests for `spawn_agent`, `wait_agent`, and `send_message`
  schemas and tool results, including leaf rejection and terminal-task message
  rejection.
- [ ] Implement tools as thin adapters over `AgentSupervisor`; do not implement
  them as blocking `Tool::call()` child loops.
- [ ] Forward typed subagent events alongside existing `AgentEvent` values.
- [ ] Run `cargo fmt --all && cargo test -p yi-agent-core`.
- [ ] Update `docs/project-management/subagent-runtime.md` only for completed
  core items and commit `feat: expose supervised subagent tools`.
