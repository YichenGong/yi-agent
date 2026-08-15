# Subagent Runtime Test Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add deterministic P0/P1 tests that harden subagent lifecycle, IPC, reducer, scheduler, worktree, and configuration-isolation behavior.

**Architecture:** Split new test coverage by failure domain: public daemon lifecycle and raw socket protocol tests live in `yi-agent-store`; pure state-machine and scheduler invariants live in `yi-agent-core`; real Git failure paths remain with the existing worktree integration tests. Reuse public APIs and local test helpers only; production code changes are limited to fixing a test-proven behavior defect or the config test environment isolation.

**Tech Stack:** Rust 2024, `tokio`, local Unix sockets, `rusqlite`, `tempfile`, `chrono`, `serde_json`, real local Git repositories.

## Global Constraints

- Work only in the existing `feat/subagent-core` linked worktree.
- Use mock workers/providers, temporary repositories, temporary SQLite stores, and local Unix sockets; do not call real LLMs or external network services.
- Do not add a property-test/fuzzing dependency; use fixed, bounded, table-driven event and admission sequences.
- Run Cargo invocations one at a time; do not run multiple Cargo test processes concurrently.
- Preserve append-only events, direct-parent review authority, and existing public runtime APIs; do not add test-only production bypasses.
- Every newly added test must be deterministic and use a finite timeout when waiting on asynchronous work.
- Before each commit, run `cargo fmt --all` from `yi-agent-rs/`.

---

## File structure

| File | Responsibility |
|---|---|
| Create `yi-agent-rs/crates/yi-agent-core/tests/subagent_invariants.rs` | Replay reducer event tables and exercise deterministic scheduler sequences. |
| Create `yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs` | Exercise request framing at the public Unix-socket boundary and prove daemon recovery. |
| Create `yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs` | Exercise daemon lifecycle, durable reports/reviews, restart, and competing controls. |
| Modify `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs` | Add actual-Git stale/missing/double-accept cleanup failure coverage. |
| Modify `yi-agent-rs/crates/yi-agent/src/config.rs` | Serialize and isolate the two compact-default tests. |
| Modify `docs/project-management/subagent-runtime.md` | Record completed hardening coverage with exact verification commands. |
| Modify `docs/project-management/README.md` | Update the subagent-runtime count only if a previously unchecked feature is fully verified. |

## Task 1: Make compact-default configuration tests environment-safe

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs:732-806`

**Interfaces:**
- Consumes: existing `ENV_TEST_MUTEX: Mutex<()>` and `isolated_config_env() -> EnvVarGuard` in the `config::tests` module.
- Produces: isolated tests `load_includes_compact_defaults` and `load_falls_back_to_default_context_length` that always observe `200_000 * 80 / 100`.

- [ ] **Step 1: Write the failing regression test setup**

At the start of both tests, intentionally demonstrate the missing isolation by adding the existing lock and guard only to one test first:

```rust
let _lock = ENV_TEST_MUTEX
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
let _env = isolated_config_env();
```

Run the complete binary test suite before modifying the second test. The current failure is expected to be sensitive to parallel environment mutation; record the observed result in the task notes rather than changing the asserted threshold.

- [ ] **Step 2: Run the binary test suite to reproduce the failure**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
```

Expected before the complete fix: either `load_includes_compact_defaults` or `load_falls_back_to_default_context_length` can observe `200_000` instead of `160_000` when `YI_AGENT_COMPACT_RATIO` leaks from a parallel test.

- [ ] **Step 3: Apply the minimal isolation fix to both tests**

Add the same lock and `isolated_config_env()` guard to both compact-default tests before constructing `Cli`. Keep both assertions unchanged:

```rust
assert_eq!(config.compact_threshold, 160_000);
```

Do not alter production default calculation at `effective_context_length = model_context_length.unwrap_or(200_000)`.

- [ ] **Step 4: Verify the regression and formatting**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --bin yi-agent config::tests::load_includes_compact_defaults
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent config::tests::load_falls_back_to_default_context_length
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
```

Expected: all commands exit 0; the full binary suite contains no compact-default failure.

- [ ] **Step 5: Commit the isolated regression fix**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "test: isolate compact default configuration tests"
```

