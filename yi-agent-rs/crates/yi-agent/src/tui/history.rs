use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;

use yi_agent_core::{AgentEvent, DoneReason};

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
        self.scroll_offset = 0;
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

impl HistoryState {
    /// Process an AgentEvent and update the cell list accordingly.
    pub fn push_event(&mut self, event: AgentEvent, width: u16) {
        match event {
            AgentEvent::Start => {}
            AgentEvent::AssistantText(text) => {
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
        s.push(HistoryCell::UserMessage { text: "hello".into() });
        s.push(HistoryCell::Separator { label: None });
        assert_eq!(s.total_lines(80), 2);
    }

    use yi_agent_core::{AgentEvent, DoneReason, ToolResult};

    #[test]
    fn push_event_assistant_text_appends_to_existing() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("hello".into()), 80);
        s.push_event(AgentEvent::AssistantText(" world".into()), 80);
        assert_eq!(s.cells.len(), 1, "two text chunks should merge into 1 cell");
        match &s.cells[0] {
            HistoryCell::AssistantMessage { markdown, .. } => assert_eq!(*markdown, "hello world"),
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
            result: ToolResult {
                content: vec![yi_agent_core::ContentBlock::Text("ok".into())],
                is_error: false,
            },
        }, 80);
        assert!(matches!(
            &s.cells[0],
            HistoryCell::ToolCall { state: crate::tui::cell::CallState::Success, .. }
        ));
        assert!(matches!(s.cells.get(1), Some(HistoryCell::ToolResult { .. })));
    }
}
