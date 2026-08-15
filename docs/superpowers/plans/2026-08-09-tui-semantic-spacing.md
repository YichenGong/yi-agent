# TUI Semantic Spacing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make TUI history visually separate a resumed model response from the completed tool-work group that precedes it, while retaining compact tool activity.

**Architecture:** Keep layout policy in `HistoryState`/`HistoryView`, where all history-line counting and viewport anchoring already live. Do not add agent events or change the core event contract: derive the extra boundary from adjacent `ToolResult` and `AssistantMessage` cells, so an assistant response following a result is separated exactly once and streamed chunks remain in the same response cell.

**Tech Stack:** Rust, ratatui, `yi-agent-core::AgentEvent`, Cargo unit tests.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs` — centralize semantic spacer detection; apply it in flattened line counts, viewport anchor capture/restore, and rendered history lines; add behavior and layout tests.
- Modify: `docs/project-management/yi-agent-tui.md` — record semantic conversation spacing with a reproducible focused test command.
- Modify: `docs/project-management/README.md` — increment the completed TUI module feature count from `17 / 18` to `18 / 19`.

### Task 1: Specify the TUI spacing behavior with failing tests

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs:805-1180`

- [ ] **Step 1: Add a test helper that builds tool-result events**

  Add this helper next to `use yi_agent_core::{AgentEvent, DoneReason, ToolResult};` in the existing test module:

  ```rust
  fn tool_result(id: &str, is_error: bool) -> AgentEvent {
      AgentEvent::ToolResult {
          id: id.into(),
          result: ToolResult {
              content: vec![yi_agent_core::ContentBlock::Text("result".into())],
              is_error,
          },
      }
  }
  ```

- [ ] **Step 2: Add a failing test for one semantic boundary after tool work**

  Add `flattened_lines_inserts_spacer_before_assistant_after_tool_result`. Feed `AssistantText("checking")`, one `ToolCall`, `tool_result("1", false)`, then `AssistantText("done")`. Build `HistoryView`, call `flattened_lines(80)`, and assert that the blank line immediately before the final assistant line is attributed to the preceding `ToolResult` cell. Also assert that the result itself immediately follows the tool-call line, with no blank line between them.

- [ ] **Step 3: Add failing tests for streaming, batching, and failed work**

  Add all three focused tests:

  ```rust
  #[test]
  fn assistant_chunks_after_tool_result_share_one_spacer() { /* ... */ }

  #[test]
  fn consecutive_tool_results_create_one_spacer_before_response() { /* ... */ }

  #[test]
  fn failed_tool_result_still_separates_follow_up_assistant_text() { /* ... */ }
  ```

  In the first test, append a second `AssistantText(" again")` and assert that only one empty line is attributed to a `ToolResult`. In the second, create two `ToolCall`/`tool_result` pairs and assert exactly one empty result-owned spacer before `AssistantText("summary")`. In the third, pass `true` to `tool_result` and assert the same one-spacer relationship.

- [ ] **Step 4: Run the new tests and confirm the first boundary test fails**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::history::tests::flattened_lines_inserts_spacer_before_assistant_after_tool_result
  ```

  Expected: FAIL because `HistoryView::flattened_lines` only creates spacers after `UserMessage` cells.

- [ ] **Step 5: Commit the test specification**

  ```bash
  cargo fmt --all
  git add yi-agent-rs/crates/yi-agent/src/tui/history.rs
  git commit -m "test(tui): cover semantic history spacing"
  ```

### Task 2: Implement semantic tool-to-response spacing

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs:62-151`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs:471-488`

- [ ] **Step 1: Introduce one shared spacer predicate**

  Add a private `has_spacer_after(cell, next_cell)` helper near `HistoryState`. It must return `true` for either of these pairs:

  ```rust
  matches!(cell, HistoryCell::UserMessage { .. })
  matches!(cell, HistoryCell::ToolResult { .. })
      && matches!(next_cell, Some(HistoryCell::AssistantMessage { .. }))
  ```

  Require a following cell for both cases, so no trailing blank row appears. Do not add a spacer after `ToolCall`, between tool results, or around `Separator`/permission cells.

- [ ] **Step 2: Apply the predicate to all logical line accounting**

  Replace the user-only conditions in `HistoryState::flattened_line_count`, `capture_viewport_anchor`, and `restore_viewport_anchor` with `has_spacer_after(cell, self.cells.get(index + 1))`. Keep `AnchorPosition::AfterCellSpacer`; it now describes either a user boundary or the final tool-result boundary. Update its doc comments to say “semantic spacer.”

- [ ] **Step 3: Apply the predicate to rendered history lines**

  In `HistoryView::flattened_lines`, replace the user-only branch with the shared predicate and continue attributing `Line::raw("")` to the preceding cell index. This preserves Ctrl+O selection and makes scroll/scrollbar calculations match the visible transcript.

- [ ] **Step 4: Run focused behavior and anchoring tests**

  Run:

  ```bash
  cargo test -p yi-agent --bin yi-agent tui::history::tests::flattened_lines_
  cargo test -p yi-agent --bin yi-agent tui::history::tests::viewport_anchor_
  ```

  Expected: PASS. The first command proves compact tool groups and exactly one resumed-response boundary; the second proves spacer-aware reflow anchoring still works.

- [ ] **Step 5: Commit the implementation**

  ```bash
  cargo fmt --all
  git add yi-agent-rs/crates/yi-agent/src/tui/history.rs
  git commit -m "feat(tui): space assistant responses after tool work"
  ```

### Task 3: Record completion and validate the TUI crate

**Files:**
- Modify: `docs/project-management/yi-agent-tui.md:34-36`
- Modify: `docs/project-management/README.md:14`

- [ ] **Step 1: Document the completed semantic spacing behavior**

  Add a checked item to `docs/project-management/yi-agent-tui.md`:

  ```markdown
  - [x] 语义化对话留白 — `tui/history.rs` 在用户输入后、工具结果后的首段模型回复前各保留一行空白，工具调用/结果连续显示；验证：`cargo test -p yi-agent --bin yi-agent tui::history::tests::flattened_lines_`
  ```

  Update `docs/project-management/README.md` from `17 / 18` to `18 / 19` for `yi-agent-tui`.

- [ ] **Step 2: Run the complete crate test suite serially**

  First confirm no stale Cargo, Rust, or yi-agent test process remains:

  ```bash
  ps aux | rg '[c]argo|[r]ustc|yi_agent'
  ```

  Then run:

  ```bash
  cargo test -p yi-agent --jobs 2
  ```

  Expected: PASS with no test failures.

- [ ] **Step 3: Format and run the repository formatting check**

  ```bash
  cargo fmt --all
  just fmt-check
  ```

  Expected: both commands exit successfully with no formatting diff.

- [ ] **Step 4: Inspect the final diff and commit project tracking**

  ```bash
  git diff --check
  git status --short
  git add docs/project-management/yi-agent-tui.md docs/project-management/README.md
  git commit -m "docs: track TUI semantic spacing"
  ```

## Plan Self-Review

- Spec coverage: Task 1 covers compact tool chains, resumed-response spacing, repeated streamed chunks, and failed results. Task 2 applies one rule consistently to rendering, line counts, scrollbar inputs, and viewport anchors. Task 3 records the verified feature and runs crate-level validation.
- Placeholder scan: no TBDs, deferred work, or unspecified tests remain.
- Type consistency: all tasks modify the existing `HistoryState`, `HistoryView`, `HistoryCell`, `AnchorPosition`, and `AgentEvent` interfaces; no new public type or event is introduced.
