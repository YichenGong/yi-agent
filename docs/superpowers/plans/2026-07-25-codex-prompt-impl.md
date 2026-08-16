# Codex-style Minimal Prompt Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace reedline's default prompt (workdir + datetime + `〉`) with a codex-style `> ` indicator on a gray background that extends across the full input line.

**Architecture:** Two small new types in `crates/yi-agent/src/app.rs` — a `CodexPrompt` implementing reedline's `Prompt` trait (empty left/right, `> ` indicator with ANSI gray bg) and a `CodexHighlighter` implementing `Highlighter` (wraps user's typed text in gray bg via `StyledText`). Both wire into the existing `run_input_loop` function. Adds `nu-ansi-term` as a direct dependency.

**Tech Stack:** Rust, reedline 0.38 (`Prompt`, `Highlighter`, `StyledText` traits), `nu-ansi-term` 0.50 (for `Style` and `Color` used by `StyledText`), ANSI escape codes for gray background.

---

## Context

### Current state

`crates/yi-agent/src/app.rs:337` uses `reedline::DefaultPrompt::default()`. Per reedline 0.38's source (`prompt/default.rs:94-101`), this renders:
- **Left**: working directory path (e.g., `~/projects/yi-agent`) — **noisy, user doesn't need this**
- **Right**: current date/time — **noisy, user doesn't need this**
- **Indicator**: `〉` (Unicode angle bracket)

### Target

