//! The subagent tab of the runtime popup: the current root's children, and a
//! detail level that later work fills with the child's live trace.
//!
//! This module owns only state and rendering. Fetching the child list needs a
//! socket, so the caller resolves it and hands the popup the result; that keeps
//! this file testable without a daemon.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// One child of the root the popup is looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentListItem {
    pub task_id: String,
    /// What the child is working on. Absent when the source only knows task
    /// identity and state, in which case the id is rendered instead.
    pub objective: Option<String>,
    pub state: String,
    pub last_step: Option<String>,
    /// True while the child can still make progress; a finished child stays in
    /// the list but stops advertising a current step.
    pub active: bool,
}

impl SubagentListItem {
    pub fn is_finished(&self) -> bool {
        !self.active
    }
}

#[derive(Debug, Clone)]
pub enum TracePopup {
    Detail(TraceDetailPopup),
}

#[derive(Debug, Clone)]
pub struct TraceListPopup {
    pub selected: usize,
}

impl TraceListPopup {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self, len: usize) {
        if len > 0 && self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn selected_id<'a>(&self, items: &'a [SubagentListItem]) -> Option<&'a str> {
        items.get(self.selected).map(|item| item.task_id.as_str())
    }
}

impl Default for TraceListPopup {
    fn default() -> Self {
        Self::new()
    }
}

/// The detail level. Task 11 renders the live trace here; for now it carries the
/// task identity and the drill-down stack so the navigation can be tested.
#[derive(Debug, Clone)]
pub struct TraceDetailPopup {
    /// The stack of tasks entered, outermost first. `last()` is what is shown.
    stack: Vec<String>,
}

impl TraceDetailPopup {
    pub fn new(task_id: String) -> Self {
        Self {
            stack: vec![task_id],
        }
    }

