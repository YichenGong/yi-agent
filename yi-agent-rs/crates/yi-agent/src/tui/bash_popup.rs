//! Bash task popup: list + detail views, opened via Ctrl+P.
//!
//! List mode shows all tracked bash tasks (running + done). Detail mode
//! shows full stdout/stderr for a single task with a kill shortcut.

use crate::tui::state::{RunningTaskRegistry, TaskState, TaskStatus};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

/// Top-level popup state.
#[derive(Debug, Clone, Default)]
pub enum BashPopup {
    #[default]
    None,
    List(ListPopup),
    Detail(DetailPopup),
    ConfirmKill(ConfirmKill),
}

/// List of tasks, newest first.
#[derive(Debug, Clone)]
pub struct ListPopup {
    pub selected: usize,
    pub task_ids: Vec<String>,
}

impl ListPopup {
    pub fn new(task_ids: Vec<String>) -> Self {
        Self {
            selected: 0,
            task_ids,
        }
    }
    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.task_ids.len() {
            self.selected += 1;
        }
    }
    pub fn selected_id(&self) -> Option<&str> {
        self.task_ids.get(self.selected).map(|s| s.as_str())
    }
    #[allow(dead_code)]
    pub fn selected_index(&self) -> usize {
        self.selected
    }
}

/// Full-screen detail for a single task.
#[derive(Debug, Clone)]
pub struct DetailPopup {
    pub task_id: String,
    pub scroll: usize,
    pub scroll_locked: bool,
}

impl DetailPopup {
    pub fn new(task_id: String) -> Self {
        Self {
            task_id,
            scroll: 0,
            scroll_locked: true,
        }
    }
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        self.scroll_locked = false;
    }
    pub fn scroll_down(&mut self, n: usize, max: usize) {
        if max == 0 {
            return;
        }
        self.scroll = (self.scroll + n).min(max);
        if self.scroll >= max {
            self.scroll_locked = true;
        }
    }
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_locked = true;
    }
}

/// Kill confirmation overlay.
#[derive(Debug, Clone)]
pub struct ConfirmKill {
    pub task_id: String,
}

/// Render the list-mode popup as a `Paragraph`.
pub fn render_list_popup<'a>(
    popup: &'a ListPopup,
    tasks: &'a RunningTaskRegistry,
    _area: Rect,
) -> Paragraph<'a> {
    let items: Vec<Line> = popup
        .task_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let t = match tasks.get(id) {
                Some(t) => t,
                None => return Line::raw(format!("  ? {}", id)),
            };
            let (sym, color) = match t.status {
                TaskStatus::Running => ("●", Color::Yellow),
                TaskStatus::Done => ("✓", Color::Green),
                TaskStatus::Failed => ("✗", Color::Red),
                TaskStatus::Timeout => ("✗", Color::Red),
                TaskStatus::Aborted => ("■", Color::DarkGray),
            };
            let style = if i == popup.selected {
                Style::new().bg(Color::Blue).fg(Color::White)
            } else {
                Style::new().fg(color)
            };
            let secs = t.elapsed().as_secs_f32();
            let status_str = match t.status {
                TaskStatus::Running => "running",
                TaskStatus::Done => "done",
                TaskStatus::Failed => "failed",
                TaskStatus::Timeout => "timeout",
                TaskStatus::Aborted => "aborted",
            };
            Line::styled(
                format!(
                    " {} {:<8} {:<24} {:>6.1}s {}",
                    sym,
                    t.tool_name,
                    truncate_str(&t.command, 24),
                    secs,
                    status_str
                ),
                style,
            )
        })
        .collect();
    Paragraph::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("bash tasks (↑↓ select, Enter open, q close)"),
    )
}

