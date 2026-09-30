use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::wrap::{MAX_COLLAPSED_LINES, wrap_by_display_width};
use unicode_width::UnicodeWidthStr;

/// One unit of conversation history displayed in the history area.
#[derive(Debug, Clone)]
pub enum HistoryCell {
    /// User's input message. Always expanded.
    UserMessage { text: String },
    /// Pre-formatted markdown content (tables, lists, etc.). Rendered with
    /// `render_markdown`, preserving table structure without re-flowing.
    Markdown { text: String },
    /// Assistant's markdown response. Always expanded.
    AssistantMessage { markdown: String },
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
        #[allow(dead_code)]
        id: String,
        result_text: String,
        is_error: bool,
        expanded: bool,
    },
    /// Full-width dim separator line between turns.
    Separator { label: Option<String> },
    /// Permission request prompt. Shows a menu for the user to choose a decision.
    /// Permission request prompt. Shows a menu for the user to choose a decision.
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        /// Compact, always-visible description of what is being requested.
        summary: String,
        /// Complete, unabridged description shown when expanded.
        full: String,
        prefix_suggestion: Option<String>,
        kind: yi_agent_core::permission::PermissionKind,
        resolved: bool,
        /// Whether the body is shown in full instead of truncated.
        expanded: bool,
    },
    /// Permission resolved notification. Shows the decision that was made.
    PermissionResolved {
        decision: yi_agent_core::permission::Decision,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Running,
    Success,
    Failed,
}

impl HistoryCell {
    /// Number of terminal lines this cell occupies at the given width.
    #[allow(dead_code)]
    pub fn line_count(&self, width: u16) -> usize {
        self.lines(width).len()
    }

    /// Render this cell into ratatui Lines for display at the given width.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        #[cfg(test)]
        note_line_render();
        match self {
            Self::UserMessage { text } => render_user_message(text, width),
            Self::Markdown { text } => super::markdown::render_markdown(text, width),
            Self::AssistantMessage { markdown } => {
                super::markdown::render_markdown(markdown, width)
            }
            Self::ToolCall {
                name,
                input,
                state,
                expanded,
                ..
            } => render_tool_call(name, input, *state, *expanded, width),
            Self::ToolResult {
                id: _,
                result_text,
                is_error,
                expanded,
            } => render_tool_result(result_text, *is_error, *expanded, width),
            Self::Separator { label } => render_separator(label.as_deref(), width),
            Self::PermissionRequest {
                tool_name,
                summary,
                full,
                prefix_suggestion,
                kind,
                resolved,
                expanded,
                ..
            } => render_permission_request(
                tool_name,
                summary,
                full,
                prefix_suggestion.as_deref(),
                kind,
                *resolved,
                *expanded,
                width,
            ),
            Self::PermissionResolved { decision } => render_permission_resolved(decision),
        }
    }

    /// Whether this cell is foldable (can be toggled with Ctrl+O).
    #[allow(dead_code)]
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

    /// Create an AssistantMessage cell from a text chunk.
    pub fn from_assistant_text(text: &str) -> Self {
        Self::AssistantMessage {
            markdown: text.to_string(),
        }
    }

    /// Append more text to an existing AssistantMessage.
    pub fn append_assistant_text(&mut self, more: &str) {
        if let Self::AssistantMessage { markdown } = self {
            markdown.push_str(more);
        }
    }
}

