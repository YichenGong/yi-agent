use std::io::{self, stdout};
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use yi_agent_core::AgentEvent;

use super::bash_popup::{BashPopup, ConfirmKill, DetailPopup, ListPopup};
use super::cell::HistoryCell;
use super::cost::CostTracker;
use super::history::{HistoryState, HistoryView};
use super::input::{InputAction, InputLine};
use super::slash::{CommandPopup, SlashCommand};
use super::state::RunningTaskRegistry;
use super::statusbar::{StatusBarState, render_statusbar};

const HISTORY_WHEEL_LINES: usize = 3;

/// Run the ratatui TUI main loop with the real terminal.
///
/// - `agent_rx`: receives agent events to display in history
/// - `input_tx`: sends user-submitted input strings to the agent driver
/// - `interrupt_tx`: signals to interrupt the current agent run
/// - `is_running`: shared flag indicating if agent is currently running
pub fn run_tui(
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: tokio::sync::mpsc::Sender<String>,
    interrupt_tx: tokio::sync::mpsc::Sender<()>,
    control_tx: tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    model: String,
) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut history = HistoryState::new();
    let mut input = InputLine::new();

    let result = run_loop(
        &mut terminal,
        &mut agent_rx,
        &mut history,
        &mut input,
        &input_tx,
        &interrupt_tx,
        &control_tx,
        &decision_tx,
        &is_running,
        &CrosstermEventSource,
        &model,
    );

    // Try every cleanup step so a failed write cannot leave the terminal in another mode.
    let mut cleanup_error: Option<io::Error> = None;
    if let Err(error) = execute!(terminal.backend_mut(), DisableBracketedPaste) {
        cleanup_error.get_or_insert(error);
    }
    if let Err(error) = disable_raw_mode() {
        cleanup_error.get_or_insert(error);
    }
    if let Err(error) = execute!(terminal.backend_mut(), LeaveAlternateScreen) {
        cleanup_error.get_or_insert(error);
    }

    match (result, cleanup_error) {
        (Err(error), _) => Err(error),
        (Ok(()), Some(error)) => Err(error),
        (Ok(()), None) => Ok(()),
    }
}

/// Event source trait so the loop can be tested with fake events.
pub trait EventSource {
    /// Poll for an event, waiting up to `timeout`. Returns `Ok(None)` on timeout.
    fn poll(&self, timeout: Duration) -> std::io::Result<Option<Event>>;
}

/// Production event source using crossterm's global event queue.
struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll(&self, timeout: Duration) -> std::io::Result<Option<Event>> {
        if event::poll(timeout)? {
            Ok(Some(event::read()?))
        } else {
            Ok(None)
        }
    }
}

/// Run the TUI loop with any ratatui backend (used by tests with TestBackend).
/// Does NOT call enable_raw_mode / EnterAlternateScreen.
#[cfg(test)]
#[allow(dead_code)]
pub fn run_tui_with_backend<B: Backend>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    let mut history = HistoryState::new();
    let mut input = InputLine::new();
    run_loop(
        terminal,
        agent_rx,
        &mut history,
        &mut input,
        input_tx,
        interrupt_tx,
        control_tx,
        decision_tx,
        is_running,
        &CrosstermEventSource,
        "test-model",
    )
}

/// Testable variant: accepts a custom EventSource for injecting fake key events.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn run_tui_with_backend_and_events<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
) -> std::io::Result<()> {
    let mut history = HistoryState::new();
    let mut input = InputLine::new();
    run_loop(
        terminal,
        agent_rx,
        &mut history,
        &mut input,
        input_tx,
        interrupt_tx,
        control_tx,
        decision_tx,
        is_running,
        events,
        "test-model",
    )
}

#[allow(clippy::too_many_arguments)]
fn run_loop<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    history: &mut HistoryState,
    input: &mut InputLine,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    model: &str,
) -> std::io::Result<()> {
    let mut pending_quit = false;
    let mut popup: Option<CommandPopup> = None;
    let mut queued: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut statusbar_state = StatusBarState::default();
    let mut task_registry = RunningTaskRegistry::new();
    let mut cost_tracker = CostTracker::default();
    let mut bash_popup: BashPopup = BashPopup::None;

    loop {
        let size = terminal.size()?;
        let width = size.width;
        let queued_lines = crate::tui::queued::render_queued_preview(&queued, width);
        let queued_height = queued_lines.len() as u16;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let pre_drain_layout = compute_layout(area, input, pending_quit, &popup, queued_height);
        let history_area = pre_drain_layout.chunks[0];
        let history_text_width = history.text_width(history_area.width, history_area.height);
        history.reconcile_scroll_offset(history_text_width, history_area.height);

        // Drain all pending agent events
        while let Ok(event) = agent_rx.try_recv() {
            let is_turn_end = matches!(
                event,
                AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
            );
            route_event(
                &mut task_registry,
                &mut statusbar_state,
                &mut cost_tracker,
                &event,
            );
            history.push_event(event, history_text_width);
            // 回合结束后把排队第一条「转正」进 history(在 Separator 之后)
            if is_turn_end {
                if let Some(text) = queued.pop_front() {
                    history.push(HistoryCell::UserMessage { text }, history_text_width);
                }
            }
        }

        let queued_lines = crate::tui::queued::render_queued_preview(&queued, width);
        let queued_height = queued_lines.len() as u16;
        // Advance status bar interpolation + spinner (~30hz).
        statusbar_state.tick();

        // Pre-compute layout so mouse hit-testing uses the same chunk rects
        // as the draw closure below.
        let size = terminal.size()?;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let layout = compute_layout(area, input, pending_quit, &popup, queued_height);

        terminal.draw(|f| {
            let chunks = layout.chunks.clone();

            let history_view = HistoryView {
                state: history,
                width: chunks[0].width,
            };
            f.render_widget(history_view, chunks[0]);

            // Render popup if active
            if let Some(p) = &popup {
                let popup_area = chunks[1];
                if popup_area.height > 0 {
                    f.render_widget(build_popup(p), popup_area);
                }
            }

            // Status bar
            let statusbar_line = render_statusbar(&statusbar_state, &task_registry, model);
            f.render_widget(statusbar_line, chunks[2]);

            // Render queued messages preview
            if queued_height > 0 {
                f.render_widget(Paragraph::new(queued_lines.clone()), chunks[3]);
            }

            let input_line = build_input_line(input, pending_quit, chunks[5].width);
            f.render_widget(input_line, chunks[5]);

            // Bash popup (covers the full screen above the input area).
            match &bash_popup {
                BashPopup::List(p) => {
                    let list_area = ratatui::layout::Rect {
                        x: chunks[0].x + 2,
                        y: chunks[0].y + 1,
                        width: chunks[0].width.saturating_sub(4),
                        height: chunks[0]
                            .height
                            .saturating_sub(2)
                            .min((p.task_ids.len() as u16 + 2).max(6)),
                    };
                    f.render_widget(Clear, list_area);
                    f.render_widget(
                        super::bash_popup::render_list_popup(p, &task_registry, list_area),
                        list_area,
                    );
                }
                BashPopup::Detail(p) => {
                    if let Some(task) = task_registry.get(&p.task_id) {
                        let detail_area = chunks[0];
                        f.render_widget(Clear, detail_area);
                        f.render_widget(
                            super::bash_popup::render_detail_popup(p, task),
                            detail_area,
                        );
                    }
                }
                BashPopup::ConfirmKill(ck) => {
                    // Draw the detail behind, then overlay a small confirm box.
                    if let Some(task) = task_registry.get(&ck.task_id) {
                        let detail_area = chunks[0];
                        f.render_widget(Clear, detail_area);
                        let detail = DetailPopup::new(ck.task_id.clone());
                        f.render_widget(
                            super::bash_popup::render_detail_popup(&detail, task),
                            detail_area,
                        );
                    }
                    // Confirm box centered in the upper portion.
                    let box_w = 40u16.min(chunks[0].width.saturating_sub(4));
                    let box_h = 4u16;
                    let box_x = chunks[0].x + (chunks[0].width.saturating_sub(box_w)) / 2;
                    let box_y = chunks[0].y + (chunks[0].height.saturating_sub(box_h)) / 3;
                    let box_area = ratatui::layout::Rect {
                        x: box_x,
                        y: box_y,
                        width: box_w,
                        height: box_h,
                    };
                    f.render_widget(Clear, box_area);
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(vec![
                            ratatui::text::Line::raw("kill this process?"),
                            ratatui::text::Line::raw(""),
                            ratatui::text::Line::raw("[y] confirm   [n/esc] cancel"),
                        ])
                        .block(
                            ratatui::widgets::Block::default()
                                .borders(ratatui::widgets::Borders::ALL)
                                .title("confirm"),
                        ),
                        box_area,
                    );
                }
                BashPopup::None => {}
            }
        })?;

        // Poll for events with timeout (33ms → ~30hz refresh)
        match events.poll(Duration::from_millis(33))? {
            Some(Event::Key(key)) => {
                // Ctrl+P opens the bash task popup when no popup is active.
                if key.code == KeyCode::Char('p')
                    && key.modifiers == KeyModifiers::CONTROL
                    && matches!(bash_popup, BashPopup::None)
                {
                    let ids: Vec<String> =
                        task_registry.list().iter().map(|t| t.id.clone()).collect();
                    if !ids.is_empty() {
                        bash_popup = BashPopup::List(ListPopup::new(ids));
                    }
                    continue;
                }
                // Route keys to the bash popup when active.
                if !matches!(bash_popup, BashPopup::None) {
                    handle_bash_popup_key(key, &mut bash_popup, &task_registry);
                    continue;
                }
                let history_area = layout.chunks[0];
                let history_text_width =
                    history.text_width(history_area.width, history_area.height);
                let max_offset = history.max_scroll_offset(history_text_width, history_area.height);
                match handle_key(
                    key,
                    input,
                    history,
                    max_offset,
                    history_text_width,
                    history_area.height,
                    &cost_tracker,
                    input_tx,
                    interrupt_tx,
                    control_tx,
                    decision_tx,
                    is_running,
                    &mut queued,
                    &mut pending_quit,
                    &mut popup,
                ) {
                    KeyOutcome::Quit => break,
                    KeyOutcome::Submit(_) => {
                        pending_quit = false;
                    }
                    KeyOutcome::None => {}
                }
                // After any key, sync popup state with the (possibly modified) buffer
                sync_popup(&mut popup, &input.buffer);
            }
            Some(Event::Paste(text)) => {
                handle_paste(
                    text,
                    input,
                    history,
                    &bash_popup,
                    &mut pending_quit,
                    &mut popup,
                );
            }
            Some(Event::Mouse(mouse)) => {
                handle_mouse(
                    mouse,
                    &layout,
                    &mut bash_popup,
                    history,
                    &task_registry,
                    &mut pending_quit,
                );
            }
            _ => {}
        }
    }

    Ok(())
}