    pub fn task_id(&self) -> &str {
        self.stack
            .last()
            .map(String::as_str)
            .expect("a detail popup always has a task")
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Drill into a descendant, remembering where to return to.
    ///
    /// The subagent tab's drill-down lands in the task that follows; the stack
    /// is already enforced by `pop` and covered by its test.
    #[allow(dead_code)]
    pub fn push(&mut self, task_id: String) {
        self.stack.push(task_id);
    }

    /// Step back toward the list. Returns false when the detail is at its root,
    /// which tells the caller to fall back to the list.
    pub fn pop(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.stack.pop();
        true
    }
}

/// The children of the current root, in the order the daemon reported them
/// (creation order), with the root itself excluded by the caller.
#[derive(Debug, Clone, Default)]
pub struct SubagentListState {
    pub items: Vec<SubagentListItem>,
}

impl SubagentListState {
    pub fn new(items: Vec<SubagentListItem>) -> Self {
        Self { items }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, task_id: &str) -> Option<&SubagentListItem> {
        self.items.iter().find(|item| item.task_id == task_id)
    }
}

fn state_color(state: &str) -> Color {
    match state {
        "completed" | "completed_no_changes" => Color::Green,
        "failed" | "cancelled" => Color::Red,
        "running" | "waiting_for_children" => Color::Yellow,
        _ => Color::DarkGray,
    }
}

pub fn render_subagent_list(
    popup: &TraceListPopup,
    children: &SubagentListState,
    _area: Rect,
) -> Paragraph<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if children.is_empty() {
        lines.push(Line::raw("暂无子 agent"));
    }
    for (index, item) in children.items.iter().enumerate() {
        let marker = if index == popup.selected { "> " } else { "  " };
        let step = match (!item.is_finished(), item.last_step.as_deref()) {
            (true, Some(step)) => format!("  {step}"),
            _ => String::new(),
        };
        let label = item
            .objective
            .clone()
            .unwrap_or_else(|| item.task_id.clone());
        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                format!("[{}] ", item.state),
                Style::default().fg(state_color(&item.state)),
            ),
            Span::raw(label),
            Span::styled(step, Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::styled(
            format!("    {}", item.task_id),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("子 agent (Enter 查看, Esc 关闭)"),
    )
}

/// The detail level: which child is open, and where it sits in the drill-down.
///
/// The child's live trace is rendered here once the trace view lands; this
/// panel is the identity and context it will be shown against.
pub fn render_subagent_detail(
    popup: &TraceDetailPopup,
    children: &SubagentListState,
    area: Rect,
) -> Paragraph<'static> {
    let task_id = popup.task_id().to_string();
    let mut lines = Vec::new();
    match children.get(&task_id) {
        Some(item) => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("[{}] ", item.state),
                    Style::default().fg(state_color(&item.state)),
                ),
                Span::raw(
                    item.objective
                        .clone()
                        .unwrap_or_else(|| item.task_id.clone()),
                ),
            ]));
        }
        None => lines.push(Line::raw("该任务不在当前子 agent 列表中")),
    }
    lines.push(Line::styled(
        task_id.clone(),
        Style::default().fg(Color::DarkGray),
    ));
    if popup.depth() > 1 {
        lines.push(Line::styled(
            format!("下钻层级 {}", popup.depth()),
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "轨迹视图即将接入",
        Style::default().fg(Color::DarkGray),
    ));
    let _ = area;
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("子 agent 详情 (Esc 返回)"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(task_id: &str, state: &str, active: bool) -> SubagentListItem {
        SubagentListItem {
            task_id: task_id.into(),
            objective: Some(format!("objective of {task_id}")),
            state: state.into(),
            last_step: None,
            active,
        }
    }

    #[test]
    fn the_subagent_list_moves_within_its_bounds() {
        let mut popup = TraceListPopup::new();
        popup.move_up();
        assert_eq!(popup.selected, 0);
        popup.move_down(3);
        assert_eq!(popup.selected, 1);
        popup.move_down(3);
        popup.move_down(3);
        assert_eq!(popup.selected, 2, "never past the last row");
        popup.move_down(0);
        assert_eq!(popup.selected, 2, "an empty list cannot move the selection");
    }

    #[test]
    fn selected_id_addresses_the_item_under_the_cursor() {
        let popup = TraceListPopup { selected: 1 };
        let items = vec![item("a", "running", true), item("b", "completed", false)];
        assert_eq!(popup.selected_id(&items), Some("b"));
        let empty: Vec<SubagentListItem> = Vec::new();
        assert_eq!(popup.selected_id(&empty), None);
    }

    #[test]
    fn the_detail_returns_to_the_parent_agent_before_the_list() {
        let mut detail = TraceDetailPopup::new("parent".into());
        assert_eq!(detail.task_id(), "parent");
        assert_eq!(detail.depth(), 1);

        detail.push("grandchild".into());
        assert_eq!(detail.task_id(), "grandchild");
        assert_eq!(detail.depth(), 2);

        assert!(detail.pop(), "returns to the parent agent");
        assert_eq!(detail.task_id(), "parent");
        assert!(!detail.pop(), "the outermost level falls back to the list");
        assert_eq!(detail.task_id(), "parent", "and does not lose its task");
    }

    #[test]
    fn a_finished_child_stays_listed_but_reports_no_current_step() {
        let running = item("a", "running", true);
        let done = item("b", "completed", false);
        assert!(!running.is_finished());
        assert!(done.is_finished());

        let state = SubagentListState::new(vec![running, done]);
        assert_eq!(state.len(), 2, "a finished child is not dropped");
        assert!(state.get("b").is_some());
    }

    #[test]
    fn the_detail_shows_the_open_task_and_its_state() {
        let state = SubagentListState::new(vec![item("task-1", "running", true)]);
        let detail = TraceDetailPopup::new("task-1".into());
        let text = format!(
            "{:?}",
            render_subagent_detail(&detail, &state, Rect::new(0, 0, 80, 10))
        );
        assert!(text.contains("task-1"));
        assert!(text.contains("running"));
        assert!(text.contains("objective of task-1"));
    }

    #[test]
    fn the_detail_says_so_when_the_task_is_not_in_the_list() {
        let state = SubagentListState::new(vec![item("task-1", "running", true)]);
        let detail = TraceDetailPopup::new("task-9".into());
        let text = format!(
            "{:?}",
            render_subagent_detail(&detail, &state, Rect::new(0, 0, 80, 10))
        );
        assert!(text.contains("task-9"));
        assert!(text.contains("不在当前子 agent 列表"));
    }

    #[test]
    fn the_list_renders_the_objective_state_and_id_of_every_child() {
        let mut finished = item("task-2", "completed", false);
        finished.last_step = Some("should not render as a current step".into());
        let state = SubagentListState::new(vec![item("task-1", "running", true), finished]);
        let text = format!(
            "{:?}",
            render_subagent_list(&TraceListPopup::new(), &state, Rect::new(0, 0, 80, 10))
        );
        assert!(text.contains("objective of task-1"));
        assert!(text.contains("task-2"));
        assert!(text.contains("completed"));
        assert!(
            !text.contains("should not render as a current step"),
            "a finished child must not advertise a current step"
        );
    }

    #[test]
    fn an_empty_list_says_so_instead_of_rendering_a_blank_box() {
        let state = SubagentListState::new(Vec::new());
        let text = format!(
            "{:?}",
            render_subagent_list(&TraceListPopup::new(), &state, Rect::new(0, 0, 80, 6))
        );
        assert!(text.contains("暂无子 agent"));
    }
}
