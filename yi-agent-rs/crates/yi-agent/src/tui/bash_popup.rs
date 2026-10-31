//! Bash task popup: list + detail views, opened via Ctrl+P.
//!
//! List mode shows all tracked bash tasks (running + done). Detail mode
//! shows full stdout/stderr for a single task with a kill shortcut.

use crate::tui::state::{RunningTaskRegistry, TaskState, TaskStatus};
use crate::tui::theme::TuiTheme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
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
///
/// Colors come from [`TuiTheme::DARK`]. Render paths that know the terminal
/// background must use [`render_list_popup_with`].
#[allow(dead_code)] // Kept as the pre-theming signature; `app.rs` uses `_with`.
pub fn render_list_popup<'a>(
    popup: &'a ListPopup,
    tasks: &'a RunningTaskRegistry,
    area: Rect,
) -> Paragraph<'a> {
    render_list_popup_with(popup, tasks, area, TuiTheme::DARK)
}

/// Theme-aware [`render_list_popup`]. Also paints the raised surface, so the
/// card has an edge on a light terminal and `Clear` leaves no ghost.
pub fn render_list_popup_with<'a>(
    popup: &'a ListPopup,
    tasks: &'a RunningTaskRegistry,
    area: Rect,
    theme: TuiTheme,
) -> Paragraph<'a> {
    // `area.width` is the real budget (the block borders take 2 columns).
    // Rows are padded/truncated to it: a wider `Line` is right-clipped by
    // ratatui, hiding the status column on a narrow terminal.
    let row_width = (area.width as usize).saturating_sub(2);
    let items: Vec<Line> = popup
        .task_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let t = match tasks.get(id) {
                Some(t) => t,
                None => {
                    return Line::raw(crate::tui::wrap::truncate_to_width(
                        &format!("  ? {}", id),
                        row_width,
                    ));
                }
            };
            let (sym, color) = match t.status {
                TaskStatus::Running => ("●", theme.warn),
                TaskStatus::Done => ("✓", theme.success),
                TaskStatus::Failed => ("✗", theme.danger),
                TaskStatus::Timeout => ("✗", theme.danger),
                TaskStatus::Aborted => ("■", theme.fg_faint),
            };
            let style = if i == popup.selected {
                Style::new().bg(theme.sel_bg).fg(theme.sel_fg)
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
                crate::tui::wrap::truncate_to_width(
                    &format!(
                        " {} {:<8} {:<24} {:>6.1}s {}",
                        sym,
                        t.tool_name,
                        truncate_str(&t.command, 24),
                        secs,
                        status_str
                    ),
                    row_width,
                ),
                style,
            )
        })
        .collect();
    Paragraph::new(items)
        .style(Style::new().bg(theme.bg_raised))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("bash tasks (↑↓ select, Enter open, q close)"),
        )
}

/// Render the detail-mode popup as a `Paragraph` with header + stdout/stderr.
///
/// Colors come from [`TuiTheme::DARK`]. Render paths that know the terminal
/// background must use [`render_detail_popup_with`].
#[allow(dead_code)] // Kept as the pre-theming signature; `app.rs` uses `_with`.
pub fn render_detail_popup(
    popup: &DetailPopup,
    task: &TaskState,
    area: Rect,
) -> Paragraph<'static> {
    render_detail_popup_with(popup, task, area, TuiTheme::DARK)
}

/// Theme-aware [`render_detail_popup`]. The raised surface is painted under the
/// text so `Clear` leaves a card rather than a hole in the terminal.
pub fn render_detail_popup_with(
    popup: &DetailPopup,
    task: &TaskState,
    area: Rect,
    theme: TuiTheme,
) -> Paragraph<'static> {
    Paragraph::new(detail_lines_with(task, area.width, theme))
        .style(Style::new().bg(theme.bg_raised))
        .alignment(Alignment::Left)
        .scroll((popup.scroll as u16, 0))
}

/// Number of visual detail lines at the given terminal width.
pub fn detail_line_count(task: &TaskState, width: u16) -> usize {
    // Folding depends on the width only, so the line count is theme-independent
    // and this can stay on the `DARK` default.
    detail_lines(task, width).len()
}

/// Detail lines with the colors of [`TuiTheme::DARK`].
fn detail_lines(task: &TaskState, width: u16) -> Vec<Line<'static>> {
    detail_lines_with(task, width, TuiTheme::DARK)
}

