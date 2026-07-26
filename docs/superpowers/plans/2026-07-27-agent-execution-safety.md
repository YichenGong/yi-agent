# Agent Execution Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ensure agent cancellation and failure paths leave no hidden tool execution, no hung headless command, and no malformed provider history.

**Architecture:** Propagate the agent cancellation token through the streaming tool interface. Keep protocol recovery inside the agent loop, while provider adapters surface stream failures as errors. Bash updates its persistent cwd only for an explicitly successful standalone directory change.

**Tech Stack:** Rust 2024, Tokio, futures, Cargo tests.

---

### Task 1: Cancellation and shell state

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`

- [ ] Write failing tests proving cancellation terminates bash and `false && cd x` does not persist cwd.
- [ ] Run the focused tests and verify they fail against the current behavior.
- [ ] Propagate cancellation to streaming tools, terminate bash on cancellation, and update cwd only after successful standalone `cd`.
- [ ] Re-run focused tests and verify they pass.

### Task 2: Recoverable agent failures

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/client.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

- [ ] Write failing tests for unknown-tool tool-result pairing, provider stream errors, and headless blacklisted commands.
- [ ] Run focused tests and verify they fail against the current behavior.
- [ ] Return complete error tool results, surface stream errors, and omit the unusable headless confirmation receiver.
- [ ] Re-run focused tests and verify they pass.

### Task 3: Full verification

**Files:**
- Modify: affected tests and implementation files only.

- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo test --workspace`; document any environment-dependent test failure separately.