// Test-only counter of how many times a cell was actually re-rendered. The
// render path is memoized (see `HistoryState::ensure_cache`); this counter is
// how tests prove the memoization holds and that a frame does not re-render the
// whole scrollback.
#[cfg(test)]
thread_local! {
    static LINES_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_lines_call_count() {
    LINES_CALLS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn lines_call_count() -> usize {
    LINES_CALLS.with(|count| count.get())
}

#[cfg(test)]
fn note_line_render() {
    LINES_CALLS.with(|count| count.set(count.get() + 1));
}

// --- Renderers ---

fn render_user_message(text: &str, width: u16) -> Vec<Line<'static>> {
    let prefix = Span::styled(
        "> ",
        Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
    );
    wrap_with_prefix(text, width, prefix, "  ")
}

fn render_tool_call(
    name: &str,
    input: &Value,
    state: CallState,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let (bullet, bullet_color) = match state {
        CallState::Running => ("●", Color::Yellow),
        CallState::Success => ("●", Color::Green),
        CallState::Failed => ("●", Color::Red),
    };
    let w = width.max(1) as usize;
    let bullet_style = Style::new().fg(bullet_color).add_modifier(Modifier::BOLD);
    // Fold by display width instead of truncating by character count: a
    // character-based 60 "char" cut is 120 columns for CJK, and the tail of a
    // clipped `Line` is dropped by ratatui rather than wrapped.
    let summary = summarize_json(input, w.saturating_sub(3 + name.chars().count()));
    let mut lines: Vec<Line<'static>> =
        wrap_by_display_width(&format!(" {name}({summary})"), w, "●", "  ")
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                if i == 0 {
                    Line::from(vec![
                        Span::styled(bullet, bullet_style),
                        Span::raw(chunk.strip_prefix('●').unwrap_or(&chunk).to_string()),
                    ])
                } else {
                    Line::styled(chunk, Style::new().fg(Color::DarkGray))
                }
            })
            .collect();
    if expanded {
        let full = format!("{input:#}");
        for line in full.lines() {
            lines.extend(
                wrap_by_display_width(line, w, "  └ ", "    ")
                    .into_iter()
                    .map(|chunk| Line::styled(chunk, Style::new().fg(Color::DarkGray))),
            );
        }
    }
    lines
}

fn render_tool_result(
    text: &str,
    is_error: bool,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let arrow_color = if is_error { Color::Red } else { Color::Green };
    let w = width.max(1) as usize;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let summary_style = Style::new().fg(arrow_color).add_modifier(Modifier::DIM);
    // Only the first physical line is previewed when folded; the full text is
    // available via [e]. Fold it by display width so a multi-byte preview is
    // neither over-wide nor silently cut.
    let summary = text.lines().next().unwrap_or("");
    let mut lines: Vec<Line<'static>> = wrap_by_display_width(summary, w, "  └ ", "    ")
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            if i == 0 {
                Line::styled(chunk, summary_style)
            } else {
                Line::styled(chunk, dim)
            }
        })
        .collect();
    if expanded {
        for line in text.lines() {
            lines.extend(
                wrap_by_display_width(line, w, "    ", "    ")
                    .into_iter()
                    .map(|chunk| Line::styled(chunk, dim)),
            );
        }
    }
    lines
}

