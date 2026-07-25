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

// --- Renderers ---

fn render_user_message(text: &str, width: u16) -> Vec<Line<'static>> {
    let prefix = Span::styled("> ", Style::new().add_modifier(Modifier::BOLD | Modifier::DIM));
    wrap_with_prefix(text, width, prefix, "  ")
}

fn render_tool_call(name: &str, input: &Value, state: CallState, expanded: bool, _width: u16) -> Vec<Line<'static>> {
    let (bullet, bullet_color) = match state {
        CallState::Running => ("●", Color::Yellow),
        CallState::Success => ("●", Color::Green),
        CallState::Failed => ("●", Color::Red),
    };
    let input_summary = summarize_json(input, 60);
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

fn render_tool_result(text: &str, is_error: bool, expanded: bool, _width: u16) -> Vec<Line<'static>> {
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
    let max_w = width as usize;
    let mut lines = vec![];
    let mut current = String::new();
    for word in text.split_whitespace() {
        let prefix_len = if lines.is_empty() { 2 } else { cont_prefix.len() };
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
        let spans: Vec<String> = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
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
        let s: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(s.chars().count(), 40);
        assert!(s.chars().all(|c| c == '─'));
    }

    #[test]
    fn separator_with_label_has_dashes_around() {
        let cell = HistoryCell::Separator { label: Some("Worked for 2m".into()) };
        let lines = cell.lines(40);
        assert_eq!(lines.len(), 1);
        let s: String = lines[0].spans.iter().map(|s| s.content.to_string()).collect();
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
            id: "1".into(), name: "t".into(), input: serde_json::json!({}),
            state: CallState::Success, expanded: false,
        };
        assert!(cell.is_foldable());
    }
}
