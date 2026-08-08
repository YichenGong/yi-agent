# Runtime Scheduling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans task-by-task.

**Goal:** Enforce global/project budgets, fair permits, watchdogs, and conservative schedules.

**Architecture:** Generic resource leases live beside the runtime; project adapters request named keys.

### Task 1: Add effective configuration and resource leases

- [ ] Write tests that prove configuration only narrows and Cargo workspace keys serialize.
- [ ] Implement TOML resolver, `ResourceRequest`, lease accounting, and idempotent release.
- [ ] Run `cargo test -p yi-agent-store scheduler::policy_tests`.
- [ ] Commit `feat: add runtime resource policies`.

### Task 2: Add fair admission and watchdogs

- [ ] Use fake-clock tests for root/subtree rotation, age boost, coordination reserve,
  queue capacity, and one-time terminal release.
- [ ] Implement admission scoring, queue cursor persistence, limits, and retry classification.
- [ ] Run `cargo test -p yi-agent-store scheduler::tests`.
- [ ] Commit `feat: schedule subagent resources fairly`.

### Task 3: Add schedules

- [ ] Test separate scheduled roots, no-overlap skip, missed-run skip, and read-only defaults.
- [ ] Implement schedule persistence/fire evaluation and daemon integration.
- [ ] Run `cargo test -p yi-agent-store scheduler::tests`.
- [ ] Commit `feat: run conservative scheduled agent tasks`.