/// A separator is always exactly `width` columns wide when it has no label.
/// With a label the text is folded to `width` (a long provider error used to
/// become one over-wide `Line` whose tail was clipped), and the last physical
/// line is padded with `─` so the separator still reads as a full-width rule.
fn render_separator(label: Option<&str>, width: u16) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let dim = Style::new().add_modifier(Modifier::DIM);
    match label {
        None => vec![Line::from("─".repeat(w)).style(dim)],
        Some(l) => {
            let mut wrapped = wrap_by_display_width(l, w, "─ ", "  ");
            let last = wrapped.len().saturating_sub(1);
            for (i, chunk) in wrapped.iter_mut().enumerate() {
                if i == last {
                    let used = UnicodeWidthStr::width(chunk.as_str());
                    chunk.push_str(&"─".repeat(w.saturating_sub(used)));
                }
            }
            wrapped
                .into_iter()
                .map(|chunk| Line::styled(chunk, dim))
                .collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_permission_request(
    tool_name: &str,
    summary: &str,
    full: &str,
    prefix_suggestion: Option<&str>,
    kind: &yi_agent_core::permission::PermissionKind,
    resolved: bool,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let warn_style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let dim_style = Style::new().add_modifier(Modifier::DIM);
    let menu_style = Style::new().fg(Color::Cyan);
    let w = width.max(1) as usize;

    if resolved {
        return wrap_by_display_width(summary, w, "  [resolved] ", "  ")
            .into_iter()
            .map(|chunk| Line::from(chunk).style(dim_style))
            .collect();
    }

    let body = if expanded { full } else { summary };
    let body_lines = wrap_by_display_width(body, w, "  ", "  ");
    let truncated = !expanded && body_lines.len() > MAX_COLLAPSED_LINES;

    // Every line goes through the wrapper, including the header and the
    // menu: at narrow widths those are themselves wider than the terminal
    // and would otherwise be truncated by ratatui.
    let mut lines: Vec<Line<'static>> = {
        let mut header: Vec<Line<'static>> = Vec::new();
        let header_text = format!("? Permission needed: {tool_name}");
        for chunk in wrap_by_display_width(&header_text, w, "", "  ") {
            header.push(Line::from(Span::styled(chunk, warn_style)));
        }
        header
    };

    let shown = if truncated {
        MAX_COLLAPSED_LINES
    } else {
        body_lines.len()
    };
    for line in body_lines.iter().take(shown) {
        lines.push(Line::from(line.clone()).style(dim_style));
    }
    if truncated {
        let hidden = body_lines.len() - MAX_COLLAPSED_LINES;
        for chunk in wrap_by_display_width(
            &format!("  … (+{hidden} more lines, [e] to expand)"),
            w,
            "",
            "  ",
        ) {
            lines.push(Line::from(chunk).style(dim_style));
        }
    } else if expanded {
        for chunk in wrap_by_display_width("  [e] to collapse", w, "", "  ") {
            lines.push(Line::from(chunk).style(dim_style));
        }
    }

    let blacklisted = matches!(
        kind,
        yi_agent_core::permission::PermissionKind::Blacklisted(_)
    );
    if blacklisted {
        let style = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
        for chunk in wrap_by_display_width("  [!] Blacklisted command", w, "", "  ") {
            lines.push(Line::from(Span::styled(chunk, style)));
        }
    }

    let option_lines: Vec<(String, Style)> = match prefix_suggestion {
        Some(p) => vec![
            ("  [1] Allow once".to_string(), menu_style),
            ("  [2] Always allow tool".to_string(), menu_style),
            (format!("  [3] Always allow prefix: {p}"), menu_style),
            ("  [4] Deny".to_string(), menu_style),
        ],
        None => vec![
            ("  [1] Allow once".to_string(), menu_style),
            ("  [2] Always allow tool".to_string(), menu_style),
            ("  [4] Deny".to_string(), menu_style),
        ],
    };
    for (text, style) in option_lines {
        for chunk in wrap_by_display_width(&text, w, "", "  ") {
            lines.push(Line::from(Span::styled(chunk, style)));
        }
    }

    let default_hint = if blacklisted {
        "  Enter = Deny"
    } else {
        "  Enter = Allow once"
    };
    for chunk in wrap_by_display_width(default_hint, w, "", "  ") {
        lines.push(Line::from(chunk).style(dim_style));
    }

    lines
}

fn render_permission_resolved(
    decision: &yi_agent_core::permission::Decision,
) -> Vec<Line<'static>> {
    let (label, color) = match decision {
        yi_agent_core::permission::Decision::AllowOnce => ("allowed (once)", Color::Green),
        yi_agent_core::permission::Decision::AlwaysAllowTool => ("allowed (always)", Color::Green),
        yi_agent_core::permission::Decision::AlwaysAllowPrefix(_) => {
            ("allowed (prefix)", Color::Green)
        }
        yi_agent_core::permission::Decision::Deny => ("denied", Color::Red),
    };
    vec![Line::from(vec![
        Span::styled("  -> ", Style::new().add_modifier(Modifier::DIM)),
        Span::styled(label, Style::new().fg(color)),
    ])]
}

// --- Helpers ---