## Task 2: Add reducer replay and scheduler invariant tests

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/tests/subagent_invariants.rs`

**Interfaces:**
- Consumes: `yi_agent_core::subagent::task::{AgentTask, TaskEvent, TaskState, reduce}` and scheduler public types `ResourceCoordinator`, `ResourceRequest`, `ResourceScope`, `LeaseMode`, `AdmissionPriority`.
- Produces: deterministic tests proving failed reduction is atomic, equivalent replay is deterministic, terminal attempts need a successor attempt, and resource leases remain safe under a bounded sequence.

- [ ] **Step 1: Write failing reducer atomicity and replay tests**

Create helpers that construct two identical child tasks and an active attempt. Define a fixed valid event vector, using one timestamp per item, such as:

```rust
vec![
    TaskEvent::AdmissionGranted { attempt_id: attempt.clone() },
    TaskEvent::WorkerCompletedNoChanges { attempt_id: attempt.clone() },
    TaskEvent::RetryRequested { attempt_id: attempt.clone() },
]
```

For each event, clone the task before `reduce`; on `Err`, assert `assert_eq!(task, before)`. Replay the valid vector into both identical tasks and assert equality after every successful step. Add explicit stale-attempt and terminal-to-running negative events, asserting they return `Err` and leave state unchanged.

- [ ] **Step 2: Run the new reducer test before helper/API corrections**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_invariants reducer_
```

Expected before completing the test helpers: compilation failures for missing construction helpers or assertion mismatches that reveal the exact public type fields required.

- [ ] **Step 3: Complete test-local constructors without changing reducer production code**

Use the public constructors and IDs already used in `subagent_scheduler.rs` and `subagent_supervisor.rs`. Add only test-local helpers, for example:

```rust
fn apply_atomically(task: &mut AgentTask, event: TaskEvent, now: DateTime<Utc>) {
    let before = task.clone();
    if reduce(task, event, now).is_err() {
        assert_eq!(*task, before);
    }
}
```

Assert that a successful `RetryRequested` produces `TransitionResult { new_attempt: Some(_) }`, changes the active attempt ID, and never directly changes a terminal task to `Running`.

- [ ] **Step 4: Write a failing bounded scheduler sequence test**

Create a coordinator with small explicit capacities. Submit requests from three root sessions with both shared and exclusive modes. Execute this fixed sequence: admit, enqueue conflicting exclusive request, release, enqueue/cancel one request, enqueue an expired request, then repeatedly grant/release round-robin requests. After each operation assert:

```rust
assert!(coordinator.used_units("resident") <= 2);
assert!(coordinator.active_leases_for("workspace:target").len() <= 1);
```

Use actual public inspection APIs; if they are insufficient, assert safety from returned leases and subsequent admissions, rather than exposing internal coordinator state.

- [ ] **Step 5: Run the scheduler test to establish its initial result**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --test subagent_invariants scheduler_
```

Expected before completing the test: compile or assertion failure identifies the available scheduler inspection surface.

- [ ] **Step 6: Complete the scheduler test with public observations**

Use `try_acquire`, `release`, cancellation/expiry APIs, and returned `GrantedLease` values. Assert: no conflicting lease is returned while exclusive is held; a duplicate release succeeds or yields the documented idempotent result; cancelled/expired requests do not receive later grants; every root in a finite continuously-runnable round-robin set receives a grant.

- [ ] **Step 7: Verify the core invariant suite and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-core --test subagent_invariants
cd yi-agent-rs && cargo test -p yi-agent-core
```

Expected: both commands exit 0.

```bash
git add yi-agent-rs/crates/yi-agent-core/tests/subagent_invariants.rs
git commit -m "test: add subagent reducer and scheduler invariants"
```

## Task 3: Add public Unix-socket framing and recovery tests

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs`

**Interfaces:**
- Consumes: `yi_agent_store::ipc::{Daemon, IpcRequest, IpcResponse, PROTOCOL_VERSION, request, subscribe}`, `std::os::unix::net::UnixStream`, and `tempfile::TempDir`.
- Produces: raw-frame helpers and protocol tests proving malformed clients do not take down healthy clients.

- [ ] **Step 1: Write the failing fragmented-frame test**

Create a temporary daemon and a raw socket helper:

```rust
fn send_in_chunks(socket: &Path, bytes: &[u8], chunk_size: usize) -> serde_json::Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    for chunk in bytes.chunks(chunk_size) {
        stream.write_all(chunk).unwrap();
    }
    stream.write_all(b"\n").unwrap();
    read_response(&mut BufReader::new(stream))
}
```

Serialize a versioned `Status` request envelope and send it for every `chunk_size` from `1..=frame.len()`. Assert each response has the original request ID, `PROTOCOL_VERSION`, and `IpcResponse::Status` payload.

- [ ] **Step 2: Run the fragmented-frame test**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_ipc_protocol fragmented_
```

Expected before helper completion: compilation error for local envelope/response parsing helper names. Do not access private IPC frame functions.

- [ ] **Step 3: Implement test-local wire helpers and pass fragmentation coverage**

