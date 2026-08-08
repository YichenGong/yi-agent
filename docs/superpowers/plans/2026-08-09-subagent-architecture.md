# Subagent Runtime Architecture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a manually started, observable two-level subagent runtime with
commit-based Git worktree delivery and global resource coordination.

**Architecture:** Keep task identity, execution attempts, and delivery review
separate in `yi-agent-core`; place durable state, local IPC, scheduling, and the
daemon in `yi-agent-store`; use `yi-agent-tools` for Git/worktree operations;
wire CLI/TUI control clients in `yi-agent`. All user-visible control actions
travel through the daemon event API.

**Tech Stack:** Rust 2024, Tokio, serde, SQLite, local Unix sockets, Git CLI,
ratatui, clap, tempfile test repositories.

---

## File Map

- Create `crates/yi-agent-core/src/subagent/{mod.rs,task.rs,contract.rs,mailbox.rs,supervisor.rs,scheduler.rs}` for pure task-tree semantics.
- Modify `crates/yi-agent-core/src/{lib.rs,agent.rs,tool.rs,permission.rs}` to expose subagent types, spawn workers, tool schemas, and delegated authority.
- Create `crates/yi-agent-tools/src/worktree.rs` for isolated Git worktree lifecycle.
- Expand `crates/yi-agent-store/src/` with SQLite repositories, event journal, scheduler, daemon, and local IPC.
- Modify `crates/yi-agent/src/{config.rs,main.rs,tui/slash.rs,tui/app.rs}` for daemon CLI, client routing, Slash commands, and task-tree rendering.
- Create focused tests beside each component plus temporary-repository integration tests.

### Task 1: Define durable task and attempt state

**Files:**
- Create: `crates/yi-agent-core/src/subagent/task.rs`
- Create: `crates/yi-agent-core/src/subagent/mod.rs`
- Modify: `crates/yi-agent-core/src/lib.rs`

- [ ] Add a failing test that rejects a leaf agent spawning another child and rejects `Completed -> Running` without a rework event.

```rust
assert!(TaskState::Completed.can_transition_to(TaskState::Running).is_err());
assert!(TaskDepth::Leaf.can_spawn_child().is_err());
```

- [ ] Implement `TaskId`, `AttemptId`, `TaskDepth`, `TaskState`, `AgentTask`, and `TaskAttempt`; include `Queued`, `Running`, waiting states, review state, all terminal states, and `RecoveryRequired`.
- [ ] Run `cargo test -p yi-agent-core subagent::task::tests`; expected result: state-transition and depth tests pass.
- [ ] Commit `feat: add subagent task state model`.

### Task 2: Add contracts, delegated authority, and mailbox semantics

**Files:**
- Create: `crates/yi-agent-core/src/subagent/{contract.rs,mailbox.rs}`
- Modify: `crates/yi-agent-core/src/permission.rs`
- Test: `crates/yi-agent-core/src/subagent/{contract.rs,mailbox.rs}`

- [ ] Add failing tests proving a contract cannot widen its parent's path/tool authority and repeated `Progress` messages coalesce without a wake-up event.
- [ ] Define serializable `DelegationContract`, `ContractAmendment`, `DelegatedAuthority`, `MailboxMessage`, `MessageKind`, and `DeliveryPolicy` using the fields and invariants in the design specification.
- [ ] Implement `DelegatedAuthority::derive_child` as an intersection/minimum operation and `Mailbox::push` with correlation-aware progress coalescing.
- [ ] Run `cargo test -p yi-agent-core subagent::contract::tests subagent::mailbox::tests`; expected result: unauthorized widening and duplicate progress are rejected.
- [ ] Commit `feat: add delegated contracts and mailboxes`.

### Task 3: Implement supervisor, bounded spawning, and agent tools

**Files:**
- Create: `crates/yi-agent-core/src/subagent/supervisor.rs`
- Modify: `crates/yi-agent-core/src/{agent.rs,tool.rs,lib.rs}`
- Test: `crates/yi-agent-core/tests/subagent_supervisor.rs`

- [ ] Write scripted-provider tests for `spawn_agent`, `wait_agent`, and `send_message`: spawning returns immediately, `wait_agent(..., all)` returns only after all requested terminal reports, and a leaf spawn fails with a tool error.
- [ ] Implement `AgentSupervisor` with a task map, parent-child index, cancellation-token tree, and structured event stream. Enforce max depth two and four direct children per agent before worker creation.
- [ ] Register built-in `spawn_agent`, `wait_agent`, and `send_message` tools with JSON schemas. Use the supervisor rather than `Tool::call()` to own worker lifetime.
- [ ] Run `cargo test -p yi-agent-core --test subagent_supervisor`; expected result: deterministic scripted tests pass without a provider API key.
- [ ] Commit `feat: add supervised subagent tools`.

### Task 4: Add generic resource permits and Git worktree delivery

**Files:**
- Create: `crates/yi-agent-core/src/subagent/scheduler.rs`
- Create: `crates/yi-agent-tools/src/worktree.rs`
- Modify: `crates/yi-agent-tools/src/lib.rs`
- Test: `crates/yi-agent-tools/tests/subagent_worktree.rs`

