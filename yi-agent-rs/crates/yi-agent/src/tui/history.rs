use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget};

use yi_agent_core::{AgentEvent, DoneReason};

use super::cell::HistoryCell;

/// A location in the history content, independent of its current wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ViewportAnchor {
    cell_index: usize,
    line_in_cell: usize,
}

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
        Self {
            cells: Vec::new(),
            selected: None,
            scroll_offset: 0,
        }
    }

    /// Push a new cell while preserving the visible position when scrolled up.
    pub fn push(&mut self, cell: HistoryCell, width: u16) {
        let was_scrolled = self.scroll_offset != 0;
        let lines_before = self.flattened_line_count(width);
        self.cells.push(cell);
        self.apply_scroll_delta(was_scrolled, lines_before, width);
    }

    /// Clear all cells and reset state.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.selected = None;
        self.scroll_offset = 0;
    }

    /// Total number of display lines across all cells at given width.
    #[allow(dead_code)]
    pub fn total_lines(&self, width: u16) -> usize {
        self.cells.iter().map(|c| c.line_count(width)).sum()
    }

    /// Number of display lines including the blank spacer inserted after
    /// each `UserMessage` cell (except the last). This matches the line count
    /// used by `HistoryView::flattened_lines` / `render`.
    pub fn flattened_line_count(&self, width: u16) -> usize {
        let n = self.cells.len();
        let mut count = 0usize;
        for (i, cell) in self.cells.iter().enumerate() {
            count += cell.line_count(width);
            if matches!(cell, HistoryCell::UserMessage { .. }) && i + 1 < n {
                count += 1; // spacer line
            }
        }
        count
    }

    /// Width available to history text after reserving a scrollbar column
    /// when the content overflows the viewport.
    pub fn text_width(&self, area_width: u16, viewport_height: u16) -> u16 {
        let candidate_width = area_width.saturating_sub(1);
        if candidate_width > 0
            && self.flattened_line_count(candidate_width) > viewport_height as usize
        {
            candidate_width
        } else {
            area_width
        }
    }

    /// Maximum meaningful `scroll_offset` for the current content at the given
    /// width and viewport height. Scrolling beyond this would leave blank rows
    /// at the bottom of the viewport, so `scroll_up` clamps to this value.
    ///
    /// Returns 0 when the content fits entirely within the viewport.
    pub fn max_scroll_offset(&self, width: u16, visible_height: u16) -> usize {
        let total = self.flattened_line_count(width);
        total.saturating_sub(visible_height as usize)
    }

    /// Clamp the stored offset to the current viewport after a resize.
    pub fn reconcile_scroll_offset(&mut self, width: u16, visible_height: u16) {
        self.scroll_offset = self
            .scroll_offset
            .min(self.max_scroll_offset(width, visible_height));
    }

    /// Capture the top visible content location when the viewport is not
    /// following the bottom. A user-message spacer belongs to that user cell.
    pub(super) fn capture_viewport_anchor(
        &self,
        text_width: u16,
        viewport_height: u16,
    ) -> Option<ViewportAnchor> {
        let total = self.flattened_line_count(text_width);
        let effective_offset = self
            .scroll_offset
            .min(total.saturating_sub(viewport_height as usize));
        if effective_offset == 0 {
            return None;
        }

        let anchor_top = total.saturating_sub(viewport_height as usize + effective_offset);
        let mut lines_before = 0;
        for (cell_index, cell) in self.cells.iter().enumerate() {
            let cell_lines = cell.line_count(text_width);
            if anchor_top < lines_before + cell_lines {
                return Some(ViewportAnchor {
                    cell_index,
                    line_in_cell: anchor_top - lines_before,
                });
            }
            lines_before += cell_lines;

            if matches!(cell, HistoryCell::UserMessage { .. }) && cell_index + 1 < self.cells.len()
            {
                if anchor_top == lines_before {
                    return Some(ViewportAnchor {
                        cell_index,
                        line_in_cell: cell_lines,
                    });
                }
                lines_before += 1;
            }
        }

        None
    }

    /// Restore a captured top-of-viewport location after wrapping changes.
    pub(super) fn restore_viewport_anchor(
        &mut self,
        anchor: ViewportAnchor,
        text_width: u16,
        viewport_height: u16,
    ) {
        let Some(anchor_cell) = self.cells.get(anchor.cell_index) else {
            self.reconcile_scroll_offset(text_width, viewport_height);
            return;
        };

        let mut anchor_top = 0;
        for (cell_index, cell) in self.cells.iter().enumerate() {
            if cell_index == anchor.cell_index {
                anchor_top += anchor.line_in_cell.min(anchor_cell.line_count(text_width));
                break;
            }

            anchor_top += cell.line_count(text_width);
            if matches!(cell, HistoryCell::UserMessage { .. }) {
                anchor_top += 1;
            }
        }

        let total = self.flattened_line_count(text_width);
        self.scroll_offset = total
            .saturating_sub(viewport_height as usize)
            .saturating_sub(anchor_top)
            .min(self.max_scroll_offset(text_width, viewport_height));
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

    /// Scroll up by `n` lines, clamped to `max_offset` so the viewport never
    /// scrolls past the top of the content (which would leave blank rows at
    /// the bottom). Callers should pass `max_scroll_offset(width, height)`.
    pub fn scroll_up(&mut self, n: usize, max_offset: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n).min(max_offset);
    }

    /// Scroll down by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll up by one viewport, treating a zero-height viewport as one line.
    pub fn scroll_page_up(&mut self, viewport_height: u16, max_offset: usize) {
        self.scroll_up(viewport_height.max(1) as usize, max_offset);
    }

    /// Scroll down by one viewport, treating a zero-height viewport as one line.
    pub fn scroll_page_down(&mut self, viewport_height: u16) {
        self.scroll_down(viewport_height.max(1) as usize);
    }

    /// Jump to the greatest valid scroll offset for the current viewport.
    pub fn scroll_to_top(&mut self, width: u16, visible_height: u16) {
        self.scroll_offset = self.max_scroll_offset(width, visible_height);
    }

    /// Jump to the newest history content.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    fn apply_scroll_delta(&mut self, was_scrolled: bool, lines_before: usize, width: u16) {
        if !was_scrolled {
            return;
        }

        let lines_after = self.flattened_line_count(width);
        self.scroll_offset = if lines_after >= lines_before {
            self.scroll_offset
                .saturating_add(lines_after - lines_before)
        } else {
            self.scroll_offset
                .saturating_sub(lines_before - lines_after)
        };
    }

    /// Returns info about the most recent unresolved permission request, if any.
    pub fn pending_permission_info(
        &self,
    ) -> Option<(
        u64,
        &str,
        Option<&str>,
        &yi_agent_core::permission::PermissionKind,
    )> {
        self.cells.iter().rev().find_map(|c| match c {
            HistoryCell::PermissionRequest {
                request_id,
                tool_name,
                prefix_suggestion,
                kind,
                resolved: false,
                ..
            } => Some((
                *request_id,
                tool_name.as_str(),
                prefix_suggestion.as_deref(),
                kind,
            )),
            _ => None,
        })
    }
}

