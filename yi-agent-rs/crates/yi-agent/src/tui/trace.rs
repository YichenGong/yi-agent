//! The subagent tab of the runtime popup: the current root's children, and a
//! detail level that later work fills with the child's live trace.
//!
//! This module owns only state and rendering. Fetching the child list needs a
//! socket, so the caller resolves it and hands the popup the result; that keeps
//! this file testable without a daemon.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::cell::{CallState, HistoryCell};
use super::history::{HistoryState, HistoryView};
use yi_agent_core::subagent::trace::TraceFact;

/// The persisted row kinds the daemon can report, and what a subscription asks
/// for when it wants everything.
pub const TRACE_FACT_KINDS: [&str; 4] =
    ["assistant_text", "tool_call", "tool_result", "state_note"];

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
    /// The task's parent. Lets the detail level show only the children of the
    /// task on screen, which is what makes drilling down a tree walk.
    pub parent_task_id: Option<String>,
}

impl SubagentListItem {
    pub fn is_finished(&self) -> bool {
        !self.active
    }
}

#[derive(Debug)]
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

/// The kinds of pending decision the detail level can raise for the caller.
///
/// The popup owns no socket, so it records the intent and the event loop
/// performs it. `SendMessage` carries its text; `Cancel` is only ever raised
/// after the two-step cancel preview has been confirmed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceAction {
    /// Queue a user message for the task.
    SendMessage { task_id: String, text: String },
    /// Cancel the task. `confirmation_token` is `None` on the first press,
    /// which asks the daemon for a preview, and `Some` on the confirming press,
    /// which is the only form the daemon treats as the actual cancel.
    Cancel {
        task_id: String,
        confirmation_token: Option<String>,
    },
}

/// The read-only trace of one task, rendered as history cells.
///
/// Rows arrive from the daemon already tagged by kind. The view folds them the
/// same way the main transcript folds `AgentEvent`s: a run of assistant text
/// becomes one growing message, and a tool call and its result are separate
/// cells so the tool's state is visible without expanding anything. The view
/// holds no subscription of its own; the caller feeds it rows.
#[derive(Debug, Default)]
pub struct TraceFeed {
    history: HistoryState,
    /// The id of the newest row folded in. A subscription resumes after it.
    high_water_id: i64,
}

impl TraceFeed {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn high_water_id(&self) -> i64 {
        self.high_water_id
    }

    pub fn history(&self) -> &HistoryState {
        &self.history
    }

    pub fn is_empty(&self) -> bool {
        self.history.cells.is_empty()
    }

    /// How many history cells the trace has folded into so far.
    pub fn row_count(&self) -> usize {
        self.history.cells.len()
    }

    /// Scroll the view. Negative moves up from the bottom.
    pub fn scroll(&mut self, delta: isize) {
        let offset = self.history.scroll_offset as isize + delta;
        self.history.scroll_offset = offset.max(0) as usize;
    }

    pub fn scroll_to_top(&mut self) {
        self.history.scroll_offset = usize::MAX;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.history.scroll_offset = 0;
    }

    /// Fold one persisted row in, at the given width.
    ///
    /// A row that does not parse is dropped rather than aborting the view: a
    /// future writer may add a kind this build does not know, and losing one
    /// line beats losing the stream. The high-water mark only advances for
    /// rows that were accepted, so a dropped row is not skipped over silently
    /// if it later becomes parseable.
    pub fn push_row(&mut self, event_id: i64, kind: &str, payload_json: &str, width: u16) {
        if event_id > self.high_water_id {
            self.high_water_id = event_id;
        }
        let Some(fact) = parse_trace_fact(kind, payload_json) else {
            return;
        };
        self.push_fact(&fact, width);
    }