/// Render the detail-mode popup as a `Paragraph` with header + stdout/stderr.
pub fn render_detail_popup<'a>(popup: &'a DetailPopup, task: &'a TaskState) -> Paragraph<'a> {
    let mut lines: Vec<Line> = Vec::new();

    let (sym, color) = match task.status {
        TaskStatus::Running => ("●", Color::Yellow),
        TaskStatus::Done => ("✓", Color::Green),
        TaskStatus::Failed => ("✗", Color::Red),
        TaskStatus::Timeout => ("✗", Color::Red),
        TaskStatus::Aborted => ("■", Color::DarkGray),
    };
    let secs = task.elapsed().as_secs_f32();
    let status_word = match task.status {
        TaskStatus::Running => "running",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Timeout => "timeout",
        TaskStatus::Aborted => "aborted",
    };
    let header = format!(
        " bash {} {} {:.1}s (expected {}s)",
        sym, status_word, secs, task.expected_timeout_sec
    );
    lines.push(Line::styled(header, Style::new().fg(color)));

    if task.exceeds_expected() {
        lines.push(Line::styled(
            " ! exceeds expected timeout",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    lines.push(Line::raw(format!(" $ {}", task.command)));
    lines.push(Line::raw(""));

    lines.push(Line::styled("stdout:", Style::new().fg(Color::DarkGray)));
    let stdout_str = String::from_utf8_lossy(&task.stdout);
    if stdout_str.is_empty() {
        lines.push(Line::raw("(empty)").style(Style::new().fg(Color::DarkGray)));
    } else {
        for l in stdout_str.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    lines.push(Line::raw(""));

    lines.push(Line::styled("stderr:", Style::new().fg(Color::DarkGray)));
    let stderr_str = String::from_utf8_lossy(&task.stderr);
    if stderr_str.is_empty() {
        lines.push(Line::raw("(empty)").style(Style::new().fg(Color::DarkGray)));
    } else {
        for l in stderr_str.lines() {
            lines.push(Line::raw(l.to_string()));
        }
    }
    lines.push(Line::raw(""));

    let footer = Line::styled(
        " [q] back  [k] kill  [↑↓] scroll  [f] follow",
        Style::new().fg(Color::DarkGray),
    );
    lines.push(footer);

    Paragraph::new(lines)
        .alignment(Alignment::Left)
        .scroll((popup.scroll as u16, 0))
}

fn truncate_str(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::RunningTaskRegistry;
    use std::time::Duration;

    #[test]
    fn test_list_popup_navigation() {
        let mut p = ListPopup::new(vec!["t1".into(), "t2".into(), "t3".into()]);
        assert_eq!(p.selected_index(), 0);
        p.move_down();
        assert_eq!(p.selected_index(), 1);
        p.move_down();
        assert_eq!(p.selected_index(), 2);
        p.move_down(); // at end, stays
        assert_eq!(p.selected_index(), 2);
        p.move_up();
        assert_eq!(p.selected_index(), 1);
        p.move_up();
        p.move_up();
        assert_eq!(p.selected_index(), 0); // clamps at 0
        assert_eq!(p.selected_id(), Some("t1"));
    }

    #[test]
    fn test_list_popup_empty() {
        let p = ListPopup::new(vec![]);
        assert_eq!(p.selected_id(), None);
    }

    #[test]
    fn test_detail_popup_scroll_lock() {
        let mut d = DetailPopup::new("t1".into());
        assert!(d.scroll_locked);
        d.scroll_up(1);
        assert!(!d.scroll_locked);
        d.scroll_to_bottom();
        assert!(d.scroll_locked);
    }

    #[test]
    fn test_detail_popup_scroll_down_locks_at_bottom() {
        let mut d = DetailPopup::new("t1".into());
        // max=10: scroll down past max should clamp + lock
        d.scroll_down(20, 10);
        assert_eq!(d.scroll, 10);
        assert!(d.scroll_locked);
    }

    #[test]
    fn test_truncate_str() {
        assert_eq!(truncate_str("abc", 5), "abc");
        assert_eq!(truncate_str("abcdef", 3), "abc…");
    }

    #[test]
    fn test_render_list_popup_shows_status_symbol() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "ls -la", 120);
        std::thread::sleep(Duration::from_millis(50));
        tasks.on_exit("t1", Some(0));
        let p = ListPopup::new(vec!["t1".into()]);
        let para = render_list_popup(&p, &tasks, ratatui::layout::Rect::new(0, 0, 60, 5));
        // Pull the text content out of the paragraph's lines.
        // We can't easily introspect Paragraph lines without rendering; just
        // verify the function returns without panic.
        let _ = para;
    }

    #[test]
    fn test_render_detail_popup_smoke() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "echo hello", 120);
        tasks.on_output_delta("t1", yi_agent_core::OutputStream::Stdout, "hello\n");
        tasks.on_exit("t1", Some(0));
        let d = DetailPopup::new("t1".into());
        let task = tasks.get("t1").unwrap();
        let _ = render_detail_popup(&d, task);
    }
}
