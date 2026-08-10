# Grep Output Limits Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound every `grep` tool result before it reaches the LLM conversation.

**Architecture:** Keep the safeguard local to `GrepTool`. `grep_search` accounts for rendered records and stops at fixed entry or byte budgets, so no global `ToolResult` behavior changes.

**Tech Stack:** Rust 2024, `walkdir`, `regex`, `tokio` tests.

---

### Task 1: Regression coverage

**Files:**
- Modify and test: `yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs`

- [x] Add a file-list test that creates 201 matching files and requires a truncation marker.
- [x] Run the test to observe its expected failure before implementation.
- [x] Add a large-content test that requires a truncation marker.
- [x] Run the test to observe its expected failure before implementation.

### Task 2: Local output budgets

**Files:**
- Modify and test: `yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs`

- [x] Define a 200-entry and 32-KiB internal budget.
- [x] Route content records, context records, file paths, and count records through a bounded append helper.
- [x] Stop walking and append one ASCII truncation notice after a budget rejection.
- [x] Run both new regression tests and the full `grep_` test filter.

### Task 3: Documentation and branch delivery

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md`

- [x] Update the FS tool completion criterion with the grep output-limit verification command.
- [x] Run formatting and final verification, commit, and merge the branch.