impl HistoryState {
    /// Process an AgentEvent and update the cell list accordingly.
    pub fn push_event(&mut self, event: AgentEvent, width: u16) {
        let was_scrolled = self.scroll_offset != 0;
        let lines_before = self.flattened_line_count(width);

        match event {
            AgentEvent::Start => {}
            AgentEvent::AssistantText(text) => match self.cells.last_mut() {
                Some(HistoryCell::AssistantMessage { .. }) => {
                    self.cells.last_mut().unwrap().append_assistant_text(&text);
                }
                _ => {
                    self.cells.push(HistoryCell::from_assistant_text(&text));
                }
            },
            AgentEvent::ToolCall { id, name, input } => {
                self.cells.push(HistoryCell::ToolCall {
                    id,
                    name,
                    input,
                    state: super::cell::CallState::Running,
                    expanded: false,
                });
            }
            AgentEvent::ToolResult { id, result } => {
                let is_error = result.is_error;
                let result_text = result
                    .content
                    .iter()
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
                self.cells.push(HistoryCell::ToolResult {
                    id,
                    result_text,
                    is_error,
                    expanded: false,
                });
            }
            AgentEvent::Done { reason } => match reason {
                DoneReason::EndTurn => {
                    self.cells.push(HistoryCell::Separator { label: None });
                }
                DoneReason::MaxTurns => {
                    self.cells.push(HistoryCell::Separator {
                        label: Some("Max turns".into()),
                    });
                }
                DoneReason::Interrupted { reason } => {
                    self.cells.push(HistoryCell::Separator {
                        label: Some(format!("Interrupted: {reason}")),
                    });
                }
            },
            AgentEvent::Usage { .. } => {}
            AgentEvent::Cancelled => {
                self.cells.push(HistoryCell::Separator {
                    label: Some("Interrupted".into()),
                });
            }
            AgentEvent::Error(err) => {
                self.cells.push(HistoryCell::Separator {
                    label: Some(format!("Error: {err}")),
                });
            }
            AgentEvent::PermissionRequest {
                request_id,
                tool_name,
                tool_input,
                prefix_suggestion,
                kind,
            } => {
                let display = format!("{}: {}", tool_name, tool_input);
                self.cells.push(HistoryCell::PermissionRequest {
                    request_id,
                    tool_name,
                    display,
                    prefix_suggestion,
                    kind,
                    resolved: false,
                });
            }
            AgentEvent::PermissionResolved {
                request_id,
                decision,
            } => {
                // Update the corresponding PermissionRequest cell
                for cell in self.cells.iter_mut() {
                    if let HistoryCell::PermissionRequest {
                        request_id: rid,
                        resolved,
                        ..
                    } = cell
                    {
                        if *rid == request_id {
                            *resolved = true;
                            break;
                        }
                    }
                }
                self.cells
                    .push(HistoryCell::PermissionResolved { decision });
            }
            AgentEvent::ToolOutputDelta { .. }
            | AgentEvent::ToolExit { .. }
            | AgentEvent::ToolTimeout { .. }
            | AgentEvent::EstimatedPrefill(_)
            | AgentEvent::DecodeDelta(_)
            | AgentEvent::AutoCompacting { .. } => {
                // Not tracked in history
            }
        }

        self.apply_scroll_delta(was_scrolled, lines_before, width);
    }
}

