# Runtime Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans task-by-task.

**Goal:** Add a manually started, persistent local runtime daemon.

**Architecture:** `yi-agent-store` owns SQLite, local IPC, and daemon lifecycle;
the binary injects the core worker factory. See `2026-08-09-runtime-daemon-design.md`.

### Task 1: Add SQLite repository and migrations

**Files:** Create `crates/yi-agent-store/src/{repository.rs,migrations.rs,event_log.rs}`; modify Cargo manifests.

- [ ] Write tempfile-database tests for idempotent migration and atomic
  task-snapshot/event transactions.
- [ ] Add the schema, indexes, and typed repositories defined by the daemon specification.
- [ ] Run `cargo test -p yi-agent-store repository::tests`.
- [ ] Commit `feat: persist subagent runtime state`.

### Task 2: Add local IPC and single-instance daemon

**Files:** Create `crates/yi-agent-store/src/{ipc.rs,runtime.rs}`; modify `lib.rs`.

- [ ] Write tests for socket single-instance rejection, version mismatch,
  snapshot-plus-replay, and subscriber resync after buffer overflow.
- [ ] Implement versioned NDJSON frames, socket permission setup, lock checks,
  command dispatch, and event subscription.
- [ ] Run `cargo test -p yi-agent-store --test runtime_ipc`.
- [ ] Commit `feat: add local runtime daemon IPC`.

### Task 3: Wire worker factory, stop, and recovery

**Files:** Modify `crates/yi-agent/src/{main.rs,config.rs}` and store runtime tests.

- [ ] Add tests for manual `daemon start|status|stop`, graceful checkpointing,
  restart-to-`RecoveryRequired`, and secret-free inspection.
- [ ] Inject `AgentWorkerFactory`; implement draining and recovery algorithms.
- [ ] Run `cargo test -p yi-agent-store && cargo test -p yi-agent --bin yi-agent`.
- [ ] Commit `feat: run supervised agents from daemon`.
