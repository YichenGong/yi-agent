# TUI LaTeX Rendering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render common LaTeX formulas in the TUI Markdown history as readable Unicode mathematics for all four supported delimiters.

**Architecture:** Keep `tui/markdown.rs` as the single Markdown rendering boundary. Normalize backslash delimiters outside Markdown code, enable pulldown-cmark math events, and use a focused, allocation-only TeX-to-Unicode helper to convert formula bodies before adding them to ratatui spans. Existing wrapping, history, and streaming paths remain unchanged.

**Tech Stack:** Rust, pulldown-cmark 0.12, ratatui 0.29, unicode-width, Cargo tests.

---

## File Structure

- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs` — delimiter normalization, math-event handling, Unicode TeX conversion, and unit coverage.
- Modify: `docs/project-management/yi-agent-tui.md` — mark terminal LaTeX rendering complete with an executable verification command.
- Modify: `docs/project-management/README.md` — increment the TUI completed-feature count from 14 / 15 to 15 / 16.

### Task 1: Enable and prove Markdown math event handling

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:7-15`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:404-680`

- [ ] **Step 1: Write the failing dollar-delimited math tests**

Add after `inline_code_is_cyan`:

```rust
#[test]
fn inline_dollar_math_omits_delimiters_and_is_cyan() {
    let lines = render_markdown("area is $\\pi r^2$.", 80);
    let formula = lines[0]
        .spans
        .iter()
        .find(|span| span.content == "π r²")
        .expect("formula span");
    assert_eq!(formula.style.fg, Some(Color::Cyan));
}