Single `> ` prompt with continuous gray background covering the entire input line (prompt indicator + user's typed text). Nothing else visible.

### How reedline renders the prompt

From `painting/painter.rs:334-376` (reedline 0.38 source), the painter prints in order:

1. `prompt.render_prompt_left()` with `get_prompt_color()` fg
2. `prompt.render_prompt_indicator()` with `get_indicator_color()` fg
3. `ResetColor` + `SetAttribute(Reset)` — clears all styling
4. `before_cursor` + `after_cursor` — the user's typed text, styled by the `Highlighter` trait's `highlight()` method returning `StyledText`

This means:
- We can put ANSI gray-bg codes inside `render_prompt_indicator()` — they'll set the bg before printing `> `
- The `ResetColor` at step 3 clears the bg, so to color the user's typed text we MUST implement `Highlighter` to re-apply gray bg via `StyledText`'s `Style::new().on(Color::DarkGrey)` per character range

### ANSI codes

- `\x1b[48;5;238m` — 256-color dark gray background (color index 238, matches `InlineRenderer::COLOR_USER_BG` = `48;5;240`)
- `\x1b[0m` — reset all attributes

We use `48;5;238` (slightly darker than the renderer's `48;5;240`) to differentiate the prompt area from the echoed-user-input area (which uses `48;5;240`).

Actually, for consistency, let's match the renderer's `48;5;240`. Reading `inline.rs:14`:
```rust
const COLOR_USER_BG: &str = "\x1b[48;5;240m"; // 浅灰背景
```

Use the same `48;5;240` so the prompt blends with the echoed user input.

---

## Task 1: Add `nu-ansi-term` dependency

**Files:**
- Modify: `yi-agent-rs/Cargo.toml` (workspace deps section)
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml` (yi-agent's `[dependencies]`)

**Step 1: Add to workspace deps**

In `yi-agent-rs/Cargo.toml`, in the `[workspace.dependencies]` section, add (alphabetical order, after `nu-ansi-term` if it doesn't exist; near `crossterm`):

```toml
nu-ansi-term = "0.50"
```

Find the existing `crossterm = "0.28"` line in `[workspace.dependencies]` and add `nu-ansi-term` below it (alphabetical: crossterm < nu-ansi-term < reedline).

**Step 2: Add to yi-agent's dependencies**

In `yi-agent-rs/crates/yi-agent/Cargo.toml`, in `[dependencies]` section, add after `crossterm = { workspace = true }`:

```toml
nu-ansi-term = { workspace = true }
```

**Step 3: Verify build**

Run from `yi-agent-rs/`:
```bash
cargo build -p yi-agent
```

Expected: Compiles successfully (nu-ansi-term was already a transitive dep, now direct).

**Step 4: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent/Cargo.toml
git commit -m "chore: add nu-ansi-term as direct dep for prompt styling"
```

---

## Task 2: Implement `CodexPrompt` and `CodexHighlighter` types

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs` (add types before `run_input_loop` function, around line 314)

**Step 1: Write the failing tests**

Add a new `#[cfg(test)]` module at the end of `app.rs` (or extend the existing test module). Place these tests in the existing `mod tests` block at the end of the file.

Add these imports near the top of the test module:
```rust
use reedline::{Highlighter, Prompt, PromptEditMode, StyledText};
```

Add these test functions:

```rust
#[test]
fn codex_prompt_left_is_empty() {
    let prompt = super::CodexPrompt;
    assert_eq!(prompt.render_prompt_left().as_ref(), "");
}

#[test]
fn codex_prompt_right_is_empty() {
    let prompt = super::CodexPrompt;
    assert_eq!(prompt.render_prompt_right().as_ref(), "");
}

#[test]
fn codex_prompt_indicator_starts_with_gray_bg_and_gt() {
    let prompt = super::CodexPrompt;
    let indicator = prompt.render_prompt_indicator(PromptEditMode::Emacs);
    let s = indicator.as_ref();
    assert!(s.starts_with("\x1b[48;5;240m"), "indicator should start with gray bg ANSI code, got: {s:?}");
    assert!(s.contains(">"), "indicator should contain > character, got: {s:?}");
}

#[test]
fn codex_highlighter_wraps_input_with_gray_bg() {
    let highlighter = super::CodexHighlighter;
    let styled = highlighter.highlight("hello world", 0);
    // StyledText has a buffer field
    assert_eq!(styled.buffer.len(), 1, "should produce exactly one styled segment");
    let (style, text) = &styled.buffer[0];
    assert_eq!(text, "hello world");
    // style should have a background color (the gray)
    assert!(style.is_background(), "style should have a background color");
}
```

Note: `is_background()` is a method on `nu_ansi_term::Style` that returns true if a background color is set. Verify by checking `nu-ansi-term` 0.50's API.

**Step 2: Run tests to verify they fail**

Run from `yi-agent-rs/`:
```bash
cargo test -p yi-agent --bin yi-agent -- codex
```

Expected: FAIL with "cannot find type `CodexPrompt` in this scope" or similar — types don't exist yet.

**Step 3: Implement `CodexPrompt` and `CodexHighlighter`**

In `crates/yi-agent/src/app.rs`, **before** the `run_input_loop` function (currently at line 322), add these types:

```rust
/// ANSI gray background (256-color index 240, matches `InlineRenderer::COLOR_USER_BG`).
const PROMPT_BG: &str = "\x1b[48;5;240m";
const ANSI_RESET: &str = "\x1b[0m";

/// Codex-style prompt: `> ` on gray background, no workdir, no datetime.
struct CodexPrompt;

impl reedline::Prompt for CodexPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _mode: reedline::PromptEditMode) -> std::borrow::Cow<str> {
        std::borrow::Cow::Owned(format!("{PROMPT_BG}> "))
    }
    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<str> {
        std::borrow::Cow::Borrowed(" ")
    }
    fn render_prompt_history_search_indicator(
        &self,
        search: reedline::PromptHistorySearch,
    ) -> std::borrow::Cow<str> {
        let prefix = match search.status {
            reedline::PromptHistorySearchStatus::Passing => "",
            reedline::PromptHistorySearchStatus::Failing => "failing ",
        };
        std::borrow::Cow::Owned(format!("{PROMPT_BG}({prefix}reverse-search: {}) ", search.term))
    }
}

/// Highlighter that wraps user's typed text in gray background to extend
/// the prompt's gray bg across the full input line.
struct CodexHighlighter;

impl reedline::Highlighter for CodexHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> reedline::StyledText {
        let style = nu_ansi_term::Style::new().on(nu_ansi_term::Color::Fixed(240));
        let mut styled = reedline::StyledText::new();
        styled.push((style, line.to_string()));
        styled
    }
}
```

**Step 4: Run tests to verify they pass**

Run:
```bash
cargo test -p yi-agent --bin yi-agent -- codex
```

Expected: All 4 codex tests PASS.

**Step 5: Commit**

```bash
git add crates/yi-agent/src/app.rs
git commit -m "feat(tui): add CodexPrompt and CodexHighlighter types"
```

---

## Task 3: Wire up `CodexPrompt` and `CodexHighlighter` in `run_input_loop`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs` (in `run_input_loop` function, currently lines 322-352)

**Step 1: Replace `DefaultPrompt::default()` with `CodexPrompt`**

In `run_input_loop` (line 322), find:
```rust
let prompt = DefaultPrompt::default();
```
Replace with:
```rust
let prompt = CodexPrompt;
```

Also remove the `DefaultPrompt` from the `use` statement on line 324:
```rust
use reedline::{DefaultPrompt, Emacs, Reedline};
```
Change to:
```rust
use reedline::{Emacs, Reedline};
```

**Step 2: Add `.with_highlighter()` to the `Reedline::create()` chain**

Find (around line 334):
```rust
let mut line_editor = Reedline::create()
    .with_edit_mode(Box::new(Emacs::new(keybindings)))
    .with_external_printer(printer);
```
Change to:
```rust
let mut line_editor = Reedline::create()
    .with_edit_mode(Box::new(Emacs::new(keybindings)))
    .with_highlighter(Box::new(CodexHighlighter))
    .with_external_printer(printer);
```

**Step 3: Verify build and all tests**

Run:
```bash
cargo build -p yi-agent
cargo test --workspace
```

Expected: Build succeeds. All tests pass (previous 280 + new 4 = 284).

**Step 4: Verify clippy and fmt**

Run:
```bash
cargo clippy --workspace --all-targets
cargo fmt --all -- --check
```

Expected: Clean, no warnings.

**Step 5: Commit**

```bash
git add crates/yi-agent/src/app.rs
git commit -m "feat(tui): replace DefaultPrompt with CodexPrompt"
```

---

## Task 4: Manual visual verification

This is a manual task — no automation.

**Step 1: Build the binary**

```bash
cargo build -p yi-agent
```

**Step 2: Run the agent**

From `yi-agent-rs/`:
```bash
./target/debug/yi-agent
```

**Step 3: Verify the following visual properties**

1. The prompt shows `> ` (just `>` + space) on a gray background
2. NO working directory is displayed
3. NO date/time is displayed
4. When you type text, the text appears on a gray background
5. Gray background extends continuously from `>` to the end of your typed text
6. When you press Enter, the echoed user input (`你: ...`) shows on the same gray bg (unchanged from before)
7. When streaming agent text renders, it appears above the prompt without visual corruption
8. ESC clears the current line (unchanged behavior)

If any of these fail, report the issue before proceeding.

---

## Critical Files Summary

| File | Purpose |
|------|---------|
| `yi-agent-rs/Cargo.toml` | Add `nu-ansi-term` to workspace deps |
| `yi-agent-rs/crates/yi-agent/Cargo.toml` | Add `nu-ansi-term` to yi-agent's deps |
| `yi-agent-rs/crates/yi-agent/src/app.rs` | Add `CodexPrompt`, `CodexHighlighter`, wire up in `run_input_loop` |

## What stays unchanged

- `/config` command still shows workdir (explicit user request)
- `InlineRenderer` output styling (assistant text, tool calls, etc.) unchanged
- `render_user_input` still shows `你: ...` with `COLOR_USER_BG` (240) — matches the new prompt bg

## Verification Checklist

- [ ] `nu-ansi-term` added as direct dep
- [ ] `CodexPrompt` implements `Prompt`, returns empty left/right, `> ` with ANSI gray bg indicator
- [ ] `CodexHighlighter` implements `Highlighter`, returns `StyledText` with gray bg `Style`
- [ ] `run_input_loop` uses `CodexPrompt` and `CodexHighlighter`
- [ ] 4 new tests added, all pass
- [ ] All previous tests still pass
- [ ] clippy clean, fmt clean
- [ ] Manual visual verification passes
