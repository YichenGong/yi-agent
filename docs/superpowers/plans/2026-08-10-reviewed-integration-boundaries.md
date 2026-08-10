# Reviewed Integration Boundaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:test-driven-development and superpowers:systematic-debugging task-by-task. This checkpoint must be executed by the sole current agent in the existing worktree.

**Goal:** Separate local-user delivery approval from trusted parent integration, make review mailbox state recoverable and identity-stable, and leave failed worker admission queued in both memory and SQLite.

**Architecture:** IPC `Accept` records a durable `approved` review plus a local-user notification to the canonical direct parent without reducing the child. Trusted runtime completion retains the `IntegrationValidation` guard and alone emits `review_accepted`. Review operations stage reducer/mailbox changes under the supervisor lock, preallocate every `MessageId`, persist one SQLite transaction, and roll memory back on persistence failure; startup reconstructs review participants and hydrates pending durable mailbox rows. Rework delivery gets a dedicated durable acknowledgement instead of using the external user-override protocol.

**Tech Stack:** Rust, Tokio, rusqlite/SQLite, serde, existing `yi-agent-core` supervisor and `yi-agent-store` runtime/repository/IPC tests.

---

### Task 1: User Approval Versus Trusted Integration

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`

- [ ] Add an IPC serialization/behavior test that `Accept` carries no integration evidence, returns `ReviewApproved`, audits local-user provenance, wakes the direct parent, and leaves the child `awaiting_parent_review`.
- [ ] Run the named IPC test after a process scan and capture the expected RED.
- [ ] Add a coordinator test that failed or absent integration cannot complete, while a passed `IntegrationValidation` through the trusted completion method emits `review_accepted` and completes.
- [ ] Run the named coordinator test and capture the expected RED.
- [ ] Add `RuntimeEvent::ReviewApproved`, repository approval persistence, `RuntimeCoordinator::approve_review`, unit-shaped `IpcReviewDecision::Accept`, and `IpcResponse::ReviewApproved`; keep trusted integration completion separate.
- [ ] Rerun both named tests and capture GREEN.

### Task 2: Atomic Review Staging And Durable Hydration

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/mailbox.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/tests/subagent_contract_mailbox.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/repository_decisions.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

- [ ] Add deterministic tests for rollback on review repository failure and exact preallocated durable/in-memory message identity.
- [ ] Run each named test after a process scan and capture RED.
- [ ] Add supervisor review transactions that snapshot reducer/mailbox state, stage exact-ID messages under the supervisor lock, persist, and restore snapshots on repository error.
- [ ] Add a trusted mailbox hydration method that accepts exact durable IDs and terminal recipients without weakening ordinary live send checks.
- [ ] Add repository queries for pending review mailbox rows and runtime startup hydration into canonical session supervisors.
- [ ] Rerun named tests and capture GREEN.

### Task 3: Exactly-Once Rework Worker Input

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/mailbox.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

- [ ] Add a restart/successor test proving durable rework feedback reaches the first normal `WorkerStart` with its SQLite ID and is absent from a later start in the same attempt.
- [ ] Run it after a process scan and capture RED.
- [ ] Stage rework as a pending worker input, persist a dedicated delivered acknowledgement for `kind = 'rework'`, and confirm it after worker admission succeeds; do not classify it as an external user override.
- [ ] Rerun the named test and capture GREEN.

### Task 4: Admission Persistence Rollback

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

- [ ] Add an injected SQLite trigger failure test for a child proving both states remain queued, provisional resident accounting is released, and retry starts after removing the trigger.
- [ ] Run it after a process scan and capture RED.
- [ ] Remove the failed-state mutation/second persistence write from the admission error path and always release provisional subagent residency before returning the original repository error.
- [ ] Rerun the named test and capture GREEN.

### Task 5: Verification And Commit

**Files:**
- Review all modified files except `docs/project-management/**`, which must remain untouched.

- [ ] Self-review `git diff`, review protocol fields, actor/session/delivery ownership, transition guards, event distinctions, IDs, and rollback paths.
- [ ] Run `cargo fmt --all`, `just fmt-check`, and `git diff --check`.
- [ ] Run the five requested focused suites serially, with the required process scan before every Cargo command, and record test counts.
- [ ] Confirm branch, status, and diff scope; commit once as `fix: enforce reviewed integration boundaries` with no co-author line and do not merge.
