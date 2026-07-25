# TUI History Redesign Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the reedline+InlineRenderer TUI with a ratatui full-screen layout featuring structured HistoryCell model, collapsible tool calls (Ctrl+O), markdown rendering, and scrollable history.

**Architecture:** New `tui/` module under `crates/yi-agent/src/` with ratatui managing a full-screen split: scrollable history area (flex:1) on top, fixed 1-line input area at bottom. History is stored as `Vec<HistoryCell>` with per-cell fold state. Input is a self-implemented line editor (no reedline). The existing `Renderer` trait is kept; a new `TuiRenderer` implements it.

**Tech Stack:** ratatui 0.29, crossterm 0.28 (existing), pulldown-cmark 0.12, syntect 5 (optional feature), tokio 1 (existing).

**Design doc:** `docs/plans/2026-07-25-tui-history-redesign.md`

---

## Task 0: Add Workspace Dependencies

**Files:**
- Modify: `yi-agent-rs/Cargo.toml` (workspace deps)
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`

**Step 1: Add ratatui and pulldown-cmark to workspace deps**

In `yi-agent-rs/Cargo.toml`, add to `[workspace.dependencies]`:

```toml
ratatui = { version = "0.29", default-features = false, features = ["crossterm"] }
pulldown-cmark = { version = "0.12", default-features = false }
syntect = { version = "5", default-features = false, features = ["default-fancy"], optional = true }
```

**Step 2: Add deps to yi-agent crate**

In `yi-agent-rs/crates/yi-agent/Cargo.toml`, add to `[dependencies]`:

```toml
ratatui = { workspace = true }
pulldown-cmark = { workspace = true }
syntect = { workspace = true, optional = true }
```

Add feature at bottom of file:

```toml
[features]
default = []
syntax-highlight = ["dep:syntect"]
```

**Step 3: Verify it compiles**

Run: `cargo check -p yi-agent 2>&1 | tail -5`
Expected: `Finished` with no errors (warnings ok).

**Step 4: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent/Cargo.toml
git commit -m "build: add ratatui, pulldown-cmark, syntect deps for new TUI"
```

---

## Task 1: HistoryCell Types

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`
- Create: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs` (in-module tests)

**Step 1: Create tui module stub**

Create `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`:

```rust
//! ratatui-based TUI with structured history cells.

pub mod cell;
```

Add `mod tui;` to `yi-agent-rs/crates/yi-agent/src/main.rs` (after `mod app;` line).

**Step 2: Write failing tests for HistoryCell**

Create `yi-agent-rs/crates/yi-agent/src/tui/cell.rs`:

```rust
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// One unit of conversation history displayed in the history area.
#[derive(Debug, Clone)]
pub enum HistoryCell {
    /// User's input message. Always expanded.
    UserMessage { text: String },
    /// Assistant's markdown response. Always expanded.
    AssistantMessage {
        markdown: String,
        rendered_lines: Vec<Line<'static>>,
    },
    /// Tool call. Foldable (default folded).
    ToolCall {
        id: String,
        name: String,
        input: Value,
        state: CallState,
        expanded: bool,
    },
    /// Tool result. Foldable (default folded).
    ToolResult {
        id: String,
        result_text: String,
        is_error: bool,
        expanded: bool,
    },
    /// Full-width dim separator line between turns.
    Separator { label: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Running,
    Success,
    Failed,
}

impl HistoryCell {
    /// Number of terminal lines this cell occupies at the given width.
    pub fn line_count(&self, width: u16) -> usize {
        self.lines(width).len()
    }

    /// Render this cell into ratatui Lines for display at the given width.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Self::UserMessage { text } => render_user_message(text, width),
            Self::AssistantMessage { rendered_lines, .. } => rendered_lines.clone(),
            Self::ToolCall { name, input, state, expanded, .. } => {
                render_tool_call(name, input, *state, *expanded, width)
            }
            Self::ToolResult { id: _, result_text, is_error, expanded } => {
                render_tool_result(result_text, *is_error, *expanded, width)
            }
            Self::Separator { label } => vec![render_separator(label.as_deref(), width)],
        }
    }

    /// Whether this cell is foldable (can be toggled with Ctrl+O).
    pub fn is_foldable(&self) -> bool {
        matches!(self, Self::ToolCall { .. } | Self::ToolResult { .. })
    }

    /// Toggle the expanded state. No-op for non-foldable cells.
    pub fn toggle_fold(&mut self) {
        match self {
            Self::ToolCall { expanded, .. } => *expanded = !*expanded,
            Self::ToolResult { expanded, .. } => *expanded = !*expanded,
            _ => {}
        }
    }
}

// --- Renderers (minimal stubs, tests come first) ---

fn render_user_message(text: &str, width: u16) -> Vec<Line<'static>> {
    let prefix = Span::styled("> ", Style::new().add_modifier(Modifier::BOLD | Modifier::DIM));
    wrap_with_prefix(text, width, prefix, "  ")
}

fn render_tool_call(name: &str, input: &Value, state: CallState, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let (bullet, bullet_color) = match state {
        CallState::Running => ("●", Color::Yellow),
        CallState::Success => ("●", Color::Green),
        CallState::Failed => ("●", Color::Red),
    };
    let input_summary = summarize_json(input, 60);
    let header = format!("{bullet} {name}({input_summary})");
    let mut lines = vec![Line::from(vec![
        Span::styled(bullet, Style::new().fg(bullet_color).add_modifier(Modifier::BOLD)),
        Span::raw(format!(" {name}({input_summary})")),
    ])];
    if expanded {
        let full = format!("{input:#}");
        for line in full.lines() {
            lines.push(Line::from(format!("  └ {line}")).style(Style::new().fg(Color::DarkGray)));
        }
    }
    lines
}

fn render_tool_result(text: &str, is_error: bool, expanded: bool, width: u16) -> Vec<Line<'static>> {
    let arrow_color = if is_error { Color::Red } else { Color::Green };
    let summary = truncate(text, 80);
    let mut lines = vec![Line::from(vec![
        Span::styled("  └ ", Style::new().fg(arrow_color).add_modifier(Modifier::DIM)),
        Span::styled(summary, Style::new().add_modifier(Modifier::DIM)),
    ])];
    if expanded {
        for line in text.lines() {
            lines.push(Line::from(format!("    {line}")).style(Style::new().add_modifier(Modifier::DIM)));
        }
    }
    lines
}

fn render_separator(label: Option<&str>, width: u16) -> Line<'static> {
    let w = width as usize;
    match label {
        None => Line::from("─".repeat(w)).style(Style::new().add_modifier(Modifier::DIM)),
        Some(l) => {
            let prefix = format!("─ {l} ");
            let remaining = w.saturating_sub(prefix.chars().count());
            Line::from(format!("{prefix}{}", "─".repeat(remaining)))
                .style(Style::new().add_modifier(Modifier::DIM))
        }
    }
}

// --- Helpers ---

fn summarize_json(v: &Value, max_len: usize) -> String {
    let s = v.to_string();
    truncate(&s, max_len)
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}

fn wrap_with_prefix(text: &str, width: u16, first_prefix: Span<'static>, cont_prefix: &str) -> Vec<Line<'static>> {
    // Simple word-wrap; detailed tests in Task 6
    let max_w = width as usize;
    let mut lines = vec![];
    let mut current = String::new();
    for word in text.split_whitespace() {
        let prefix_len = if lines.is_empty() { 2 } else { cont_prefix.len() }; // "> " = 2
        if current.is_empty() {
            current = word.to_string();
        } else if current.len() + 1 + word.len() + prefix_len <= max_w {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines.into_iter()
        .enumerate()
        .map(|(i, text)| {
            if i == 0 {
                Line::from(vec![first_prefix.clone(), Span::raw(text)])
            } else {
                Line::from(format!("{cont_prefix}{text}"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_renders_with_prefix() {
        let cell = HistoryCell::UserMessage { text: "hello".into() };
        let lines = cell.lines(80);
        assert_eq!(lines.len(), 1);
        let spans: Vec<String> = lines[0].spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(spans[0], "> ");
        assert_eq!(spans[1], "hello");
    }

    #[test]
    fn tool_call_default_folded_shows_summary_only() {
        let cell = HistoryCell::ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "/tmp/x"}),
            state: CallState::Success,
            expanded: false,
        };
        let lines = cell.lines(80);
        assert_eq!(lines.len(), 1, "folded tool call should be 1 line, got {}", lines.len());
    }

    #[test]
    fn tool_call_expanded_shows_more_lines() {
        let cell = HistoryCell::ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "/tmp/x"}),
            state: CallState::Success,
            expanded: true,
        };
        let lines = cell.lines(80);
        assert!(lines.len() > 1, "expanded tool call should have >1 line");
    }

    #[test]
    fn toggle_fold_switches_expanded() {
        let mut cell = HistoryCell::ToolCall {
            id: "1".into(), name: "t".into(), input: serde_json::json!({}),
            state: CallState::Success, expanded: false,
        };
        assert!(!is_expanded(&cell));
        cell.toggle_fold();
        assert!(is_expanded(&cell));
        cell.toggle_fold();
        assert!(!is_expanded(&cell));
    }

    fn is_expanded(c: &HistoryCell) -> bool {
        match c {
            HistoryCell::ToolCall { expanded, .. } => *expanded,
            HistoryCell::ToolResult { expanded, .. } => *expanded,
            _ => false,
        }
    }

    #[test]
    fn separator_no_label_is_all_dashes() {
        let cell = HistoryCell::Separator { label: None };
        let lines = cell.lines(40);
        assert_eq!(lines.len(), 1);
        let s: String = lines[0].spans.iter().map(|s| s.content.clone()).collect();
        assert_eq!(s.chars().count(), 40);
        assert!(s.chars().all(|c| c == '─'));
    }

    #[test]
    fn separator_with_label_has_dashes_around() {
        let cell = HistoryCell::Separator { label: Some("Worked for 2m".into()) };
        let lines = cell.lines(40);
        assert_eq!(lines.len(), 1);
        let s: String = lines[0].spans.iter().map(|s| s.content.clone()).collect();
        assert!(s.contains("Worked for 2m"));
        assert!(s.starts_with("─ "));
        assert!(s.ends_with("─") || s.ends_with("──"));
    }

    #[test]
    fn user_message_is_not_foldable() {
        let cell = HistoryCell::UserMessage { text: "x".into() };
        assert!(!cell.is_foldable());
    }

    #[test]
    fn tool_call_is_foldable() {
        let cell = HistoryCell::ToolCall {
            id: "1".into(), name: "t".into(), input: serde_json::json!({}),
            state: CallState::Success, expanded: false,
        };
        assert!(cell.is_foldable());
    }
}
```