#[test]
fn display_dollar_math_is_its_own_line() {
    let rendered: Vec<String> = render_markdown("before\\n\\n$$\\frac{a}{b}$$\\n\\nafter", 80)
        .iter()
        .map(spans_text)
        .collect();
    assert_eq!(rendered, ["before", "a⁄b", "after"]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests::inline_dollar_math_omits_delimiters_and_is_cyan -- --nocapture && cargo test -p yi-agent tui::markdown::tests::display_dollar_math_is_its_own_line -- --nocapture`

Expected: FAIL because `ENABLE_MATH` is not set and `InlineMath` / `DisplayMath` events are not handled.

- [ ] **Step 3: Enable math parsing and handle the two events**

Change the renderer setup and `handle_event` match:

```rust
let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
let parser = Parser::new_ext(&normalize_backslash_math_delimiters(src), opts);
```

```rust
Event::InlineMath(math) => self.push_span(Span::styled(
    render_tex_to_unicode(&math),
    Style::new().fg(Color::Cyan),
)),
Event::DisplayMath(math) => {
    self.flush_line_if_nonempty();
    self.lines.push(Line::styled(
        render_tex_to_unicode(&math),
        Style::new().fg(Color::Cyan),
    ));
}
```

Add `flush_line_if_nonempty`, which invokes `flush_line` only when
`current_spans` is non-empty. This avoids adding blank lines before a display
formula while preserving paragraph breaks emitted by the parser.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests::inline_dollar_math_omits_delimiters_and_is_cyan -- --nocapture && cargo test -p yi-agent tui::markdown::tests::display_dollar_math_is_its_own_line -- --nocapture`

Expected: PASS.

- [ ] **Step 5: Commit the parser event support**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/markdown.rs
git commit -m "feat(tui): recognize Markdown math events"
```

### Task 2: Convert supported TeX syntax to terminal Unicode

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:20-400`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:404-680`

- [ ] **Step 1: Write failing conversion tests**

Add these tests:

```rust
#[test]
fn inline_math_renders_symbols_scripts_fractions_and_roots() {
    let rendered = render_markdown(
        "$\\alpha + x^2 + a_{i+1} + \\frac{m}{n} + \\sqrt{z}$",
        80,
    );
    assert_eq!(spans_text(&rendered[0]), "α + x² + aᵢ₊₁ + m⁄n + √z");
}

#[test]
fn unsupported_math_command_remains_readable() {
    let rendered = render_markdown("$\\unknown{x}$", 80);
    assert_eq!(spans_text(&rendered[0]), "unknownx");
}
```

- [ ] **Step 2: Run conversion tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests::inline_math_renders_symbols_scripts_fractions_and_roots -- --nocapture && cargo test -p yi-agent tui::markdown::tests::unsupported_math_command_remains_readable -- --nocapture`

Expected: FAIL because `render_tex_to_unicode` does not yet parse TeX groups,
commands, or scripts.

- [ ] **Step 3: Add a focused recursive TeX reader and renderer**

Add a private `TexRenderer<'a> { input: Chars<'a> }` below `LineBuilder`. Its
`render_until_group_end` method reads characters recursively and handles:

```rust
match command.as_str() {
    "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ",
    "epsilon" => "ε", "theta" => "θ", "lambda" => "λ", "mu" => "μ",
    "pi" => "π", "sigma" => "σ", "phi" => "φ", "omega" => "ω",
    "Gamma" => "Γ", "Delta" => "Δ", "Theta" => "Θ", "Lambda" => "Λ",
    "Pi" => "Π", "Sigma" => "Σ", "Phi" => "Φ", "Omega" => "Ω",
    "sum" => "∑", "prod" => "∏", "int" => "∫", "infty" => "∞",
    "le" | "leq" => "≤", "ge" | "geq" => "≥", "neq" => "≠",
    "times" => "×", "cdot" => "·", "pm" => "±", "to" | "rightarrow" => "→",
    "left" | "right" => "", "sin" => "sin", "cos" => "cos", "tan" => "tan",
    _ => command.as_str(),
}
```

For `\\frac`, recursively consume exactly two braced groups and return
`{numerator}⁄{denominator}`. For `\\sqrt`, consume one braced group and return
`√{radicand}`. For `^` and `_`, consume either one character or one braced
group, then map supported characters through explicit superscript/subscript
maps; when a character has no Unicode script equivalent, preserve it unchanged.
`render_tex_to_unicode` constructs the reader and returns its output.

- [ ] **Step 4: Run conversion and existing Markdown tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests -- --nocapture`

Expected: PASS, including existing Markdown, CJK-width, and table regression
tests.

- [ ] **Step 5: Commit the terminal math renderer**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/markdown.rs
git commit -m "feat(tui): render LaTeX math as Unicode"
```

### Task 3: Normalize backslash delimiters without touching Markdown code

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:7-15`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:404-680`

- [ ] **Step 1: Write failing backslash-delimiter and code-preservation tests**

```rust
#[test]
fn backslash_math_delimiters_render_inline_and_display_formulas() {
    let inline = render_markdown("mass \\(\\alpha + \\beta\\)", 80);
    assert_eq!(spans_text(&inline[0]), "mass α + β");

    let display: Vec<String> = render_markdown("before\\n\\[\\sqrt{x}\\]\\nafter", 80)
        .iter()
        .map(spans_text)
        .collect();
    assert_eq!(display, ["before", "√x", "after"]);
}

#[test]
fn code_keeps_latex_delimiters_literal() {
    let rendered: Vec<String> = render_markdown("`$x^2$`\\n\\n```text\\n\\\\[x\\\\]\\n```", 80)
        .iter()
        .map(spans_text)
        .collect();
    assert!(rendered.iter().any(|line| line.contains("$x^2$")));
    assert!(rendered.iter().any(|line| line.contains("\\\\[x\\\\]")));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests::backslash_math_delimiters_render_inline_and_display_formulas -- --nocapture && cargo test -p yi-agent tui::markdown::tests::code_keeps_latex_delimiters_literal -- --nocapture`

Expected: FAIL because `\\(...\\)` and `\\[...\\]` are not converted into
pulldown-cmark math delimiters.

- [ ] **Step 3: Implement code-aware delimiter normalization**

Implement `normalize_backslash_math_delimiters(src: &str) -> String` as a
single-pass state machine with `Normal`, `InlineCode`, and `FencedCode` states.
In `Normal`, replace a matched `\\(` / `\\)` pair with `$` and a matched
`\\[` / `\\]` pair with `$$`. In both code states, append bytes unchanged.
Enter/exit inline code on unescaped backticks and fenced code on lines starting
with three backticks. If no closing delimiter is found, copy the opening
delimiter unchanged. Do not normalize escaped backslashes.

- [ ] **Step 4: Run the targeted tests and all Markdown tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests -- --nocapture`

Expected: PASS; code examples retain literal LaTeX and all four delimiter forms
render formulas.

- [ ] **Step 5: Commit normalization support**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/markdown.rs
git commit -m "feat(tui): support backslash math delimiters"
```

### Task 4: Verify wrapping and record completion

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs:404-680`
- Modify: `docs/project-management/yi-agent-tui.md:18-48`
- Modify: `docs/project-management/README.md:8-20`

- [ ] **Step 1: Write the failing narrow-display-formula regression test**

```rust
#[test]
fn display_math_wraps_at_the_requested_terminal_width() {
    let lines = render_markdown("$$\\alpha + \\beta + \\gamma + \\delta$$", 8);
    assert!(lines.iter().all(|line| {
        UnicodeWidthStr::width(spans_text(line).as_str()) <= 8
    }));
}
```

- [ ] **Step 2: Run it to verify the current display-math branch fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests::display_math_wraps_at_the_requested_terminal_width -- --nocapture`

Expected: FAIL if display math is emitted as a raw unwrapped `Line`.

- [ ] **Step 3: Route display formulas through the existing wrapper**

Replace raw display-line insertion with a helper that pushes the cyan formula
as `current_spans` and calls `flush_line`, then starts a fresh empty current
line. This applies existing Unicode-width wrapping to display formulas while
keeping them isolated from surrounding prose.

- [ ] **Step 4: Update progress documentation**

Add this completed feature to `docs/project-management/yi-agent-tui.md`:

```markdown
- [x] LaTeX 公式渲染 — `tui/markdown.rs::render_tex_to_unicode` 支持 `$...$`、`$$...$$`、`\\(...\\)`、`\\[...\\]`; 验证：`cargo test -p yi-agent tui::markdown::tests`
```

Change the TUI index row in `docs/project-management/README.md` to:

```markdown
| yi-agent-tui | 15 / 16 | [详情](./yi-agent-tui.md) |
```

- [ ] **Step 5: Format and run final verification**

Run:

```bash
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent tui::markdown::tests -- --nocapture
just fmt-check
```

Expected: formatting succeeds and every Markdown renderer test passes.

- [ ] **Step 6: Commit the final regression coverage and documentation**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/markdown.rs docs/project-management/yi-agent-tui.md docs/project-management/README.md
git commit -m "test(tui): cover LaTeX formula rendering"
```

### Task 5: Review and integrate safely

**Files:**
- Verify: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs`
- Verify: `docs/project-management/yi-agent-tui.md`
- Verify: `docs/project-management/README.md`

- [ ] **Step 1: Inspect the final branch diff**

Run: `git diff main...HEAD --check && git diff --stat main...HEAD`

Expected: no whitespace errors and only the documented TUI renderer and project
tracking files changed beyond the already committed design and plan documents.

- [ ] **Step 2: Run the final targeted verification once more**

Run: `cd yi-agent-rs && cargo test -p yi-agent tui::markdown::tests -- --nocapture && just fmt-check`

Expected: PASS.

- [ ] **Step 3: Merge only after review**

From the main worktree, after confirming the branch is clean and verification
passed:

```bash
git merge --no-ff feat/tui-latex-rendering
git worktree remove .worktrees/feat-tui-latex-rendering
git branch -d feat/tui-latex-rendering
```
