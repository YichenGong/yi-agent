# Runtime Recovery Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enforce and durably attest recovery inspection before a recovered task can use normal worker tools or resume its objective.

**Architecture:** A recovered attempt starts in a durable recovery-gate state with no ordinary worker capability. The runtime runs deterministic Git/checkpoint/tool-state inspection, commits an attestation with the successor attempt and leases, then permits normal worker admission. Any failed inspection remains a durable `RecoveryConflict` boundary.

**Tech Stack:** Rust, Tokio, SQLite/rusqlite, existing `yi-agent-core` supervisor and worker interfaces.

---

### Task 1: Persist recovery gate data and leases atomically

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

- [ ] **Step 1: Write failing repository/runtime tests**

Add tests proving that a normal worker admission has a `workspace:*` and `worktree:*` active lease plus checkpoint/tool evidence before the factory can observe its start, and that recovery successor metadata survives reopening the repository.

- [ ] **Step 2: Run the focused test to verify RED**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator admission_persists_recovery_boundary_before_worker_start`

Expected: FAIL because current admission invokes the factory before durable context/leases exist.

- [ ] **Step 3: Implement transaction helpers**

Make the admission transaction update task/attempt snapshot, write workspace and worktree `resource_leases`, save checkpoint/tool evidence, and append `TaskStarted`. Add a successor-attempt helper that stores the recovery controller payload before it can be observed by a worker.

- [ ] **Step 4: Run focused and store tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator && cargo test -p yi-agent-store`

Expected: PASS.

### Task 2: Enforce deterministic recovery inspection

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/subagent_runtime.rs`

- [ ] **Step 1: Write failing worker tests**

Add tests that a recovery attempt does not construct an Agent or expose normal tools until deterministic inspection validates recorded worktree, clean-or-known Git status, HEAD, checkpoint, and tool-state evidence; mismatch must emit `RecoveryConflict` with zero provider calls.

- [ ] **Step 2: Verify RED**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent subagent_runtime::tests::recovery_gate`

Expected: FAIL because the current recovery phase is an advisory provider prompt.

- [ ] **Step 3: Implement a gate result**

Replace the recovery prompt phase with a deterministic, no-provider inspection function. Only its successful, durable attestation may create an Agent with ordinary tools; mismatch reports `WorkerEvent::RecoveryConflict` before provider/tool work.

- [ ] **Step 4: Verify GREEN**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent subagent_runtime::tests::recovery_gate`

Expected: PASS.

### Task 3: Make resume and worker admission crash-safe

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs`
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

- [ ] **Step 1: Write failing crash-boundary tests**

Test that the coordinator persists admission before calling `AgentWorkerFactory::start`, and that a recovery successor remains gated after reopening the database before attestation.

- [ ] **Step 2: Verify RED**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator recovered_successor_remains_gated_after_restart`

Expected: FAIL because current gate instruction is supervisor memory only.

- [ ] **Step 3: Implement ordering and recovery hydration**

Persist the gate successor before factory start, hydrate gated successors at open, and make factory failure durably close the admitted attempt as failed. Do not remove recovery context until successful inspection is durably attested.

- [ ] **Step 4: Verify GREEN and commit**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store && cargo test -p yi-agent --bin yi-agent && cargo fmt --all && just fmt-check && git diff --check`

Commit: `fix: enforce durable recovery inspection gate`

### Task 4: Review and final Checkpoint 17 verification

- [ ] **Step 1: Request fresh specification review**

Require explicit verification that no provider/tool/Git action is possible before deterministic inspection and durable attestation, and that workspace/worktree leases plus successor controller state survive restart.

- [ ] **Step 2: Request independent code-quality review**

Inspect lock ordering, database transaction ordering, process-spawn safety, cleanup, handler retention, and test determinism.

- [ ] **Step 3: Run serial final verification**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store && cargo test -p yi-agent --bin yi-agent && cargo clippy -p yi-agent-core -p yi-agent-store --no-deps -- -D warnings && cargo fmt --all && just fmt-check && git diff --check`
