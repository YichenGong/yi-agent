use crate::tui::theme::TuiTheme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use yi_agent_tools::{ManagedProcessSnapshot, ProcessReadResult, ProcessStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTab {
    BashTasks,
    Processes,
    Agents,
}

#[derive(Debug, Clone)]
pub enum ProcessPopup {
    List(ProcessListPopup),
    Detail(ProcessDetailPopup),
    ConfirmKill(ConfirmProcessKill),
}

#[derive(Debug, Clone)]
pub struct ProcessListPopup {
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct ProcessDetailPopup {
    pub process_id: String,
    pub scroll: usize,
    pub scroll_locked: bool,
}

#[derive(Debug, Clone)]
pub struct ConfirmProcessKill {
    pub process_id: String,
}

impl RuntimeTab {
    pub fn next(self) -> Self {
        match self {
            Self::BashTasks => Self::Processes,
            Self::Processes => Self::Agents,
            Self::Agents => Self::BashTasks,
        }
    }
}

impl ProcessListPopup {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn selected_id<'a>(&self, processes: &'a [ManagedProcessSnapshot]) -> Option<&'a str> {
        processes.get(self.selected).map(|p| p.process_id.as_str())
    }
}

impl Default for ProcessListPopup {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessDetailPopup {
    pub fn new(process_id: String) -> Self {
        Self {
            process_id,
            scroll: 0,
            scroll_locked: true,
        }
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        self.scroll_locked = false;
    }

    pub fn scroll_up_from_bottom(&mut self, n: usize, max: usize) {
        if self.scroll_locked {
            self.scroll = max;
        }
        self.scroll_up(n);
    }

    pub fn scroll_down(&mut self, n: usize, max: usize) {
        self.scroll = (self.scroll + n).min(max);
        if self.scroll >= max {
            self.scroll_locked = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_locked = true;
    }
}

fn status_word(status: &ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Starting => "starting",
        ProcessStatus::Running => "running",
        ProcessStatus::Ready => "ready",
        ProcessStatus::Exited { .. } => "exited",
        ProcessStatus::Killed => "killed",
        ProcessStatus::FailedToStart { .. } => "failed",
    }
}

/// Status is an attention signal: in flight is `warn`, ready is `success`,
/// exited is `fg_faint`, killed or failed is `danger`.
fn status_color_with(status: &ProcessStatus, theme: TuiTheme) -> Color {
    match status {
        ProcessStatus::Starting | ProcessStatus::Running => theme.warn,
        ProcessStatus::Ready => theme.success,
        ProcessStatus::Exited { .. } => theme.fg_faint,
        ProcessStatus::Killed | ProcessStatus::FailedToStart { .. } => theme.danger,
    }
}

/// Colors come from [`TuiTheme::DARK`]. Render paths that know the terminal
/// background must use [`render_process_list_popup_with`].
#[allow(dead_code)] // Kept as the pre-theming signature; `app.rs` uses `_with`.
pub fn render_process_list_popup<'a>(
    popup: &'a ProcessListPopup,
    processes: &'a [ManagedProcessSnapshot],
    area: Rect,
) -> Paragraph<'a> {
    render_process_list_popup_with(popup, processes, area, TuiTheme::DARK)
}

/// Theme-aware [`render_process_list_popup`]. Also paints the raised surface.
pub fn render_process_list_popup_with<'a>(
    popup: &'a ProcessListPopup,
    processes: &'a [ManagedProcessSnapshot],
    _area: Rect,
    theme: TuiTheme,
) -> Paragraph<'a> {
    let lines: Vec<Line> = processes
        .iter()
        .enumerate()
        .map(|(i, process)| {
            let name = process.name.as_deref().unwrap_or("-");
            let pid = process
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into());
            let style = if i == popup.selected {
                Style::new().bg(theme.sel_bg).fg(theme.sel_fg)
            } else {
                Style::new().fg(status_color_with(&process.status, theme))
            };
            Line::styled(
                format!(
                    " {:<10} {:<16} pid={:<8} {:>6.1}s {}",
                    process.process_id,
                    name,
                    pid,
                    process.elapsed_sec,
                    status_word(&process.status)
                ),
                style,
            )
        })
        .collect();

    Paragraph::new(lines)
        .style(Style::new().bg(theme.bg_raised))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("processes (Up/Down select, Enter open, Tab switch, q close)"),
        )
}

