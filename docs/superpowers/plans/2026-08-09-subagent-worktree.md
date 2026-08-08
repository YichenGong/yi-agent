# Subagent Worktree Delivery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans task-by-task.

**Goal:** Safely create, validate, integrate, rework, and clean delegated Git worktrees.

**Architecture:** `yi-agent-tools` owns controlled Git operations; core owns leases and state.

### Task 1: Add worktree lease and creation validation

**Files:** Create `crates/yi-agent-tools/src/worktree.rs`; modify `lib.rs`; create `tests/subagent_worktree.rs`.

- [ ] Write temporary-repository tests for dirty-root rejection, deterministic
  branch/path naming, and committed-parent-base enforcement.
- [ ] Implement canonical repository inspection and `git worktree add` wrapper.
- [ ] Run `cargo test -p yi-agent-tools --test subagent_worktree`.
- [ ] Commit `feat: create isolated subagent worktrees`.

### Task 2: Add delivery review, integration, and rework

- [ ] Write tests for report/base validation, direct-parent-only merge, merge
  conflict evidence, and new-base rework branches.
- [ ] Implement report validation and controlled `merge --no-ff` integration.
- [ ] Run `cargo test -p yi-agent-tools --test subagent_worktree`.
- [ ] Commit `feat: integrate reviewed subagent deliveries`.

### Task 3: Add cleanup and recovery inspection

- [ ] Test dirty cleanup refusal, accepted clean removal, and retained failed worktrees.
- [ ] Implement status inspection, cleanup preview, safe removal, and audit output.
- [ ] Run `cargo test -p yi-agent-tools --test subagent_worktree`.
- [ ] Commit `feat: manage subagent worktree retention`.