/// One-line summary of a tool input, capped at `max_width` display columns.
///
/// Capped by display width (not character count) so a CJK payload cannot reach
/// twice the intended width; `wrap_by_display_width` still folds the assembled
/// `name(...)` header if a long tool name leaves no room.
fn summarize_json(v: &Value, max_width: usize) -> String {
    let s = v.to_string();
    let truncated = crate::tui::wrap::truncate_to_width(&s, max_width);
    if truncated == s {
        s
    } else {
        // truncate_to_width appends '…'; match the previous "..." marker look.
        let cut = truncated.chars().count();
        format!(
            "{}...",
            s.chars().take(cut.saturating_sub(1)).collect::<String>()
        )
    }
}

fn wrap_with_prefix(
    text: &str,
    width: u16,
    first_prefix: Span<'static>,
    cont_prefix: &str,
) -> Vec<Line<'static>> {
    let max_w = width as usize;
    // A terminal narrower than the two-column prefix cannot show it; drop the
    // prefix entirely so the content still fits the line it is rendered into.
    let (first_prefix, cont_prefix): (Span<'static>, &str) =
        if max_w <= UnicodeWidthStr::width(first_prefix.content.as_ref()) {
            (Span::raw(""), "")
        } else {
            (first_prefix, cont_prefix)
        };
    // 段内自动换行的辅助函数：按显示宽度把单词拼到 current 里。
    // 单个"词"超过行宽时（CJK 无空格文本常见），按字符拆分。
    let wrap_segment = |seg: &str, out: &mut Vec<String>| {
        let mut current = String::new();
        for word in seg.split_whitespace() {
            // 首行有 first_prefix（2 字符），后续行有 cont_prefix
            let prefix_len = if out.is_empty() && current.is_empty() {
                2
            } else {
                cont_prefix.len()
            };
            let current_w = UnicodeWidthStr::width(current.as_str());
            let word_w = UnicodeWidthStr::width(word);
            if current.is_empty() && word_w + prefix_len <= max_w {
                current = word.to_string();
            } else if !current.is_empty() && current_w + 1 + word_w + prefix_len <= max_w {
                current.push(' ');
                current.push_str(word);
            } else if word_w + prefix_len <= max_w {
                // Word fits on its own line; start new line
                out.push(std::mem::take(&mut current));
                current = word.to_string();
            } else {
                // Single word exceeds available width: break char-by-char.
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                let avail = max_w.saturating_sub(prefix_len).max(1);
                let mut chunk = String::new();
                let mut chunk_w: usize = 0;
                for ch in word.chars() {
                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if ch_w == 0 {
                        continue;
                    }
                    if chunk_w + ch_w > avail && !chunk.is_empty() {
                        out.push(std::mem::take(&mut chunk));
                        chunk_w = 0;
                    }
                    chunk.push(ch);
                    chunk_w += ch_w;
                }
                if !chunk.is_empty() {
                    current = chunk;
                }
            }
        }
        out.push(std::mem::take(&mut current));
    };

    let mut raw_lines: Vec<String> = Vec::new();
    // 先按 \n 切分，保留显式换行（包括空行）
    for seg in text.split('\n') {
        wrap_segment(seg, &mut raw_lines);
    }
    // 移除末尾 wrap_segment 产生的空行（当 text 不以 \n 结尾时不会有；
    // 当 text 以 \n 结尾时 split 会多产出一个空段，这里保留它以维持尾空行）
    // 注：split('\n') 对 "a\n" 会产出 ["a", ""]，两个段都会生成一行，
    // 因此末尾的空行会被保留；这与预期一致。

    if raw_lines.is_empty() {
        raw_lines.push(String::new());
    }

    raw_lines
        .into_iter()
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
        let cell = HistoryCell::UserMessage {
            text: "hello".into(),
        };
        let lines = cell.lines(80);
        assert_eq!(lines.len(), 1);
        let spans: Vec<String> = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
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
        assert_eq!(
            lines.len(),
            1,
            "folded tool call should be 1 line, got {}",
            lines.len()
        );
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
            id: "1".into(),
            name: "t".into(),
            input: serde_json::json!({}),
            state: CallState::Success,
            expanded: false,
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
        let s: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(s.chars().count(), 40);
        assert!(s.chars().all(|c| c == '─'));
    }

    #[test]
    fn separator_with_label_has_dashes_around() {
        let cell = HistoryCell::Separator {
            label: Some("Worked for 2m".into()),
        };
        let lines = cell.lines(40);
        assert_eq!(lines.len(), 1, "a short label must stay on one line");
        let s: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(s.contains("Worked for 2m"));
        assert!(s.starts_with("─ "));
    }

    #[test]
    fn user_message_is_not_foldable() {
        let cell = HistoryCell::UserMessage { text: "x".into() };
        assert!(!cell.is_foldable());
    }

    #[test]
    fn tool_call_is_foldable() {
        let cell = HistoryCell::ToolCall {
            id: "1".into(),
            name: "t".into(),
            input: serde_json::json!({}),
            state: CallState::Success,
            expanded: false,
        };
        assert!(cell.is_foldable());
    }

    #[test]
    fn user_message_preserves_explicit_newlines() {
        let cell = HistoryCell::UserMessage {
            text: "line1\nline2\nline3".into(),
        };
        let lines = cell.lines(80);
        assert_eq!(
            lines.len(),
            3,
            "three explicit lines should render as 3 lines, got {}: {:?}",
            lines.len(),
            lines
                .iter()
                .map(|l| l
                    .spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn user_message_empty_line_between_text() {
        let cell = HistoryCell::UserMessage {
            text: "para1\n\npara2".into(),
        };
        let lines = cell.lines(80);
        assert_eq!(lines.len(), 3, "blank line should be preserved");
    }

    #[test]
    fn user_message_multiline_with_long_line_wraps() {
        let cell = HistoryCell::UserMessage {
            text: "short\nthis is a very long line that should wrap when terminal is narrow".into(),
        };
        let lines = cell.lines(20);
        assert!(
            lines.len() >= 3,
            "should preserve newline AND wrap long line"
        );
        // First line is "short"
        let first: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(first.contains("short"));
    }

    #[test]
    fn user_message_cjk_wraps_at_display_width() {
        // CJK chars are 2 display columns. With width=10 and 2-char prefix,
        // available width is 8 cols = 4 CJK chars per continuation line.
        let cell = HistoryCell::UserMessage {
            text: "一二三四五六七八九十".into(),
        };
        let lines = cell.lines(10);
        assert!(
            lines.len() > 1,
            "expected CJK user message to wrap at width 10, got {} lines",
            lines.len()
        );
        // Verify no line exceeds 10 display columns
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= 10,
                "line {} display width {w} exceeds 10: {:?}",
                i,
                line.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn user_message_wraps_long_unbroken_token_at_terminal_boundaries() {
        // A path or command with no spaces is a single "word" here; the wrapper
        // splits it only when it exceeds the whole line, so its head and tail
        // used to be emitted on one over-wide line and clipped on the right.
        let path = "/Users/someone/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs/crates/yi-agent/src/tui/cell.rs";
        for width in [20u16, 40] {
            let cell = HistoryCell::UserMessage {
                text: path.to_string(),
            };
            assert_cell_fits(&cell, width, "long user path");
        }
    }

    #[test]
    fn assistant_text_creates_new_cell() {
        let cell = HistoryCell::from_assistant_text("hello");
        match cell {
            HistoryCell::AssistantMessage { markdown, .. } => {
                assert_eq!(markdown, "hello");
            }
            _ => panic!("expected AssistantMessage"),
        }
    }

    #[test]
    fn markdown_cell_renders_table_with_box_drawing() {
        let src = "| h1 | h2 |\n| --- | --- |\n| a | b |\n";
        let cell = HistoryCell::Markdown { text: src.into() };
        let lines = cell.lines(40);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            joined.contains('│'),
            "should have box-drawing vertical bars: {joined}"
        );
        assert!(
            joined.contains('─'),
            "should have box-drawing horizontal bars: {joined}"
        );
        assert!(
            joined.contains("h1") && joined.contains("h2"),
            "should have headers: {joined}"
        );
        assert!(
            joined.contains("a") && joined.contains("b"),
            "should have data: {joined}"
        );
    }
    fn perm_cell(summary: &str, full: &str, expanded: bool) -> HistoryCell {
        HistoryCell::PermissionRequest {
            request_id: 1,
            tool_name: "bash".into(),
            summary: summary.into(),
            full: full.into(),
            prefix_suggestion: Some("cd".into()),
            kind: yi_agent_core::permission::PermissionKind::Normal,
            resolved: false,
            expanded,
        }
    }

    fn line_widths(lines: &[Line<'static>]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum()
            })
            .collect()
    }

    fn joined_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect()
    }

    const LONG_CMD: &str = "cd /Users/someone/projects/personalProjects/yi-agent && cargo test -p yi-agent-core --lib permission";

    #[test]
    fn permission_body_never_exceeds_width() {
        // Includes narrow widths where the header and menu themselves wrap.
        for width in [20u16, 30, 40, 80] {
            for expanded in [false, true] {
                let cell = perm_cell(LONG_CMD, LONG_CMD, expanded);
                let widths = line_widths(&cell.lines(width));
                assert!(
                    widths.iter().all(|w| *w <= width as usize),
                    "width {width} expanded={expanded}: some line exceeds it: {widths:?}"
                );
            }
        }
    }

    #[test]
    fn permission_wrap_preserves_exact_command_whitespace() {
        let cmd = r#"git commit -m "fix:  two  spaces""#;
        let cell = perm_cell(cmd, cmd, true);
        let text = joined_text(&cell.lines(200));
        assert!(
            text.contains(cmd),
            "command text was rewritten during rendering: {text:?}"
        );
    }

    #[test]
    fn permission_collapsed_body_is_bounded() {
        // Hard requirement: fit the history area (21 rows in a 24-row terminal).
        for width in [20u16, 30, 40, 80] {
            let cell = perm_cell(LONG_CMD, LONG_CMD, false);
            let lines = cell.lines(width);
            assert!(
                lines.len() <= 21,
                "width {width}: collapsed cell too tall: {} lines",
                lines.len()
            );
        }
        // At a normal width only the body wraps, so the tight bound holds.
        let cell = perm_cell(LONG_CMD, LONG_CMD, false);
        assert!(cell.lines(80).len() <= 1 + MAX_COLLAPSED_LINES + 1 + 5 + 1);
    }

    #[test]
    fn permission_collapsed_shows_expand_hint_when_truncated() {
        let cell = perm_cell(LONG_CMD, LONG_CMD, false);
        let text = joined_text(&cell.lines(20));
        assert!(text.contains("[e]"), "expected expand hint: {text:?}");
    }

    #[test]
    fn permission_not_truncated_has_no_expand_hint() {
        let cell = perm_cell("ls", "ls", false);
        let text = joined_text(&cell.lines(80));
        assert!(!text.contains("[e]"), "unexpected hint: {text:?}");
    }

    #[test]
    fn permission_expanded_recovers_command_tail() {
        // Width 20 forces truncation; width 40 happens to fit in 4 lines.
        let cell = perm_cell(LONG_CMD, LONG_CMD, true);
        let text = joined_text(&cell.lines(20));
        assert!(
            text.contains("permission"),
            "expanded body should reach the command tail: {text:?}"
        );
        assert!(
            !text.contains("more lines"),
            "expanded body must not claim to be truncated: {text:?}"
        );
    }

    #[test]
    fn permission_menu_rendered_after_body() {
        let cell = perm_cell(LONG_CMD, LONG_CMD, false);
        let lines = cell.lines(80);
        let text = joined_text(&lines);
        for opt in [
            "[1] Allow once",
            "[2] Always allow tool",
            "[3] Always allow prefix: cd",
            "[4] Deny",
        ] {
            assert!(text.contains(opt), "missing menu option {opt}: {text:?}");
        }
        let body_line = lines
            .iter()
            .position(|l| joined_text(std::slice::from_ref(l)).contains("&&"))
            .expect("body marker '&&' not found");
        let menu_line = lines
            .iter()
            .position(|l| joined_text(std::slice::from_ref(l)).contains("[1] Allow once"))
            .expect("menu not found");
        assert!(menu_line > body_line, "menu must render after body");
    }

    #[test]
    fn permission_cjk_body_wraps_at_display_width() {
        let cjk = "这是一条很长的中文命令需要按显示宽度换行";
        for width in [30u16, 80] {
            let cell = perm_cell(cjk, cjk, true);
            let widths = line_widths(&cell.lines(width));
            assert!(
                widths.iter().all(|w| *w <= width as usize),
                "width {width}: CJK line too wide: {widths:?}"
            );
        }
    }

    fn assert_cell_fits(cell: &HistoryCell, width: u16, what: &str) {
        for (i, line) in cell.lines(width).iter().enumerate() {
            let w = UnicodeWidthStr::width(joined_text(std::slice::from_ref(line)).as_str());
            assert!(
                w <= width as usize,
                "{what}: line {i} is {w} cols, width is {width}: {:?}",
                joined_text(std::slice::from_ref(line))
            );
        }
    }

    const LONG_CJK: &str =
        "这是一条很长的中文内容需要按显示宽度折行否则右侧会被截断掉看不见真的看不见";

    #[test]
    fn tool_call_summary_and_expansion_never_exceed_width() {
        let input = serde_json::json!({
            "command": LONG_CJK,
            "note": "a plain ascii payload that is also long enough to overflow a narrow terminal for sure"
        });
        for expanded in [false, true] {
            for width in [20u16, 40, 60] {
                let cell = HistoryCell::ToolCall {
                    id: "1".into(),
                    name: "bash".into(),
                    input: input.clone(),
                    state: CallState::Success,
                    expanded,
                };
                assert_cell_fits(&cell, width, "tool call");
            }
        }
    }

    #[test]
    fn tool_result_summary_and_expansion_never_exceed_width() {
        let text = format!(
            "{LONG_CJK}\n\nsecond line that is also quite long and will overflow a narrow terminal"
        );
        for expanded in [false, true] {
            for width in [20u16, 40, 60] {
                let cell = HistoryCell::ToolResult {
                    id: "1".into(),
                    result_text: text.clone(),
                    is_error: false,
                    expanded,
                };
                assert_cell_fits(&cell, width, "tool result");
            }
        }
    }

    #[test]
    fn tool_result_cannot_be_hidden_by_character_count_truncation() {
        // Character-count truncation kept 40 CJK chars, which are 80 display
        // columns: the "shortened" summary was still twice the width.
        let cell = HistoryCell::ToolResult {
            id: "1".into(),
            result_text: LONG_CJK.into(),
            is_error: false,
            expanded: false,
        };
        assert_cell_fits(&cell, 40, "cjk tool result");
    }

    #[test]
    fn separator_label_folds_long_error_text() {
        let label = format!("Error: {LONG_CJK}");
        for width in [20u16, 40, 80] {
            assert_cell_fits(
                &HistoryCell::Separator {
                    label: Some(label.clone()),
                },
                width,
                "separator with label",
            );
        }
    }

    #[test]
    fn separator_without_label_still_spans_the_width() {
        let cell = HistoryCell::Separator { label: None };
        let lines = cell.lines(40);
        assert_eq!(lines.len(), 1);
        assert_eq!(UnicodeWidthStr::width(joined_text(&lines).as_str()), 40);
    }

    #[test]
    fn permission_resolved_line_wraps() {
        let mut cell = perm_cell(LONG_CMD, LONG_CMD, false);
        if let HistoryCell::PermissionRequest { resolved, .. } = &mut cell {
            *resolved = true;
        }
        let widths = line_widths(&cell.lines(40));
        assert!(
            widths.iter().all(|w| *w <= 40),
            "resolved line too wide: {widths:?}"
        );
    }
}