impl Default for HistoryState {
    fn default() -> Self {
        Self::new()
    }
}

/// Ratatui widget that renders the history area.
pub struct HistoryView<'a> {
    pub state: &'a HistoryState,
    #[allow(dead_code)]
    pub width: u16,
}

impl<'a> HistoryView<'a> {
    /// Flatten all cells into display lines, inserting a blank spacer line
    /// after each `UserMessage` cell (unless it is the last cell) to
    /// visually separate user input from the system reply.
    fn flattened_lines(&self, text_width: u16) -> Vec<(usize, ratatui::text::Line<'static>)> {
        let n = self.state.cells.len();
        let mut all_lines: Vec<(usize, ratatui::text::Line<'static>)> = Vec::new();
        for (i, cell) in self.state.cells.iter().enumerate() {
            for line in cell.lines(text_width) {
                all_lines.push((i, line));
            }
            // Insert a blank spacer line after user messages (except the last
            // cell) to visually separate user input from the system reply.
            if matches!(cell, HistoryCell::UserMessage { .. }) && i + 1 < n {
                all_lines.push((i, ratatui::text::Line::raw("")));
            }
        }
        all_lines
    }
}

impl<'a> Widget for HistoryView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text_width = self.state.text_width(area.width, area.height);
        let show_scrollbar = text_width < area.width;
        let all_lines = self.flattened_lines(text_width);