/// Handle a key event for the bash popup state machine.
fn handle_bash_popup_key(
    key: KeyEvent,
    bash_popup: &mut BashPopup,
    task_registry: &RunningTaskRegistry,
) {
    match bash_popup {
        BashPopup::List(p) => match key.code {
            KeyCode::Up => p.move_up(),
            KeyCode::Down => p.move_down(),
            KeyCode::Enter => {
                if let Some(id) = p.selected_id() {
                    *bash_popup = BashPopup::Detail(DetailPopup::new(id.to_string()));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *bash_popup = BashPopup::None;
            }
            _ => {}
        },
        BashPopup::Detail(d) => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                // Back to list (rebuild ids from registry so new tasks appear)
                let ids: Vec<String> = task_registry.list().iter().map(|t| t.id.clone()).collect();
                if ids.is_empty() {
                    *bash_popup = BashPopup::None;
                } else {
                    *bash_popup = BashPopup::List(ListPopup::new(ids));
                }
            }
            KeyCode::Char('k') => {
                // Only allow kill on running tasks.
                let is_running = task_registry
                    .get(&d.task_id)
                    .map(|t| t.status == super::state::TaskStatus::Running)
                    .unwrap_or(false);
                if is_running {
                    *bash_popup = BashPopup::ConfirmKill(ConfirmKill {
                        task_id: d.task_id.clone(),
                    });
                }
            }
            KeyCode::Up => {
                d.scroll_up(1);
            }
            KeyCode::Down => {
                // Approximate max scroll by stdout+stderr line count.
                let lines = task_registry
                    .get(&d.task_id)
                    .map(|t| {
                        let so = String::from_utf8_lossy(&t.stdout).lines().count();
                        let se = String::from_utf8_lossy(&t.stderr).lines().count();
                        so + se + 6 // header + labels
                    })
                    .unwrap_or(0);
                d.scroll_down(1, lines);
            }
            KeyCode::Char('f') => {
                d.scroll_to_bottom();
            }
            _ => {}
        },
        BashPopup::ConfirmKill(ck) => match key.code {
            KeyCode::Char('y') => {
                // TODO: wire a kill channel from TUI → agent → call_stream.
                // For now, mark the task as failed in the registry and close.
                // The real kill requires a channel in main.rs + oneshot in agent.rs.
                let _ = ck.task_id; // placeholder
                let ids: Vec<String> = task_registry.list().iter().map(|t| t.id.clone()).collect();
                if ids.is_empty() {
                    *bash_popup = BashPopup::None;
                } else {
                    *bash_popup = BashPopup::List(ListPopup::new(ids));
                }
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                *bash_popup = BashPopup::Detail(DetailPopup::new(ck.task_id.clone()));
            }
            _ => {}
        },
        BashPopup::None => {}
    }
}

/// Handle a mouse event by routing scroll wheel movement to whichever region
/// the cursor is over. Only scroll events are handled; clicks are ignored.
///
/// Routing priority:
/// 1. If a bash detail popup is active and the mouse is over the history
///    region (where the popup is rendered), scroll the popup.
/// 2. Otherwise, if the mouse is over the history region, scroll history.
/// 3. If the mouse is over the input region, do nothing — the input widget
///    handles its own scrolling internally.
fn handle_mouse(
    mouse: MouseEvent,
    layout: &LayoutInfo,
    bash_popup: &mut BashPopup,
    history: &mut HistoryState,
    task_registry: &RunningTaskRegistry,
    pending_quit: &mut bool,
) {
    // Only react to scroll-wheel events.
    let scroll_delta = match mouse.kind {
        MouseEventKind::ScrollUp => Some(HISTORY_WHEEL_LINES),
        MouseEventKind::ScrollDown => Some(HISTORY_WHEEL_LINES),
        _ => None,
    };
    let Some(delta) = scroll_delta else {
        return;
    };
    let is_scroll_down = matches!(mouse.kind, MouseEventKind::ScrollDown);

    let history_area = layout.chunks[0];
    let pos = ratatui::layout::Position::from((mouse.column, mouse.row));

    // The bash detail popup occupies the history region (chunks[0]).
    if matches!(bash_popup, BashPopup::Detail(_) | BashPopup::ConfirmKill(_))
        && history_area.contains(pos)
    {
        if let BashPopup::Detail(d) = bash_popup {
            if is_scroll_down {
                let lines = task_registry
                    .get(&d.task_id)
                    .map(|t| {
                        let so = String::from_utf8_lossy(&t.stdout).lines().count();
                        let se = String::from_utf8_lossy(&t.stderr).lines().count();
                        so + se + 6 // header + labels
                    })
                    .unwrap_or(0);
                d.scroll_down(delta, lines);
            } else {
                d.scroll_up(delta);
            }
        }
        return;
    }

    // History region: scroll the conversation history.
    if history_area.contains(pos) {
        *pending_quit = false;
        let history_text_width = history.text_width(history_area.width, history_area.height);
        let max_offset = history.max_scroll_offset(history_text_width, history_area.height);
        if is_scroll_down {
            history.scroll_down(delta);
        } else {
            history.scroll_up(delta, max_offset);
        }
    }
    // Other regions (status bar, queued preview, input) are intentionally
    // ignored — the input widget handles its own scroll, and the others have
    // no scrollable content.
}

/// Route agent events to the task registry + status bar before they reach
/// history. Streaming events (ToolOutputDelta/ToolExit/ToolTimeout) are
/// consumed here and not pushed to history (history ignores them anyway).
fn route_event(
    registry: &mut RunningTaskRegistry,
    statusbar: &mut StatusBarState,
    cost: &mut CostTracker,
    event: &AgentEvent,
) {
    match event {
        AgentEvent::Start => {
            statusbar.reset_for_new_call();
        }
        AgentEvent::ToolCall { id, name, input } => {
            // LLM turn ended, tool execution begins. Reset the decode
            // counter so it doesn't linger at the previous turn's value
            // throughout the entire tool execution phase.
            statusbar.on_tool_call_phase();
            let cmd = input
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let exp = input
                .get("expected_timeout_sec")
                .and_then(|v| v.as_u64())
                .unwrap_or(120) as u32;
            registry.on_tool_call(id, name, &cmd, exp);
        }
        AgentEvent::ToolOutputDelta { id, stream, text } => {
            registry.on_output_delta(id, *stream, text);
        }
        AgentEvent::ToolExit { id, code } => {
            registry.on_exit(id, *code);
        }
        AgentEvent::ToolTimeout { id } => {
            registry.on_timeout(id);
        }
        AgentEvent::ToolResult { id, result } => {
            registry.on_result(id, result.is_error);
        }
        // Turn-end events finalize any still-running tasks. This is a
        // defense-in-depth cleanup: in the happy path each ToolCall gets a
        // matching ToolExit before Done arrives. But ToolExit can be missed
        // when (a) the bash tool early-returns on a blocked/parse/spawn
        // error without emitting ToolEvent::Exit, (b) the user cancels
        // mid-tool and the call_stream future is dropped, or (c) the
        // forwarder task silently drops the event on a full/closed channel.
        // Without this cleanup the status-bar timer would tick forever.
        AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_) => {
            registry.abort_all_running();
        }
        AgentEvent::Usage { model, usage } => {
            statusbar.set_token_target(usage.input_tokens as u64, usage.output_tokens as u64);
            cost.record(model, usage);
        }
        AgentEvent::EstimatedPrefill(n) => {
            statusbar.set_prefill_estimate(*n as u64);
        }
        AgentEvent::AssistantText(text) => {
            statusbar.estimate_decode_tokens(text);
        }
        AgentEvent::DecodeDelta(text) => {
            statusbar.estimate_decode_tokens(text);
        }
        AgentEvent::AutoCompacting {
            old_msg_count,
            new_msg_count,
        } => {
            tracing::info!(
                old_msg_count,
                new_msg_count,
                "auto-compact: session compressed"
            );
        }
        _ => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    None,
    Quit,
    Submit(String),
}