**Step 3: Run tests to verify they pass**

Run: `cargo test -p yi-agent tui::cell::tests 2>&1 | tail -20`
Expected: All tests PASS.

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/ yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(tui): add HistoryCell types with render and fold logic"
```

---

## Task 2: InputLine Self-Implemented Editor

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/input.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` (add `pub mod input;`)

**Step 1: Write failing tests for InputLine**

Create `yi-agent-rs/crates/yi-agent/src/tui/input.rs`:

```rust
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Self-implemented single-line input editor (no reedline).
#[derive(Debug, Clone)]
pub struct InputLine {
    pub buffer: String,
    /// Byte offset of cursor within buffer.
    pub cursor: usize,
    pub history: Vec<String>,
    /// None = editing current line; Some(i) = browsing history[i].
    pub history_idx: Option<usize>,
    /// Saved current line when browsing history.
    saved_current: String,
}

impl Default for InputLine {
    fn default() -> Self {
        Self::new()
    }
}

impl InputLine {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            saved_current: String::new(),
        }
    }

    /// Returns true if the key was consumed by the editor.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        match key.code {
            KeyCode::Char(c) if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT => {
                self.insert_char(c);
                InputAction::Consumed
            }
            KeyCode::Backspace => { self.backspace(); InputAction::Consumed }
            KeyCode::Delete => { self.delete(); InputAction::Consumed }
            KeyCode::Left => { self.move_left(); InputAction::Consumed }
            KeyCode::Right => { self.move_right(); InputAction::Consumed }
            KeyCode::Home | KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                self.cursor = 0; InputAction::Consumed
            }
            KeyCode::End | KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                self.cursor = self.buffer.len(); InputAction::Consumed
            }
            KeyCode::Up => { self.history_prev(); InputAction::Consumed }
            KeyCode::Down => { self.history_next(); InputAction::Consumed }
            KeyCode::Enter => {
                if !self.buffer.trim().is_empty() {
                    InputAction::Submit
                } else {
                    InputAction::Consumed
                }
            }
            _ => InputAction::NotHandled,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let byte_idx = self.cursor;
        self.buffer.insert(byte_idx, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 { return; }
        // Find previous char boundary
        let prev = self.buffer[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.buffer.len() { return; }
        let next = self.buffer[self.cursor..].char_indices().nth(1).map(|(i, _)| self.cursor + i).unwrap_or(self.buffer.len());
        self.buffer.replace_range(self.cursor..next, "");
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 { return; }
        let prev = self.buffer[..self.cursor].char_indices().last().map(|(i, _)| i).unwrap_or(0);
        self.cursor = prev;
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() { return; }
        let next = self.buffer[self.cursor..].char_indices().nth(1).map(|(i, _)| self.cursor + i).unwrap_or(self.buffer.len());
        self.cursor = next;
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() { return; }
        if self.history_idx.is_none() {
            self.saved_current = self.buffer.clone();
            self.history_idx = Some(self.history.len() - 1);
        } else if let Some(i) = self.history_idx {
            if i > 0 {
                self.history_idx = Some(i - 1);
            } else {
                return;
            }
        }
        if let Some(i) = self.history_idx {
            self.buffer = self.history[i].clone();
            self.cursor = self.buffer.len();
        }
    }

    pub fn history_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) => {
                if i + 1 >= self.history.len() {
                    self.history_idx = None;
                    self.buffer = std::mem::take(&mut self.saved_current);
                } else {
                    self.history_idx = Some(i + 1);
                    self.buffer = self.history[i + 1].clone();
                }
                self.cursor = self.buffer.len();
            }
        }
    }

    /// Consume the current buffer as submitted text, push to history.
    pub fn take_submitted(&mut self) -> String {
        let text = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_idx = None;
        self.saved_current.clear();
        if !text.trim().is_empty() {
            self.history.push(text.clone());
        }
        text
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.saved_current.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    /// Key was handled by the editor.
    Consumed,
    /// Enter was pressed with non-empty input.
    Submit,
    /// Key was not recognized, caller may handle.
    NotHandled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn insert_and_backspace_basic() {
        let mut inp = InputLine::new();
        inp.insert_char('h');
        inp.insert_char('i');
        assert_eq!(inp.buffer, "hi");
        assert_eq!(inp.cursor, 2);
        inp.backspace();
        assert_eq!(inp.buffer, "h");
        assert_eq!(inp.cursor, 1);
    }

    #[test]
    fn cursor_left_right() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 3;
        inp.move_left();
        assert_eq!(inp.cursor, 2);
        inp.move_left();
        inp.move_left();
        assert_eq!(inp.cursor, 0);
        inp.move_left(); // at start, no-op
        assert_eq!(inp.cursor, 0);
        inp.move_right();
        assert_eq!(inp.cursor, 1);
    }

    #[test]
    fn insert_in_middle() {
        let mut inp = InputLine::new();
        inp.buffer = "ac".into();
        inp.cursor = 1;
        inp.insert_char('b');
        assert_eq!(inp.buffer, "abc");
        assert_eq!(inp.cursor, 2);
    }

    #[test]
    fn delete_key() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 1;
        inp.delete();
        assert_eq!(inp.buffer, "ac");
        assert_eq!(inp.cursor, 1);
    }

    #[test]
    fn utf8_boundary_backspace() {
        let mut inp = InputLine::new();
        inp.buffer = "你好".into();
        inp.cursor = "你好".len(); // 6 bytes
        inp.backspace();
        assert_eq!(inp.buffer, "你");
        assert_eq!(inp.cursor, 3);
    }

    #[test]
    fn history_navigation() {
        let mut inp = InputLine::new();
        inp.history = vec!["first".into(), "second".into()];
        inp.history_prev();
        assert_eq!(inp.buffer, "second");
        inp.history_prev();
        assert_eq!(inp.buffer, "first");
        inp.history_prev(); // at oldest, no-op
        assert_eq!(inp.buffer, "first");
        inp.history_next();
        assert_eq!(inp.buffer, "second");
        inp.history_next(); // back to current
        assert_eq!(inp.history_idx, None);
    }

    #[test]
    fn history_saves_current() {
        let mut inp = InputLine::new();
        inp.history = vec!["old".into()];
        inp.insert_char('n');
        inp.history_prev();
        assert_eq!(inp.buffer, "old");
        inp.history_next();
        assert_eq!(inp.buffer, "n");
    }

    #[test]
    fn take_submitted_pushes_history() {
        let mut inp = InputLine::new();
        inp.buffer = "hello".into();
        let text = inp.take_submitted();
        assert_eq!(text, "hello");
        assert_eq!(inp.history, vec!["hello"]);
        assert!(inp.is_empty());
    }

    #[test]
    fn take_submitted_ignores_empty() {
        let mut inp = InputLine::new();
        inp.buffer = "   ".into();
        let text = inp.take_submitted();
        assert_eq!(text, "   ");
        assert!(inp.history.is_empty(), "whitespace-only should not be saved");
    }

    #[test]
    fn handle_key_enter_submits() {
        let mut inp = InputLine::new();
        inp.buffer = "hi".into();
        let action = inp.handle_key(key(KeyCode::Enter));
        assert_eq!(action, InputAction::Submit);
    }

    #[test]
    fn handle_key_enter_empty_does_not_submit() {
        let mut inp = InputLine::new();
        let action = inp.handle_key(key(KeyCode::Enter));
        assert_eq!(action, InputAction::Consumed);
    }

    #[test]
    fn handle_key_ctrl_a_home() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 3;
        inp.handle_key(key_ctrl('a'));
        assert_eq!(inp.cursor, 0);
    }

    #[test]
    fn handle_key_ctrl_e_end() {
        let mut inp = InputLine::new();
        inp.buffer = "abc".into();
        inp.cursor = 0;
        inp.handle_key(key_ctrl('e'));
        assert_eq!(inp.cursor, 3);
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p yi-agent tui::input::tests 2>&1 | tail -20`
Expected: All 12 tests PASS.

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/
git commit -m "feat(tui): add InputLine self-implemented line editor"
```

---

## Task 3: Markdown Renderer

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` (add `pub mod markdown;`)

