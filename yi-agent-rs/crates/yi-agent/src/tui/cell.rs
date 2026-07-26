use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
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
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        display: String,
        prefix_suggestion: Option<String>,
        kind: yi_agent_core::permission::PermissionKind,
        resolved: bool,
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
            Self::Separator { label } => vec![render_separator(label.as_deref(), width)],
            Self::PermissionRequest {
                tool_name,
                display,
                prefix_suggestion,
                kind,
                resolved,
                ..
            } => render_permission_request(
                tool_name,
                display,
                prefix_suggestion.as_deref(),
                kind,
                *resolved,
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
    _width: u16,
) -> Vec<Line<'static>> {
    let (bullet, bullet_color) = match state {
        CallState::Running => ("●", Color::Yellow),
        CallState::Success => ("●", Color::Green),
        CallState::Failed => ("●", Color::Red),
    };
    let input_summary = summarize_json(input, 60);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            bullet,
            Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
        ),
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

fn render_tool_result(
    text: &str,
    is_error: bool,
    expanded: bool,
    _width: u16,
) -> Vec<Line<'static>> {
    let arrow_color = if is_error { Color::Red } else { Color::Green };
    let summary = truncate(text, 80);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "  └ ",
            Style::new().fg(arrow_color).add_modifier(Modifier::DIM),
        ),
        Span::styled(summary, Style::new().add_modifier(Modifier::DIM)),
    ])];
    if expanded {
        for line in text.lines() {
            lines.push(
                Line::from(format!("    {line}")).style(Style::new().add_modifier(Modifier::DIM)),
            );
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

fn render_permission_request(
    tool_name: &str,
    display: &str,
    prefix_suggestion: Option<&str>,
    kind: &yi_agent_core::permission::PermissionKind,
    resolved: bool,
    _width: u16,
) -> Vec<Line<'static>> {
    let warn_style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let dim_style = Style::new().add_modifier(Modifier::DIM);
    let menu_style = Style::new().fg(Color::Cyan);

    if resolved {
        return vec![Line::from(vec![
            Span::styled("  [resolved] ", dim_style),
            Span::raw(display.to_string()),
        ])];
    }

    let mut lines = vec![
        Line::from(vec![
            Span::styled("? ", warn_style),
            Span::styled(format!("Permission needed: {tool_name}"), warn_style),
        ]),
        Line::from(format!("  {display}")).style(dim_style),
    ];

    // Menu options
    let blacklisted = matches!(
        kind,
        yi_agent_core::permission::PermissionKind::Blacklisted(_)
    );
    if blacklisted {
        lines.push(Line::from(vec![Span::styled(
            "  [!] Blacklisted command",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )]));
    }

    let option_lines = match prefix_suggestion {
        Some(p) => vec![Span::styled("  [1] Allow once", menu_style)]
            .into_iter()
            .chain(vec![Span::styled("  [2] Always allow tool", menu_style)])
            .chain(vec![Span::styled(
                format!("  [3] Always allow prefix: {p}"),
                menu_style,
            )])
            .chain(vec![Span::styled("  [4] Deny", menu_style)])
            .collect::<Vec<_>>(),
        None => vec![
            Span::styled("  [1] Allow once", menu_style),
            Span::styled("  [2] Always allow tool", menu_style),
            Span::styled("  [4] Deny", menu_style),
        ],
    };
    for span in option_lines {
        lines.push(Line::from(span));
    }

    let default_hint = if blacklisted {
        "  Enter = Deny"
    } else {
        "  Enter = Allow once"
    };
    lines.push(Line::from(default_hint).style(dim_style));

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

fn wrap_with_prefix(
    text: &str,
    width: u16,
    first_prefix: Span<'static>,
    cont_prefix: &str,
) -> Vec<Line<'static>> {
    let max_w = width as usize;
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
        assert_eq!(lines.len(), 1);
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
}