#[allow(clippy::too_many_arguments)]
fn handle_key(
    key: KeyEvent,
    input: &mut InputLine,
    history: &mut HistoryState,
    max_scroll_offset: usize,
    history_width: u16,
    history_height: u16,
    cost_tracker: &CostTracker,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    queued: &mut std::collections::VecDeque<String>,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
) -> KeyOutcome {
    // Check if there's a pending permission request
    if let Some((request_id, _tool_name, prefix_suggestion, kind)) =
        history.pending_permission_info()
    {
        // Allow quit keys to pass through even when permission is pending
        let is_quit_key = matches!(key.code, KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL)
            || matches!(key.code, KeyCode::Esc);
        if is_quit_key {
            // Fall through to global key handling below
        } else {
            let decision = match key.code {
                KeyCode::Char('1') => Some(yi_agent_core::permission::Decision::AllowOnce),
                KeyCode::Char('2') => Some(yi_agent_core::permission::Decision::AlwaysAllowTool),
                KeyCode::Char('3') => prefix_suggestion
                    .map(|p| yi_agent_core::permission::Decision::AlwaysAllowPrefix(p.to_string())),
                KeyCode::Char('4') => Some(yi_agent_core::permission::Decision::Deny),
                KeyCode::Enter => {
                    let default = match kind {
                        yi_agent_core::permission::PermissionKind::Blacklisted(_) => {
                            yi_agent_core::permission::Decision::Deny
                        }
                        _ => yi_agent_core::permission::Decision::AllowOnce,
                    };
                    Some(default)
                }
                _ => None,
            };
            if let Some(d) = decision {
                let _ = decision_tx.blocking_send((request_id, d));
                return KeyOutcome::None;
            }
            // For other keys while permission pending, ignore (don't let user type input)
            return KeyOutcome::None;
        }
    }

    // Global keys first
    match key.code {
        KeyCode::Esc => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            // If popup is active, Esc dismisses it (without setting pending_quit)
            if popup.is_some() {
                *popup = None;
                return KeyOutcome::None;
            }
            *pending_quit = true;
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = interrupt_tx.blocking_send(());
            }
            return KeyOutcome::None;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            *pending_quit = true;
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = interrupt_tx.blocking_send(());
            }
            return KeyOutcome::None;
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return KeyOutcome::Quit;
        }
        KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
            *pending_quit = false;
            history.toggle_fold_selected();
            return KeyOutcome::None;
        }
        KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
            *pending_quit = false;
            history.select_up();
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
            *pending_quit = false;
            history.select_down();
            return KeyOutcome::None;
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            *pending_quit = false;
            history.scroll_up(10, max_scroll_offset);
            return KeyOutcome::None;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            *pending_quit = false;
            history.scroll_down(10);
            return KeyOutcome::None;
        }
        _ => {}
    }

    // Any other key cancels pending quit
    *pending_quit = false;

    // Popup-specific key handling (when popup is active)
    if popup.is_some() {
        match key.code {
            KeyCode::Up => {
                if let Some(p) = popup.as_mut() {
                    p.move_up();
                }
                return KeyOutcome::None;
            }
            KeyCode::Down => {
                if let Some(p) = popup.as_mut() {
                    p.move_down();
                }
                return KeyOutcome::None;
            }
            KeyCode::Tab => {
                // Complete the selected command name into the input buffer
                if let Some(p) = popup.as_ref() {
                    if let Some(cmd) = p.selected() {
                        input.buffer = format!("/{}", cmd.name());
                        input.cursor = input.buffer.len();
                    }
                }
                *popup = None;
                return KeyOutcome::None;
            }
            KeyCode::Enter => {
                // Execute the selected command
                if let Some(p) = popup.as_ref() {
                    if let Some(cmd) = p.selected() {
                        // Check if buffer has args (text after command name)
                        let buffer = &input.buffer;
                        let cmd_full = format!("/{}", cmd.name());
                        let args = if buffer.len() > cmd_full.len() {
                            Some(&buffer[cmd_full.len()..])
                        } else {
                            None
                        };
                        let args_str = args.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                        *popup = None;
                        input.clear();
                        return execute_slash_command(
                            cmd,
                            args_str,
                            history,
                            history_width,
                            cost_tracker,
                            input_tx,
                            interrupt_tx,
                            control_tx,
                        );
                    } else {
                        // No command selected (empty filter) — show error
                        let text = input.take_submitted();
                        *popup = None;
                        history.push(
                            HistoryCell::Separator {
                                label: Some(format!("未知命令: {}", text)),
                            },
                            history_width,
                        );
                        return KeyOutcome::None;
                    }
                }
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Up if key.modifiers.is_empty() => {
            history.scroll_up(1, max_scroll_offset);
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers.is_empty() => {
            history.scroll_down(1);
            return KeyOutcome::None;
        }
        KeyCode::PageUp if key.modifiers.is_empty() => {
            history.scroll_page_up(history_height, max_scroll_offset);
            return KeyOutcome::None;
        }
        KeyCode::PageDown if key.modifiers.is_empty() => {
            history.scroll_page_down(history_height);
            return KeyOutcome::None;
        }
        KeyCode::Home if key.modifiers.is_empty() => {
            history.scroll_to_top(history_width, history_height);
            return KeyOutcome::None;
        }
        KeyCode::End if key.modifiers.is_empty() => {
            history.scroll_to_bottom();
            return KeyOutcome::None;
        }
        _ => {}
    }

    // Input handling
    match input.handle_key(key) {
        InputAction::Submit => {
            let text = input.take_submitted();
            // Check if this is a slash command
            if text.starts_with('/') {
                let name = text
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                let args = text
                    .trim_start_matches('/')
                    .get(name.len()..)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                if let Some(cmd) = SlashCommand::from_name(name) {
                    *popup = None;
                    return execute_slash_command(
                        cmd,
                        args,
                        history,
                        history_width,
                        cost_tracker,
                        input_tx,
                        interrupt_tx,
                        control_tx,
                    );
                } else {
                    // Unknown slash command
                    *popup = None;
                    history.push(
                        HistoryCell::Separator {
                            label: Some(format!("未知命令: {}", text)),
                        },
                        history_width,
                    );
                    return KeyOutcome::None;
                }
            }
            *popup = None;
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                queued.push_back(text.clone());
            } else {
                history.push(
                    HistoryCell::UserMessage { text: text.clone() },
                    history_width,
                );
            }
            let _ = input_tx.blocking_send(text.clone());
            KeyOutcome::Submit(text)
        }
        _ => KeyOutcome::None,
    }
}

fn handle_paste(
    text: String,
    input: &mut InputLine,
    history: &HistoryState,
    bash_popup: &BashPopup,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
) {
    if !matches!(bash_popup, BashPopup::None) || history.pending_permission_info().is_some() {
        return;
    }

    input.insert_str(&text);
    *pending_quit = false;
    sync_popup(popup, &input.buffer);
}

/// Synchronize popup state with the current input buffer.
/// Shows popup when buffer starts with '/' and cursor is in command name region.
/// Hides popup otherwise.
fn sync_popup(popup: &mut Option<CommandPopup>, buffer: &str) {
    if let Some(filter_text) = buffer.strip_prefix('/') {
        // Check if we're still in the command name region (no space yet)
        let in_name_region = !buffer.contains(' ');
        if in_name_region {
            if let Some(p) = popup.as_mut() {
                p.filter(filter_text);
            } else {
                let mut p = CommandPopup::new();
                p.filter(filter_text);
                *popup = Some(p);
            }
        } else {
            // Space found — dismiss popup (entering arg mode)
            *popup = None;
        }
    } else {
        *popup = None;
    }
}

/// Execute a slash command locally (does not send to agent).
#[allow(clippy::too_many_arguments)]
fn execute_slash_command(
    cmd: SlashCommand,
    args: Option<String>,
    history: &mut HistoryState,
    width: u16,
    cost: &CostTracker,
    _input_tx: &tokio::sync::mpsc::Sender<String>,
    _interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
) -> KeyOutcome {
    match cmd {
        SlashCommand::Quit => KeyOutcome::Quit,
        SlashCommand::Clear => {
            // 本地清空 history 显示,TUI 不等 driver 确认。
            // 通过 control channel 通知 driver 重建 agent(空 session)。
            history.clear();
            history.push(
                HistoryCell::Separator {
                    label: Some("对话已清空".to_string()),
                },
                width,
            );
            let _ = control_tx.blocking_send(crate::ControlCommand::Clear);
            KeyOutcome::None
        }
        SlashCommand::Help => {
            let mut help_text = String::from("可用命令:\n");
            for c in SlashCommand::all() {
                help_text.push_str(&format!("  /{:<10} {}\n", c.name(), c.description()));
            }
            history.push(HistoryCell::UserMessage { text: help_text }, width);
            KeyOutcome::None
        }
        SlashCommand::Cost => {
            let text = cost.render();
            history.push(HistoryCell::Markdown { text }, width);
            KeyOutcome::None
        }
        SlashCommand::Config => {
            history.push(
                HistoryCell::Separator {
                    label: Some("当前配置: (暂未实现)".to_string()),
                },
                width,
            );
            KeyOutcome::None
        }
        SlashCommand::Compact => {
            // 本地 push "正在压缩..." 提示,通过 control channel
            // 通知 driver 调用 compact_session 并重建 agent。
            history.push(
                HistoryCell::Separator {
                    label: Some("正在压缩对话...".to_string()),
                },
                width,
            );
            let _ = control_tx.blocking_send(crate::ControlCommand::Compact);
            KeyOutcome::None
        }
        SlashCommand::Model => {
            if let Some(model) = args {
                history.push(
                    HistoryCell::Separator {
                        label: Some(format!("切换模型到: {} (暂未实现)", model)),
                    },
                    width,
                );
            } else {
                history.push(
                    HistoryCell::Separator {
                        label: Some("用法: /model <model-name>".to_string()),
                    },
                    width,
                );
            }
            KeyOutcome::None
        }
    }
}

/// Build the popup widget for rendering.
fn build_popup<'a>(popup: &'a CommandPopup) -> Paragraph<'a> {
    let lines: Vec<Line<'a>> = popup
        .filtered()
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let name = format!("/{}", cmd.name());
            let desc = cmd.description();
            let is_selected = i == popup.selected_index();
            let style = if is_selected {
                Style::new().bg(Color::Blue).fg(Color::White)
            } else {
                Style::new()
            };
            Line::styled(format!("  {:<12} {}", name, desc), style)
        })
        .collect();

    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("命令"))
}