**Step 1: Write the markdown renderer with tests**

Create `yi-agent-rs/crates/yi-agent/src/tui/markdown.rs`:

```rust
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render a markdown string into ratatui Lines, wrapped at `width`.
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(src, opts);
    let mut builder = LineBuilder::new(width);
    for event in parser {
        builder.handle_event(event);
    }
    builder.finish()
}

struct LineBuilder {
    width: u16,
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    current_style: Style,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
}

impl LineBuilder {
    fn new(width: u16) -> Self {
        Self {
            width,
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_style: Style::new(),
            in_code_block: false,
            code_block_lang: None,
            code_block_buffer: String::new(),
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_block_buffer.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                self.push_span(Span::styled(code.to_string(), Style::new().fg(Color::Cyan)));
            }
            Event::SoftBreak | Event::HardBreak => {
                self.flush_line();
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.current_style = match level {
                    pulldown_cmark::HeadingLevel::H1 => Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    pulldown_cmark::HeadingLevel::H2 => Style::new().add_modifier(Modifier::BOLD),
                    _ => Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                };
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => Some(lang.into_string()),
                    _ => None,
                };
                self.code_block_buffer.clear();
            }
            Tag::Emphasis => {
                self.current_style = self.current_style.add_modifier(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.current_style = self.current_style.add_modifier(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.current_style = self.current_style.add_modifier(Modifier::CROSSED_OUT);
            }
            Tag::BlockQuote => {
                self.current_style = self.current_style.fg(Color::Green);
            }
            Tag::Link { dest_url, .. } => {
                self.push_span(Span::styled(dest_url.to_string(), Style::new().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)));
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) | TagEnd::Paragraph => {
                self.flush_line();
                self.current_style = Style::new();
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.in_code_block = false;
                self.code_block_lang = None;
                self.code_block_buffer.clear();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::BlockQuote => {
                self.current_style = Style::new();
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        self.push_span(Span::styled(text.to_string(), self.current_style));
    }

    fn push_span(&mut self, span: Span<'static>) {
        self.current_spans.push(span);
    }

    fn flush_line(&mut self) {
        if self.current_spans.is_empty() {
            self.lines.push(Line::raw(""));
        } else {
            let line = Line::from(std::mem::take(&mut self.current_spans));
            self.lines.push(self.wrap_line(line));
        }
    }

    fn wrap_line(&self, line: Line<'static>) -> Line<'static> {
        // Simple wrap: if line width exceeds, just return as-is (YAGNI for now)
        // Real wrapping handled by caller using textwrap or manual
        line
    }

    fn flush_code_block(&mut self) {
        let lang = self.code_block_lang.as_deref().unwrap_or("");
        let _ = lang; // placeholder for syntax highlighting (Task 10)
        for code_line in self.code_block_buffer.lines() {
            self.lines.push(Line::styled(
                format!("  {code_line}"),
                Style::new().fg(Color::Yellow),
            ));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.clone()).collect()
    }

    #[test]
    fn plain_text_renders_as_single_line() {
        let lines = render_markdown("hello world", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0]), "hello world");
    }

    #[test]
    fn h1_is_bold_underlined() {
        let lines = render_markdown("# Title", 80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans[0].style.add_modifier == Some(Modifier::BOLD | Modifier::UNDERLINED));
    }

    #[test]
    fn inline_code_is_cyan() {
        let lines = render_markdown("use `foo` here", 80);
        assert_eq!(lines.len(), 1);
        let code_span = &lines[0].spans[1];
        assert_eq!(code_span.content, "foo");
        assert_eq!(code_span.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn code_block_renders_as_separate_lines() {
        let src = "```rust\nfn main() {}\n```\n";
        let lines = render_markdown(src, 80);
        // Should contain a line with "fn main() {}"
        let has_code = lines.iter().any(|l| spans_text(l).contains("fn main()"));
        assert!(has_code, "expected code block content, got: {:?}", lines.iter().map(spans_text).collect::<Vec<_>>());
    }

    #[test]
    fn bold_and_italic_toggle() {
        let lines = render_markdown("**bold** and *italic*", 80);
        assert_eq!(lines.len(), 1);
        let text = spans_text(&lines[0]);
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn paragraph_break_creates_new_line() {
        let lines = render_markdown("para one\n\npara two", 80);
        assert_eq!(lines.len(), 2);
        assert_eq!(spans_text(&lines[0]), "para one");
        assert_eq!(spans_text(&lines[1]), "para two");
    }

    #[test]
    fn empty_string_returns_empty() {
        let lines = render_markdown("", 80);
        assert!(lines.is_empty());
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p yi-agent tui::markdown::tests 2>&1 | tail -20`
Expected: All 7 tests PASS.

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/
git commit -m "feat(tui): add markdown renderer with pulldown-cmark"
```

---

## Task 4: History Area Widget

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` (add `pub mod history;`)