    /// Fold a decoded fact in, at the given width.
    pub fn push_fact(&mut self, fact: &TraceFact, width: u16) {
        match fact {
            TraceFact::AssistantText { text } => self.append_assistant_text(text, width),
            TraceFact::ToolCall { name, summary } => {
                self.history.push(tool_call_cell(name, summary), width);
            }
            TraceFact::ToolResult {
                name,
                is_error,
                summary,
            } => {
                self.history
                    .push(tool_result_cell(name, *is_error, summary), width);
            }
            TraceFact::StateNote { note } => {
                self.history.push(
                    HistoryCell::Separator {
                        label: Some(format!("· {note}")),
                    },
                    width,
                );
            }
        }
    }

    /// Extend the trailing assistant message, or start one.
    ///
    /// The aggregator on the writer side releases whatever text it has when a
    /// tool boundary arrives, so a run of `assistant_text` rows is genuinely one
    /// message split by the flush threshold, not two messages. Folding them
    /// back into one cell is what makes the view read like the main transcript.
    fn append_assistant_text(&mut self, text: &str, width: u16) {
        if self.history.extend_last_assistant_text(text, width) {
            return;
        }
        self.history
            .push(HistoryCell::from_assistant_text(text), width);
    }
}

/// Decode a persisted trace row.
///
/// The payload is the fact's own serde encoding, so it decodes on its own; the
/// `kind` column is a cross-check. A row whose payload and column disagree is
/// treated as unparseable, which keeps a corrupted row from being rendered
/// under the wrong heading.
fn parse_trace_fact(kind: &str, payload_json: &str) -> Option<TraceFact> {
    let fact: TraceFact = serde_json::from_str(payload_json).ok()?;
    if trace_fact_kind(&fact) == kind {
        Some(fact)
    } else {
        None
    }
}

fn trace_fact_kind(fact: &TraceFact) -> &'static str {
    match fact {
        TraceFact::AssistantText { .. } => "assistant_text",
        TraceFact::ToolCall { .. } => "tool_call",
        TraceFact::ToolResult { .. } => "tool_result",
        TraceFact::StateNote { .. } => "state_note",
    }
}

fn tool_call_cell(name: &str, summary: &str) -> HistoryCell {
    HistoryCell::ToolCall {
        id: summary.to_string(),
        name: name.to_string(),
        input: serde_json::json!({ "summary": summary }),
        state: CallState::Running,
        expanded: false,
    }
}

fn tool_result_cell(_name: &str, is_error: bool, summary: &str) -> HistoryCell {
    HistoryCell::ToolResult {
        id: String::new(),
        result_text: summary.to_string(),
        is_error,
        expanded: false,
    }
}

/// The detail level: the open task, its live trace, and where the drill-down
/// sits. The stack is the path from the list down to the task on screen, so
/// `Esc` walks back up it one level at a time.
#[derive(Debug)]
pub struct TraceDetailPopup {
    /// The stack of tasks entered, outermost first. `last()` is what is shown.
    stack: Vec<String>,
    /// Scratch text for the message action, so a half-typed message survives a
    /// repaint but never a level change.
    input: String,
    /// The trace of the task on screen. Reset whenever the level changes, so a
    /// drill-down never shows the previous task's rows.
    feed: TraceFeed,
}

