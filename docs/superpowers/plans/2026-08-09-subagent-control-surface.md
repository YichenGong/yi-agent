# Subagent Control Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans task-by-task.

**Goal:** Expose the daemon task tree safely through CLI, TUI, Slash commands, and help.

### Task 1: Add shared command schema and CLI client

- [ ] Add parser tests for all documented commands, unavailable daemon behavior,
  and destructive preview-token confirmation.
- [ ] Implement `CommandSpec`, clap commands, IPC client encoding, and JSON output.
- [ ] Run `cargo test -p yi-agent --bin yi-agent subagent_`.
- [ ] Commit `feat: add subagent runtime CLI controls`.

### Task 2: Add TUI task projection and Slash routing

- [ ] Test snapshot/event projection, resync, completion, contextual help, and
  direct-user leaf override notice.
- [ ] Add `tui/agents.rs`, route Slash commands, and render tree/detail/action panels.
- [ ] Run `cargo test -p yi-agent --bin yi-agent tui::`.
- [ ] Commit `feat: show and control subagent tasks in TUI`.

### Task 3: Add permission and review interaction

- [ ] Test review routing to direct parent and user-only security approval.
- [ ] Implement cards/confirmations for permission, accept/rework/reject, and budget changes.
- [ ] Run `cargo test -p yi-agent --bin yi-agent subagent_`.
- [ ] Commit `feat: add subagent review and permission controls`.