Copy only the stable wire JSON shape from existing `runtime_ipc.rs` raw helpers. Parse response JSON into `serde_json::Value` and assert `protocol_version`, `request_id`, and `result` fields; do not deserialize private envelope types.

- [ ] **Step 4: Write failing malformed-client recovery cases**

Add table-driven raw inputs:

```rust
let cases = [
    b"{\"protocol_version\":1".as_slice(),
    b"{\"protocol_version\":999,\"request_id\":\"bad-version\",\"command\":\"status\"}".as_slice(),
    b"{\"protocol_version\":1,\"request_id\":\"unknown\",\"command\":\"unknown\"}".as_slice(),
];
```

For each case, write bytes plus newline and allow either a typed error response or peer close. After every case invoke public `request(socket, IpcRequest::Status)` and assert it returns `IpcResponse::Status`. Add one oversized request built as `MAX_FRAME_BYTES + 1` bytes indirectly by a large JSON string; assert only that the subsequent healthy status succeeds.

- [ ] **Step 5: Add subscription reconnect ordering test**

Create a session/task transition that emits at least two events. Subscribe, save the received event ID, close the subscription, emit another event, then subscribe from the saved cursor. Assert returned replay event IDs are strictly increasing and contain no saved cursor ID. Use `Subscription::next_frame` with a finite thread timeout or socket read timeout.

- [ ] **Step 6: Verify the IPC protocol suite and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-store --test subagent_ipc_protocol
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc
```

Expected: both commands exit 0.

```bash
git add yi-agent-rs/crates/yi-agent-store/tests/subagent_ipc_protocol.rs
git commit -m "test: harden subagent IPC protocol framing"
```

## Task 4: Add durable daemon lifecycle and competing-control tests

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs`

**Interfaces:**
- Consumes: `Daemon::start_with_factory`, public IPC request/response types, `AgentWorkerFactory`, `WorkerStart`, `WorkerEvent`, `WorkerWorkspace`, `TempDir`, and `Barrier`.
- Produces: mock-worker lifecycle tests that verify durable report/review/restart facts and one-winner control semantics.

- [ ] **Step 1: Write the failing lifecycle test with a deterministic delivery factory**

Create an injected factory that records every `WorkerStart` and emits a single `WorkerEvent::Delivered` built from the workspace passed in `WorkerStart`. The test must:

```text
attach application root -> activate -> spawn child -> wait all -> preview review -> confirm review -> inspect -> stop daemon -> restart -> inspect again
```

Before restart record: child task ID, active/terminal attempt ID, terminal report, review response, and ordered event IDs. After restart assert these values are unchanged and the factory start count has not increased.

- [ ] **Step 2: Run the lifecycle test to establish the exact public construction surface**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test subagent_runtime_e2e lifecycle_
```

Expected before helper completion: errors identifying the exact `WorkerEvent::Delivered` delivery fields and application-root capability arguments.

- [ ] **Step 3: Complete the lifecycle helper using existing public test patterns**

Use the injected factories in `runtime_ipc.rs` and `runtime_coordinator.rs` as the source for `AgentWorkerFactory` implementation. Construct delivery evidence from the child `WorkerWorkspace`, never from caller input. Confirm the review through preview token. Assert recovery only hydrates durable facts; it does not invoke the factory again.

- [ ] **Step 4: Write the failing concurrent cancel confirmation test**

Spawn a running child, request one `PreviewCancel`, and clone its confirmation token into two threads synchronized with `Barrier::new(2)`. Each thread sends `ConfirmCancel` using a separate socket client. Join both threads and assert exactly one returns `TaskCancelled`; the other is a typed rejection. Inspect task events and assert exactly one cancellation terminal event exists.

- [ ] **Step 5: Write the failing competing review confirmation test**

Prepare one child delivery. Obtain two review previews before confirmation: accept and reject. Start two barrier-synchronized confirmation threads. Assert exactly one of `ReviewApproved` or `ReviewRejected` occurs, the other is rejected, and the persisted task has exactly one corresponding review transition. Do not assert which branch wins.

- [ ] **Step 6: Complete synchronization and durable assertions**

Use `std::thread::scope`, `Barrier`, and bounded joins. Read ordered task events through `IpcRequest::ReadTaskEvents`; verify no duplicate terminal/review events and a reopened daemon reports the same final task state. If a production atomicity defect is revealed, first add a smallest focused regression test, then change only the transaction/confirmation boundary required for one winner.

- [ ] **Step 7: Verify lifecycle and race coverage and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-store --test subagent_runtime_e2e
cd yi-agent-rs && cargo test -p yi-agent-store
```

Expected: both commands exit 0.

```bash
git add yi-agent-rs/crates/yi-agent-store/tests/subagent_runtime_e2e.rs
git commit -m "test: cover durable subagent lifecycle controls"
```

