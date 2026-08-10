use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use yi_agent_tools::{ManagedProcessSnapshot, ProcessReadResult, ProcessStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTab {
    BashTasks,
    Processes,
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
            Self::Processes => Self::BashTasks,
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

fn status_color(status: &ProcessStatus) -> Color {
    match status {
        ProcessStatus::Starting | ProcessStatus::Running => Color::Yellow,
        ProcessStatus::Ready => Color::Green,
        ProcessStatus::Exited { .. } => Color::DarkGray,
        ProcessStatus::Killed | ProcessStatus::FailedToStart { .. } => Color::Red,
    }
}

pub fn render_process_list_popup<'a>(
    popup: &'a ProcessListPopup,
    processes: &'a [ManagedProcessSnapshot],
    _area: Rect,
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
                Style::new().bg(Color::Blue).fg(Color::White)
            } else {
                Style::new().fg(status_color(&process.status))
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

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("processes (Up/Down select, Enter open, Tab switch, q close)"),
    )
}

pub fn render_process_detail_popup(
    popup: &ProcessDetailPopup,
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    area: Rect,
) -> Paragraph<'static> {
    Paragraph::new(process_detail_lines(process, output, area.width))
        .alignment(Alignment::Left)
        .scroll((popup.scroll as u16, 0))
}

pub fn process_detail_line_count(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    width: u16,
) -> usize {
    process_detail_lines(process, output, width).len()
}

pub fn process_detail_lines(
    process: &ManagedProcessSnapshot,
    output: Option<&ProcessReadResult>,
    _width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let name = process.name.as_deref().unwrap_or("-");
    lines.push(Line::styled(
        format!(
            " process {} name={} pid={:?} status={} ready={}",
            process.process_id,
            name,
            process.pid,
            status_word(&process.status),
            process.ready
        ),
        Style::new().fg(status_color(&process.status)),
    ));
    lines.push(Line::raw(format!(" cwd: {}", process.cwd)));
    lines.push(Line::raw(format!(" cmd: {}", process.command)));
    lines.push(Line::raw(format!(" on_exit: {:?}", process.on_exit)));
    lines.push(Line::raw(""));
    lines.push(Line::styled("stdout:", Style::new().fg(Color::DarkGray)));
    match output.map(|o| o.stdout.as_str()).filter(|s| !s.is_empty()) {
        Some(stdout) => lines.extend(stdout.lines().map(|line| Line::raw(line.to_string()))),
        None => lines.push(Line::styled("(empty)", Style::new().fg(Color::DarkGray))),
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("stderr:", Style::new().fg(Color::DarkGray)));
    match output.map(|o| o.stderr.as_str()).filter(|s| !s.is_empty()) {
        Some(stderr) => lines.extend(stderr.lines().map(|line| Line::raw(line.to_string()))),
        None => lines.push(Line::styled("(empty)", Style::new().fg(Color::DarkGray))),
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " [q] back  [k] kill  [Up/Down] scroll  [f] follow  [Tab] switch",
        Style::new().fg(Color::DarkGray),
    ));
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
    }
}
