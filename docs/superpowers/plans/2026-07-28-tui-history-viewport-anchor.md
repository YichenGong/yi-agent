# TUI History Viewport Anchor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve the same non-bottom history content through reflow, queue-layout changes, and new output.

**Architecture:** `HistoryState` captures a content-relative top-of-viewport anchor using a cell index and line offset at the old effective text width. It restores that anchor with the final effective text width and viewport height. `app.rs` captures before each frame's mutations/layout transition and restores after the final history rectangle is known; bottom follow has no anchor and retains offset zero.

**Tech Stack:** Rust 2024, Ratatui 0.29, Crossterm 0.28, Cargo test.

---

### Task 1: Add content-relative viewport anchors

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`

- [ ] **Step 1: Write failing state tests**

Add a private test helper that makes two multi-line `AssistantMessage` cells. Add a test that scrolls off bottom at width 20/height 3, captures an anchor, then restores it at width 10/height 4 and asserts that the anchored cell and its line remain at viewport top. Add a second test that captures at offset zero and asserts `None`.

- [ ] **Step 2: Verify red**

Run `cargo test -p yi-agent --bin yi-agent tui::history::tests::capture_viewport_anchor`.
Expected: FAIL because `capture_viewport_anchor` does not exist.

- [ ] **Step 3: Implement anchor API**

Add an internal `ViewportAnchor { cell_index: usize, line_in_cell: usize }`. Implement `capture_viewport_anchor(text_width, viewport_height) -> Option<ViewportAnchor>` by translating `total - height - scroll_offset` into a flattened cell/line location, treating inserted user spacers as belonging to the preceding cell. Implement `restore_viewport_anchor(anchor, text_width, viewport_height)` by counting lines before the anchor at the new width and deriving `scroll_offset = total - height - anchored_top`, then clamp it.

- [ ] **Step 4: Verify green and commit**

Run `cargo test -p yi-agent --bin yi-agent tui::history::tests` and `cargo fmt --all`.

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/history.rs
git commit -m "fix: anchor TUI history viewport to content"
```

### Task 2: Restore anchors around frame geometry and history mutations

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`

- [ ] **Step 1: Write failing integration tests**

Add a fake-event test that starts scrolled up, changes terminal width between loop iterations, and asserts the same text marker remains at the top row. Add tests where queued preview grows after input submission and shrinks after `Done`, asserting the old top marker remains visible.

- [ ] **Step 2: Verify red**

Run `cargo test -p yi-agent --bin yi-agent tui::app::tests::history_anchor`.
Expected: FAIL because the frame loop does not retain a content anchor.

- [ ] **Step 3: Replace global-line compensation**

Before draining events, calculate the current history layout and `text_width`, then capture the state anchor. Calculate the post-event/post-promotion layout, mutate history using its final effective width, and call `restore_viewport_anchor` when an anchor exists. Delete the `initial_cells`, `initial_lines`, and global reflow-delta compensation block; retain bottom-follow when capture returned `None`.

- [ ] **Step 4: Verify green and commit**

Run `cargo test -p yi-agent --bin yi-agent tui::app::tests` and `cargo fmt --all`.

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "fix: preserve TUI history anchor across layout changes"
```

### Task 3: Cover scrollbar activation and complete verification

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `docs/project-management/yi-agent-tui.md`

- [ ] **Step 1: Write failing overflow-boundary test**

Add a test where a scrolled-up local user-message insertion changes fitting content into overflowing content. Assert restoring the previously captured anchor at the new `text_width` keeps the same top marker rather than using the old full width.

- [ ] **Step 2: Verify red and implement only the missing anchor behavior**

Run the focused test, then ensure all local insertions and agent events restore the frame anchor after final scrollbar width is known.

- [ ] **Step 3: Run final verification and commit documentation**

```bash
cd yi-agent-rs
cargo test -p yi-agent --bin yi-agent tui::app::tests
cargo test -p yi-agent --bin yi-agent tui::history::tests
cargo clippy -p yi-agent --bin yi-agent -- -D warnings
just fmt-check
```

Update the TUI progress item with the anchor-based verification command, then commit it.

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/history.rs yi-agent-rs/crates/yi-agent/src/tui/app.rs docs/project-management/yi-agent-tui.md
git commit -m "test: cover TUI history viewport anchoring"
```