- [ ] Add fake-clock scheduler tests proving one root session cannot consume all permits while another runnable session waits, and an aged background task eventually receives a permit.
- [ ] Define generic `ResourceRequest { scope, key, mode, units }` and coordinator-facing permit interfaces; implement default limits from the design: 16 resident tasks, 8 LLM permits with one coordination reservation, 6 coding permits, and one Git integration lease per target branch.
- [ ] Add worktree tests with `tempfile::TempDir`: a child must start from a committed parent ref, cannot receive the parent worktree path, and an accepted clean child branch merges only into its direct parent branch.
- [ ] Implement worktree create, inspect, merge, and clean-only removal commands with explicit base/head commit recording.
- [ ] Run `cargo test -p yi-agent-core subagent::scheduler::tests && cargo test -p yi-agent-tools --test subagent_worktree`; expected result: fairness and branch-direction tests pass.
- [ ] Commit `feat: add subagent resource and worktree leases`.

### Task 5: Persist tasks and expose a manually started daemon

**Files:**
- Create: `crates/yi-agent-store/src/{runtime.rs,repository.rs,event_log.rs,ipc.rs}`
- Modify: `crates/yi-agent-store/src/lib.rs`, `crates/yi-agent-store/Cargo.toml`, workspace `Cargo.toml`
- Test: `crates/yi-agent-store/tests/runtime_ipc.rs`

- [ ] Add failing tests that create a task, append events, reconnect a second IPC client from an event cursor, and observe the same snapshot and replayed events.
- [ ] Add SQLite schema migrations for sessions, tasks, attempts, contracts, mailboxes, permits, schedules, approvals, reviews, and append-only events. Store credential references only, never API-key values.
- [ ] Implement a single-instance user-private daemon with a local socket, versioned request/response/event messages, and `start`, `status`, `stop`, and subscription operations.
- [ ] On daemon restart, mark previously running attempts `RecoveryRequired`, reclaim process-local permits, and do not replay provider/tool/Git actions.
- [ ] Run `cargo test -p yi-agent-store --test runtime_ipc`; expected result: replay and recovery tests pass.
- [ ] Commit `feat: add persistent local subagent runtime`.

### Task 6: Add configuration, scheduling, and conservative failure controls

**Files:**
- Create: `crates/yi-agent-store/src/schedule.rs`
- Modify: `crates/yi-agent/src/config.rs`, `crates/yi-agent-store/src/runtime.rs`
- Test: `crates/yi-agent-store/tests/scheduler.rs`

- [ ] Add tests for configuration narrowing: a child cannot raise a session limit, a project Cargo lease remains exclusive, and a scheduled task defaults to read-only/background/no-overlap/no-catch-up.
- [ ] Parse user runtime settings, project `.yi-agent` settings, root-session options, and task budgets into a single effective policy using minimum/intersection semantics.
- [ ] Implement scheduled root-session creation with wall-clock, idle, turn, token/cost, retry, and resource-wait limits. Classify limit exhaustion as a visible terminal state.
- [ ] Run `cargo test -p yi-agent-store --test scheduler`; expected result: configuration precedence and overlap-policy tests pass.
- [ ] Commit `feat: add subagent runtime policies and schedules`.

### Task 7: Wire CLI, TUI, Slash commands, help, and review actions

**Files:**
- Modify: `crates/yi-agent/src/{config.rs,main.rs,tui/slash.rs,tui/app.rs,tui/state.rs}`
- Create: `crates/yi-agent/src/tui/agents.rs`
- Test: `crates/yi-agent/src/tui/{slash.rs,agents.rs}`, `crates/yi-agent/tests/subagent_cli.rs`

- [ ] Add failing parser and UI tests for `/agents`, `/agent`, `/message`, `/pause`, `/resume`, `/cancel`, `/retry`, `/approve`, `/review`, `/accept`, `/rework`, `/budget`, `/daemon status`, `/help <command>`, and `/?`.
- [ ] Define one command schema shared by clap help, Slash completion, contextual `/help`, confirmation requirements, and daemon request routing.
- [ ] Implement CLI control commands and a TUI task-tree/detail view backed only by daemon snapshots and event subscriptions. Show task status, resource waits, contracts, logs, diffs, commits, and verification evidence.
- [ ] Ensure accept records a review decision and wakes the direct parent; only the parent lease owner performs normal integration.
- [ ] Run `cargo test -p yi-agent --bin yi-agent subagent_ && cargo test -p yi-agent --test subagent_cli`; expected result: command, help, and review-routing tests pass.
- [ ] Commit `feat: add subagent control surfaces`.

### Task 8: Exercise failure, cancellation, documentation, and final verification

**Files:**
- Modify: `docs/project-management/subagent-runtime.md`, `docs/project-management/README.md`
- Test: affected core, tools, store, CLI, and TUI suites

- [ ] Add integration tests for recursive cancellation, permission timeout, mailbox ping-pong suppression, dirty-worktree cleanup refusal, rework creating a new attempt, and daemon-stop recovery.
- [ ] Run `cargo fmt --all` from `yi-agent-rs/`.
- [ ] Run, one Cargo invocation at a time: `cargo test -p yi-agent-core`, `cargo test -p yi-agent-tools`, `cargo test -p yi-agent-store`, and `cargo test -p yi-agent`.
- [ ] Run `cargo clippy -p yi-agent-core -p yi-agent-tools -p yi-agent-store -p yi-agent -- -D warnings` and `git diff --check main...HEAD`.
- [ ] Update each `[ ]` project-management item to `[x]` only when its named verification command passes; update the module index count in the same commit.
- [ ] Commit `test: cover subagent runtime recovery and controls`.