**Step 1: Write the history widget with tests**

Create `yi-agent-rs/crates/yi-agent/src/tui/history.rs`:

```rust
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

use super::cell::HistoryCell;

/// State for the scrollable history area.
pub struct HistoryState {
    pub cells: Vec<HistoryCell>,
    /// Index of the currently selected cell (for Ctrl+O folding).
    pub selected: Option<usize>,
    /// Vertical scroll offset in lines (0 = bottom).
    pub scroll_offset: usize,
}

impl HistoryState {
    pub fn new() -> Self {
        Self { cells: Vec::new(), selected: None, scroll_offset: 0 }
    }

    /// Push a new cell and auto-scroll to bottom.
    pub fn push(&mut self, cell: HistoryCell) {
        self.cells.push(cell);
        self.scroll_offset = 0; // reset to bottom
    }

    /// Total number of display lines across all cells at given width.
    pub fn total_lines(&self, width: u16) -> usize {
        self.cells.iter().map(|c| c.line_count(width)).sum()
    }

    /// Move selection up by one cell.
    pub fn select_up(&mut self) {
        match self.selected {
            None => self.selected = Some(self.cells.len().saturating_sub(1).saturating_sub(1)),
            Some(0) => {}
            Some(i) => self.selected = Some(i - 1),
        }
    }

    /// Move selection down by one cell.
    pub fn select_down(&mut self) {
        match self.selected {
            None => {}
            Some(i) if i + 1 >= self.cells.len() => self.selected = None,
            Some(i) => self.selected = Some(i + 1),
        }
    }

    /// Toggle fold on selected cell.
    pub fn toggle_fold_selected(&mut self) {
        if let Some(i) = self.selected {
            if let Some(cell) = self.cells.get_mut(i) {
                cell.toggle_fold();
            }
        }
    }

    /// Scroll up by `n` lines.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n);
    }

    /// Scroll down by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }
}

impl Default for HistoryState {
    fn default() -> Self { Self::new() }
}

/// Ratatui widget that renders the history area.
pub struct HistoryView<'a> {
    pub state: &'a HistoryState,
    pub width: u16,
}

impl<'a> Widget for HistoryView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Collect all lines with cell index for selection highlight
        let mut all_lines: Vec<(usize, ratatui::text::Line<'static>)> = Vec::new();
        for (i, cell) in self.state.cells.iter().enumerate() {
            for line in cell.lines(self.width) {
                all_lines.push((i, line));
            }
        }

        let visible_height = area.height as usize;
        let total = all_lines.len();
        let start = total.saturating_sub(visible_height + self.state.scroll_offset);
        let end = total.saturating_sub(self.state.scroll_offset).min(total);
        let visible = &all_lines[start..end];

        for (row, (cell_idx, line)) in visible.iter().enumerate() {
            let y = area.y + row as u16;
            let x = area.x;
            let is_selected = self.state.selected == Some(*cell_idx);
            let mut line = line.clone();
            if is_selected {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            line.render(Rect { x, y, width: area.width, height: 1 }, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::cell::HistoryCell;

    #[test]
    fn push_resets_scroll_to_bottom() {
        let mut s = HistoryState::new();
        s.scroll_up(5);
        s.push(HistoryCell::UserMessage { text: "x".into() });
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn select_up_from_none_selects_second_to_last() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "a".into() });
        s.push(HistoryCell::UserMessage { text: "b".into() });
        s.push(HistoryCell::UserMessage { text: "c".into() });
        s.select_up();
        // From None, select the cell before the last
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn select_down_past_last_clears_selection() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "a".into() });
        s.selected = Some(0);
        s.select_down();
        assert_eq!(s.selected, None);
    }

    #[test]
    fn toggle_fold_selected_toggles() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::ToolCall {
            id: "1".into(), name: "t".into(),
            input: serde_json::json!({}),
            state: crate::tui::cell::CallState::Success,
            expanded: false,
        });
        s.selected = Some(0);
        s.toggle_fold_selected();
        match &s.cells[0] {
            HistoryCell::ToolCall { expanded, .. } => assert!(*expanded),
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn total_lines_sums_all_cells() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "hello".into() });     // 1 line
        s.push(HistoryCell::Separator { label: None });                 // 1 line
        assert_eq!(s.total_lines(80), 2);
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p yi-agent tui::history::tests 2>&1 | tail -20`
Expected: All 5 tests PASS.

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/
git commit -m "feat(tui): add HistoryView widget with selection and scroll"
```

---

## Task 5: AgentEvent to HistoryCell Conversion

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/cell.rs` (add `from_event` methods)
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs` (add `push_event` method)

**Step 1: Write failing tests for event conversion**

Add to `tui/cell.rs` test module:

```rust
    #[test]
    fn assistant_text_creates_new_cell_if_none_in_progress() {
        let cell = HistoryCell::from_assistant_text("hello", 80);
        match cell {
            HistoryCell::AssistantMessage { markdown, .. } => {
                assert_eq!(markdown, "hello");
            }
            _ => panic!("expected AssistantMessage"),
        }
    }