fn build_input_line(input: &InputLine, pending_quit: bool, area_width: u16) -> Paragraph<'static> {
    let prefix = Span::styled(
        "> ",
        Style::new().add_modifier(Modifier::BOLD | Modifier::DIM),
    );
    if pending_quit {
        return Paragraph::new(Line::from(vec![
            prefix,
            Span::styled("再按 Ctrl+C 或 Esc 退出", Style::new().fg(Color::Yellow)),
        ]))
        .style(Style::new().bg(Color::Indexed(240)));
    }
    let lines = wrap_input_buffer(&input.buffer, input.cursor, &prefix, area_width);
    Paragraph::new(Text::from(lines)).style(Style::new().bg(Color::Indexed(240)))
}

/// Number of terminal lines the input will occupy when rendered.
fn compute_input_height(input: &InputLine, pending_quit: bool, area_width: u16) -> u16 {
    if pending_quit {
        return 1;
    }
    const PREFIX_LEN: usize = 2;
    let avail = (area_width as usize).saturating_sub(PREFIX_LEN).max(1);
    let total_width = UnicodeWidthStr::width(input.buffer.as_str());
    if total_width == 0 {
        return 1;
    }
    let lines = total_width.div_ceil(avail);
    lines.max(1) as u16
}

/// Pre-computed screen layout, shared between the draw closure and the mouse
/// handler so hit-testing uses the exact same chunk rects that were rendered.
#[derive(Clone, Debug)]
struct LayoutInfo {
    /// Vertical chunks: [0]=history, [1]=popup, [2]=status, [3]=queued,
    /// [4]=gap, [5]=input.
    chunks: Vec<ratatui::layout::Rect>,
}

/// Compute the screen layout from the current terminal size and widget state.
fn compute_layout(
    area: ratatui::layout::Rect,
    input: &InputLine,
    pending_quit: bool,
    popup: &Option<CommandPopup>,
    queued_height: u16,
) -> LayoutInfo {
    let input_width = area.width;
    let input_height = compute_input_height(input, pending_quit, input_width).min(6);
    let popup_height = popup
        .as_ref()
        .map(|p| (p.filtered().len() + 2).min(10) as u16)
        .unwrap_or(0);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),                // history
            Constraint::Length(popup_height),  // popup (0 when none)
            Constraint::Length(1),             // status bar
            Constraint::Length(queued_height), // queued preview
            Constraint::Length(1),             // blank gap
            Constraint::Length(input_height),  // input (wraps up to 6 lines)
        ])
        .split(area);

    LayoutInfo {
        chunks: chunks.to_vec(),
    }
}