/// Colors come from [`TuiTheme::DARK`]. Render paths that know the terminal
/// background must use [`render_process_detail_popup_with`].
#[allow(dead_code)] // Kept as the pre-theming signature; `app.rs` uses `_with`.
pub fn render_process_detail_popup(
    popup: &ProcessDetailPopup,
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    area: Rect,
) -> Paragraph<'static> {
    render_process_detail_popup_with(popup, process, output, area, TuiTheme::DARK)
}

/// Theme-aware [`render_process_detail_popup`]. The raised surface is painted
/// under the text so `Clear` leaves a card rather than a hole.
pub fn render_process_detail_popup_with(
    popup: &ProcessDetailPopup,
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    area: Rect,
    theme: TuiTheme,
) -> Paragraph<'static> {
    let scroll = process_detail_scroll(popup, process, output, area);
    Paragraph::new(process_detail_lines_with(
        process, output, area.width, theme,
    ))
    .style(Style::new().bg(theme.bg_raised))
    .alignment(Alignment::Left)
    .scroll((scroll as u16, 0))
}

pub fn process_detail_line_count(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    width: u16,
) -> usize {
    // Folding depends on the width only, so the count is theme-independent and
    // this can stay on the `DARK` default.
    process_detail_lines(process, output, width).len()
}

pub fn process_detail_scroll(
    popup: &ProcessDetailPopup,
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    area: Rect,
) -> usize {
    if popup.scroll_locked {
        process_detail_line_count(process, output, area.width).saturating_sub(area.height as usize)
    } else {
        popup.scroll
    }
}

/// Detail lines with the colors of [`TuiTheme::DARK`].
fn process_detail_lines(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    width: u16,
) -> Vec<Line<'static>> {
    process_detail_lines_with(process, output, width, TuiTheme::DARK)
}