```

Add to `tui/history.rs` test module:

```rust
    use yi_agent_core::AgentEvent;
    use yi_agent_core::DoneReason;

    #[test]
    fn push_event_assistant_text_appends_to_existing() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("hello".into()), 80);
        s.push_event(AgentEvent::AssistantText(" world".into()), 80);
        assert_eq!(s.cells.len(), 1, "two text chunks should merge into 1 cell");
        match &s.cells[0] {
            HistoryCell::AssistantMessage { markdown, .. } => assert_eq!(markdown, "hello world"),
            _ => panic!("expected AssistantMessage"),
        }
    }

    #[test]
    fn push_event_tool_call_creates_separate_cell() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("text".into()), 80);
        s.push_event(AgentEvent::ToolCall {
            id: "1".into(), name: "read".into(), input: serde_json::json!({}),
        }, 80);
        assert_eq!(s.cells.len(), 2);
    }

    #[test]
    fn push_event_done_endturn_adds_separator() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::Done { reason: DoneReason::EndTurn }, 80);
        assert_eq!(s.cells.len(), 1);
        assert!(matches!(s.cells[0], HistoryCell::Separator { .. }));
    }

    #[test]
    fn push_event_tool_result_updates_tool_call_state() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::ToolCall {
            id: "1".into(), name: "read".into(), input: serde_json::json!({}),
        }, 80);
        s.push_event(AgentEvent::ToolResult {
            id: "1".into(),
            result: yi_agent_core::ToolResult {
                content: vec![yi_agent_core::ContentBlock::Text("ok".into())],
                is_error: false,
            },
        }, 80);
        // ToolCall should be Success now, and a ToolResult cell added
        assert!(matches!(
            &s.cells[0],
            HistoryCell::ToolCall { state: crate::tui::cell::CallState::Success, .. }
        ));
        assert!(matches!(s.cells.get(1), Some(HistoryCell::ToolResult { .. })));
    }
```

**Step 2: Implement the conversion logic**

Add to `tui/cell.rs` (after the `impl HistoryCell` block):

```rust
impl HistoryCell {
    /// Create an AssistantMessage cell from a text chunk.
    pub fn from_assistant_text(text: &str, width: u16) -> Self {
        let rendered = super::markdown::render_markdown(text, width);
        Self::AssistantMessage {
            markdown: text.to_string(),
            rendered_lines: rendered,
        }
    }

    /// Append more text to an existing AssistantMessage, re-rendering.
    pub fn append_assistant_text(&mut self, more: &str, width: u16) {
        if let Self::AssistantMessage { markdown, rendered_lines } = self {
            markdown.push_str(more);
            *rendered_lines = super::markdown::render_markdown(markdown, width);
        }
    }
}
```

Add to `tui/history.rs` (after the `impl HistoryState` block):

```rust
use yi_agent_core::{AgentEvent, DoneReason, ToolResult};