## Task 5: Extend real-Git worktree failure-path coverage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs`

**Interfaces:**
- Consumes: existing local `repository()`, `git()`, and `WorktreeService::{create_root, create_child, inspect_delivery, merge_inspected_delivery, remove_clean}` helpers.
- Produces: tests proving invalid/stale/missing delivery state cannot be accepted or silently cleaned.

- [ ] **Step 1: Write a failing missing-worktree inspection test**

Create a root and child worktree, commit a child change, then delete only the child worktree directory using `git worktree remove --force <path>` while retaining the service's recorded child metadata. Call `inspect_delivery` and assert `WorktreeError` rather than a successful delivery. Assert the parent branch head has not changed.

- [ ] **Step 2: Run the missing-worktree test**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --test subagent_worktree missing_worktree
```

Expected before any production correction: a test result that establishes whether existing domain error handling is already sufficient. Do not weaken the assertion to accept a merge.

- [ ] **Step 3: Add a stale-inspection/double-accept failing test**

Inspect a committed child delivery, merge it once with `merge_inspected_delivery`, then call the same merge method again. Assert the second invocation returns `WorktreeError`, the parent has one merge commit for that child, and the child branch/delivery head has not been merged a second time.

- [ ] **Step 4: Add a missing-branch and dirty-cleanup test**

For a separate prepared child, delete its branch manually after inspection and assert merge returns `WorktreeError` without changing the parent. For another child, make an uncommitted file change after acceptance and assert `remove_accepted_clean` returns a dirty-worktree error and leaves the directory in place.

- [ ] **Step 5: Apply only test-proven worktree behavior fixes**

If any test exposes a false success, add guards in `worktree.rs` before `git merge` or `git worktree remove` that verify the recorded branch/head/path still exist and match the inspected delivery. Return the existing `WorktreeError` variant that most precisely reports the failed Git operation; do not remove a dirty worktree to make tests pass.

- [ ] **Step 6: Verify worktree coverage and commit**

Run:

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-tools --test subagent_worktree
cd yi-agent-rs && cargo test -p yi-agent-tools
```

Expected: both commands exit 0.

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/worktree.rs yi-agent-rs/crates/yi-agent-tools/tests/subagent_worktree.rs
git commit -m "test: cover subagent worktree delivery failures"
```

## Task 6: Update project tracking and run the complete verification gate

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`
- Modify: `docs/project-management/README.md`

**Interfaces:**
- Consumes: all passed commands from Tasks 1-5 and the feature checklist in `subagent-runtime.md`.
- Produces: accurate completed-item evidence and matching module-index count.

- [ ] **Step 1: Write the project-management evidence update**

Add a completed hardening bullet to `subagent-runtime.md` with exact test locations and commands:

```markdown
- [x] P0/P1 runtime hardening — daemon lifecycle/restart and competing-control atomicity: `cargo test -p yi-agent-store --test subagent_runtime_e2e`; fragmented/invalid Unix-socket requests and cursor reconnect: `cargo test -p yi-agent-store --test subagent_ipc_protocol`; reducer/scheduler sequence invariants: `cargo test -p yi-agent-core --test subagent_invariants`; Git failure paths: `cargo test -p yi-agent-tools --test subagent_worktree`; config environment isolation: `cargo test -p yi-agent --bin yi-agent`.
```

Do not mark an existing unchecked feature complete unless its named acceptance criterion has been run and fully covers it.

- [ ] **Step 2: Update the module index count accurately**

If the added hardening bullet is a new feature line, change `subagent-runtime` from `8 / 15` to `9 / 16`. If instead an existing unchecked line has been fully verified, increment only the completed numerator while retaining the denominator. Match the final count to literal `[x]` and `[-]` entries in `subagent-runtime.md`.

- [ ] **Step 3: Run the complete serial verification gate**

Run each command only after the previous command exits:

```bash
cd yi-agent-rs && cargo test -p yi-agent-core
cd yi-agent-rs && cargo test -p yi-agent-tools
cd yi-agent-rs && cargo test -p yi-agent-store
cd yi-agent-rs && cargo test -p yi-agent
cd yi-agent-rs && cargo fmt --all -- --check
cd yi-agent-rs && cargo clippy -p yi-agent-core -p yi-agent-tools -p yi-agent-store -p yi-agent -- -D warnings
git diff --check main...HEAD
git status --short --branch
```

Expected: every command exits 0 and status reports only the feature branch with no uncommitted files.

- [ ] **Step 4: Commit tracking and verification documentation**

```bash
git add docs/project-management/subagent-runtime.md docs/project-management/README.md
git commit -m "docs: record subagent runtime hardening coverage"
```