        let visible_height = area.height as usize;
        let total = all_lines.len();
        // Clamp the scroll offset defensively: even if the state's
        // `scroll_offset` is larger than the maximum (e.g. content was
        // removed after scrolling, or the caller didn't clamp), the render
        // must still fill the whole viewport without leaving stale blank
        // rows at the bottom.
        let effective_offset = self
            .state
            .scroll_offset
            .min(total.saturating_sub(visible_height));
        let start = total.saturating_sub(visible_height + effective_offset);
        let end = (start + visible_height).min(total);
        let visible = &all_lines[start..end];

        for (row, (cell_idx, line)) in visible.iter().enumerate() {
            let y = area.y + row as u16;
            let x = area.x;
            let is_selected = self.state.selected == Some(*cell_idx);
            let mut line = line.clone();
            if is_selected {
                line = line.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            line.render(
                Rect {
                    x,
                    y,
                    width: text_width,
                    height: 1,
                },
                buf,
            );
        }

        if show_scrollbar {
            let max_offset = total.saturating_sub(visible_height);
            let top_origin_position = max_offset.saturating_sub(effective_offset);
            let mut scrollbar_state = ScrollbarState::new(total)
                .viewport_content_length(visible_height)
                .position(top_origin_position);
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_symbol("█")
                .track_symbol(Some(" "))
                .render(area, buf, &mut scrollbar_state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::cell::HistoryCell;
    use ratatui::{Terminal, backend::TestBackend};

    fn two_multiline_assistant_cells() -> HistoryState {
        HistoryState {
            cells: vec![
                HistoryCell::AssistantMessage {
                    markdown: "alpha bravo charlie delta echo foxtrot golf hotel".into(),
                },
                HistoryCell::AssistantMessage {
                    markdown: "india juliet kilo lima mike november oscar papa".into(),
                },
            ],
            selected: None,
            scroll_offset: 0,
        }
    }

    #[test]
    fn viewport_anchor_keeps_same_cell_line_through_reflow() {
        let mut state = two_multiline_assistant_cells();
        state.scroll_offset = 2;

        let anchor = state
            .capture_viewport_anchor(20, 3)
            .expect("a non-bottom viewport should have an anchor");
        assert_eq!(
            anchor,
            ViewportAnchor {
                cell_index: 0,
                line_in_cell: 1,
            }
        );

        state.restore_viewport_anchor(anchor, 10, 4);

        assert_eq!(
            state.capture_viewport_anchor(10, 4),
            Some(ViewportAnchor {
                cell_index: 0,
                line_in_cell: 1,
            })
        );
    }

    #[test]
    fn viewport_anchor_is_none_at_bottom() {
        let state = two_multiline_assistant_cells();

        assert_eq!(state.capture_viewport_anchor(20, 3), None);
    }

    #[test]
    fn push_preserves_position_when_scrolled_up() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            80,
        );
        s.scroll_offset = 2;
        s.push(
            HistoryCell::UserMessage {
                text: "second".into(),
            },
            80,
        );
        assert_eq!(s.scroll_offset, 4);
    }

    #[test]
    fn push_keeps_bottom_position_at_zero() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            80,
        );
        s.push(
            HistoryCell::UserMessage {
                text: "second".into(),
            },
            80,
        );
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn push_preserves_position_by_rendered_line_delta() {
        let width = 10;
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            width,
        );
        s.scroll_offset = 2;
        let before = s.flattened_line_count(width);

        s.push(
            HistoryCell::UserMessage {
                text: "a long user message that wraps".into(),
            },
            width,
        );