fn detail_lines_with(task: &TaskState, width: u16, theme: TuiTheme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    let (sym, color) = match task.status {
        TaskStatus::Running => ("●", theme.warn),
        TaskStatus::Done => ("✓", theme.success),
        TaskStatus::Failed => ("✗", theme.danger),
        TaskStatus::Timeout => ("✗", theme.danger),
        TaskStatus::Aborted => ("■", theme.fg_faint),
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
            Style::new().fg(theme.danger).add_modifier(Modifier::BOLD),
        ));
    }

    lines.extend(wrap_text(&task.command, width, " $ ", "   "));
    lines.push(Line::raw(""));

    lines.push(Line::styled("stdout:", Style::new().fg(theme.fg_faint)));
    let stdout_str = String::from_utf8_lossy(&task.stdout);
    if stdout_str.is_empty() {
        lines.push(Line::raw("(empty)").style(Style::new().fg(theme.fg_faint)));
    } else {
        lines.extend(wrap_text(&stdout_str, width, "", ""));
    }
    lines.push(Line::raw(""));

    lines.push(Line::styled("stderr:", Style::new().fg(theme.fg_faint)));
    let stderr_str = String::from_utf8_lossy(&task.stderr);
    if stderr_str.is_empty() {
        lines.push(Line::raw("(empty)").style(Style::new().fg(theme.fg_faint)));
    } else {
        lines.extend(wrap_text(&stderr_str, width, "", ""));
    }
    lines.push(Line::raw(""));

    let footer = Line::styled(
        " [q] back  [k] kill  [↑↓] scroll  [f] follow",
        Style::new().fg(theme.fg_faint),
    );
    lines.push(footer);

    lines
}

/// Wrap text by terminal display width while preserving explicit newlines.
///
/// Delegates to the shared whitespace-preserving wrapper so the bash detail
/// view and the permission prompt cannot drift apart.
fn wrap_text(
    text: &str,
    width: u16,
    first_prefix: &str,
    continuation_prefix: &str,
) -> Vec<Line<'static>> {
    crate::tui::wrap::wrap_by_display_width(text, width as usize, first_prefix, continuation_prefix)
        .into_iter()
        .map(Line::raw)
        .collect()
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
    use crate::tui::theme::TuiTheme;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use ratatui::widgets::Widget;
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

    /// Popup cards used `Clear` alone, which leaves the terminal's own
    /// background: on light terminals the card has no edge at all.
    #[test]
    fn detail_popup_paints_the_theme_raised_surface() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "echo hello", 120);
        tasks.on_exit("t1", Some(0));
        let task = tasks.get("t1").unwrap();
        let popup = DetailPopup::new("t1".into());
        let area = Rect::new(0, 0, 60, 10);
        let para = render_detail_popup_with(&popup, task, area, TuiTheme::LIGHT);
        let mut buf = Buffer::empty(area);
        Widget::render(para, area, &mut buf);
        assert_eq!(buf[(59, 9)].bg, TuiTheme::LIGHT.bg_raised);
    }

    /// The selected row used a hardcoded ANSI `Blue` background, whose actual
    /// shade is the terminal's choice: on a light palette it reads as "some
    /// other blue", not as the selection.
    #[test]
    fn list_popup_selected_row_uses_the_theme_selection() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "echo hello", 120);
        let p = ListPopup::new(vec!["t1".into(), "t2".into()]);
        let area = Rect::new(0, 0, 60, 6);
        let mut buf = Buffer::empty(area);
        Widget::render(
            render_list_popup_with(&p, &tasks, area, TuiTheme::LIGHT),
            area,
            &mut buf,
        );
        // Row 0 is the top border, so the first (selected) task sits on row 1.
        assert_eq!(buf[(1, 1)].bg, TuiTheme::LIGHT.sel_bg);
        assert_ne!(buf[(1, 1)].bg, Color::Blue);
        // The row below is not selected: it keeps the raised surface.
        assert_eq!(buf[(1, 2)].bg, TuiTheme::LIGHT.bg_raised);
    }

    #[test]
    fn test_render_detail_popup_smoke() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "echo hello", 120);
        tasks.on_output_delta("t1", yi_agent_core::OutputStream::Stdout, "hello\n");
        tasks.on_exit("t1", Some(0));
        let d = DetailPopup::new("t1".into());
        let task = tasks.get("t1").unwrap();
        let _ = render_detail_popup(&d, task, Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn detail_wraps_long_command_stdout_stderr_and_cjk() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call(
            "t1",
            "bash",
            "printf 'command content must remain fully visible 一二三四五六七'",
            120,
        );
        tasks.on_output_delta(
            "t1",
            yi_agent_core::OutputStream::Stdout,
            "stdout-content-must-remain-fully-visible\n",
        );
        tasks.on_output_delta(
            "t1",
            yi_agent_core::OutputStream::Stderr,
            "stderr-content-must-remain-fully-visible\n",
        );

        let detail = DetailPopup::new("t1".into());
        let task = tasks.get("t1").unwrap();
        let area = Rect::new(0, 0, 16, 20);
        let mut buffer = Buffer::empty(area);
        render_detail_popup(&detail, task, area).render(area, &mut buffer);
        let mut rendered = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
        }

        assert!(
            rendered.contains("visible"),
            "missing command: {rendered:?}"
        );
        assert!(rendered.contains("stdout-content-must-remain-fully-visible"));
        assert!(rendered.contains("stderr-content-must-remain-fully-visible"));
        for ch in "一二三四五六七".chars() {
            assert!(rendered.contains(ch), "missing {ch} in {rendered:?}");
        }
    }
}
