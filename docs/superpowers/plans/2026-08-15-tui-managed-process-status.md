# TUI Managed Process Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Display the active managed-background-process count as `N proc running` in the TUI status bar, without elapsed time.

**Architecture:** `app.rs` owns fresh `ProcessManager` snapshots in the TUI loop and derives an active-process count from their `ProcessStatus` values. `statusbar.rs` remains a presentation-only renderer: it receives that count alongside its existing foreground Bash task registry and renders a separate text field before token/model data.

**Tech Stack:** Rust, ratatui, yi-agent `ProcessManager` / `ManagedProcessSnapshot`, cargo test.

## Global Constraints

- Count only `ProcessStatus::Starting`, `ProcessStatus::Running`, and `ProcessStatus::Ready` as active managed processes.
- Render the exact copy `N proc running`, including for one process; render no managed-process field for zero.
- Do not display elapsed time for managed processes.
- Preserve the existing foreground Bash-task indicator and its elapsed time.
- Do not count foreground Bash tool calls in `N proc running`.
- Update `docs/project-management/yi-agent-tui.md` and increment the yi-agent-tui completed/total count in `docs/project-management/README.md` in the implementation commit.
- Run `cargo fmt --all` before committing Rust changes.

---

## File Structure

- Modify `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs`: accept an active managed-process count, render its field, and cover it with focused unit tests.
- Modify `yi-agent-rs/crates/yi-agent/src/tui/app.rs`: derive the count from the already maintained `process_snapshots` collection and pass it into the status-bar renderer.
- Modify `docs/project-management/yi-agent-tui.md`: add a completed, verifiable feature entry.
- Modify `docs/project-management/README.md`: update yi-agent-tui from `24 / 25` to `25 / 26`.

### Task 1: Render the managed-process indicator

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs:152-203, 272-302`

**Interfaces:**
- Consumes: existing `StatusBarState`, `RunningTaskRegistry`, and `model: &str`.
- Produces: `render_statusbar(state: &StatusBarState, tasks: &RunningTaskRegistry, active_process_count: usize, model: &str) -> Line<'_>`.
- Used by: `tui/app.rs` after it derives the active count.

- [x] **Step 1: Write failing renderer tests**

Change existing renderer calls to include `0`, then add these tests in the `statusbar.rs` test module:

```rust
#[test]
fn test_render_statusbar_omits_inactive_managed_processes() {
    let line = render_statusbar(
        &StatusBarState::default(),
        &RunningTaskRegistry::new(),
        0,
        "model",
    );
    let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(!text.contains("proc running"), "zero count must be omitted: {text}");
}

#[test]
fn test_render_statusbar_shows_managed_process_count_without_duration() {
    let line = render_statusbar(
        &StatusBarState::default(),
        &RunningTaskRegistry::new(),
        2,
        "model",
    );
    let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("2 proc running"), "count should be rendered: {text}");
    assert!(!text.contains("proc running 0."), "process field must not include time: {text}");
}

#[test]
fn test_render_statusbar_shows_bash_and_managed_processes_together() {
    let mut tasks = RunningTaskRegistry::new();
    tasks.on_tool_call("bash-1", "bash", "sleep 10", 120);
    let line = render_statusbar(&StatusBarState::default(), &tasks, 1, "model");
    let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
    assert!(text.contains("● bash"), "bash indicator should remain: {text}");
    assert!(text.contains("1 proc running"), "process count should render: {text}");
}
```

- [x] **Step 2: Run the focused tests to verify the new API is missing**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::statusbar::tests
```

Expected: compilation fails because `render_statusbar` does not yet accept `active_process_count`.

- [x] **Step 3: Implement the minimal renderer change**

Update the signature and documentation layout in `statusbar.rs`:

```rust
pub fn render_statusbar<'a>(
    state: &'a StatusBarState,
    tasks: &'a RunningTaskRegistry,
    active_process_count: usize,
    model: &'a str,
) -> Line<'a>
```

After the existing foreground-task spans block and before `prefill`, append exactly this when `active_process_count > 0`:

```rust
if active_process_count > 0 {
    spans.push(Span::raw(format!("{active_process_count} proc running  ")));
}
```

Update every existing `render_statusbar` test call to pass `0` unless the test specifically exercises managed processes.

- [x] **Step 4: Run focused tests to verify rendering passes**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::statusbar::tests
```

Expected: all status-bar tests pass.

- [ ] **Step 5: Commit the renderer test and implementation**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs
git commit -m "feat(tui): show managed process count in status bar"
```

### Task 2: Derive active process count in the TUI loop and record progress

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs:20-33, 193-204, 329-334`
- Modify: `docs/project-management/yi-agent-tui.md:38-43`
- Modify: `docs/project-management/README.md:12-18`

**Interfaces:**
- Consumes: `process_snapshots: Vec<yi_agent_tools::ManagedProcessSnapshot>` and `yi_agent_tools::ProcessStatus`.
- Consumes: `render_statusbar(..., active_process_count: usize, ...)` from Task 1.
- Produces: a status-bar count that tracks `Starting`, `Running`, and `Ready` snapshots; it excludes terminal statuses.