        let added_lines = s.flattened_line_count(width) - before;
        assert!(
            added_lines > 1,
            "the wrapped message and spacer add multiple lines"
        );
        assert_eq!(s.scroll_offset, 2 + added_lines);
    }

    #[test]
    fn select_up_from_none_selects_second_to_last() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "a".into() }, 80);
        s.push(HistoryCell::UserMessage { text: "b".into() }, 80);
        s.push(HistoryCell::UserMessage { text: "c".into() }, 80);
        s.select_up();
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn select_down_past_last_clears_selection() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "a".into() }, 80);
        s.selected = Some(0);
        s.select_down();
        assert_eq!(s.selected, None);
    }

    #[test]
    fn toggle_fold_selected_toggles() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::ToolCall {
                id: "1".into(),
                name: "t".into(),
                input: serde_json::json!({}),
                state: crate::tui::cell::CallState::Success,
                expanded: false,
            },
            80,
        );
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
        s.push(
            HistoryCell::UserMessage {
                text: "hello".into(),
            },
            80,
        );
        s.push(HistoryCell::Separator { label: None }, 80);
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
    fn push_event_new_cell_preserves_non_bottom_reading_position() {
        let mut s = HistoryState::new();
        for _ in 0..5 {
            s.push(HistoryCell::Separator { label: None }, 80);
        }
        s.scroll_offset = 3;

        s.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            80,
        );

        assert_eq!(
            s.scroll_offset, 4,
            "a one-line new cell should leave the previously visible lines in place"
        );
    }

    #[test]
    fn push_event_new_content_at_bottom_keeps_offset_zero() {
        let mut s = HistoryState::new();

        s.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            80,
        );

        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn push_event_streaming_text_preserves_non_bottom_position_by_line_delta() {
        let width = 20;
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("short".into()), width);
        s.scroll_offset = 2;
        let before = s.flattened_line_count(width);

        s.push_event(
            AgentEvent::AssistantText(" text that wraps onto multiple display lines".into()),
            width,
        );

        let added_lines = s.flattened_line_count(width).saturating_sub(before);
        assert!(
            added_lines > 0,
            "streaming text should have added display lines"
        );
        assert_eq!(s.scroll_offset, 2 + added_lines);
    }

    #[test]
    fn push_event_assistant_text_after_resize_uses_current_width_for_delta() {
        let wide_width = 80;
        let narrow_width = 20;
        let initial = "this assistant response wraps across several narrow display lines";
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText(initial.into()), wide_width);
        s.scroll_offset = 3;

        let before = s.flattened_line_count(narrow_width);
        let expected_before = HistoryCell::from_assistant_text(initial).line_count(narrow_width);
        assert_eq!(
            before, expected_before,
            "history must reflow after a resize"
        );

        s.push_event(
            AgentEvent::AssistantText(" with an additional narrow-width tail".into()),
            narrow_width,
        );

        let after = s.flattened_line_count(narrow_width);
        assert_eq!(s.scroll_offset, 3 + (after - before));
    }

    #[test]
    fn push_event_permission_resolved_reduces_offset_by_removed_lines() {
        let width = 80;
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            width,
        );
        s.scroll_offset = 4;
        let before = s.flattened_line_count(width);

        s.push_event(
            AgentEvent::PermissionResolved {
                request_id: 1,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            },
            width,
        );

        let removed_lines = before - s.flattened_line_count(width);
        assert!(removed_lines > 0, "resolving the request compacts it");
        assert_eq!(s.scroll_offset, 4usize.saturating_sub(removed_lines));
    }

    #[test]
    fn push_event_tool_call_creates_separate_cell() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("text".into()), 80);
        s.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        assert_eq!(s.cells.len(), 2);
    }

    #[test]
    fn push_event_done_endturn_adds_separator() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            80,
        );
        assert_eq!(s.cells.len(), 1);
        assert!(matches!(s.cells[0], HistoryCell::Separator { .. }));
    }

    #[test]
    fn push_event_tool_result_updates_tool_call_state() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        s.push_event(
            AgentEvent::ToolResult {
                id: "1".into(),
                result: ToolResult {
                    content: vec![yi_agent_core::ContentBlock::Text("ok".into())],
                    is_error: false,
                },
            },
            80,
        );
        assert!(matches!(
            &s.cells[0],
            HistoryCell::ToolCall {
                state: crate::tui::cell::CallState::Success,
                ..
            }
        ));
        assert!(matches!(
            s.cells.get(1),
            Some(HistoryCell::ToolResult { .. })
        ));
    }

    #[test]
    fn push_event_permission_request_creates_cell() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                prefix_suggestion: Some("ls".into()),
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        assert_eq!(s.cells.len(), 1);
        assert!(matches!(s.cells[0], HistoryCell::PermissionRequest { .. }));
    }

    #[test]
    fn push_event_permission_resolved_marks_request() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        s.push_event(
            AgentEvent::PermissionResolved {
                request_id: 1,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { resolved, .. } => assert!(*resolved),
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn pending_permission_info_returns_unresolved() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 5,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: Some("git".into()),
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        let info = s.pending_permission_info();
        assert!(info.is_some());
        let (id, name, prefix, _) = info.unwrap();
        assert_eq!(id, 5);
        assert_eq!(name, "bash");
        assert_eq!(prefix, Some("git"));
    }

    #[test]
    fn pending_permission_info_none_when_resolved() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        s.push_event(
            AgentEvent::PermissionResolved {
                request_id: 1,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            },
            80,
        );
        assert!(s.pending_permission_info().is_none());
    }

    #[test]
    fn flattened_lines_inserts_spacer_after_user_message() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "hello".into(),
            },
            80,
        );
        s.push_event(AgentEvent::AssistantText("hi there".into()), 80);

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let lines = view.flattened_lines(80);

        // UserMessage = 1 line, spacer = 1 line, AssistantMessage >= 1 line
        assert!(
            lines.len() >= 3,
            "expected spacer between user and assistant, got {} lines",
            lines.len()
        );

        // The spacer line should be the second line (index 1), belonging to
        // cell index 0 (the UserMessage).
        assert_eq!(lines[1].0, 0, "spacer should be attributed to user cell");
        assert!(lines[1].1.spans.is_empty(), "spacer line should be empty");
    }

    #[test]
    fn flattened_lines_no_spacer_after_last_cell() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "orphan message".into(),
            },
            80,
        );

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let lines = view.flattened_lines(80);

        // Only the user message line, no trailing spacer.
        assert_eq!(
            lines.len(),
            1,
            "no spacer after last cell, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn flattened_lines_no_spacer_between_assistant_cells() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("part1".into()), 80);
        // Force a new AssistantMessage cell by inserting a Separator first.
        s.push(HistoryCell::Separator { label: None }, 80);
        s.push_event(AgentEvent::AssistantText("part2".into()), 80);

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let lines = view.flattened_lines(80);

        // No spacer should be inserted between non-UserMessage cells.
        // Count empty lines attributed to non-user cells — should be zero.
        let spacers = lines
            .iter()
            .filter(|(idx, l)| {
                l.spans.is_empty()
                    && !matches!(s.cells.get(*idx), Some(HistoryCell::UserMessage { .. }))
            })
            .count();
        assert_eq!(spacers, 0, "no spacers between assistant/separator cells");
    }

    #[test]
    fn render_over_scrolled_fills_viewport_without_gaps() {
        // Regression: when scroll_offset exceeds total - visible_height, the
        // render slice shrank below visible_height, leaving stale blank rows
        // at the bottom of the history area.
        let mut s = HistoryState::new();
        // 5 separator lines (no spacers inserted between non-UserMessage
        // cells), viewport height 3 → max useful offset is 2.
        for c in ['a', 'b', 'c', 'd', 'e'] {
            s.push(
                HistoryCell::Separator {
                    label: Some(c.to_string()),
                },
                80,
            );
        }
        // Over-scroll past the maximum.
        s.scroll_offset = 10;

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let area = Rect::new(0, 0, 80, 3);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);

        // Every row in the viewport should have been written by the render
        // (i.e. not left as the default empty ' ' cell with default style).
        // With over-scroll clamped to 2, rows 0..3 should show the top of
        // the content (separators "a", "b", "c"), not blanks.
        for y in 0..3u16 {
            let cell = &buf[(0, y)];
            let sym = cell.symbol();
            assert!(
                !sym.is_empty() && sym != " ",
                "row {y} should not be blank, got {sym:?}"
            );
        }
        // Sanity: the top row should contain the label 'a'.
        let top: String = (0..80u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top.contains('a'), "top row should show label 'a': {top:?}");
    }

    #[test]
    fn render_overflow_reserves_rightmost_column_for_scrollbar() {
        let mut state = HistoryState::new();
        for row in 0..6 {
            state.push(
                HistoryCell::UserMessage {
                    text: format!("row {row}"),
                },
                20,
            );
        }

        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    HistoryView {
                        state: &state,
                        width: area.width,
                    },
                    area,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert!(
            (0..5).any(|y| buffer[(19, y)].symbol() == "█"),
            "an overflowing history should render a scrollbar thumb: {buffer:?}"
        );
        for y in 0..5 {
            let symbol = buffer[(19, y)].symbol();
            assert!(
                matches!(symbol, " " | "█" | "▲" | "▼"),
                "history text must not use the scrollbar column at row {y}: {symbol:?}"
            );
        }
    }

    #[test]
    fn scrollbar_uses_top_origin_while_history_offset_uses_bottom_origin() {
        let mut state = HistoryState::new();
        for row in 0..10 {
            state.push(
                HistoryCell::UserMessage {
                    text: format!("row {row}"),
                },
                20,
            );
        }

        let thumb_rows = |state: &HistoryState| {
            let backend = TestBackend::new(20, 5);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    frame.render_widget(
                        HistoryView {
                            state,
                            width: area.width,
                        },
                        area,
                    );
                })
                .unwrap();
            (0..5u16)
                .filter(|&y| terminal.backend().buffer()[(19, y)].symbol() == "█")
                .collect::<Vec<_>>()
        };

        let bottom_rows = thumb_rows(&state);
        assert_eq!(
            bottom_rows.last(),
            Some(&3),
            "offset zero puts thumb at bottom"
        );

        let text_width = state.text_width(20, 5);
        state.scroll_offset = state.max_scroll_offset(text_width, 5);
        let top_rows = thumb_rows(&state);
        assert_eq!(
            top_rows.first(),
            Some(&1),
            "larger offsets move thumb upward"
        );
    }

    #[test]
    fn text_width_only_reserves_a_column_when_it_can_show_a_scrollbar() {
        let mut state = HistoryState::new();
        state.push(
            HistoryCell::UserMessage {
                text: "one line".into(),
            },
            20,
        );
        assert_eq!(
            state.text_width(20, 5),
            20,
            "fitting content keeps full width"
        );

        for row in 0..5 {
            state.push(
                HistoryCell::UserMessage {
                    text: format!("row {row}"),
                },
                20,
            );
        }
        assert_eq!(state.text_width(20, 5), 19, "overflow reserves one column");
        assert_eq!(
            state.text_width(1, 5),
            1,
            "one-column areas cannot reserve a scrollbar"
        );
        assert_eq!(
            state.text_width(0, 5),
            0,
            "zero-width areas stay zero-width"
        );
    }

    #[test]
    fn streaming_uses_reserved_scrollbar_width_to_preserve_scrolled_position() {
        let area_width = 20;
        let viewport_height = 3;
        let mut state = HistoryState::new();
        for _ in 0..4 {
            state.push(HistoryCell::Separator { label: None }, area_width);
        }
        let text_width = state.text_width(area_width, viewport_height);
        assert_eq!(text_width, 19, "overflow reserves the scrollbar column");

        state.push_event(
            AgentEvent::AssistantText("123456789012345678".into()),
            text_width,
        );
        state.scroll_offset = 2;
        let before = state.flattened_line_count(text_width);

        state.push_event(
            AgentEvent::AssistantText(" wrapping stream content".into()),
            text_width,
        );

        let after = state.flattened_line_count(text_width);
        assert!(
            after > before,
            "the streamed text should wrap at the reserved width"
        );
        assert_eq!(state.scroll_offset, 2 + (after - before));
    }

    #[test]
    fn render_handles_zero_and_one_column_areas() {
        let state = HistoryState {
            cells: vec![HistoryCell::UserMessage {
                text: "history that would overflow a narrow area".into(),
            }],
            selected: None,
            scroll_offset: 0,
        };

        for area in [Rect::new(0, 0, 0, 5), Rect::new(0, 0, 1, 5)] {
            let mut buffer = Buffer::empty(area);
            HistoryView {
                state: &state,
                width: area.width,
            }
            .render(area, &mut buffer);
        }
    }

    #[test]
    fn scroll_up_clamps_at_max_offset() {
        // 5 content lines (no spacers since these are not UserMessages),
        // viewport height 3 → max offset = 2.
        let mut s = HistoryState::new();
        for c in ['a', 'b', 'c', 'd', 'e'] {
            s.push(
                HistoryCell::Separator {
                    label: Some(c.to_string()),
                },
                80,
            );
        }
        // total = 5, visible_height = 3 → max = 2
        let max = s.max_scroll_offset(80, 3);
        assert_eq!(max, 2, "max should be total - visible_height = 5 - 3");

        // Scrolling up by 100 should clamp to 2, not 100.
        s.scroll_up(100, max);
        assert_eq!(s.scroll_offset, 2, "scroll_up should clamp at max");

        // Further scroll_up stays at max.
        s.scroll_up(10, max);
        assert_eq!(s.scroll_offset, 2, "clamped offset should not grow");
    }

    #[test]
    fn scroll_up_zero_max_keeps_offset_zero() {
        // Content shorter than viewport → max offset = 0, scrolling does nothing.
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "x".into() }, 80);
        let max = s.max_scroll_offset(80, 10);
        assert_eq!(max, 0, "max should be 0 when content < viewport");
        s.scroll_up(5, max);
        assert_eq!(s.scroll_offset, 0, "offset should stay 0");
    }

    #[test]
    fn page_scrolling_moves_by_viewport_height() {
        let mut s = HistoryState::new();

        s.scroll_up(12, 100);
        s.scroll_page_down(5);
        assert_eq!(s.scroll_offset, 7);

        s.scroll_page_up(5, 100);
        assert_eq!(s.scroll_offset, 12);
    }

    #[test]
    fn scroll_to_top_clamps_to_content_and_bottom_resets_offset() {
        let width = 80;
        let height = 1;
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            width,
        );
        s.push(
            HistoryCell::UserMessage {
                text: "second".into(),
            },
            width,
        );

        let max = s.max_scroll_offset(width, height);
        assert!(max > 0, "two user messages include a spacer line");

        s.scroll_to_top(width, height);
        assert_eq!(s.scroll_offset, max);

        s.scroll_to_bottom();
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn reconcile_scroll_offset_keeps_bottom_after_resize_then_append() {
        let width = 80;
        let mut s = HistoryState::new();
        s.push(HistoryCell::Separator { label: None }, width);
        s.scroll_offset = 1;

        s.reconcile_scroll_offset(width, 10);
        s.push(HistoryCell::Separator { label: None }, width);

        assert_eq!(s.scroll_offset, 0);
    }
}
