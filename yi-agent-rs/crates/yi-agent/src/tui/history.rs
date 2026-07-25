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
}