impl TraceDetailPopup {
    pub fn new(task_id: String) -> Self {
        Self {
            stack: vec![task_id],
            input: String::new(),
            feed: TraceFeed::new(),
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

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn feed(&self) -> &TraceFeed {
        &self.feed
    }

    pub fn feed_mut(&mut self) -> &mut TraceFeed {
        &mut self.feed
    }

    /// Push a character into the message being composed.
    pub fn input_push(&mut self, ch: char) {
        self.input.push(ch);
    }

    /// Drop the last character of the message being composed. Deliberately not
    /// grapheme-aware: the popup's input is a one-shot intervention message, and
    /// the full editor upstream is what handles composition properly.
    pub fn input_backspace(&mut self) {
        self.input.pop();
    }

    /// Take the composed message, leaving the field empty.
    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    /// Drill into a descendant, remembering where to return to. The trace is
    /// dropped because it belongs to the task being left.
    pub fn push(&mut self, task_id: String) {
        self.input.clear();
        self.feed = TraceFeed::new();
        self.stack.push(task_id);
    }

    /// Step back toward the list. Returns false when the detail is at its root,
    /// which tells the caller to fall back to the list.
    pub fn pop(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.input.clear();
        self.feed = TraceFeed::new();
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

/// The children of `task_id`, in daemon order. The root's children are the
/// list's own items; anyone else's are found by parent, which is what makes the
/// drill-down a tree walk over one flat snapshot.
pub fn children_of<'a>(
    children: &'a SubagentListState,
    task_id: &str,
) -> Vec<&'a SubagentListItem> {
    children
        .items
        .iter()
        .filter(|item| item.parent_task_id.as_deref() == Some(task_id))
        .collect()
}

/// The tasks an `Enter` in the detail can open, in the order they are shown.
pub fn drill_targets<'a>(
    children: &'a SubagentListState,
    task_id: &str,
) -> Vec<&'a SubagentListItem> {
    children_of(children, task_id)
}

/// Draw the open task's live trace, its identity, and its direct children.
///
/// The trace body is rendered by [`HistoryView`], the same widget the main
/// transcript uses, so folding and scrolling behave identically in both places.
pub fn render_subagent_detail(
    f: &mut Frame,
    detail: &TraceDetailPopup,
    children: &SubagentListState,
    prompt: &DetailPrompt<'_>,
    area: Rect,
) {
    if area.width < 4 || area.height < 4 {
        return;
    }
    let task_id = detail.task_id().to_string();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(area);

    let mut title = format!("子 agent 详情 · {task_id}");
    if detail.depth() > 1 {
        title.push_str(&format!(" (层级 {})", detail.depth()));
    }
    title.push_str(" (Esc 返回)");

    // Identity: whatever the snapshot knows about the task on screen.
    let mut header = vec![match children.get(&task_id) {
        Some(item) => Line::from(vec![
            Span::styled(
                format!("[{}] ", item.state),
                Style::default().fg(state_color(&item.state)),
            ),
            Span::raw(
                item.objective
                    .clone()
                    .unwrap_or_else(|| item.task_id.clone()),
            ),
        ]),
        None => Line::raw(format!("{task_id}（不在当前子 agent 快照中）")),
    }];
    header.push(Line::styled(
        format!(
            "轨迹 {} 条 · 高位 {}",
            detail.feed().row_count(),
            detail.feed().high_water_id()
        ),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(header).block(titled(&title)), rows[0]);

    // The trace itself.
    if detail.feed().is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(
                "该任务暂无轨迹（可能尚未开始或已被清理）",
                Style::default().fg(Color::DarkGray),
            ))
            .block(titled("轨迹")),
            rows[1],
        );
    } else {
        let block = titled("轨迹");
        let inner = block.inner(rows[1]);
        f.render_widget(block, rows[1]);
        f.render_widget(
            HistoryView {
                state: detail.feed().history(),
                width: inner.width,
            },
            inner,
        );
    }

    // Children to drill into, then the two actions and the current action state.
    f.render_widget(
        Paragraph::new(footer_lines(detail, children, &task_id, prompt)).block(titled("")),
        rows[2],
    );
}

/// The transient state the detail footer needs from its owner: whether a
/// message is being typed, whether a cancel is awaiting confirmation, and what
/// the last action reported.
#[derive(Debug, Default, Clone, Copy)]
pub struct DetailPrompt<'a> {
    pub composing: bool,
    pub awaiting_cancel: bool,
    pub status: Option<&'a str>,
}

fn titled(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title.to_string())
}