fn process_detail_lines_with(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    width: u16,
    theme: TuiTheme,
) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let mut lines = Vec::new();
    let name = process.name.as_deref().unwrap_or("-");
    // Every line is folded to the popup width: raw `Line::raw` here used to
    // overflow and get right-clipped by ratatui.
    let fold = |text: String, style: Style, lines: &mut Vec<Line<'static>>| {
        for chunk in crate::tui::wrap::wrap_by_display_width(&text, w, "", "") {
            lines.push(Line::styled(chunk, style));
        }
    };
    fold(
        format!(
            " process {} name={} pid={:?} status={} ready={} elapsed={:.1}s exit_code={:?}",
            process.process_id,
            name,
            process.pid,
            status_word(&process.status),
            process.ready,
            process.elapsed_sec,
            process.exit_code
        ),
        Style::new().fg(status_color_with(&process.status, theme)),
        &mut lines,
    );
    fold(format!(" cwd: {}", process.cwd), Style::new(), &mut lines);
    fold(
        format!(" cmd: {}", process.command),
        Style::new(),
        &mut lines,
    );
    fold(
        format!(" on_exit: {:?}", process.on_exit),
        Style::new(),
        &mut lines,
    );
    lines.push(Line::raw(""));
    lines.push(Line::styled("stdout:", Style::new().fg(theme.fg_faint)));
    match output.map(|o| o.stdout.as_str()).filter(|s| !s.is_empty()) {
        Some(stdout) => {
            for line in stdout.lines() {
                fold(line.to_string(), Style::new(), &mut lines);
            }
        }
        None => lines.push(Line::styled("(empty)", Style::new().fg(theme.fg_faint))),
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("stderr:", Style::new().fg(theme.fg_faint)));
    match output.map(|o| o.stderr.as_str()).filter(|s| !s.is_empty()) {
        Some(stderr) => {
            for line in stderr.lines() {
                fold(line.to_string(), Style::new(), &mut lines);
            }
        }
        None => lines.push(Line::styled("(empty)", Style::new().fg(theme.fg_faint))),
    }
    lines.push(Line::raw(""));
    fold(
        " [q] back  [k] kill  [Up/Down] scroll  [f] follow  [Tab] switch".to_string(),
        Style::new().fg(theme.fg_faint),
        &mut lines,
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_tools::OnExitPolicy;

    fn snapshot(id: &str, name: Option<&str>, status: ProcessStatus) -> ManagedProcessSnapshot {
        ManagedProcessSnapshot {
            process_id: id.into(),
            name: name.map(str::to_string),
            pid: Some(1234),
            command: "python -m http.server".into(),
            cwd: "/tmp".into(),
            status,
            ready: true,
            on_exit: OnExitPolicy::Kill,
            exit_code: None,
            elapsed_sec: 1.2,
        }
    }

    #[test]
    fn process_list_popup_selects_and_moves() {
        let mut popup = ProcessListPopup::new();
        let processes = vec![
            snapshot("proc_1", Some("a"), ProcessStatus::Running),
            snapshot("proc_2", Some("b"), ProcessStatus::Ready),
        ];

        assert_eq!(popup.selected_id(&processes), Some("proc_1"));
        popup.move_down(processes.len());
        assert_eq!(popup.selected_id(&processes), Some("proc_2"));
        popup.move_up();
        assert_eq!(popup.selected_id(&processes), Some("proc_1"));
    }

    #[test]
    fn render_process_list_includes_name_status_and_pid() {
        let popup = ProcessListPopup::new();
        let processes = vec![snapshot("proc_1", Some("dev"), ProcessStatus::Ready)];
        let paragraph = render_process_list_popup(&popup, &processes, Rect::new(0, 0, 80, 8));
        let text = format!("{:?}", paragraph);

        assert!(text.contains("dev"));
        assert!(text.contains("ready"));
        assert!(text.contains("1234"));
    }

    #[test]
    fn process_popups_paint_the_theme_raised_surface_on_light() {
        use crate::tui::theme::TuiTheme;
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        let processes = vec![snapshot("proc_1", Some("dev"), ProcessStatus::Running)];
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: "listening\n".into(),
            stderr: String::new(),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Running,
            ready: true,
        };

        // List view: the card must have an edge on a light terminal.
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        Widget::render(
            render_process_list_popup_with(
                &ProcessListPopup::new(),
                &processes,
                area,
                TuiTheme::LIGHT,
            ),
            area,
            &mut buf,
        );
        assert_eq!(buf[(59, 7)].bg, TuiTheme::LIGHT.bg_raised);
        assert_ne!(buf[(59, 7)].bg, Color::Reset);

        // Detail view: same guarantee.
        let detail_area = Rect::new(0, 0, 60, 10);
        let mut detail_buf = Buffer::empty(detail_area);
        Widget::render(
            render_process_detail_popup_with(
                &ProcessDetailPopup::new("proc_1".into()),
                &processes[0],
                Some(&output),
                detail_area,
                TuiTheme::LIGHT,
            ),
            detail_area,
            &mut detail_buf,
        );
        assert_eq!(detail_buf[(59, 9)].bg, TuiTheme::LIGHT.bg_raised);
    }

    /// The selected row used a hardcoded ANSI `Blue` background.
    #[test]
    fn process_list_selected_row_uses_the_theme_selection() {
        use crate::tui::theme::TuiTheme;
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        let processes = vec![
            snapshot("proc_1", Some("dev"), ProcessStatus::Running),
            snapshot("proc_2", Some("web"), ProcessStatus::Ready),
        ];
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        Widget::render(
            render_process_list_popup_with(
                &ProcessListPopup::new(),
                &processes,
                area,
                TuiTheme::LIGHT,
            ),
            area,
            &mut buf,
        );
        assert_eq!(buf[(1, 1)].bg, TuiTheme::LIGHT.sel_bg);
        assert_ne!(buf[(1, 1)].bg, Color::Blue);
        assert_eq!(buf[(1, 2)].bg, TuiTheme::LIGHT.bg_raised);
    }

    fn assert_lines_fit(lines: &[Line<'static>], width: u16, what: &str) {
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= width as usize,
                "{what}: line {i} is {w} cols, width is {width}"
            );
        }
    }

    #[test]
    fn detail_lines_fold_long_command_and_output_to_width() {
        let cjk = "这是一条很长的中文进程命令需要按显示宽度折行否则右侧会被截断掉看不见";
        let mut process = snapshot("proc_1", Some("dev"), ProcessStatus::Running);
        process.command = format!("python -c '{cjk}' --with-a-really-long-argument-value");
        process.cwd = format!("/Users/someone/{cjk}");
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: format!("{cjk}\n{cjk}\n"),
            stderr: format!("warning: {cjk}\n"),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Running,
            ready: true,
        };

        for width in [20u16, 40, 80] {
            assert_lines_fit(
                &process_detail_lines(&process, Some(&output), width),
                width,
                "process detail",
            );
        }
    }

    #[test]
    fn process_detail_long_output_is_folded_not_clipped() {
        let cjk = "这是一条很长的中文进程输出内容需要按显示宽度折行否则右侧会被截断掉看不见";
        let process = snapshot("proc_1", Some("dev"), ProcessStatus::Running);
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: format!("{cjk}\n"),
            stderr: String::new(),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Running,
            ready: true,
        };

        let rendered: String = process_detail_lines(&process, Some(&output), 40)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        let squashed: String = rendered.chars().filter(|c| !c.is_whitespace()).collect();
        for ch in cjk.chars().filter(|c| !c.is_whitespace()) {
            assert!(squashed.contains(ch), "detail output lost {ch:?}");
        }
    }

    #[test]
    fn detail_lines_include_stdout_and_stderr() {
        let process = snapshot("proc_1", Some("dev"), ProcessStatus::Ready);
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: "listening\n".into(),
            stderr: "warn\n".into(),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Ready,
            ready: true,
        };

        let lines = process_detail_lines(&process, Some(&output), 80);
        let text = format!("{:?}", lines);

        assert!(text.contains("listening"));
        assert!(text.contains("warn"));
        assert!(text.contains("proc_1"));
        assert!(text.contains("elapsed=1.2s"));
        assert!(text.contains("exit_code=None"));
    }

    #[test]
    fn detail_render_scroll_locked_follows_bottom() {
        let process = snapshot("proc_1", Some("dev"), ProcessStatus::Ready);
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: (0..20).map(|i| format!("line-{i}\n")).collect(),
            stderr: String::new(),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Ready,
            ready: true,
        };

        let popup = ProcessDetailPopup::new("proc_1".into());
        let scroll = process_detail_scroll(&popup, &process, Some(&output), Rect::new(0, 0, 80, 5));

        assert!(scroll > 0, "locked detail popup should render at bottom");
    }

    #[test]
    fn detail_scroll_up_from_follow_moves_one_line_above_bottom() {
        let process = snapshot("proc_1", Some("dev"), ProcessStatus::Ready);
        let output = ProcessReadResult {
            process_id: "proc_1".into(),
            name: Some("dev".into()),
            stdout: (0..20).map(|i| format!("line-{i}\n")).collect(),
            stderr: String::new(),
            next_cursor: 10,
            truncated: false,
            status: ProcessStatus::Ready,
            ready: true,
        };
        let max_scroll = process_detail_scroll(
            &ProcessDetailPopup::new("proc_1".into()),
            &process,
            Some(&output),
            Rect::new(0, 0, 80, 5),
        );
        let mut popup = ProcessDetailPopup::new("proc_1".into());

        popup.scroll_up_from_bottom(1, max_scroll);

        assert!(!popup.scroll_locked);
        assert_eq!(popup.scroll, max_scroll.saturating_sub(1));
    }
}