impl HistoryState {
    /// Process an AgentEvent and update the cell list accordingly.
    pub fn push_event(&mut self, event: AgentEvent, width: u16) {
        match event {
            AgentEvent::Start => {}
            AgentEvent::AssistantText(text) => {
                // Merge into previous AssistantMessage if last cell is one
                match self.cells.last_mut() {
                    Some(HistoryCell::AssistantMessage { .. }) => {
                        self.cells.last_mut().unwrap().append_assistant_text(&text, width);
                    }
                    _ => {
                        self.push(HistoryCell::from_assistant_text(&text, width));
                    }
                }
            }
            AgentEvent::ToolCall { id, name, input } => {
                self.push(HistoryCell::ToolCall {
                    id, name, input,
                    state: super::cell::CallState::Running,
                    expanded: false,
                });
            }
            AgentEvent::ToolResult { id, result } => {
                // Mark the matching ToolCall as Success/Failed
                let is_error = result.is_error;
                let result_text = result.content.iter()
                    .filter_map(|b| match b {
                        yi_agent_core::ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                for cell in self.cells.iter_mut() {
                    if let HistoryCell::ToolCall { id: cid, state, .. } = cell {
                        if cid == &id {
                            *state = if is_error {
                                super::cell::CallState::Failed
                            } else {
                                super::cell::CallState::Success
                            };
                            break;
                        }
                    }
                }
                self.push(HistoryCell::ToolResult {
                    id, result_text, is_error, expanded: false,
                });
            }
            AgentEvent::Done { reason } => {
                match reason {
                    DoneReason::EndTurn => {
                        self.push(HistoryCell::Separator { label: None });
                    }
                    DoneReason::MaxTurns => {
                        self.push(HistoryCell::Separator { label: Some("Max turns".into()) });
                    }
                }
            }
            AgentEvent::Usage(_) => {}
            AgentEvent::Cancelled => {
                self.push(HistoryCell::Separator { label: Some("Interrupted".into()) });
            }
            AgentEvent::Error(err) => {
                self.push(HistoryCell::Separator { label: Some(format!("Error: {err}")) });
            }
        }
    }
}
```

Add `pub mod markdown;` to `tui/mod.rs` if not already present.

**Step 3: Run tests to verify they pass**

Run: `cargo test -p yi-agent tui:: 2>&1 | tail -30`
Expected: All tests PASS.

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/
git commit -m "feat(tui): convert AgentEvent to HistoryCell with state tracking"
```

---

## Task 6: Main TUI App (Event Loop + Layout)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` (add `pub mod app;`)

**Step 1: Write the TUI app**

Create `yi-agent-rs/crates/yi-agent/src/tui/app.rs`:

```rust
use std::io::stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use yi_agent_core::AgentEvent;

use super::cell::HistoryCell;
use super::history::{HistoryState, HistoryView};
use super::input::{InputAction, InputLine};

/// Run the ratatui TUI. Returns when the user quits or the agent stream ends.
pub fn run_tui(
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: tokio::sync::mpsc::Sender<String>,
    mut interrupt_rx: tokio::sync::mpsc::Receiver<()>,
) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut history = HistoryState::new();
    let mut input = InputLine::new();

    loop {
        // Drain all pending agent events before drawing
        while let Ok(event) = agent_rx.try_recv() {
            history.push_event(event, terminal.width()?);
        }

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),    // history
                    Constraint::Length(1), // blank gap
                    Constraint::Length(1), // input
                ])
                .split(f.area());

            // History area
            let history_view = HistoryView {
                state: &history,
                width: chunks[0].width,
            };
            f.render_widget(history_view, chunks[0]);

            // Input area (gray bg, "> " prefix)
            let input_line = build_input_line(&input);
            f.render_widget(input_line, chunks[2]);
        })?;

        // Poll for events with a small timeout so we can drain agent events
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match handle_key(key, &mut input, &mut history) {
                    KeyOutcome::Submit(text) => {
                        let _ = input_tx.blocking_send(text);
                    }
                    KeyOutcome::Quit => break,
                    KeyOutcome::Interrupt => {
                        let _ = interrupt_rx.try_recv();
                    }
                    KeyOutcome::None => {}
                }
            }
        }

        // Check if agent stream is done
        if agent_rx.is_empty() && interrupt_rx.try_recv().is_err() {
            // Keep running until user quits; agent may send more later
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

enum KeyOutcome {
    None,
    Submit(String),
    Quit,
    Interrupt,
}

fn handle_key(key: KeyEvent, input: &mut InputLine, history: &mut HistoryState) -> KeyOutcome {
    // Global keys first
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => return KeyOutcome::Interrupt,
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => return KeyOutcome::Quit,
        KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
            history.toggle_fold_selected();
            return KeyOutcome::None;
        }
        KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
            history.select_up();
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
            history.select_down();
            return KeyOutcome::None;
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            history.scroll_up(10);
            return KeyOutcome::None;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            history.scroll_down(10);
            return KeyOutcome::None;
        }
        _ => {}
    }

    // Input handling
    match input.handle_key(key) {
        InputAction::Submit => KeyOutcome::Submit(input.take_submitted()),
        _ => KeyOutcome::None,
    }
}

fn build_input_line(input: &InputLine) -> Paragraph<'static> {
    let prefix = Span::styled("> ", Style::new().add_modifier(Modifier::BOLD | Modifier::DIM));
    let text = Span::raw(input.buffer.clone());
    let line = Line::from(vec![prefix, text]);
    Paragraph::new(line).style(Style::new().bg(Color::Indexed(240)))
}
```

**Step 2: Verify it compiles**

Run: `cargo check -p yi-agent 2>&1 | tail -10`
Expected: `Finished` with no errors.

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/
git commit -m "feat(tui): add main TUI event loop with ratatui layout"
```

---

## Task 7: Wire TUI into main.rs with --tui flag

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs` (add `--tui` flag)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (launch TUI when flag set)

**Step 1: Add --tui flag to CLI**

In `yi-agent-rs/crates/yi-agent/src/config.rs`, find the `Cli` struct and add:

```rust
    /// TUI mode: "inline" (default) or "ratatui"
    #[arg(long)]
    pub tui: Option<String>,
```

**Step 2: Wire into main.rs**

In `yi-agent-rs/crates/yi-agent/src/main.rs`, modify `run_agent()` to check the `tui` flag. After creating the Agent and before the current `App::run()` block, add a branch:

```rust
    // After: let printer = reedline::ExternalPrinter::default();
    // After: let renderer = InlineRenderer::with_printer(printer.sender());

    if cli.tui.as_deref() == Some("ratatui") {
        // New ratatui TUI path
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<yi_agent_core::AgentEvent>(256);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);

        // Spawn agent driver task
        let agent_handle = rt.spawn(async move {
            // Forward agent events to TUI
            // (simplified: real impl runs agent.run() and forwards events)
            while let Ok(text) = input_rx.recv().await {
                let stream = agent.run(text).await.unwrap();
                use futures::StreamExt;
                let mut stream = Box::pin(stream);
                while let Some(event) = stream.next().await {
                    if agent_tx.send(event).await.is_err() { break; }
                }
            }
        });

        rt.block_on(crate::tui::app::run_tui(agent_rx, input_tx, interrupt_rx))?;
        let _ = rt.spawn(async move { /* cleanup */ });
        return Ok(());
    }
```

Note: This is a simplified wiring. The actual integration needs to handle the agent lifecycle properly (interrupt, multiple turns). Refer to how `App::run()` in `app.rs` manages the `current_stream` and `UserCommand` flow.

**Step 3: Verify it compiles**

Run: `cargo check -p yi-agent 2>&1 | tail -10`
Expected: `Finished` with no errors.

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(tui): wire ratatui TUI behind --tui ratatui flag"
```

---

## Task 8: Integration Smoke Test

**Files:**
- Test: manual run

**Step 1: Build the binary**

Run: `cargo build -p yi-agent 2>&1 | tail -5`
Expected: `Finished` with no errors.

**Step 2: Run with ratatui TUI**

Run: `cargo run -p yi-agent -- --tui ratatui --api-key $ANTHROPIC_API_KEY --model claude-sonnet-4-20250514`
Expected: Full-screen TUI appears with history area and input line. Typing a message and pressing Enter should show user message with `> ` prefix, assistant response streaming in, tool calls folded with `●`.

**Step 3: Test Ctrl+O folding**

1. Send a message that triggers a tool call (e.g., "read the file README.md")
2. Press Shift+↑ to select the tool call cell
3. Press Ctrl+O to expand it
4. Press Ctrl+O again to fold it

**Step 4: Test scrolling**

1. Press Ctrl+U to scroll up
2. Press Ctrl+D to scroll down
3. Press Ctrl+Q to quit

**Step 5: Fix any issues found, then commit**

```bash
git add -A
git commit -m "fix(tui): integration smoke test fixes"
```

---

## Task 9: Remove Old InlineRenderer (After Validation)

**Files:**
- Delete: `yi-agent-rs/crates/yi-agent/src/render/inline.rs`
- Delete: `yi-agent-rs/crates/yi-agent/src/render/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (remove inline path)
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs` (remove reedline code)
- Modify: `yi-agent-rs/Cargo.toml` (remove reedline dep)

**Note:** Only do this after the ratatui TUI is validated and working well. This is the "Phase 3" from the design doc.

**Step 1: Remove reedline from workspace deps**

In `yi-agent-rs/Cargo.toml`, remove:
```toml
reedline = { version = "0.38", features = ["external_printer"] }
```

In `yi-agent-rs/crates/yi-agent/Cargo.toml`, remove `reedline` and `nu-ansi-term` from `[dependencies]`.

**Step 2: Delete render module**

```bash
rm yi-agent-rs/crates/yi-agent/src/render/inline.rs
rm yi-agent-rs/crates/yi-agent/src/render/mod.rs
rmdir yi-agent-rs/crates/yi-agent/src/render/
```

**Step 3: Remove reedline code from app.rs**

Remove `CodexPrompt`, `CodexHighlighter`, `PROMPT_BG`, `run_input_loop()`, and all reedline-related imports from `app.rs`. Simplify `App::run()` to drive the TUI directly.

**Step 4: Remove `--tui` flag default**

Make `ratatui` the default and remove the `--tui` flag (or keep it for backward compat with deprecation).

**Step 5: Verify build and tests**

Run: `cargo test -p yi-agent 2>&1 | tail -10`
Expected: All tests PASS.

**Step 6: Commit**

```bash
git add -A
git commit -m "refactor(tui): remove InlineRenderer and reedline, ratatui is default"
```

---

## Summary

| Task | What | Key files |
|---|---|---|
| 0 | Add deps | Cargo.toml |
| 1 | HistoryCell types | tui/cell.rs |
| 2 | InputLine editor | tui/input.rs |
| 3 | Markdown renderer | tui/markdown.rs |
| 4 | History widget | tui/history.rs |
| 5 | AgentEvent conversion | tui/cell.rs, tui/history.rs |
| 6 | Main TUI loop | tui/app.rs |
| 7 | Wire --tui flag | config.rs, main.rs |
| 8 | Smoke test | manual |
| 9 | Remove old code | render/, app.rs |

Tasks 1-5 are independent and could be parallelized. Task 6 depends on 1-5. Task 7 depends on 6. Task 9 depends on 8.