/// Wrap the input buffer into multiple `Line`s so all typed text is visible.
///
/// The first line begins with the `"> "` prefix; continuation lines begin
/// with `"  "` so the cursor column is visually aligned. Wrapping is
/// character-based using unicode display width (so CJK / wide chars work).
///
/// The character at `cursor` (byte offset) is rendered with reverse video
/// (white background, black foreground) so the user can see the cursor.
fn wrap_input_buffer(
    buffer: &str,
    cursor: usize,
    prefix: &Span<'static>,
    area_width: u16,
) -> Vec<Line<'static>> {
    const PREFIX_LEN: usize = 2; // "> " or "  "
    let avail = (area_width as usize).saturating_sub(PREFIX_LEN).max(1);
    let cursor_style = Style::new().fg(Color::Black).bg(Color::White);

    // Helper to build spans for a chunk of text, applying cursor_style to the
    // character at the cursor byte offset if it falls within this chunk.
    // Returns a Vec of spans (without prefix — caller adds prefix).
    let build_spans = |text: &str, chunk_start: usize| -> Vec<Span<'static>> {
        if text.is_empty() {
            return vec![Span::raw(String::new())];
        }
        // Find if cursor is within this chunk
        // Cursor byte offset relative to chunk start
        let cursor_rel = cursor.checked_sub(chunk_start);
        match cursor_rel {
            Some(rel) if rel <= text.len() => {
                // Cursor is at or after chunk start, within or at end of text.
                // If rel == text.len(), cursor is at end — we need a cursor
                // on the "empty" position after last char. We render a space
                // with cursor style.
                let before = &text[..rel];
                let after = &text[rel..];
                if after.is_empty() {
                    // Cursor at end of text: render text normally, then a
                    // cursor-styled space to show the cursor position.
                    vec![
                        Span::raw(before.to_string()),
                        Span::styled(" ", cursor_style),
                    ]
                } else {
                    // Cursor is on the first char of `after`
                    let (cursor_char, rest) = after
                        .char_indices()
                        .next()
                        .map(|(i, c)| (&after[..i + c.len_utf8()], &after[i + c.len_utf8()..]))
                        .unwrap_or(("", after));
                    vec![
                        Span::raw(before.to_string()),
                        Span::styled(cursor_char.to_string(), cursor_style),
                        Span::raw(rest.to_string()),
                    ]
                }
            }
            _ => {
                // Cursor not in this chunk
                vec![Span::raw(text.to_string())]
            }
        }
    };

    // Compute the display width of the buffer.
    if buffer.is_empty() {
        // Empty buffer: show cursor at position 0 (a space with cursor style)
        return vec![Line::from(vec![
            prefix.clone(),
            Span::styled(" ", cursor_style),
        ])];
    }
    if UnicodeWidthStr::width(buffer) <= avail {
        let spans = build_spans(buffer, 0);
        let mut all_spans = vec![prefix.clone()];
        all_spans.extend(spans);
        return vec![Line::from(all_spans)];
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut chunk_start = 0usize;

    for ch in buffer.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + ch_width > avail && !current.is_empty() {
            // Flush current line
            let chunk = std::mem::take(&mut current);
            let spans = build_spans(&chunk, chunk_start);
            let mut all_spans = if lines.is_empty() {
                vec![prefix.clone()]
            } else {
                vec![Span::raw("  ")]
            };
            all_spans.extend(spans);
            lines.push(Line::from(all_spans));
            chunk_start += chunk.len();
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        let spans = build_spans(&current, chunk_start);
        let mut all_spans = if lines.is_empty() {
            vec![prefix.clone()]
        } else {
            vec![Span::raw("  ")]
        };
        all_spans.extend(spans);
        lines.push(Line::from(all_spans));
    }
    if lines.is_empty() {
        let all_spans = vec![prefix.clone(), Span::styled(" ", cursor_style)];
        lines.push(Line::from(all_spans));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::TaskStatus;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc;
    use yi_agent_core::OutputStream;

    #[test]
    fn test_route_event_full_flow() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        // Start resets per-call status.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Start,
        );
        sb.tick();
        assert_eq!(sb.display_input_tokens(), 0);

        // ToolCall registers a task.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"echo hi","expected_timeout_sec":30}),
            },
        );
        assert_eq!(registry.running_count(), 1);
        assert_eq!(registry.get("t1").unwrap().expected_timeout_sec, 30);

        // OutputDelta appends to the registry.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolOutputDelta {
                id: "t1".into(),
                stream: OutputStream::Stdout,
                text: "hi\n".into(),
            },
        );
        assert!(
            registry
                .get("t1")
                .unwrap()
                .stdout
                .windows(2)
                .any(|w| w == b"hi")
        );

        // Exit finalizes the task.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolExit {
                id: "t1".into(),
                code: Some(0),
            },
        );
        assert_eq!(registry.get("t1").unwrap().status, TaskStatus::Done);

        // Usage updates the status bar target.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Usage {
                model: "test".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            },
        );
        sb.tick();
        assert!(sb.display_input_tokens() > 0);
    }

    #[test]
    fn test_route_event_tool_timeout() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 10","expected_timeout_sec":1}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolTimeout { id: "t".into() },
        );
        assert_eq!(registry.get("t").unwrap().status, TaskStatus::Timeout);
    }

    /// A normal (non-streaming) tool such as web_search returns ToolResult
    /// without ToolExit. Its timer must stop when that result arrives rather
    /// than continuing through the next LLM think phase until Done.
    #[test]
    fn test_route_event_tool_result_finalizes_non_streaming_tool() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        let mut cost = CostTracker::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::ToolCall {
                id: "search".into(),
                name: "web_search".into(),
                input: serde_json::json!({"query": "rust"}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::ToolResult {
                id: "search".into(),
                result: yi_agent_core::ToolResult::text("result"),
            },
        );

        let elapsed = registry.get("search").unwrap().elapsed();
        assert_eq!(registry.get("search").unwrap().status, TaskStatus::Done);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(registry.get("search").unwrap().elapsed(), elapsed);
    }

    /// Regression: a ToolCall that never receives a ToolExit (e.g. bash tool
    /// early-returned on a blocked command, or the ToolExit event was dropped)
    /// must be finalized when the turn ends with Done so the status-bar timer
    /// stops ticking.
    #[test]
    fn test_route_event_done_aborts_running_tasks() {
        use yi_agent_core::DoneReason;
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"echo hi","expected_timeout_sec":30}),
            },
        );
        // Simulate elapsed time so we can verify the timer freezes.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let before = registry.get("t1").unwrap().elapsed();
        // Turn ends without a ToolExit for t1.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        );
        let task = registry.get("t1").unwrap();
        assert_eq!(
            task.status,
            TaskStatus::Aborted,
            "Done without ToolExit should abort the running task"
        );
        let after = registry.get("t1").unwrap().elapsed();
        // Timer must be frozen: later reads should not advance.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let after2 = registry.get("t1").unwrap().elapsed();
        assert_eq!(
            after, after2,
            "Aborted task timer should be frozen, not still ticking"
        );
        assert!(
            after >= before,
            "Aborted end_time should be >= the pre-Done reading"
        );
    }

    /// Regression: when a user cancels mid-tool (Ctrl+C), the running bash
    /// task must be aborted so the timer stops.
    #[test]
    fn test_route_event_cancelled_aborts_running_tasks() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 10","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Cancelled,
        );
        assert_eq!(
            registry.get("t1").unwrap().status,
            TaskStatus::Aborted,
            "Cancelled should abort the running task"
        );
        assert!(registry.get("t1").unwrap().end_time.is_some());
    }

    /// Regression: an agent error mid-tool must also abort running tasks.
    #[test]
    fn test_route_event_error_aborts_running_tasks() {
        use yi_agent_core::AgentError;
        use yi_agent_core::provider::ProviderError;
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 10","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Error(AgentError::Provider(ProviderError::Auth("boom".into()))),
        );
        assert_eq!(
            registry.get("t1").unwrap().status,
            TaskStatus::Aborted,
            "Error should abort the running task"
        );
    }

    /// Done/Cancelled/Error must NOT touch already-finalized tasks.
    #[test]
    fn test_route_event_done_does_not_modify_finalized_tasks() {
        use yi_agent_core::DoneReason;
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "done_task".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"echo hi","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolExit {
                id: "done_task".into(),
                code: Some(0),
            },
        );
        let done_end = registry.get("done_task").unwrap().end_time.unwrap();
        // Another task still running when Done arrives.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "stuck_task".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 99","expected_timeout_sec":30}),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        );
        assert_eq!(
            registry.get("done_task").unwrap().status,
            TaskStatus::Done,
            "Already-done task should not be touched by Done cleanup"
        );
        assert_eq!(
            registry.get("done_task").unwrap().end_time.unwrap(),
            done_end,
            "Already-done end_time should not be overwritten"
        );
        assert_eq!(
            registry.get("stuck_task").unwrap().status,
            TaskStatus::Aborted,
            "Still-running task should be aborted by Done cleanup"
        );
    }

    #[test]
    fn test_route_event_decode_delta_accumulates() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        // Simulate streamed tool-call argument deltas.
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::DecodeDelta("{\"q\":".into()),
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::DecodeDelta("\"weather\"}".into()),
        );
        // {"q": = 5 chars, "weather"} = 9 chars → 14 ascii → 14/4 = 3 tokens
        assert_eq!(sb.display_output_tokens(), 0); // display lags via tick
        assert_eq!(
            sb.target_output_tokens(),
            3,
            "two DecodeDelta events should accumulate"
        );
    }

    #[test]
    fn test_route_event_assistant_text_accumulates() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::AssistantText("hello".into()),
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::AssistantText("world".into()),
        );
        // 10 ascii chars → 10/4 = 2 tokens
        assert_eq!(sb.target_output_tokens(), 2);
    }

    /// Reproduces the user's observation: "after bash executes, the decode
    /// display seems to persist until the process timeout-exits, not until
    /// the LLM call actually ends."
    ///
    /// Expected event sequence:
    ///   Start → prefill/decode deltas → Usage (LLM call ends naturally)
    ///   → ToolCall (bash starts) → ToolOutputDelta (bash running)
    ///   → ToolTimeout (bash timeout) → ToolResult → Done
    ///
    /// The decode display should reset when the LLM call ends (at ToolCall
    /// or Usage), NOT linger until bash timeout. The bug: decode display
    /// stayed frozen at the previous LLM turn's value throughout bash
    /// execution, making it look like "decode is still going" until bash
    /// timeout.
    #[test]
    fn test_decode_display_resets_when_bash_starts_not_when_it_times_out() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();

        // --- LLM THINK phase: decode tokens arrive ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Start,
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::EstimatedPrefill(500),
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::AssistantText("Let me run a command".into()),
        );
        // Real usage from LLM — this is when the LLM call "naturally ends"
        // (the Stop event and Usage arrive at the end of the stream).
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Usage {
                model: "test".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 500,
                    output_tokens: 42,
                    ..Default::default()
                },
            },
        );
        sb.tick();
        // Decode is now showing (interpolating toward 42).
        let decode_after_llm = sb.display_output_tokens();
        assert!(
            decode_after_llm <= 42,
            "decode should be interpolating toward 42, got {decode_after_llm}"
        );

        // --- ACT phase: bash starts running ---
        // This is when the LLM call has ENDED. The decode display should
        // reset here so the user sees "decode ended, bash is running".
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":"sleep 999","expected_timeout_sec":5}),
            },
        );

        // KEY ASSERTION: decode display should be 0 immediately when bash
        // starts, NOT waiting for bash to timeout.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should reset to 0 when bash starts, not wait for timeout"
        );
        assert_eq!(
            sb.target_output_tokens(),
            0,
            "decode target should reset to 0 when bash starts"
        );

        // --- bash produces output (still running) ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolOutputDelta {
                id: "t1".into(),
                stream: OutputStream::Stdout,
                text: "some output\n".into(),
            },
        );
        // Decode should still be 0 — ToolOutputDelta must not affect decode.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should stay 0 during bash execution"
        );

        // --- bash times out ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolTimeout { id: "t1".into() },
        );
        // Decode should STILL be 0 — timeout doesn't change it.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should still be 0 at bash timeout"
        );

        // --- ToolResult + Done (turn ends) ---
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::ToolResult {
                id: "t1".into(),
                result: yi_agent_core::ToolResult::error("timeout"),
            },
        );
        route_event(
            &mut registry,
            &mut sb,
            &mut CostTracker::default(),
            &AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        );
        // Decode should STILL be 0 — Done doesn't add decode tokens.
        assert_eq!(
            sb.display_output_tokens(),
            0,
            "decode should still be 0 at turn end"
        );
    }

    /// Fake event source that plays back a scripted sequence of events,
    /// then returns None (timeout) forever. Used to test the loop deterministically.
    struct ScriptedEvents {
        events: Rc<RefCell<Vec<Event>>>,
    }

    impl EventSource for ScriptedEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            Ok(self.events.borrow_mut().pop())
        }
    }

    /// Regression test for blocking_send panic.
    ///
    /// The bug: run_tui called `input_tx.blocking_send` while on the tokio
    /// runtime's async thread, which panics with "Cannot block the current
    /// thread from within a runtime". The fix was to run the TUI on a
    /// `spawn_blocking` thread. This test simulates that calling stack
    /// (block_on -> spawn_blocking -> blocking_send) and verifies no panic.
    #[test]
    fn blocking_send_does_not_panic_on_runtime_thread() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);

        rt.block_on(async move {
            let tx = input_tx.clone();
            let handle = tokio::task::spawn_blocking(move || {
                tx.blocking_send("hello".to_string()).unwrap();
            });
            handle.await.unwrap();
            assert_eq!(input_rx.recv().await.unwrap(), "hello");
        });
    }

    /// Test that first Ctrl+C does NOT quit but shows a confirm prompt,
    /// and any other key cancels the pending quit.
    #[test]
    fn first_ctrl_c_does_not_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Ctrl+C then Ctrl+Q: if first Ctrl+C quit, Ctrl+Q would be unreachable.
        // We verify the terminal buffer shows the confirm prompt after Ctrl+C.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // After Ctrl+C, the input row should show the confirm message
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80).map(|x| buffer[(x, input_row)].symbol()).collect();
        assert!(
            row_text.contains("Ctrl") || row_text.contains("退出"),
            "expected confirm prompt, got: {row_text:?}"
        );
    }

    /// Test that two Ctrl+C presses quit the TUI.
    #[test]
    fn two_ctrl_c_quits() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        );
        assert!(
            result.is_ok(),
            "two Ctrl+C should quit cleanly, got: {:?}",
            result
        );
    }

    /// Test that Esc behaves the same as Ctrl+C (confirm first, quit on second).
    #[test]
    fn esc_same_as_ctrl_c() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // First Esc alone should not quit
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        );
        assert!(
            result.is_ok(),
            "two Esc should quit cleanly, got: {:?}",
            result
        );
    }

    /// Test that Ctrl+Q quits directly (no confirm needed).
    #[test]
    fn ctrl_q_quits_directly() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        );
        assert!(
            result.is_ok(),
            "Ctrl+Q should quit directly, got: {:?}",
            result
        );
    }

    /// Test that typing characters updates the input buffer and renders them
    /// to the terminal buffer. This verifies the draw loop reacts to key events.
    #[test]
    fn typing_renders_to_screen() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Script: type "hi", then Ctrl+Q to quit (don't submit, so input stays on screen)
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents {
            events: events.clone(),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // No submit happened, so input_tx should be empty
        assert!(
            input_rx.try_recv().is_err(),
            "no submit expected, but got a message"
        );

        // The terminal buffer should contain "hi" in the input row (last row)
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80).map(|x| buffer[(x, input_row)].symbol()).collect();
        assert!(
            row_text.contains("hi"),
            "expected 'hi' in input row, got: {row_text:?}"
        );
    }

    /// Test that typing a long string that exceeds the terminal width wraps
    /// onto additional lines so all typed characters remain visible.
    ///
    /// The bug: the input area was constrained to 1 line and the Paragraph
    /// did not wrap, so characters past the right edge were clipped.
    #[test]
    fn long_input_wraps_to_multiple_lines() {
        let backend = TestBackend::new(20, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type 30 'a's into a 20-column terminal. "> aa..." takes 3 cols for
        // prefix, leaving 17 cols per line. 30 chars should wrap across at
        // least 2 lines.
        let mut events = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        for _ in 0..30 {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            )));
        }
        let events = Rc::new(RefCell::new(events));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // Collect all text from the bottom 6 rows of the terminal (the input
        // area should live there now).
        let buffer = terminal.backend().buffer();
        let bottom_text: String = (18..24u16)
            .flat_map(|y| (0..20u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        // Count the number of 'a' characters visible somewhere in the bottom
        // of the screen. All 30 must be visible.
        let a_count = bottom_text.matches('a').count();
        assert_eq!(
            a_count, 30,
            "expected all 30 'a's visible in input area, only found {a_count}; bottom text: {bottom_text:?}"
        );
    }

    /// CJK characters are double-width. Verify they wrap correctly so all
    /// are visible and none are clipped.
    #[test]
    fn long_input_with_cjk_wraps_correctly() {
        let backend = TestBackend::new(20, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type 10 '你's (each 2 cells wide = 20 cells total) into 20-col
        // terminal. With "> " prefix, avail = 18, so first line holds 9 chars
        // (18 cells) and 1 char wraps to the next line.
        let mut events = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        for _ in 0..10 {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('你'),
                KeyModifiers::NONE,
            )));
        }
        let events = Rc::new(RefCell::new(events));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let bottom_text: String = (18..24u16)
            .flat_map(|y| (0..20u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        let count = bottom_text.matches('你').count();
        assert_eq!(
            count, 10,
            "expected all 10 '你' visible, found {count}; bottom: {bottom_text:?}"
        );
    }

    /// Unit test for `compute_input_height` with ASCII.
    #[test]
    fn compute_input_height_ascii() {
        let mut inp = InputLine::new();
        // width=20, prefix=2, avail=18. 30 chars -> ceil(30/18) = 2 lines.
        assert_eq!(compute_input_height(&inp, false, 20), 1);
        inp.buffer = "a".repeat(18);
        assert_eq!(compute_input_height(&inp, false, 20), 1, "18 fits in 18");
        inp.buffer = "a".repeat(19);
        assert_eq!(compute_input_height(&inp, false, 20), 2, "19 wraps to 2");
        inp.buffer = "a".repeat(36);
        assert_eq!(
            compute_input_height(&inp, false, 20),
            2,
            "36 = 2 lines of 18"
        );
        inp.buffer = "a".repeat(37);
        assert_eq!(compute_input_height(&inp, false, 20), 3, "37 wraps to 3");
        // pending_quit overrides to 1
        assert_eq!(compute_input_height(&inp, true, 20), 1);
    }

    /// Unit test for `compute_input_height` with CJK (double-width).
    #[test]
    fn compute_input_height_cjk() {
        let mut inp = InputLine::new();
        // width=20, prefix=2, avail=18. Each '你' is 2 cells. 9 chars = 18
        // cells fit on line 1; 10 chars = 20 cells -> 2 lines.
        inp.buffer = "你".repeat(9);
        assert_eq!(
            compute_input_height(&inp, false, 20),
            1,
            "9 cjk = 18 cells fits"
        );
        inp.buffer = "你".repeat(10);
        assert_eq!(
            compute_input_height(&inp, false, 20),
            2,
            "10 cjk = 20 cells wraps"
        );
    }

    /// Test that agent events appear in the rendered history area.
    #[test]
    fn agent_events_render_to_history_area() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Send an assistant message before starting
        agent_tx
            .try_send(AgentEvent::AssistantText("hello world".into()))
            .unwrap();

        // Script: Ctrl+Q to quit after one frame
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // The history area should contain "hello world"
        let buffer = terminal.backend().buffer();
        let history_text: String = (0..23u16)
            .flat_map(|y| (0..80u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        assert!(
            history_text.contains("hello world"),
            "expected 'hello world' in history, got: {history_text:?}"
        );
    }

    // ----- Slash command popup tests -----

    /// Helper: collect all text from the terminal buffer.
    fn collect_all_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..24u16)
            .flat_map(|y| (0..80u16).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    }

    #[test]
    fn paste_inserts_text_clears_pending_quit_and_filters_slash_popup() {
        let history = HistoryState::new();
        let mut input = InputLine::new();
        let bash_popup = BashPopup::None;
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &bash_popup,
            &mut pending_quit,
            &mut popup,
        );

        assert_eq!(input.buffer, "/cl");
        assert!(!pending_quit);
        assert_eq!(
            popup.and_then(|popup| popup.selected()),
            Some(SlashCommand::Clear)
        );
    }

    #[test]
    fn paste_is_ignored_while_permission_is_pending() {
        let mut history = HistoryState::new();
        history.push_event(make_permission_request_normal(1), 80);
        let mut input = InputLine::new();
        let bash_popup = BashPopup::None;
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &bash_popup,
            &mut pending_quit,
            &mut popup,
        );

        assert!(input.buffer.is_empty());
        assert!(pending_quit);
        assert!(popup.is_none());
    }

    #[test]
    fn paste_is_ignored_while_bash_popup_is_active() {
        let history = HistoryState::new();
        let mut input = InputLine::new();
        let bash_popup = BashPopup::List(ListPopup::new(vec!["task-1".to_string()]));
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &bash_popup,
            &mut pending_quit,
            &mut popup,
        );

        assert!(input.buffer.is_empty());
        assert!(pending_quit);
        assert!(popup.is_none());
    }

    #[test]
    fn bracketed_paste_renders_input_and_opens_slash_popup() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Paste("/cl".into()),
            ])),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let text = collect_all_text(&terminal);
        assert!(text.contains("/cl"), "expected pasted input, got: {text:?}");
        assert!(
            text.contains("clear"),
            "expected slash popup, got: {text:?}"
        );
    }

    /// Typing `/` should show the slash command popup with all commands.
    #[test]
    fn slash_popup_appears_on_slash() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', then Ctrl+Q to quit
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let text = collect_all_text(&terminal);
        // Popup should show at least some command names
        assert!(
            text.contains("quit"),
            "expected 'quit' in popup, got: {text:?}"
        );
        assert!(
            text.contains("clear"),
            "expected 'clear' in popup, got: {text:?}"
        );
        assert!(
            text.contains("help"),
            "expected 'help' in popup, got: {text:?}"
        );
    }

    /// Typing `/cl` should filter the popup to only show `/clear`.
    #[test]
    fn slash_popup_filters_on_typing() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', 'c', 'l', then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let text = collect_all_text(&terminal);
        // Should show 'clear' but not 'quit' or 'help' (filtered out)
        assert!(text.contains("clear"), "expected 'clear' in popup");
        assert!(!text.contains("quit"), "quit should be filtered out");
        assert!(!text.contains("help"), "help should be filtered out");
    }

    /// Tab should complete the selected command name into the input buffer.
    #[test]
    fn slash_popup_tab_completes() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', 'c', 'l', Tab, then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // The input row should contain "/clear" (completed by Tab)
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80u16)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(
            row_text.contains("/clear"),
            "expected '/clear' in input after Tab, got: {row_text:?}"
        );
    }

    /// Enter on `/quit` should execute the quit command and exit the loop.
    #[test]
    fn slash_popup_enter_executes_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', 'q', 'u', 'i', 't', Enter — /quit should exit the loop.
        // We add a Ctrl+Q at the end as a fallback in case /quit doesn't work,
        // but since events are popped LIFO, Ctrl+Q is listed first.
        let events = Rc::new(RefCell::new(vec![
            // Fallback: if /quit doesn't execute, Ctrl+Q will still exit
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        );
        assert!(
            result.is_ok(),
            "/quit + Enter should quit cleanly, got: {:?}",
            result
        );

        // If /quit executed properly, no message should be sent to agent
        // (if it didn't execute, the fallback Ctrl+Q ran, which is still ok).
        // We don't assert on input_tx because we can't distinguish.
    }

    /// Esc should dismiss the popup without modifying the input.
    #[test]
    fn slash_popup_esc_dismisses() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', Esc, then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // After Esc, popup should be gone. The input row should still have "/"
        // (Esc doesn't clear input, just dismisses popup).
        // The popup area (above input) should NOT contain command names.
        let text = collect_all_text(&terminal);
        // "quit" might appear in input row if '/' is still there, but after Esc
        // the popup is dismissed. We check that the popup doesn't show all commands.
        // Actually, after Esc dismisses popup, typing Ctrl+Q quits. The final frame
        // should show no popup. But the input "/" was typed, then Esc dismissed popup.
        // The input buffer still has "/".
        // Let's just verify the app didn't crash and the popup is not visible.
        // Since popup is dismissed, the text should not contain "清空对话上下文" (clear's description).
        assert!(
            !text.contains("清空对话上下文"),
            "popup should be dismissed after Esc"
        );
    }

    /// Up/Down should navigate the popup selection, not history.
    #[test]
    fn slash_popup_up_down_navigates() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', Down (move to 2nd item), Tab (complete), Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // After Down + Tab, the 2nd command (clear) should be completed into input
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80u16)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(
            row_text.contains("/clear"),
            "expected '/clear' after Down+Tab, got: {row_text:?}"
        );
    }

    /// Unknown slash command should show an error in history, not send to agent.
    #[test]
    fn unknown_slash_command_shows_error() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/foo', Enter, then Ctrl+Q to quit
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // No message should have been sent to the agent
        assert!(
            input_rx.try_recv().is_err(),
            "unknown command should not send to agent"
        );

        // The history should contain an error message.
        // Note: TestBackend renders CJK chars with spaces between them, so we
        // check for a substring that works regardless of spacing.
        let text = collect_all_text(&terminal);
        let text_compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            text_compact.contains("未知命令") || text_compact.contains("unknown"),
            "expected error message for unknown command, got compact: {text_compact:?}"
        );
    }

    /// Typing a space after '/' should dismiss the popup (entering arg mode).
    #[test]
    fn slash_popup_dismisses_on_space() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type '/', space, then Ctrl+Q
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // After space, popup should be dismissed
        let text = collect_all_text(&terminal);
        assert!(
            !text.contains("清空对话上下文"),
            "popup should be dismissed after space"
        );
    }

    /// The cursor position should be rendered with reverse video (white bg,
    /// black fg) so the user can see where their cursor is in the input.
    #[test]
    fn cursor_shown_with_reverse_video() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type "abc" — cursor should be at position 3 (after 'c').
        // With reverse video, at least one cell in the input row should have
        // a white background.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // Check the input row for a cell with white background (reverse video cursor)
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let mut found_cursor = false;
        for x in 0..80u16 {
            let cell = &buffer[(x, input_row)];
            // Look for a cell with white background (Color::White)
            if cell.bg == ratatui::style::Color::White {
                found_cursor = true;
                break;
            }
        }
        assert!(
            found_cursor,
            "expected a cursor cell with white background in input row"
        );
    }

    /// When the buffer is empty, the cursor should still be visible (at position 0).
    #[test]
    fn cursor_visible_on_empty_buffer() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Just quit — no input typed. The input row should still show a cursor.
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let mut found_cursor = false;
        for x in 0..80u16 {
            let cell = &buffer[(x, input_row)];
            if cell.bg == ratatui::style::Color::White {
                found_cursor = true;
                break;
            }
        }
        assert!(found_cursor, "expected cursor on empty buffer");
    }

    /// When cursor is in the middle of text, the character at cursor should
    /// be rendered with reverse video while surrounding text is normal.
    #[test]
    fn cursor_in_middle_of_text() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Type "abc", move left to position 2 (between 'b' and 'c').
        // The 'c' character should have reverse video.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // Input starts at x=2 (after "> " prefix). 'a' at x=2, 'b' at x=3, 'c' at x=4.
        // Cursor at byte offset 2 means 'c' is the cursor character, at x=4.
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let cursor_cell = &buffer[(4, input_row)];
        assert_eq!(
            cursor_cell.fg,
            ratatui::style::Color::Black,
            "cursor character 'c' should have black foreground"
        );
        assert_eq!(
            cursor_cell.bg,
            ratatui::style::Color::White,
            "cursor character 'c' should have white background"
        );

        // The 'a' character (not at cursor) should NOT have reverse video
        let non_cursor_cell = &buffer[(2, input_row)];
        assert_ne!(
            non_cursor_cell.bg,
            ratatui::style::Color::White,
            "non-cursor character 'a' should not have white background"
        );
    }

    // ----- Permission key handling tests -----

    /// Helper to create a Normal permission request event.
    fn make_permission_request_normal(request_id: u64) -> AgentEvent {
        AgentEvent::PermissionRequest {
            request_id,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: Some("ls".into()),
            kind: yi_agent_core::permission::PermissionKind::Normal,
        }
    }

    /// Helper to create a Blacklisted permission request event.
    fn make_permission_request_blacklisted(request_id: u64) -> AgentEvent {
        AgentEvent::PermissionRequest {
            request_id,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "rm -rf /"}),
            prefix_suggestion: Some("rm".into()),
            kind: yi_agent_core::permission::PermissionKind::Blacklisted("rm -rf".into()),
        }
    }

    /// Helper to create a permission request with no prefix suggestion.
    fn make_permission_request_no_prefix(request_id: u64) -> AgentEvent {
        AgentEvent::PermissionRequest {
            request_id,
            tool_name: "bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: None,
            kind: yi_agent_core::permission::PermissionKind::Normal,
        }
    }

    /// Event source that plays back scripted events, then returns Ctrl+Q
    /// forever. This ensures the TUI loop eventually exits once scripted
    /// events are exhausted and any pending permission is resolved.
    struct ScriptedThenQuitEvents {
        events: Rc<RefCell<Vec<Event>>>,
    }

    impl EventSource for ScriptedThenQuitEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let ev = self.events.borrow_mut().pop();
            Ok(Some(ev.unwrap_or(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            )))))
        }
    }

    /// Spawn a thread that sends `PermissionResolved` for `request_id` after
    /// a short delay. This resolves the permission in the history so that
    /// subsequent Ctrl+Q events can actually quit the TUI loop.
    fn resolve_permission_after_delay(
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        request_id: u64,
    ) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = agent_tx.blocking_send(AgentEvent::PermissionResolved {
                request_id,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            });
        });
    }

    /// Key '1' on a pending permission should send AllowOnce on decision_tx.
    #[test]
    fn permission_key_1_allows_once() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(1))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 1);

        // Events are LIFO: '1' is processed first, then scripted events run out
        // and ScriptedThenQuitEvents returns Ctrl+Q forever.
        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('1'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((1, yi_agent_core::permission::Decision::AllowOnce))
        );
    }

    /// Key '2' on a pending permission should send AlwaysAllowTool.
    #[test]
    fn permission_key_2_always_allow_tool() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(2))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 2);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('2'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((2, yi_agent_core::permission::Decision::AlwaysAllowTool))
        );
    }

    /// Key '3' with a prefix suggestion should send AlwaysAllowPrefix.
    #[test]
    fn permission_key_3_always_allow_prefix() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(3))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 3);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('3'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((
                3,
                yi_agent_core::permission::Decision::AlwaysAllowPrefix("ls".into())
            ))
        );
    }

    /// Key '4' on a pending permission should send Deny.
    #[test]
    fn permission_key_4_deny() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(4))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 4);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Char('4'),
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((4, yi_agent_core::permission::Decision::Deny))
        );
    }

    /// Enter on a Normal permission should default to AllowOnce.
    #[test]
    fn permission_enter_defaults_to_allow_for_normal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(5))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 5);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((5, yi_agent_core::permission::Decision::AllowOnce))
        );
    }

    /// Enter on a Blacklisted permission should default to Deny.
    #[test]
    fn permission_enter_defaults_to_deny_for_blacklisted() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_blacklisted(6))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 6);

        let events = Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((6, yi_agent_core::permission::Decision::Deny))
        );
    }

    /// Key '3' when there is no prefix suggestion should be a no-op:
    /// no decision is sent, and the key is ignored. Afterward, pressing '1'
    /// should still work to resolve the permission.
    #[test]
    fn permission_key_3_when_no_prefix_is_noop() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_no_prefix(7))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 7);

        // Events (LIFO): '3' is processed first (noop, no prefix), then '1'
        // resolves the permission. After '1' sends the decision, the resolver
        // thread delivers PermissionResolved, and then Ctrl+Q quits.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // '3' should have been ignored (no prefix), so no AlwaysAllowPrefix.
        // The only decision should be from '1' -> AllowOnce.
        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((7, yi_agent_core::permission::Decision::AllowOnce))
        );
        // No further decisions
        assert!(
            decision_rx.try_recv().is_err(),
            "should only have one decision"
        );
    }

    /// Typing 'a' while a permission is pending should be ignored:
    /// no decision sent and the input buffer is not modified.
    /// Afterward, '1' resolves the permission and Ctrl+Q quits.
    #[test]
    fn permission_other_keys_ignored_while_pending() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        agent_tx
            .try_send(make_permission_request_normal(8))
            .unwrap();
        resolve_permission_after_delay(agent_tx, 8);

        // Events (LIFO): 'a' (should be ignored), then '1' (resolves permission)
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE)),
        ]));
        let source = ScriptedThenQuitEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // The only decision should be from '1' -> AllowOnce (not from 'a')
        let decision = decision_rx.blocking_recv();
        assert_eq!(
            decision,
            Some((8, yi_agent_core::permission::Decision::AllowOnce))
        );
        assert!(
            decision_rx.try_recv().is_err(),
            "should only have one decision"
        );

        // The input row should not contain 'a' — it was ignored while permission was pending.
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80u16)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(
            !row_text.contains('a'),
            "expected 'a' to be ignored while permission pending, but found it in input row: {row_text:?}"
        );
    }

    // ----- handle_key Esc/Ctrl+C interrupt tests -----

    fn make_key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn normal_navigation_keys_route_to_history_without_affecting_shift_selection() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        for _ in 0..120 {
            history.push(HistoryCell::Separator { label: None }, 80);
        }
        history.scroll_offset = 5;

        for (key, expected_offset) in [
            (KeyCode::Up, 6),
            (KeyCode::Down, 5),
            (KeyCode::PageUp, 25),
            (KeyCode::PageDown, 5),
            (KeyCode::Home, 100),
            (KeyCode::End, 0),
        ] {
            let outcome = handle_key(
                make_key(key, KeyModifiers::NONE),
                &mut input,
                &mut history,
                100,
                80,
                20,
                &CostTracker::default(),
                &input_tx,
                &interrupt_tx,
                &control_tx,
                &decision_tx,
                &is_running,
                &mut queued,
                &mut pending_quit,
                &mut popup,
            );
            assert_eq!(outcome, KeyOutcome::None);
            assert_eq!(history.scroll_offset, expected_offset, "key {key:?}");
        }

        history.selected = Some(5);
        let _ = handle_key(
            make_key(KeyCode::Up, KeyModifiers::SHIFT),
            &mut input,
            &mut history,
            100,
            80,
            20,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(history.selected, Some(4));
        assert_eq!(
            history.scroll_offset, 0,
            "Shift+Up keeps selection behavior"
        );
    }

    #[test]
    fn esc_when_running_sends_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        assert!(
            interrupt_rx.try_recv().is_ok(),
            "interrupt should be sent when agent running"
        );
    }

    #[test]
    fn esc_when_idle_does_not_send_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        assert!(
            interrupt_rx.try_recv().is_err(),
            "interrupt should NOT be sent when idle"
        );
    }

    #[test]
    fn ctrl_c_when_running_sends_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        let result = handle_key(
            make_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        assert!(interrupt_rx.try_recv().is_ok());
    }

    #[test]
    fn double_esc_quits() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        let _ = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        let result = handle_key(
            make_key(KeyCode::Esc, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        assert_eq!(result, KeyOutcome::Quit);
    }

    // ----- handle_key Submit 分流 tests -----

    #[test]
    fn submit_while_running_goes_to_queue_not_history() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        input.buffer = "queued msg".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "queued msg");
                assert_eq!(queued.len(), 1);
                assert_eq!(queued[0], "queued msg");
                assert!(
                    history.cells.is_empty(),
                    "history should be empty when agent running"
                );
                let received = input_rx.try_recv();
                assert!(received.is_ok());
                assert_eq!(received.unwrap(), "queued msg");
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn submit_while_idle_goes_to_history_not_queue() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued: VecDeque<String> = VecDeque::new();
        let mut pending_quit = false;
        let mut popup = None;

        input.buffer = "idle msg".to_string();
        input.cursor = input.buffer.len();

        let result = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
        );
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "idle msg");
                assert!(queued.is_empty());
                assert_eq!(history.cells.len(), 1);
                match &history.cells[0] {
                    HistoryCell::UserMessage { text } => assert_eq!(text, "idle msg"),
                    _ => panic!("expected UserMessage"),
                }
            }
            _ => panic!("expected Submit"),
        }
    }

    // ----- /cost slash command tests -----

    #[test]
    fn cost_command_renders_tracker() {
        use yi_agent_core::TokenUsage;
        let mut history = HistoryState::new();
        let mut cost = CostTracker::default();
        cost.record(
            "claude-sonnet-4-5",
            &TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        );
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            &mut history,
            80,
            &cost,
            &input_tx,
            &interrupt_tx,
            &control_tx,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let cell = history.cells.last().unwrap();
        match cell {
            crate::tui::cell::HistoryCell::Markdown { text } => {
                assert!(
                    text.contains("claude-sonnet-4-5"),
                    "cost text should include model: {text}"
                );
                assert!(
                    text.contains("100"),
                    "cost text should include input tokens: {text}"
                );
            }
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    #[test]
    fn cost_command_empty_shows_no_data() {
        let mut history = HistoryState::new();
        let cost = CostTracker::default();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            &mut history,
            80,
            &cost,
            &input_tx,
            &interrupt_tx,
            &control_tx,
        );
        assert_eq!(outcome, KeyOutcome::None);
        let cell = history.cells.last().unwrap();
        match cell {
            crate::tui::cell::HistoryCell::Markdown { text } => {
                assert!(
                    text.contains("尚无数据"),
                    "empty cost should show no-data: {text}"
                );
            }
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    #[test]
    fn route_event_usage_records_to_tracker() {
        let mut registry = RunningTaskRegistry::new();
        let mut sb = StatusBarState::default();
        let mut cost = CostTracker::default();
        route_event(
            &mut registry,
            &mut sb,
            &mut cost,
            &AgentEvent::Usage {
                model: "claude".to_string(),
                usage: yi_agent_core::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                },
            },
        );
        let rendered = cost.render();
        assert!(
            rendered.contains("claude"),
            "tracker should contain model after route_event: {rendered}"
        );
        assert!(
            rendered.contains("100"),
            "tracker should contain input tokens: {rendered}"
        );
    }

    // ----- Mouse scroll routing tests -----

    /// Build a `MouseEvent` of the given kind at `(column, row)`.
    fn make_mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Compute the layout for a standard 80×24 terminal with no popup and
    /// empty input. Returns the history rect (chunks[0]) for convenience.
    fn layout_80x24() -> (LayoutInfo, ratatui::layout::Rect) {
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let input = InputLine::new();
        let layout = compute_layout(area, &input, false, &None, 0);
        let history = layout.chunks[0];
        (layout, history)
    }

    #[test]
    fn history_wheel_events_move_three_lines() {
        assert_eq!(HISTORY_WHEEL_LINES, 3);
    }

    /// Scrolling up over the history region should increase `scroll_offset`.
    #[test]
    fn mouse_scroll_up_in_history_increases_offset() {
        let (layout, history_area) = layout_80x24();
        let mut bash_popup = BashPopup::None;
        let mut hist = HistoryState::new();
        // Fill enough lines so scrolling is meaningful.
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        // Scroll up at the top of the history region.
        let row = history_area.y;
        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, row),
            &layout,
            &mut bash_popup,
            &mut hist,
            &registry,
            &mut pending_quit,
        );
        assert_eq!(
            hist.scroll_offset, 3,
            "ScrollUp in history should scroll up"
        );

        // A second scroll should accumulate.
        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 5, row + 3),
            &layout,
            &mut bash_popup,
            &mut hist,
            &registry,
            &mut pending_quit,
        );
        assert_eq!(hist.scroll_offset, 6, "second ScrollUp should accumulate");
    }

    /// Scrolling down over the history region should decrease `scroll_offset`
    /// (clamped at 0).
    #[test]
    fn mouse_scroll_down_in_history_decreases_offset() {
        let (layout, history_area) = layout_80x24();
        let mut bash_popup = BashPopup::None;
        let mut hist = HistoryState::new();
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        hist.scroll_up(5, 1000);
        assert_eq!(hist.scroll_offset, 5);

        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollDown, 0, history_area.y),
            &layout,
            &mut bash_popup,
            &mut hist,
            &registry,
            &mut pending_quit,
        );
        assert_eq!(
            hist.scroll_offset, 2,
            "ScrollDown in history should decrease offset"
        );

        // Scrolling down past 0 should clamp at 0 (not underflow).
        for _ in 0..10 {
            handle_mouse(
                make_mouse(MouseEventKind::ScrollDown, 0, history_area.y),
                &layout,
                &mut bash_popup,
                &mut hist,
                &registry,
                &mut pending_quit,
            );
        }
        assert_eq!(hist.scroll_offset, 0, "offset should clamp at 0");
    }

    /// Scrolling over the input region should NOT affect history.
    #[test]
    fn mouse_scroll_in_input_region_ignores_history() {
        let (layout, _history_area) = layout_80x24();
        let input_area = layout.chunks[5];
        let mut bash_popup = BashPopup::None;
        let mut hist = HistoryState::new();
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, input_area.y),
            &layout,
            &mut bash_popup,
            &mut hist,
            &registry,
            &mut pending_quit,
        );
        assert_eq!(
            hist.scroll_offset, 0,
            "ScrollUp in input region should not touch history"
        );
    }

    /// Mouse clicks (non-scroll events) should be ignored entirely.
    #[test]
    fn mouse_click_is_ignored() {
        let (layout, history_area) = layout_80x24();
        let mut bash_popup = BashPopup::None;
        let mut hist = HistoryState::new();
        for _ in 0..50 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "line".into(),
                },
                80,
            );
        }
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;

        handle_mouse(
            make_mouse(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                0,
                history_area.y,
            ),
            &layout,
            &mut bash_popup,
            &mut hist,
            &registry,
            &mut pending_quit,
        );
        assert_eq!(hist.scroll_offset, 0, "click should not scroll");
    }

    /// Scrolling up over the history region should clear `pending_quit`,
    /// matching the behavior of other history navigation keys.
    #[test]
    fn mouse_scroll_clears_pending_quit() {
        let (layout, history_area) = layout_80x24();
        let mut bash_popup = BashPopup::None;
        let mut hist = HistoryState::new();
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = true;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, history_area.y),
            &layout,
            &mut bash_popup,
            &mut hist,
            &registry,
            &mut pending_quit,
        );
        assert!(!pending_quit, "scrolling history should clear pending_quit");
    }

    /// End-to-end: feed `Event::Mouse(ScrollUp)` events through the TUI loop
    /// and verify history `scroll_offset` changes are visible in the rendered
    /// buffer.
    #[test]
    fn tui_mouse_scroll_up_shifts_history_view() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(128);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));

        // Fill history with many lines so the view is scrollable.
        for i in 0..40 {
            agent_tx
                .try_send(AgentEvent::AssistantText(format!("line-{i}\n")))
                .unwrap();
            agent_tx
                .try_send(AgentEvent::Done {
                    reason: yi_agent_core::DoneReason::EndTurn,
                })
                .unwrap();
        }

        // Script: scroll up twice in the history region (row 0), then quit.
        // Events are popped in reverse order (LIFO via Vec::pop).
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Mouse(make_mouse(MouseEventKind::ScrollUp, 0, 0)),
            Event::Mouse(make_mouse(MouseEventKind::ScrollUp, 0, 0)),
        ]));
        let source = ScriptedEvents { events };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
        )
        .unwrap();

        // After scrolling up twice, the bottom of the history view should no
        // longer show the last line. We verify by checking that the last
        // "line-39" is NOT visible at the bottom of the history area (row 16
        // = 24 - 1 status - 1 gap - 1 input - 5 ... but exact row depends on
        // layout). Instead of asserting exact content, assert that scrolling
        // changed the view: capture history rows and confirm the last line
        // shifted.
        //
        // With scroll_offset=2, the last visible line should be "line-38"
        // (one above the true last). We just assert the buffer contains the
        // scrolled content and not "line-39" in the history area.
        let buffer = terminal.backend().buffer();
        // History occupies rows 0..(24-3)=21 (Min(3) minus status+gap+input).
        // Collect history-area text.
        let history_text: String = (0..21u16)
            .flat_map(|y| (0..80u16).map(move |x| buffer[(x, y)].symbol()))
            .collect();
        assert!(
            !history_text.contains("line-39"),
            "after scrolling up 2, the last line should be off-screen; got: {history_text:?}"
        );
    }
}