- [x] **Step 1: Write the failing active-status predicate test**

Add this helper near the status-bar call site or another module-private location in `app.rs`, and write its unit test in the existing `app.rs` test module:

```rust
fn is_active_managed_process(status: &yi_agent_tools::ProcessStatus) -> bool {
    matches!(
        status,
        yi_agent_tools::ProcessStatus::Starting
            | yi_agent_tools::ProcessStatus::Running
            | yi_agent_tools::ProcessStatus::Ready
    )
}

#[test]
fn active_managed_process_statuses_exclude_terminal_states() {
    use yi_agent_tools::ProcessStatus;

    assert!(is_active_managed_process(&ProcessStatus::Starting));
    assert!(is_active_managed_process(&ProcessStatus::Running));
    assert!(is_active_managed_process(&ProcessStatus::Ready));
    assert!(!is_active_managed_process(&ProcessStatus::Exited { code: Some(0) }));
    assert!(!is_active_managed_process(&ProcessStatus::Killed));
    assert!(!is_active_managed_process(&ProcessStatus::FailedToStart {
        reason: "spawn failed".into(),
    }));
}
```

Use the actual `ProcessStatus::Exited` field type from `yi-agent-tools/src/process/manager.rs` if it differs from `Option<i32>`.

- [x] **Step 2: Run the focused app test to verify the helper is missing**

Run:

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent active_managed_process_statuses_exclude_terminal_states
```

Expected: compilation fails because `is_active_managed_process` does not yet exist.

- [x] **Step 3: Implement count derivation and wire the renderer**

Define `is_active_managed_process` using the `matches!` expression from Step 1. Immediately before `terminal.draw`, derive the count from the snapshots refreshed by the existing TUI loop:

```rust
let active_process_count = process_snapshots
    .iter()
    .filter(|snapshot| is_active_managed_process(&snapshot.status))
    .count();
```

Pass it to the renderer in the draw closure:

```rust
let statusbar_line = render_statusbar(
    &statusbar_state,
    &task_registry,
    active_process_count,
    model,
);
```

- [x] **Step 4: Update project-management documentation**

Add this completed feature in `docs/project-management/yi-agent-tui.md` after the managed-process popup entry:

```markdown
- [x] 托管后台进程状态栏计数 — `tui/app.rs::is_active_managed_process` 统计 `Starting` / `Running` / `Ready` 快照，`tui/statusbar.rs::render_statusbar` 显示无耗时的 `N proc running`；验证：`cargo test -p yi-agent --bin yi-agent tui::statusbar::tests` 和 `cargo test -p yi-agent --bin yi-agent active_managed_process_statuses_exclude_terminal_states`
```

Change the yi-agent-tui index count in `docs/project-management/README.md` from `24 / 25` to `25 / 26`.

- [x] **Step 5: Run formatting and all relevant focused tests**

Run sequentially, after confirming no other Cargo process is active:

```bash
ps aux | grep -v grep | grep -E '[c]argo|[r]ustc|yi_agent' || true
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent --bin yi-agent tui::statusbar::tests && cargo test -p yi-agent --bin yi-agent active_managed_process_statuses_exclude_terminal_states
```

Expected: formatter succeeds and both test commands pass.

- [ ] **Step 6: Inspect the completed change and commit**

Run:

```bash
git diff --check
git diff -- yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs yi-agent-rs/crates/yi-agent/src/tui/app.rs docs/project-management/yi-agent-tui.md docs/project-management/README.md
```

Verify that the diff contains only the managed-process count, its tests, and the required progress updates. Then commit:

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs docs/project-management/yi-agent-tui.md docs/project-management/README.md
git commit -m "feat(tui): report active managed processes"
```

### Task 3: Final verification

**Files:**
- Inspect: `yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs`
- Inspect: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Inspect: `docs/project-management/yi-agent-tui.md`
- Inspect: `docs/project-management/README.md`

**Interfaces:**
- Consumes: completed Tasks 1 and 2.
- Produces: verified, reviewable feature branch ready for merge.

- [ ] **Step 1: Inspect the final source and documentation**

Run:

```bash
sed -n '152,205p' yi-agent-rs/crates/yi-agent/src/tui/statusbar.rs
sed -n '320,340p' yi-agent-rs/crates/yi-agent/src/tui/app.rs
rg -n '托管后台进程状态栏计数|yi-agent-tui' docs/project-management/yi-agent-tui.md docs/project-management/README.md
```

Verify the renderer only displays `N proc running`, and the active-status filter includes exactly `Starting`, `Running`, and `Ready`.

- [ ] **Step 2: Run final verification**

Run:

```bash
cd yi-agent-rs && cargo fmt --all -- --check && cargo test -p yi-agent --bin yi-agent tui::statusbar::tests && cargo test -p yi-agent --bin yi-agent active_managed_process_statuses_exclude_terminal_states
cd .. && git diff main...HEAD --check && git status --short --branch
```

Expected: formatting and tests pass, the cumulative branch diff has no whitespace errors, and the worktree is clean on `feat/process-statusbar`.