/// The footer: the drill targets, plus the action state (composing, or the
/// pending cancel confirmation) so the user can see what a keypress will do.
fn footer_lines(
    detail: &TraceDetailPopup,
    children: &SubagentListState,
    task_id: &str,
    prompt: &DetailPrompt<'_>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(status) = prompt.status {
        lines.push(Line::styled(
            status.to_string(),
            Style::default().fg(Color::Yellow),
        ));
    }
    if prompt.awaiting_cancel {
        lines.push(Line::styled(
            "确认取消？y 确认 / 其他键放弃",
            Style::default().fg(Color::Red),
        ));
    }
    if prompt.composing {
        lines.push(Line::from(vec![
            Span::styled("发消息> ", Style::default().fg(Color::Cyan)),
            Span::raw(detail.input().to_string()),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]));
        lines.push(Line::styled(
            "Enter 发送 · Esc 取消",
            Style::default().fg(Color::DarkGray),
        ));
        return lines;
    }
    let targets = drill_targets(children, task_id);
    if targets.is_empty() {
        lines.push(Line::styled(
            "无直接子任务",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        let mut spans = vec![Span::styled(
            "子任务: ",
            Style::default().fg(Color::DarkGray),
        )];
        for (index, item) in targets.iter().enumerate() {
            spans.push(Span::styled(
                format!("[{}] {}", index + 1, item.task_id),
                Style::default().fg(state_color(&item.state)),
            ));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
        lines.push(Line::styled(
            "Enter 进入子任务",
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.push(Line::styled(
        "m 发消息 · k 取消该任务",
        Style::default().fg(Color::DarkGray),
    ));
    lines
}

/// A single row as the stream delivers it: the same shape the daemon persists.
pub type TraceRow = yi_agent_store::ipc::IpcTraceRow;

/// Where the live trace comes from. A trait so the popup's subscription
/// bookkeeping can be tested without a daemon.
pub trait TraceSource {
    /// The rows already persisted for `task_id`, and the high-water mark they
    /// end at. The caller resumes the stream from that mark.
    fn snapshot(&mut self, task_id: &str) -> Result<(i64, Vec<TraceRow>), String>;
    /// Open a stream of rows written after `after_id`.
    fn subscribe(&mut self, task_id: &str, after_id: i64) -> Result<Box<dyn TraceRows>, String>;
}

/// One open trace stream. Implementations read without blocking.
pub trait TraceRows {
    /// The next row, or `None` when none has arrived yet or the stream ended.
    fn try_row(&mut self) -> Result<Option<TraceRow>, String>;
}

/// The real source: the daemon over its Unix socket.
pub struct IpcTraceSource {
    socket: std::path::PathBuf,
}

impl IpcTraceSource {
    pub fn new(socket: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}

impl TraceSource for IpcTraceSource {
    fn snapshot(&mut self, task_id: &str) -> Result<(i64, Vec<TraceRow>), String> {
        let snapshot = yi_agent_store::ipc::read_task_trace(&self.socket, task_id)
            .map_err(|error| error.to_string())?;
        Ok((snapshot.high_water_id, snapshot.rows))
    }

    fn subscribe(&mut self, task_id: &str, after_id: i64) -> Result<Box<dyn TraceRows>, String> {
        let subscription = yi_agent_store::ipc::subscribe_trace(
            &self.socket,
            after_id,
            &[task_id.to_owned()],
            &TRACE_FACT_KINDS.map(str::to_string),
        )
        .map_err(|error| error.to_string())?;
        // The TUI cannot block on a read, so the stream is polled instead.
        subscription
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        Ok(Box::new(IpcTraceRows { subscription }))
    }
}

struct IpcTraceRows {
    subscription: yi_agent_store::ipc::TraceSubscription,
}

impl TraceRows for IpcTraceRows {
    fn try_row(&mut self) -> Result<Option<TraceRow>, String> {
        self.subscription
            .try_row()
            .map_err(|error| error.to_string())
    }
}

/// Keeps at most one trace stream open, for the task on screen.
///
/// Opening a detail level opens exactly one stream; leaving it closes that
/// stream before the next one opens, so a drill-down never leaves a previous
/// task's rows arriving in the background. The manager is driven from the
/// event loop, once per frame, with whatever task is currently shown.
pub struct TraceStreams {
    source: Box<dyn TraceSource>,
    open: Option<OpenTrace>,
    /// Streams opened and closed so far, for tests and diagnostics.
    opened: usize,
}

struct OpenTrace {
    task_id: String,
    rows: Box<dyn TraceRows>,
}

impl TraceStreams {
    pub fn new(source: Box<dyn TraceSource>) -> Self {
        Self {
            source,
            open: None,
            opened: 0,
        }
    }

    /// How many streams have been opened so far.
    #[cfg(test)]
    pub fn opened_streams(&self) -> usize {
        self.opened
    }

    #[cfg(test)]
    pub fn open_task_id(&self) -> Option<&str> {
        self.open.as_ref().map(|open| open.task_id.as_str())
    }

    /// Drop any open stream. Called when the detail closes, so a stream never
    /// outlives the view it feeds: a stale one would otherwise be reused for a
    /// later visit to the same task and silently skip that visit's backlog.
    pub fn close(&mut self) {
        self.open = None;
    }

    /// Bring the stream in line with `want`, and append any new rows to `feed`.
    ///
    /// `want` is the task the detail is showing, or `None` when no detail is
    /// open. A change of task closes the old stream first.
    pub fn sync(&mut self, want: Option<&str>, feed: &mut TraceFeed, width: u16) {
        match want {
            None => self.close(),
            Some(task_id) => {
                if self.open.as_ref().map(|o| o.task_id.as_str()) != Some(task_id) {
                    self.open = self.open_stream(task_id, feed, width);
                }
            }
        }
        let Some(open) = self.open.as_mut() else {
            return;
        };
        loop {
            match open.rows.try_row() {
                Ok(Some(row)) => feed.push_row(row.event_id, &row.kind, &row.payload_json, width),
                Ok(None) => break,
                Err(_) => {
                    // A broken stream is dropped; the next `sync` reopens it
                    // from the feed's high-water mark, so nothing is lost.
                    self.open = None;
                    break;
                }
            }
        }
    }

    /// Load the backlog and open one stream resuming after it. Returns `None`
    /// when even the backlog cannot be read, so the detail shows an empty view
    /// instead of failing.
    fn open_stream(
        &mut self,
        task_id: &str,
        feed: &mut TraceFeed,
        width: u16,
    ) -> Option<OpenTrace> {
        let (high_water, rows) = self.source.snapshot(task_id).ok()?;
        for row in rows {
            feed.push_row(row.event_id, &row.kind, &row.payload_json, width);
        }
        // The snapshot carries its own high-water mark; the feed may have folded
        // further rows already, so resume from whichever is later.
        let resume_after = high_water.max(feed.high_water_id());
        let rows = self.source.subscribe(task_id, resume_after).ok()?;
        self.opened += 1;
        Some(OpenTrace {
            task_id: task_id.to_owned(),
            rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_with_parent(task_id: &str, parent: Option<&str>) -> SubagentListItem {
        SubagentListItem {
            task_id: task_id.into(),
            objective: None,
            state: "running".into(),
            last_step: None,
            active: true,
            parent_task_id: parent.map(str::to_string),
        }
    }

    fn item(task_id: &str, state: &str, active: bool) -> SubagentListItem {
        SubagentListItem {
            task_id: task_id.into(),
            objective: Some(format!("objective of {task_id}")),
            state: state.into(),
            last_step: None,
            active,
            parent_task_id: Some("root".into()),
        }
    }

    fn row(event_id: i64, fact: &TraceFact) -> (i64, String, String) {
        let kind = trace_fact_kind(fact).to_string();
        (event_id, kind, serde_json::to_string(fact).unwrap())
    }

    fn feed_from(rows: &[(i64, String, String)]) -> TraceFeed {
        let mut feed = TraceFeed::new();
        for (event_id, kind, payload) in rows {
            feed.push_row(*event_id, kind, payload, 80);
        }
        feed
    }

    #[test]
    fn a_trace_row_becomes_a_history_cell() {
        let rows = vec![
            row(1, &TraceFact::AssistantText { text: "hi".into() }),
            row(
                2,
                &TraceFact::ToolCall {
                    name: "bash".into(),
                    summary: "cargo test".into(),
                },
            ),
            row(
                3,
                &TraceFact::ToolResult {
                    name: "bash".into(),
                    is_error: true,
                    summary: "boom".into(),
                },
            ),
            row(
                4,
                &TraceFact::StateNote {
                    note: "completed".into(),
                },
            ),
        ];
        let feed = feed_from(&rows);
        assert_eq!(feed.high_water_id(), 4);
        assert_eq!(feed.row_count(), 4, "each kind takes exactly one cell");
        match &feed.history().cells[0] {
            HistoryCell::AssistantMessage { markdown } => assert_eq!(markdown, "hi"),
            other => panic!("assistant text must be an assistant message, got {other:?}"),
        }
        match &feed.history().cells[1] {
            HistoryCell::ToolCall { name, .. } => assert_eq!(name, "bash"),
            other => panic!("a tool call must be a tool call cell, got {other:?}"),
        }
        match &feed.history().cells[2] {
            HistoryCell::ToolResult { is_error, .. } => assert!(*is_error),
            other => panic!("a tool result must be a tool result cell, got {other:?}"),
        }
        match &feed.history().cells[3] {
            HistoryCell::Separator { label } => {
                assert_eq!(label.as_deref(), Some("· completed"));
            }
            other => panic!("a state note must be a separator, got {other:?}"),
        }
    }

    #[test]
    fn streaming_text_rows_merge_into_one_cell() {
        let rows = vec![
            row(
                1,
                &TraceFact::AssistantText {
                    text: "the first ".into(),
                },
            ),
            row(
                2,
                &TraceFact::AssistantText {
                    text: "and the second".into(),
                },
            ),
            row(
                3,
                &TraceFact::AssistantText {
                    text: " and third".into(),
                },
            ),
        ];
        let feed = feed_from(&rows);
        assert_eq!(feed.row_count(), 1, "one message, not one per row");
        match &feed.history().cells[0] {
            HistoryCell::AssistantMessage { markdown } => {
                assert_eq!(markdown, "the first and the second and third");
            }
            other => panic!("expected one assistant message, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_call_ends_the_text_run_so_the_next_text_is_a_new_cell() {
        let rows = vec![
            row(
                1,
                &TraceFact::AssistantText {
                    text: "before".into(),
                },
            ),
            row(
                2,
                &TraceFact::ToolCall {
                    name: "bash".into(),
                    summary: "ls".into(),
                },
            ),
            row(
                3,
                &TraceFact::AssistantText {
                    text: "after".into(),
                },
            ),
        ];
        let feed = feed_from(&rows);
        assert_eq!(feed.row_count(), 3);
    }

    #[test]
    fn a_row_whose_kind_and_payload_disagree_is_dropped_without_advancing_the_mark() {
        let mut feed = TraceFeed::new();
        // A tool_call payload under an assistant_text kind is corrupt.
        let (_, _, payload) = row(
            7,
            &TraceFact::ToolCall {
                name: "bash".into(),
                summary: "ls".into(),
            },
        );
        feed.push_row(7, "assistant_text", &payload, 80);
        assert!(feed.is_empty(), "the mismatched row is not rendered");
        assert_eq!(
            feed.high_water_id(),
            7,
            "but it was seen, so it is not re-read"
        );
        feed.push_row(8, "assistant_text", "not json", 80);
        assert!(feed.is_empty());
    }

    #[test]
    fn esc_from_a_child_detail_returns_to_its_parent_agent() {
        let mut detail = TraceDetailPopup::new("parent".into());
        detail.push("child".into());
        detail.push("grandchild".into());
        assert_eq!(detail.task_id(), "grandchild");
        assert_eq!(detail.depth(), 3);

        assert!(detail.pop(), "grandchild -> child");
        assert_eq!(detail.task_id(), "child");
        assert!(detail.pop(), "child -> parent agent");
        assert_eq!(detail.task_id(), "parent", "back at the parent agent");
        assert!(!detail.pop(), "the parent agent is the last level");
    }

    /// A scripted source: the backlog, then the rows to stream.
    struct FakeSource {
        snapshot: Vec<(i64, String, String)>,
        high_water: i64,
        stream: Vec<(i64, String, String)>,
        subscribe_calls: std::rc::Rc<std::cell::Cell<usize>>,
    }

    struct FakeRows {
        rows: std::vec::IntoIter<(i64, String, String)>,
    }

    impl TraceRows for FakeRows {
        fn try_row(&mut self) -> Result<Option<TraceRow>, String> {
            Ok(self
                .rows
                .next()
                .map(|(event_id, kind, payload_json)| TraceRow {
                    event_id,
                    task_id: "task".into(),
                    kind,
                    payload_json,
                }))
        }
    }

    impl TraceSource for FakeSource {
        fn snapshot(&mut self, _task_id: &str) -> Result<(i64, Vec<TraceRow>), String> {
            Ok((
                self.high_water,
                self.snapshot
                    .iter()
                    .map(|(event_id, kind, payload_json)| TraceRow {
                        event_id: *event_id,
                        task_id: "task".into(),
                        kind: kind.clone(),
                        payload_json: payload_json.clone(),
                    })
                    .collect(),
            ))
        }

        fn subscribe(
            &mut self,
            _task_id: &str,
            _after_id: i64,
        ) -> Result<Box<dyn TraceRows>, String> {
            self.subscribe_calls.set(self.subscribe_calls.get() + 1);
            Ok(Box::new(FakeRows {
                rows: self.stream.clone().into_iter(),
            }))
        }
    }

    #[test]
    fn opening_a_detail_opens_exactly_one_trace_subscription() {
        let subscribe_calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let source = FakeSource {
            snapshot: vec![row(
                1,
                &TraceFact::AssistantText {
                    text: "backlog".into(),
                },
            )],
            high_water: 1,
            stream: vec![row(
                2,
                &TraceFact::StateNote {
                    note: "done".into(),
                },
            )],
            subscribe_calls: subscribe_calls.clone(),
        };
        let mut streams = TraceStreams::new(Box::new(source));
        let mut feed = TraceFeed::new();

        streams.sync(Some("task-1"), &mut feed, 80);
        assert_eq!(subscribe_calls.get(), 1, "one stream for the open detail");
        assert_eq!(
            feed.high_water_id(),
            2,
            "the backlog and the stream arrived"
        );
        assert_eq!(streams.open_task_id(), Some("task-1"));

        // Re-syncing the same task reuses the stream instead of reopening it.
        streams.sync(Some("task-1"), &mut feed, 80);
        assert_eq!(
            subscribe_calls.get(),
            1,
            "the same task is not re-subscribed"
        );
        assert_eq!(streams.opened_streams(), 1);

        // Leaving the detail drops the stream entirely.
        streams.sync(None, &mut feed, 80);
        assert_eq!(streams.open_task_id(), None);
    }

    #[test]
    fn closing_the_detail_drops_the_stream_so_reopening_replays_the_backlog() {
        let subscribe_calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let source = FakeSource {
            snapshot: vec![row(
                1,
                &TraceFact::AssistantText {
                    text: "backlog".into(),
                },
            )],
            high_water: 1,
            stream: Vec::new(),
            subscribe_calls: subscribe_calls.clone(),
        };
        let mut streams = TraceStreams::new(Box::new(source));
        let mut feed = TraceFeed::new();

        streams.sync(Some("task-1"), &mut feed, 80);
        assert_eq!(subscribe_calls.get(), 1);

        // The detail closes: the stream must go with it, or a reopen would
        // resume past a backlog the user never saw.
        streams.close();
        assert_eq!(streams.open_task_id(), None);

        // Reopening the same task opens a fresh stream rather than reusing a
        // stale one, so the backlog is delivered again.
        let mut reopened = TraceFeed::new();
        streams.sync(Some("task-1"), &mut reopened, 80);
        assert_eq!(subscribe_calls.get(), 2, "a reopen re-subscribes");
        assert_eq!(reopened.high_water_id(), 1, "the backlog is replayed");
        assert_eq!(reopened.row_count(), 1);
    }

    #[test]
    fn drilling_into_another_task_replaces_the_open_subscription() {
        let subscribe_calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let source = FakeSource {
            snapshot: Vec::new(),
            high_water: 0,
            stream: Vec::new(),
            subscribe_calls: subscribe_calls.clone(),
        };
        let mut streams = TraceStreams::new(Box::new(source));
        let mut feed = TraceFeed::new();

        streams.sync(Some("task-1"), &mut feed, 80);
        streams.sync(Some("task-2"), &mut feed, 80);

        assert_eq!(subscribe_calls.get(), 2, "a new task opens a new stream");
        assert_eq!(streams.open_task_id(), Some("task-2"));
        assert_eq!(
            streams.opened_streams(),
            2,
            "the previous stream was closed, not left running alongside"
        );
    }

    #[test]
    fn a_drill_down_lists_only_the_direct_children() {
        let state = SubagentListState::new(vec![
            item_with_parent("a", Some("root")),
            item_with_parent("b", Some("a")),
            item_with_parent("c", Some("a")),
            item_with_parent("d", Some("b")),
        ]);
        let children = children_of(&state, "a");
        assert_eq!(
            children
                .iter()
                .map(|i| i.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"],
            "only direct children"
        );
        assert!(children_of(&state, "d").is_empty());
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

    /// Render the detail into an off-screen terminal and hand back its text,
    /// with whitespace removed.
    ///
    /// A width-2 CJK glyph occupies two cells, and the buffer reports the
    /// trailing cell as a space, so a literal substring match would fail on
    /// every Chinese label. Stripping whitespace makes the assertion about the
    /// text, not about how ratatui lays it out.
    fn rendered_detail(detail: &TraceDetailPopup, state: &SubagentListState) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|f| {
                let area = f.area();
                render_subagent_detail(f, detail, state, &DetailPrompt::default(), area);
            })
            .expect("draw the detail");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text.retain(|c| !c.is_whitespace());
        text
    }

    #[test]
    fn the_detail_shows_the_open_task_and_its_state() {
        let state = SubagentListState::new(vec![item("task-1", "running", true)]);
        let detail = TraceDetailPopup::new("task-1".into());
        let text = rendered_detail(&detail, &state);
        assert!(text.contains("task-1"));
        assert!(text.contains("running"));
        assert!(text.contains("objectiveoftask-1"), "shows the objective");
        assert!(text.contains("暂无轨迹"), "an empty trace says so");
    }

    #[test]
    fn the_detail_says_so_when_the_task_is_not_in_the_list() {
        let state = SubagentListState::new(vec![item("task-1", "running", true)]);
        let detail = TraceDetailPopup::new("task-9".into());
        let text = rendered_detail(&detail, &state);
        assert!(text.contains("task-9"));
        assert!(
            text.contains("不在当前子agent快照中"),
            "says the task is not in the snapshot"
        );
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
        assert!(text.contains("objective of task-1"), "shows the objective");
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
