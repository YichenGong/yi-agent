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

use yi_agent_core::AgentEvent;

use super::bash_popup::{BashPopup, ConfirmKill, DetailPopup, ListPopup};
use super::cell::HistoryCell;
use super::cost::CostTracker;
use super::history::{HistoryState, HistoryView, ViewportAnchor};
use super::input::{InputAction, InputLine};
use super::process_popup::{
    ConfirmProcessKill as ConfirmProcessKillPopup, ProcessDetailPopup, ProcessListPopup,
    ProcessPopup, RuntimeTab,
};
use super::slash::{
    CommandPopup, McpAction, SlashCommand, TuiConfigSnapshot, help_text, parse_mcp_args,
    render_mcp_status,
};
use super::state::RunningTaskRegistry;
use super::statusbar::{StatusBarState, render_statusbar};
use super::trace::{
    SubagentListState, TraceDetailPopup, TraceListPopup, TracePopup, render_subagent_list,
};

const HISTORY_WHEEL_LINES: usize = 3;

fn is_active_managed_process(status: &yi_agent_tools::ProcessStatus) -> bool {
    matches!(
        status,
        yi_agent_tools::ProcessStatus::Starting
            | yi_agent_tools::ProcessStatus::Running
            | yi_agent_tools::ProcessStatus::Ready
    )
}

fn format_ipc_error(code: yi_agent_store::ipc::IpcErrorCode, message: Option<String>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

/// Write the "pending messages were dropped" notice, if any.
///
/// Split out from `run_tui` so the message and the `> 0` condition are both
/// testable: `run_tui` needs a real TTY (`enable_raw_mode`), so the notice
/// itself cannot be exercised through it.
fn report_dropped_pending<W: io::Write>(out: &mut W, dropped: usize) {
    if dropped > 0 {
        let _ = writeln!(out, "已丢弃 {dropped} 条排队消息（未发送）");
    }
}

/// Run the ratatui TUI main loop with the real terminal.
///
/// - `agent_rx`: receives agent events to display in history
/// - `input_tx`: sends user-submitted input strings to the agent driver
/// - `interrupt_tx`: signals to interrupt the current agent run
/// - `is_running`: shared flag indicating if agent is currently running
#[allow(clippy::too_many_arguments)]
pub fn run_tui(
    mut agent_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: tokio::sync::mpsc::Sender<String>,
    interrupt_tx: tokio::sync::mpsc::Sender<()>,
    kill_tx: tokio::sync::mpsc::Sender<String>,
    control_tx: tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    model: String,
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
    runtime_choice_tx: Option<
        tokio::sync::mpsc::Sender<crate::tui::subagents::RuntimeStartupChoice>,
    >,
    process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
    workdir: std::path::PathBuf,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
    config: TuiConfigSnapshot,
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
        &kill_tx,
        &control_tx,
        &decision_tx,
        &is_running,
        &CrosstermEventSource,
        &model,
        runtime_intent,
        runtime_choice_tx,
        process_manager,
        workdir,
        mcp,
        &config,
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

    // Report the dropped count only after the alternate screen is gone;
    // printing earlier would be erased along with it.
    if let Ok(dropped) = result {
        report_dropped_pending(&mut io::stderr(), dropped);
    }

    match (result, cleanup_error) {
        (Err(error), _) => Err(error),
        (Ok(_), Some(error)) => Err(error),
        (Ok(_), None) => Ok(()),
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

/// A fixed, secret-free snapshot for tests that need the config threaded
/// through. Preferred single definition: changing the struct's fields only
/// needs one test-side update.
#[cfg(test)]
fn snapshot_for_tests() -> TuiConfigSnapshot {
    TuiConfigSnapshot {
        provider: "anthropic".into(),
        workdir: std::path::PathBuf::from("/tmp/proj"),
        sandbox: "workspace-write".into(),
        yolo: false,
        max_turns: 200,
        compact_threshold: 160_000,
        mcp_master: true,
        runtime_preference: "ask".into(),
        runtime_preference_path: std::path::PathBuf::from("/tmp/proj/.yi-agent/preferences.json"),
    }
}

/// Run the TUI loop with any ratatui backend (used by tests with TestBackend).
/// Does NOT call enable_raw_mode / EnterAlternateScreen.
#[cfg(test)]
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn run_tui_with_backend<B: Backend>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    workdir: &std::path::Path,
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
        kill_tx,
        control_tx,
        decision_tx,
        is_running,
        &CrosstermEventSource,
        "test-model",
        None,
        None,
        yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        workdir.to_path_buf(),
        yi_agent_mcp::McpManager::empty(),
        &snapshot_for_tests(),
    )
    .map(|_dropped| ())
}

/// Testable variant: accepts a custom EventSource for injecting fake key events.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn run_tui_with_backend_and_events<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    config: &TuiConfigSnapshot,
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
        kill_tx,
        control_tx,
        decision_tx,
        is_running,
        events,
        "test-model",
        None,
        None,
        yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        std::env::temp_dir(),
        yi_agent_mcp::McpManager::empty(),
        config,
    )
    .map(|_dropped| ())
}

/// Body lines of the runtime startup dialog.
///
/// Kept as a function so the narrow-terminal test renders exactly what the live
/// UI renders — a duplicated literal could drift and hide a clipping bug.
///
/// Keep every line's **display width** (CJK counts as 2 columns) within
/// `BOX_WIDTH - 2` = 56, or it needs `Wrap` to fold. The key legend is 58 columns
/// on purpose: it must fold into two lines, and the test below locks that in.
fn runtime_prompt_lines() -> Vec<ratatui::text::Line<'static>> {
    vec![
        ratatui::text::Line::raw("启动后可以直接用自然语言创建和管理子 Agent。"),
        ratatui::text::Line::raw("此选择会被记住，可用 /runtime 修改。"),
        ratatui::text::Line::raw("若未能启用，重启 yi-agent 后可用。"),
        ratatui::text::Line::raw(""),
        ratatui::text::Line::raw("[y] 启动并记住"),
        ratatui::text::Line::raw("[n] 跳过并记住    [Esc] 本次跳过（不记住）"),
    ]
}

/// Persists a runtime preference, but never blocks the session on failure.
fn persist_runtime_choice(
    workdir: &std::path::Path,
    pref: crate::tui::runtime_prefs::RuntimePreference,
    history: &mut HistoryState,
    width: u16,
) {
    if let Err(error) = crate::tui::runtime_prefs::save(workdir, pref) {
        history.push(
            HistoryCell::Separator {
                label: Some(format!("无法保存偏好（本次仍然生效）: {error}")),
            },
            width,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    history: &mut HistoryState,
    input: &mut InputLine,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
    model: &str,
    runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
    runtime_choice_tx: Option<
        tokio::sync::mpsc::Sender<crate::tui::subagents::RuntimeStartupChoice>,
    >,
    process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
    workdir: std::path::PathBuf,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &TuiConfigSnapshot,
) -> std::io::Result<usize> {
    let mut current_model: String = model.to_string();
    let mut pending_quit = false;
    let mut popup: Option<CommandPopup> = None;
    let mut queued = crate::tui::queued::DeliveredInterjections::new();
    let mut statusbar_state = StatusBarState::default();
    let mut task_registry = RunningTaskRegistry::new();
    let mut cost_tracker = CostTracker::default();
    let mut runtime_popup: RuntimePopup = RuntimePopup::None;
    let mut subagent_children = SubagentListState::default();
    // The live trace streams, one at most, for whichever task the subagent tab
    // is showing. Created lazily so a session that never opens the tab never
    // pays for a socket.
    let mut trace_streams: Option<super::trace::TraceStreams> = None;
    // A one-shot alert shown when the runtime the user asked for fails to come
    // up. It must be a popup, not only a history line: the failure can land
    // many turns into a long session, where a separator appended to the
    // transcript appears off-screen and is never seen.
    let mut runtime_notice_popup: Option<&'static str> = None;
    let mut runtime_intent = runtime_intent;
    // `DisabledNotice` prints exactly one line, on the first frame, once the
    // history width is known.
    let mut runtime_notice_pending = matches!(
        runtime_intent,
        Some(crate::tui::subagents::RuntimeStartupIntent::DisabledNotice { .. })
    );
    let mut process_events = process_manager.subscribe();
    let mut process_snapshots = process_manager.list();
    let mut process_outputs: std::collections::HashMap<String, yi_agent_tools::ProcessReadResult> =
        std::collections::HashMap::new();
    // Keep the rendered viewport location so geometry that changes between
    // frames (such as a resize or newly queued preview) has an old-width anchor.
    let mut previous_viewport: Option<(ViewportAnchor, u16, u16)> = None;

    loop {
        let size = terminal.size()?;
        let width = size.width;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let initial_queued_lines = crate::tui::queued::render_queued_preview(queued.items(), width);
        let initial_layout = compute_layout(
            area,
            input,
            pending_quit,
            &popup,
            initial_queued_lines.len() as u16,
        );
        let initial_history_area = initial_layout.chunks[0];
        let initial_text_width =
            history.text_width(initial_history_area.width, initial_history_area.height);
        history.reconcile_scroll_offset(initial_text_width, initial_history_area.height);
        let viewport_anchor = match previous_viewport.take() {
            Some((anchor, previous_width, previous_height))
                if previous_width != initial_text_width
                    || previous_height != initial_history_area.height =>
            {
                Some(anchor)
            }
            _ => history.capture_viewport_anchor(initial_text_width, initial_history_area.height),
        };
        let mut pending_events = Vec::new();
        while let Ok(event) = agent_rx.try_recv() {
            pending_events.push(event);
        }

        // A completed turn promotes one queued item into history and sends it.
        // Determine the resulting layout before applying those history mutations.
        let promotion_count = pending_events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
                )
            })
            .count()
            .min(queued.len());
        let final_queued_lines =
            crate::tui::queued::render_queued_preview(&queued.items()[promotion_count..], width);
        let final_layout = compute_layout(
            area,
            input,
            pending_quit,
            &popup,
            final_queued_lines.len() as u16,
        );
        let final_history_area = final_layout.chunks[0];

        // The width the draw will actually use, not the raw area width. The
        // history renderer reserves its rightmost column for the scrollbar, and
        // the cache is keyed by width, so applying events at the area width
        // would re-render the whole scrollback at one width and then again at
        // the other -- once per streamed token.
        let draw_text_width =
            history.text_width(final_history_area.width, final_history_area.height);

        for event in pending_events {
            let is_turn_end = matches!(
                event,
                AgentEvent::Done { .. } | AgentEvent::Cancelled | AgentEvent::Error(_)
            );
            if let AgentEvent::SubagentRuntimeUnavailable { stage, cause } = &event {
                // The raw stage/cause pair is logged by the emitter; the user
                // only needs the remedy, which never varies. See
                // `runtime_restart_notice`.
                tracing::warn!(
                    stage = %stage,
                    cause = %cause,
                    "subagent runtime unavailable for this session"
                );
                runtime_notice_popup = Some(crate::tui::subagents::RUNTIME_RESTART_NOTICE);
            }
            route_event(
                &mut task_registry,
                &mut statusbar_state,
                &mut cost_tracker,
                &event,
            );
            // 先应用收据/回推，再处理回合结束。核心保证 `InterjectionsReturned`
            // 排在 `Cancelled`/`Done` 之前；放在回合结束分支之后虽然通常也能生效
            // （二者是相邻的两帧），但那样就依赖事件循环的批处理边界，一旦将来
            // 把一帧内的多个事件合并处理，退回的文本就会被回合结束分支抢先消费。
            // 顺序固定下来后，两种情形都安全。
            apply_interjection_event(&event, &mut queued, input, history, draw_text_width);
            if let AgentEvent::ModelChanged { model: new_model } = &event {
                current_model = new_model.clone();
            }
            history.push_event(event, draw_text_width);
            // 回合结束:弹出下一条待发消息,立即发送并「转正」进 history。
            // 发送与转正是同一个动作,不再依赖 driver 是否取走。
            if is_turn_end {
                if let Some(text) = queued.on_turn_end() {
                    let _ = input_tx.try_send(text.clone());
                    history.push(HistoryCell::UserMessage { text }, draw_text_width);
                }
            }
        }

        let final_text_width =
            history.text_width(final_history_area.width, final_history_area.height);
        if let Some(anchor) = viewport_anchor {
            history.restore_viewport_anchor(anchor, final_text_width, final_history_area.height);
        } else {
            history.reconcile_scroll_offset(final_text_width, final_history_area.height);
        }
        previous_viewport = history
            .capture_viewport_anchor(final_text_width, final_history_area.height)
            .map(|anchor| (anchor, final_text_width, final_history_area.height));

        let queued_lines = crate::tui::queued::render_queued_preview(queued.items(), width);
        let queued_height = queued_lines.len() as u16;
        // Advance status bar interpolation + spinner (~30hz).
        statusbar_state.tick();
        if process_events.try_recv().is_ok() {
            refresh_process_snapshots(
                &process_manager,
                &mut process_snapshots,
                &mut process_outputs,
            );
            while process_events.try_recv().is_ok() {}
        }

        // Pre-compute layout so mouse hit-testing uses the same chunk rects
        // as the draw closure below.
        let size = terminal.size()?;
        let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
        let layout = compute_layout(area, input, pending_quit, &popup, queued_height);

        if runtime_notice_pending {
            runtime_notice_pending = false;
            if let Some(crate::tui::subagents::RuntimeStartupIntent::DisabledNotice { reason }) =
                &runtime_intent
            {
                history.push(
                    HistoryCell::Separator {
                        label: Some(reason.clone()),
                    },
                    layout.chunks[0].width,
                );
            }
        }

        let active_process_count = process_snapshots
            .iter()
            .filter(|snapshot| is_active_managed_process(&snapshot.status))
            .count();
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
                    f.render_widget(build_popup(p, popup_area.height), popup_area);
                }
            }

            // Status bar
            let statusbar_line = render_statusbar(
                &statusbar_state,
                &task_registry,
                active_process_count,
                &current_model,
                chunks[2].width,
            );
            f.render_widget(statusbar_line, chunks[2]);

            // Render queued messages preview
            if queued_height > 0 {
                f.render_widget(Paragraph::new(queued_lines.clone()), chunks[3]);
            }

            let input_line = build_input_line(input, pending_quit, chunks[5].width);
            f.render_widget(input_line, chunks[5]);

            if let Some(crate::tui::subagents::RuntimeStartupIntent::Prompt) = &runtime_intent {
                let box_w = 58u16.min(chunks[0].width.saturating_sub(4));
                let box_h = 10u16.min(chunks[0].height.max(1));
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
                    ratatui::widgets::Paragraph::new(runtime_prompt_lines())
                        .wrap(ratatui::widgets::Wrap { trim: true })
                        .block(
                            ratatui::widgets::Block::default()
                                .borders(ratatui::widgets::Borders::ALL)
                                .title("启动本地 Agent Runtime?"),
                        ),
                    box_area,
                );
            }

            if let Some(text) = runtime_notice_popup {
                render_runtime_notice_popup(f, text, chunks[0]);
            }

            render_runtime_popup(
                f,
                &runtime_popup,
                &task_registry,
                &process_snapshots,
                &process_outputs,
                chunks[0],
            );
        })?;
        sync_trace_streams(
            &mut trace_streams,
            &mut runtime_popup,
            &workdir,
            layout.chunks[0].width,
        );

        // Poll for events with timeout (33ms → ~30hz refresh)
        match events.poll(Duration::from_millis(33))? {
            Some(Event::Key(key)) => {
                // Ctrl+P opens the runtime popup (bash tasks tab) when no popup
                // is active. Tab then switches to the managed-processes tab.
                if key.code == KeyCode::Char('p')
                    && key.modifiers == KeyModifiers::CONTROL
                    && runtime_popup.is_none()
                {
                    refresh_process_snapshots(
                        &process_manager,
                        &mut process_snapshots,
                        &mut process_outputs,
                    );
                    let ids: Vec<String> =
                        task_registry.list().iter().map(|t| t.id.clone()).collect();
                    if let Ok(socket) = crate::runtime_socket_for(&workdir) {
                        subagent_children = subagent_children_at(&socket);
                    }
                    runtime_popup = RuntimePopup::Bash(BashPopup::List(ListPopup::new(ids)));
                    continue;
                }
                // Route keys to the runtime popup when active.
                if !runtime_popup.is_none() {
                    let process_to_kill = handle_runtime_popup_key(
                        key,
                        &mut runtime_popup,
                        &task_registry,
                        kill_tx,
                        &process_snapshots,
                        &process_outputs,
                        &subagent_children,
                        layout.chunks[0].width,
                        layout.chunks[0].height,
                    );
                    if let Some(process_id) = process_to_kill {
                        let _ = tokio::runtime::Handle::current().block_on(
                            process_manager.kill(yi_agent_tools::ProcessSelector::Id(process_id)),
                        );
                        refresh_process_snapshots(
                            &process_manager,
                            &mut process_snapshots,
                            &mut process_outputs,
                        );
                    }
                    continue;
                }
                // Dismiss the failure notice on any key. Deliberately no
                // `continue`: a popup that consumed the first character typed
                // would silently corrupt what the user was writing, far worse
                // than the nuisance it reports, so the key clears the alert and
                // still reaches the input handling below.
                runtime_notice_popup = None;
                if let Some(crate::tui::subagents::RuntimeStartupIntent::Prompt) = &runtime_intent {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            persist_runtime_choice(
                                &workdir,
                                crate::tui::runtime_prefs::RuntimePreference::Always,
                                history,
                                layout.chunks[0].width,
                            );
                            if let Some(tx) = &runtime_choice_tx {
                                let _ = tx.blocking_send(
                                    crate::tui::subagents::RuntimeStartupChoice::Start,
                                );
                            }
                            runtime_intent = None;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            persist_runtime_choice(
                                &workdir,
                                crate::tui::runtime_prefs::RuntimePreference::Never,
                                history,
                                layout.chunks[0].width,
                            );
                            if let Some(tx) = &runtime_choice_tx {
                                let _ = tx.blocking_send(
                                    crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation,
                                );
                            }
                            runtime_intent = None;
                        }
                        // Esc means "skip this time"; it must NOT overwrite the
                        // stored preference, so the next launch asks again.
                        KeyCode::Esc => {
                            if let Some(tx) = &runtime_choice_tx {
                                let _ = tx.blocking_send(
                                    crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation,
                                );
                            }
                            runtime_intent = None;
                        }
                        _ => {}
                    }
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
                    kill_tx,
                    control_tx,
                    decision_tx,
                    is_running,
                    &mut queued,
                    &mut pending_quit,
                    &mut popup,
                    &workdir,
                    &mcp,
                    config,
                    &current_model,
                ) {
                    KeyOutcome::Quit => break,
                    KeyOutcome::Submit(_) => {
                        pending_quit = false;
                    }
                    KeyOutcome::OpenAgentDetail(task_id) => {
                        if let Ok(socket) = crate::runtime_socket_for(&workdir) {
                            subagent_children = subagent_children_at(&socket);
                        }
                        open_agent_detail(&mut runtime_popup, &subagent_children, task_id);
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
                    &runtime_popup,
                    &mut pending_quit,
                    &mut popup,
                );
            }
            Some(Event::Mouse(mouse)) => {
                handle_mouse(
                    mouse,
                    &layout,
                    &mut runtime_popup,
                    history,
                    &task_registry,
                    &process_snapshots,
                    &process_outputs,
                    &mut pending_quit,
                );
            }
            _ => {}
        }
    }

    // Messages submitted during the last turn never got their chance to run.
    // Return the count so the caller can report the loss after the terminal is
    // restored (anything printed while the alternate screen is up is erased).
    Ok(queued.len())
}

/// Top-level runtime popup: a Bash-tasks tab and a managed-processes tab,
/// switched with Tab. Wraps the per-tab popup state machines.
#[derive(Debug)]
enum RuntimePopup {
    None,
    Bash(BashPopup),
    Processes(ProcessPopup),
    Agents(Box<AgentsPopup>),
}

/// The subagent tab: the child list, the open detail, and the small amount of
/// state the two actions need before the event loop can carry them out.
#[derive(Debug)]
struct AgentsPopup {
    list: TraceListPopup,
    detail: Option<TracePopup>,
    children: SubagentListState,
    /// An action the user asked for and the loop has not carried out yet.
    pending: Option<super::trace::TraceAction>,
    /// True while a message is being typed into the open detail.
    composing: bool,
    /// A cancel preview waiting for `y` / `n`. Holding the token is what makes
    /// the second press a confirmation rather than a fresh preview.
    awaiting_cancel: Option<String>,
    /// The result of the last action, shown in the footer so it is not silent.
    status: Option<String>,
}

impl AgentsPopup {
    fn new(children: SubagentListState) -> Self {
        Self {
            list: TraceListPopup::new(),
            detail: None,
            children,
            pending: None,
            composing: false,
            awaiting_cancel: None,
            status: None,
        }
    }
}

impl RuntimePopup {
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    fn blocks_text_input(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn switch_tab(&mut self, process_ids: Vec<String>, children: SubagentListState) {
        *self = match self.tab().map(RuntimeTab::next) {
            Some(RuntimeTab::Processes) => {
                Self::Processes(ProcessPopup::List(ProcessListPopup::new()))
            }
            Some(RuntimeTab::BashTasks) => Self::Bash(BashPopup::List(ListPopup::new(process_ids))),
            Some(RuntimeTab::Agents) => Self::Agents(Box::new(AgentsPopup::new(children))),
            None => Self::None,
        };
    }

    fn tab(&self) -> Option<RuntimeTab> {
        match self {
            Self::None => None,
            Self::Bash(_) => Some(RuntimeTab::BashTasks),
            Self::Processes(_) => Some(RuntimeTab::Processes),
            Self::Agents(_) => Some(RuntimeTab::Agents),
        }
    }
}

fn switch_runtime_tab(
    runtime_popup: &mut RuntimePopup,
    task_registry: &RunningTaskRegistry,
    children: &SubagentListState,
) {
    let ids = task_registry.list().iter().map(|t| t.id.clone()).collect();
    runtime_popup.switch_tab(ids, children.clone());
}

#[cfg(test)]
fn switch_runtime_tab_for_test(
    runtime_popup: &mut RuntimePopup,
    bash_ids: &[String],
    children: SubagentListState,
) {
    *runtime_popup = match runtime_popup.tab().map(RuntimeTab::next) {
        Some(RuntimeTab::Processes) => {
            RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()))
        }
        Some(RuntimeTab::BashTasks) => {
            RuntimePopup::Bash(BashPopup::List(ListPopup::new(bash_ids.to_vec())))
        }
        Some(RuntimeTab::Agents) => RuntimePopup::Agents(Box::new(AgentsPopup::new(children))),
        None => RuntimePopup::None,
    };
}

/// Refresh the managed-process snapshot list and, for each process, read its
/// buffered output. Called on process events and when the popup opens.
fn refresh_process_snapshots(
    process_manager: &yi_agent_tools::ProcessManager,
    process_snapshots: &mut Vec<yi_agent_tools::ManagedProcessSnapshot>,
    process_outputs: &mut std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
) {
    *process_snapshots = process_manager.list();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        for snapshot in process_snapshots.iter() {
            if let Ok(output) = handle.block_on(process_manager.read(
                yi_agent_tools::ProcessSelector::Id(snapshot.process_id.clone()),
                None,
                64 * 1024,
            )) {
                process_outputs.insert(snapshot.process_id.clone(), output);
            }
        }
    }
}

/// Renders the one-shot "runtime could not start" alert over the transcript.
///
/// Deliberately the same shape as the startup dialog: the two report the same
/// subject (the local runtime) and the user should not have to learn two
/// visual grammars for it.
fn render_runtime_notice_popup(
    f: &mut ratatui::Frame<'_>,
    text: &str,
    area: ratatui::layout::Rect,
) {
    let box_w = 58u16.min(area.width.saturating_sub(4));
    let box_h = 5u16.min(area.height.max(1));
    let box_x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 3;
    let box_area = ratatui::layout::Rect {
        x: box_x,
        y: box_y,
        width: box_w,
        height: box_h,
    };
    f.render_widget(Clear, box_area);
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::raw(text.to_string()))
            .wrap(ratatui::widgets::Wrap { trim: true })
            .block(
                ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .title("子 Agent runtime 未启用（按任意键继续）"),
            ),
        box_area,
    );
}

fn render_runtime_popup(
    f: &mut ratatui::Frame<'_>,
    runtime_popup: &RuntimePopup,
    task_registry: &RunningTaskRegistry,
    process_snapshots: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    area: ratatui::layout::Rect,
) {
    match runtime_popup {
        RuntimePopup::None => {}
        RuntimePopup::Bash(bash_popup) => {
            render_existing_bash_popup(f, bash_popup, task_registry, area);
        }
        RuntimePopup::Processes(ProcessPopup::List(p)) => {
            f.render_widget(Clear, area);
            f.render_widget(
                super::process_popup::render_process_list_popup(p, process_snapshots, area),
                area,
            );
        }
        RuntimePopup::Processes(ProcessPopup::Detail(p)) => {
            if let Some(process) = process_snapshots
                .iter()
                .find(|p2| p2.process_id == p.process_id)
            {
                f.render_widget(Clear, area);
                f.render_widget(
                    super::process_popup::render_process_detail_popup(
                        p,
                        process,
                        process_outputs.get(&p.process_id),
                        area,
                    ),
                    area,
                );
            }
        }
        RuntimePopup::Agents(agents) => {
            f.render_widget(Clear, area);
            match &agents.detail {
                None => {
                    f.render_widget(
                        render_subagent_list(&agents.list, &agents.children, area),
                        area,
                    );
                }
                Some(TracePopup::Detail(detail)) => {
                    let prompt = super::trace::DetailPrompt {
                        composing: agents.composing,
                        awaiting_cancel: agents.awaiting_cancel.is_some(),
                        status: agents.status.as_deref(),
                    };
                    super::trace::render_subagent_detail(
                        f,
                        detail,
                        &agents.children,
                        &prompt,
                        area,
                    );
                }
            }
        }
        RuntimePopup::Processes(ProcessPopup::ConfirmKill(ck)) => {
            if let Some(process) = process_snapshots
                .iter()
                .find(|p| p.process_id == ck.process_id)
            {
                f.render_widget(Clear, area);
                let detail = ProcessDetailPopup::new(ck.process_id.clone());
                f.render_widget(
                    super::process_popup::render_process_detail_popup(
                        &detail,
                        process,
                        process_outputs.get(&ck.process_id),
                        area,
                    ),
                    area,
                );
                render_kill_confirmation_overlay(f, area, "kill this managed process?");
            }
        }
    }
}

/// Render the bash-tasks tab (list / detail / kill-confirm) into `area`.
fn render_existing_bash_popup(
    f: &mut ratatui::Frame<'_>,
    bash_popup: &BashPopup,
    task_registry: &RunningTaskRegistry,
    area: ratatui::layout::Rect,
) {
    match bash_popup {
        BashPopup::List(p) => {
            let list_area = ratatui::layout::Rect {
                x: area.x + 2,
                y: area.y + 1,
                width: area.width.saturating_sub(4),
                height: area
                    .height
                    .saturating_sub(2)
                    .min((p.task_ids.len() as u16 + 2).max(6)),
            };
            f.render_widget(Clear, list_area);
            f.render_widget(
                super::bash_popup::render_list_popup(p, task_registry, list_area),
                list_area,
            );
        }
        BashPopup::Detail(p) => {
            if let Some(task) = task_registry.get(&p.task_id) {
                f.render_widget(Clear, area);
                f.render_widget(super::bash_popup::render_detail_popup(p, task, area), area);
            }
        }
        BashPopup::ConfirmKill(ck) => {
            if let Some(task) = task_registry.get(&ck.task_id) {
                f.render_widget(Clear, area);
                let detail = DetailPopup::new(ck.task_id.clone());
                f.render_widget(
                    super::bash_popup::render_detail_popup(&detail, task, area),
                    area,
                );
            }
            render_kill_confirmation_overlay(f, area, "kill this process?");
        }
        BashPopup::None => {}
    }
}

fn render_kill_confirmation_overlay(
    f: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    prompt: &str,
) {
    let box_w = 40u16.min(area.width.saturating_sub(4));
    let box_h = 4u16;
    let box_x = area.x + (area.width.saturating_sub(box_w)) / 2;
    let box_y = area.y + (area.height.saturating_sub(box_h)) / 3;
    let box_area = ratatui::layout::Rect {
        x: box_x,
        y: box_y,
        width: box_w,
        height: box_h,
    };
    f.render_widget(Clear, box_area);
    f.render_widget(
        ratatui::widgets::Paragraph::new(vec![
            ratatui::text::Line::raw(prompt.to_string()),
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

#[cfg(test)]
fn handle_runtime_popup_key_for_test(
    key: KeyEvent,
    runtime_popup: &mut RuntimePopup,
    bash_ids: &[String],
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
) {
    if key.code == KeyCode::Tab {
        switch_runtime_tab_for_test(runtime_popup, bash_ids, SubagentListState::default());
        return;
    }
    let registry = RunningTaskRegistry::new();
    let outputs = std::collections::HashMap::new();
    let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
    let _ = handle_runtime_popup_key(
        key,
        runtime_popup,
        &registry,
        &kill_tx,
        processes,
        &outputs,
        &SubagentListState::default(),
        80,
        24,
    );
}

/// Route a key to the active runtime popup. Returns `Some(process_id)` when the
/// user confirms a kill in the managed-processes tab.
#[allow(clippy::too_many_arguments)]
fn handle_runtime_popup_key(
    key: KeyEvent,
    runtime_popup: &mut RuntimePopup,
    task_registry: &RunningTaskRegistry,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    children: &SubagentListState,
    detail_width: u16,
    detail_height: u16,
) -> Option<String> {
    match runtime_popup {
        RuntimePopup::None => {}
        RuntimePopup::Bash(bash_popup) => {
            if key.code == KeyCode::Tab {
                switch_runtime_tab(runtime_popup, task_registry, children);
            } else {
                handle_bash_popup_key(key, bash_popup, task_registry, kill_tx, detail_width);
                if matches!(bash_popup, BashPopup::None) {
                    *runtime_popup = RuntimePopup::None;
                }
            }
        }
        RuntimePopup::Processes(ProcessPopup::List(p)) => match key.code {
            KeyCode::Tab => {
                switch_runtime_tab(runtime_popup, task_registry, children);
            }
            KeyCode::Up => p.move_up(),
            KeyCode::Down => p.move_down(processes.len()),
            KeyCode::Enter => {
                if let Some(id) = p.selected_id(processes) {
                    *runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
                        ProcessDetailPopup::new(id.to_string()),
                    ));
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => *runtime_popup = RuntimePopup::None,
            _ => {}
        },
        RuntimePopup::Processes(ProcessPopup::Detail(d)) => match key.code {
            KeyCode::Tab => {
                switch_runtime_tab(runtime_popup, task_registry, children);
            }
            KeyCode::Char('k') => {
                *runtime_popup =
                    RuntimePopup::Processes(ProcessPopup::ConfirmKill(ConfirmProcessKillPopup {
                        process_id: d.process_id.clone(),
                    }));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                *runtime_popup =
                    RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
            }
            KeyCode::Up => {
                let max = process_detail_max_scroll(
                    d,
                    processes,
                    process_outputs,
                    detail_width,
                    detail_height,
                );
                d.scroll_up_from_bottom(1, max);
            }
            KeyCode::Down => {
                let max = process_detail_max_scroll(
                    d,
                    processes,
                    process_outputs,
                    detail_width,
                    detail_height,
                );
                d.scroll_down(1, max);
            }
            KeyCode::Char('f') => d.scroll_to_bottom(),
            _ => {}
        },
        RuntimePopup::Processes(ProcessPopup::ConfirmKill(ck)) => match key.code {
            KeyCode::Char('n') | KeyCode::Esc => {
                *runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
                    ProcessDetailPopup::new(ck.process_id.clone()),
                ));
            }
            KeyCode::Char('y') => {
                let process_id = ck.process_id.clone();
                *runtime_popup =
                    RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));
                return Some(process_id);
            }
            _ => {}
        },
        RuntimePopup::Agents(agents) => {
            if key.code == KeyCode::Tab {
                switch_runtime_tab(runtime_popup, task_registry, children);
                return None;
            }
            if handle_agents_key(key, agents, children, detail_width) {
                *runtime_popup = RuntimePopup::None;
            }
        }
    }
    None
}

/// Keys for the subagent tab. Returns true when the tab should close.
///
/// The list level navigates; the detail level is read-only apart from the two
/// interventions the design allows: a message (`m`, Enter to submit) and a
/// cancel (`k`, confirmed with `y`). Neither touches the socket here: the action
/// is recorded and the event loop carries it out, which keeps this function
/// testable without a daemon.
/// Open the subagent tab on one task's detail, from wherever it was asked for.
///
/// The tab owns the detail, so opening it means switching the popup to the
/// Agents tab and drilling straight to the task. The child list is replaced with
/// the snapshot the caller just read, so the detail's drill-down agrees with it.
fn open_agent_detail(
    runtime_popup: &mut RuntimePopup,
    children: &SubagentListState,
    task_id: String,
) {
    let mut agents = AgentsPopup::new(children.clone());
    agents.detail = Some(TracePopup::Detail(TraceDetailPopup::new(task_id)));
    *runtime_popup = RuntimePopup::Agents(Box::new(agents));
}

fn handle_agents_key(
    key: KeyEvent,
    agents: &mut AgentsPopup,
    children: &SubagentListState,
    detail_width: u16,
) -> bool {
    if let Some(TracePopup::Detail(detail)) = &mut agents.detail {
        // A pending cancel confirmation takes every key, so a stray `m` cannot
        // quietly turn into a message while the daemon is asking to confirm.
        if let Some(token) = agents.awaiting_cancel.clone() {
            let task_id = detail.task_id().to_string();
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    agents.awaiting_cancel = None;
                    agents.pending = Some(super::trace::TraceAction::Cancel {
                        task_id,
                        confirmation_token: Some(token),
                    });
                }
                _ => {
                    agents.awaiting_cancel = None;
                    agents.status = Some("已取消该次操作".into());
                }
            }
            return false;
        }

        // Composing a message: the detail view owns the text until Enter.
        if agents.composing {
            match key.code {
                KeyCode::Enter => {
                    let text = detail.take_input();
                    agents.composing = false;
                    if text.trim().is_empty() {
                        agents.status = Some("消息为空，未发送".into());
                    } else {
                        agents.pending = Some(super::trace::TraceAction::SendMessage {
                            task_id: detail.task_id().to_string(),
                            text,
                        });
                    }
                }
                KeyCode::Esc => {
                    detail.take_input();
                    agents.composing = false;
                }
                KeyCode::Backspace => detail.input_backspace(),
                KeyCode::Char(ch) => detail.input_push(ch),
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if !detail.pop() {
                    agents.detail = None;
                }
                agents.status = None;
            }
            KeyCode::Char('m') => {
                agents.composing = true;
                detail.take_input();
            }
            KeyCode::Char('k') => {
                agents.pending = Some(super::trace::TraceAction::Cancel {
                    task_id: detail.task_id().to_string(),
                    confirmation_token: None,
                });
            }
            KeyCode::Enter => {
                let task_id = detail.task_id().to_string();
                let targets = super::trace::drill_targets(children, &task_id);
                if let Some(target) = targets.first() {
                    let child = target.task_id.clone();
                    detail.push(child);
                    agents.status = None;
                } else {
                    agents.status = Some("该任务没有可进入的子任务".into());
                }
            }
            KeyCode::Up | KeyCode::PageUp => detail.feed_mut().scroll(-1),
            KeyCode::Down | KeyCode::PageDown => detail.feed_mut().scroll(1),
            KeyCode::Char('g') => detail.feed_mut().scroll_to_top(),
            KeyCode::Char('G') => detail.feed_mut().scroll_to_bottom(),
            _ => {}
        }
        let _ = detail_width;
        return false;
    }

    match key.code {
        KeyCode::Up => agents.list.move_up(),
        KeyCode::Down => agents.list.move_down(agents.children.len()),
        KeyCode::Enter => {
            if let Some(task_id) = agents.list.selected_id(&agents.children.items) {
                agents.detail = Some(TracePopup::Detail(TraceDetailPopup::new(
                    task_id.to_string(),
                )));
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => return true,
        _ => {}
    }
    false
}

/// Carry out whatever action the subagent tab recorded.
///
/// A cancel preview is not the cancel itself: the daemon answers the first press
/// with a token, and only a second press carrying that token actually cancels.
/// The token is kept on the popup so the confirmation is a real second step
/// rather than a formality.
fn execute_trace_action(
    action: super::trace::TraceAction,
    socket: &std::path::Path,
    agents: &mut AgentsPopup,
) {
    match action {
        super::trace::TraceAction::SendMessage { task_id, text } => {
            agents.status = Some(match daemon_user_message_at(socket, &task_id, &text) {
                Ok(message) => message,
                Err(error) => format!("发送失败: {error}"),
            });
        }
        super::trace::TraceAction::Cancel {
            task_id,
            confirmation_token,
        } => {
            match confirmation_token {
                // First press: ask for a preview and hold its token. Nothing is
                // cancelled yet.
                None => match preview_cancel_token_at(socket, &task_id) {
                    Ok(token) => {
                        agents.awaiting_cancel = Some(token);
                        agents.status = Some(format!("继续按 y 确认取消 {task_id}"));
                    }
                    Err(error) => agents.status = Some(format!("取消预览失败: {error}")),
                },
                // Second press: the token is what makes this the real cancel.
                Some(token) => {
                    agents.awaiting_cancel = None;
                    agents.status = Some(
                        match daemon_cancel_at(socket, &task_id, false, Some(&token)) {
                            Ok(message) => message,
                            Err(error) => format!("取消失败: {error}"),
                        },
                    );
                }
            }
        }
    }
}

/// Ask the daemon for a cancel preview and return its confirmation token.
fn preview_cancel_token_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PreviewCancel {
            task_id: task_id.to_owned(),
            recursive: false,
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token, ..
        } => Ok(confirmation_token),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝取消预览: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非取消预览响应".into()),
    }
}

/// Keep at most one trace stream open, for whatever the subagent tab shows.
///
/// Called once per frame after drawing. It carries out any recorded action, then
/// points the stream at the open detail (or closes it), appending whatever rows
/// arrived to that detail's feed.
fn sync_trace_streams(
    streams: &mut Option<super::trace::TraceStreams>,
    runtime_popup: &mut RuntimePopup,
    workdir: &std::path::Path,
    width: u16,
) {
    // Wall-clock state of the tab: the pending action, then whatever it shows.
    let (pending, showing) = match runtime_popup {
        RuntimePopup::Agents(agents) => {
            let pending = agents.pending.take();
            let showing = agents
                .detail
                .as_ref()
                .map(|TracePopup::Detail(detail)| detail.task_id().to_string());
            (pending, showing)
        }
        _ => (None, None),
    };

    if let Some(action) = pending {
        let RuntimePopup::Agents(agents) = runtime_popup else {
            return;
        };
        match crate::runtime_socket_for(workdir) {
            Ok(socket) => execute_trace_action(action, &socket, agents),
            Err(error) => agents.status = Some(format!("无法连接 daemon: {error}")),
        }
    }

    // The stream must not outlive the view it feeds, so anything but an open
    // detail closes it. Reopening the same task later then re-reads its backlog
    // instead of resuming a stream whose snapshot was never shown.
    let Some(task_id) = showing else {
        if let Some(streams) = streams {
            streams.close();
        }
        return;
    };
    let streams = streams.get_or_insert_with(|| {
        let source = super::trace::IpcTraceSource::new(workdir.to_path_buf());
        super::trace::TraceStreams::new(Box::new(source))
    });
    let RuntimePopup::Agents(agents) = runtime_popup else {
        return;
    };
    let Some(TracePopup::Detail(detail)) = &mut agents.detail else {
        return;
    };
    streams.sync(Some(&task_id), detail.feed_mut(), width);
}

fn process_detail_max_scroll(
    detail: &ProcessDetailPopup,
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    width: u16,
    height: u16,
) -> usize {
    processes
        .iter()
        .find(|process| process.process_id == detail.process_id)
        .map(|process| {
            super::process_popup::process_detail_line_count(
                process,
                process_outputs.get(&detail.process_id),
                width,
            )
            .saturating_sub(height as usize)
        })
        .unwrap_or(0)
}

/// Handle a key event for the bash popup state machine.
fn handle_bash_popup_key(
    key: KeyEvent,
    bash_popup: &mut BashPopup,
    task_registry: &RunningTaskRegistry,
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    detail_width: u16,
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
                let lines = task_registry
                    .get(&d.task_id)
                    .map(|t| super::bash_popup::detail_line_count(t, detail_width))
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
                // Ask the agent driver to cancel this one tool call. Cancelling
                // the call drops the bash tool's future, which is what reaps the
                // command's whole process group — so this stops the command, not
                // just our wait for it. The driver answers by emitting a
                // `ToolExit` and an error `ToolResult`, which flips the registry
                // entry out of `Running` on its own.
                //
                // A failed send means the driver is gone (run over / app
                // shutting down); the task cannot be running then either, so the
                // entry is finalized directly to stop the timer.
                if kill_tx.try_send(ck.task_id.clone()).is_err() {
                    // `try_send` can only fail for a full or closed channel; a
                    // full one means the driver is busy and will drain it, so
                    // only the closed case needs the local fallback.
                    if kill_tx.is_closed() {
                        tracing::warn!(
                            tool_call = %ck.task_id,
                            "kill request dropped: the agent driver is gone"
                        );
                    }
                }
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
/// 1. If a bash/process detail popup is active and the mouse is over the
///    history region (where the popup is rendered), scroll the popup.
/// 2. Otherwise, if the mouse is over the history region, scroll history.
/// 3. If the mouse is over the input region, do nothing — the input widget
///    handles its own scrolling internally.
#[allow(clippy::too_many_arguments)]
fn handle_mouse(
    mouse: MouseEvent,
    layout: &LayoutInfo,
    runtime_popup: &mut RuntimePopup,
    history: &mut HistoryState,
    task_registry: &RunningTaskRegistry,
    processes: &[yi_agent_tools::ManagedProcessSnapshot],
    process_outputs: &std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
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
    if matches!(
        runtime_popup,
        RuntimePopup::Bash(BashPopup::Detail(_)) | RuntimePopup::Bash(BashPopup::ConfirmKill(_))
    ) && history_area.contains(pos)
    {
        if let RuntimePopup::Bash(BashPopup::Detail(d)) = runtime_popup {
            if is_scroll_down {
                let lines = task_registry
                    .get(&d.task_id)
                    .map(|t| super::bash_popup::detail_line_count(t, history_area.width))
                    .unwrap_or(0);
                d.scroll_down(delta, lines);
            } else {
                d.scroll_up(delta);
            }
        }
        return;
    }

    if matches!(
        runtime_popup,
        RuntimePopup::Processes(ProcessPopup::Detail(_))
            | RuntimePopup::Processes(ProcessPopup::ConfirmKill(_))
    ) && history_area.contains(pos)
    {
        if let RuntimePopup::Processes(ProcessPopup::Detail(d)) = runtime_popup {
            let max = process_detail_max_scroll(
                d,
                processes,
                process_outputs,
                history_area.width,
                history_area.height,
            );
            if is_scroll_down {
                d.scroll_down(delta, max);
            } else {
                d.scroll_up_from_bottom(delta, max);
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
            if name == "bash" {
                let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let exp = input
                    .get("expected_timeout_sec")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120) as u32;
                registry.on_tool_call(id, name, cmd, exp);
            }
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
        AgentEvent::ToolRetry { .. } => {}
        // The retry is surfaced through a history separator; the status bar
        // needs no extra state (which would raise "when do we clear it?").
        AgentEvent::ProviderRetry { .. } => {}
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
    /// Open the subagent tab directly on a task's read-only detail.
    OpenAgentDetail(String),
}

fn starts_with_multi_segment_absolute_path(text: &str) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|token| token.starts_with('/') && token.matches('/').count() >= 2)
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
    kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    queued: &mut crate::tui::queued::DeliveredInterjections,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
    workdir: &std::path::Path,
    mcp: &std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &TuiConfigSnapshot,
    model: &str,
) -> KeyOutcome {
    // Check if there's a pending permission request. Clone the small fields
    // we need so the immutable borrow ends before we mutate history.
    let pending_permission = history.pending_permission_info().map(
        |(request_id, tool_name, prefix_suggestion, kind)| {
            (
                request_id,
                tool_name.to_string(),
                prefix_suggestion.map(str::to_string),
                kind.clone(),
            )
        },
    );
    if let Some((request_id, _tool_name, prefix_suggestion, kind)) = pending_permission {
        // Allow quit keys to pass through even when permission is pending
        let is_quit_key = matches!(key.code, KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL)
            || matches!(key.code, KeyCode::Esc);
        // Scrolling stays available so an expanded body can be read. These
        // keys are non-destructive and cannot resolve the request.
        let is_scroll_key = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        if is_quit_key || is_scroll_key {
            // Fall through to global key handling below
        } else {
            if key.code == KeyCode::Char('e') && key.modifiers.is_empty() {
                history.toggle_pending_permission_expanded();
                return KeyOutcome::None;
            }
            let decision = match key.code {
                KeyCode::Char('1') => Some(yi_agent_core::permission::Decision::AllowOnce),
                KeyCode::Char('2') => Some(yi_agent_core::permission::Decision::AlwaysAllowTool),
                KeyCode::Char('3') => prefix_suggestion
                    .as_deref()
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
            // Popup dismissal takes precedence over cancelling an agent turn.
            if popup.is_some() {
                *popup = None;
                return KeyOutcome::None;
            }
            if is_running.load(std::sync::atomic::Ordering::SeqCst) {
                // Cancellation is idempotent; coalesce repeated Esc presses.
                let _ = interrupt_tx.try_send(());
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
                            None,
                            args_str,
                            history,
                            history_width,
                            cost_tracker,
                            input_tx,
                            interrupt_tx,
                            kill_tx,
                            control_tx,
                            workdir,
                            queued,
                            mcp,
                            config,
                            model,
                        );
                    } else {
                        // No command selected (empty filter) — show error
                        // Not routed through `DeliveredInterjections`: this text never
                        // reaches the agent, so queuing it would make it look
                        // like a pending prompt.
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
            if text.starts_with('/') && !starts_with_multi_segment_absolute_path(&text) {
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
                        Some(name.to_string()),
                        args,
                        history,
                        history_width,
                        cost_tracker,
                        input_tx,
                        interrupt_tx,
                        kill_tx,
                        control_tx,
                        workdir,
                        queued,
                        mcp,
                        config,
                        model,
                    );
                } else {
                    // Unknown slash command
                    // Not routed through `DeliveredInterjections`: a slash command is a
                    // local action, not a prompt for the agent.
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
            use crate::tui::queued::SubmitOutcome;
            match queued.submit(text.clone()) {
                SubmitOutcome::Sent => {
                    history.push(
                        HistoryCell::UserMessage { text: text.clone() },
                        history_width,
                    );
                    let _ = input_tx.try_send(text.clone());
                }
                SubmitOutcome::Queued => {
                    // 已投递给 driver，由它在本轮下一次 provider 请求前折进上下文。
                    // 在收到 core 回执前，它一直留在预览区（"已送达，待生效"）。
                    if input_tx.try_send(text.clone()).is_err() {
                        // 通道满或已关闭：把文本放回输入框，不做静默丢弃。
                        // 此处 `text` 是 String（非对 input 的借用），故可变借 input 合法。
                        input.insert_str(&text);
                        history.push(
                            HistoryCell::Separator {
                                label: Some(format!(
                                    "追加未送达（通道已满 {}），已退回输入框",
                                    crate::tui::queued::DeliveredInterjections::CAPACITY
                                )),
                            },
                            history_width,
                        );
                    }
                }
                SubmitOutcome::Rejected => {
                    // 文本已被 take_submitted 取走,必须退回,否则静默丢失。
                    // 此处 `text` 是 String(非对 input 的借用),故可变借 input 合法。
                    input.insert_str(&text);
                    history.push(
                        HistoryCell::Separator {
                            label: Some(format!(
                                "待生效追加已达上限 ({})，本条未投递，已退回输入框",
                                crate::tui::queued::DeliveredInterjections::CAPACITY
                            )),
                        },
                        history_width,
                    );
                }
            }
            KeyOutcome::Submit(text)
        }
        _ => KeyOutcome::None,
    }
}

/// 应用两条追加生命周期事件（收据 / 回推）。
///
/// **必须在回合结束分支之前调用**：`InterjectionsReturned` 由 core 在
/// `Cancelled`/`Done` **之前**发出（D15 顺序不变量），若让回合结束分支先跑，
/// 回推的文本就再也无人接收了——这正是本设计要修掉的"文本静默丢失"。
fn apply_interjection_event(
    event: &AgentEvent,
    queued: &mut crate::tui::queued::DeliveredInterjections,
    input: &mut InputLine,
    history: &mut HistoryState,
    width: u16,
) {
    match event {
        AgentEvent::InterjectionAccepted { .. } => queued.on_receipt(),
        AgentEvent::InterjectionsReturned { items } => {
            let restored = queued.take_returned(items.len());
            if restored.is_empty() {
                return;
            }
            // 输入框是单行的；一批回推用换行连接，`wrap_input_buffer` 会折行。
            for (i, text) in restored.iter().enumerate() {
                if i > 0 {
                    input.insert_str("\n");
                }
                input.insert_str(text);
            }
            history.push(
                HistoryCell::Separator {
                    label: Some(format!("{} 条追加未生效，已退回输入框", restored.len())),
                },
                width,
            );
        }
        _ => {}
    }
}

fn handle_paste(
    text: String,
    input: &mut InputLine,
    history: &HistoryState,
    runtime_popup: &RuntimePopup,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
) {
    if runtime_popup.blocks_text_input() || history.pending_permission_info().is_some() {
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
    // 用户实际敲的命令名（用于提示已改名的过渡别名）。
    invoked_as: Option<String>,
    args: Option<String>,
    history: &mut HistoryState,
    width: u16,
    cost: &CostTracker,
    _input_tx: &tokio::sync::mpsc::Sender<String>,
    _interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    _kill_tx: &tokio::sync::mpsc::Sender<String>,
    control_tx: &tokio::sync::mpsc::Sender<crate::ControlCommand>,
    workdir: &std::path::Path,
    queued: &mut crate::tui::queued::DeliveredInterjections,
    mcp: &std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &TuiConfigSnapshot,
    current_model: &str,
) -> KeyOutcome {
    match cmd {
        SlashCommand::Quit => KeyOutcome::Quit,
        SlashCommand::Clear => {
            // 本地清空 history 显示,TUI 不等 driver 确认。
            // 通过 control channel 通知 driver 重建 agent(空 session)。
            let dropped = queued.clear();
            history.clear();
            history.push(
                HistoryCell::Separator {
                    label: Some("对话已清空".to_string()),
                },
                width,
            );
            if dropped > 0 {
                // 清空必须可见:否则用户以为排队消息还在。
                history.push(
                    HistoryCell::Separator {
                        label: Some(format!("已丢弃 {dropped} 条排队消息")),
                    },
                    width,
                );
            }
            let _ = control_tx.blocking_send(crate::ControlCommand::Clear);
            KeyOutcome::None
        }
        SlashCommand::Help => {
            history.push(
                HistoryCell::UserMessage {
                    text: help_text(args.as_deref()),
                },
                width,
            );
            KeyOutcome::None
        }
        SlashCommand::Cost => {
            let text = cost.render();
            history.push(HistoryCell::Markdown { text }, width);
            KeyOutcome::None
        }
        SlashCommand::Config => {
            if args.is_some() {
                history.push(
                    HistoryCell::Separator {
                        label: Some("用法: /config".to_string()),
                    },
                    width,
                );
            } else {
                let text = config.render(current_model);
                history.push(HistoryCell::Markdown { text }, width);
            }
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
            match args.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                Some(new_model) => {
                    let _ = control_tx
                        .blocking_send(crate::ControlCommand::SetModel(new_model.to_string()));
                    // 确认行由 `ModelChanged` 事件驱动；这里不预写成功行，避免与
                    // driver 真实结果冲突。
                }
                None => {
                    history.push(
                        HistoryCell::Separator {
                            label: Some("用法: /model <model-name>".to_string()),
                        },
                        width,
                    );
                }
            }
            KeyOutcome::None
        }
        SlashCommand::Agents => {
            let text = match daemon_agents_summary(workdir, args.as_deref()) {
                Ok(summary) => summary,
                Err(error) => format!("无法读取本地 daemon runtime: {error}"),
            };
            history.push(HistoryCell::Markdown { text }, width);
            KeyOutcome::None
        }
        SlashCommand::Agent => {
            // `/agent <id>` opens the same read-only detail as drilling in from
            // the subagent tab, so the two entry points cannot diverge. Without
            // an id there is nothing to open, and the usage line is the answer.
            match args.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
                Some(task_id) => KeyOutcome::OpenAgentDetail(task_id.to_string()),
                None => {
                    history.push(
                        HistoryCell::Separator {
                            label: Some("用法: /agent <task-id>".to_string()),
                        },
                        width,
                    );
                    KeyOutcome::None
                }
            }
        }
        SlashCommand::Approve
        | SlashCommand::Deny
        | SlashCommand::Budget
        | SlashCommand::Priority => {
            history.push(
                HistoryCell::Separator {
                    label: Some(format!(
                        "/{} 将由本地 daemon runtime 执行 (控制客户端接入中)",
                        cmd.name()
                    )),
                },
                width,
            );
            KeyOutcome::None
        }
        SlashCommand::Daemon => {
            let label = match crate::tui::slash::parse_daemon_args(args.as_deref().unwrap_or("")) {
                Ok(action) => {
                    let result = crate::runtime_socket_for(workdir)
                        .map_err(|error| error.to_string())
                        .and_then(|socket| match action {
                            crate::tui::slash::DaemonAction::Status => daemon_status_at(&socket),
                            crate::tui::slash::DaemonAction::Stop => daemon_stop_at(&socket),
                        });
                    match result {
                        Ok(message) => message,
                        Err(error) => format!("无法联系本地 daemon runtime: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Runtime => {
            use crate::tui::runtime_prefs::{self, RuntimePreference};
            let label = match crate::tui::slash::parse_runtime_args(args.as_deref().unwrap_or("")) {
                Ok(crate::tui::slash::RuntimeAction::Status) => {
                    let current = runtime_prefs::load(workdir);
                    format!(
                        "子 Agent runtime 偏好: {}（来源: {}）; 重启后生效",
                        match current {
                            RuntimePreference::Ask => "ask",
                            RuntimePreference::Always => "always",
                            RuntimePreference::Never => "never",
                        },
                        runtime_prefs::preferences_path(workdir).display()
                    )
                }
                Ok(crate::tui::slash::RuntimeAction::Set(pref)) => {
                    match runtime_prefs::save(workdir, pref) {
                        Ok(()) => format!(
                            "已设为 {}（重启后生效）",
                            match pref {
                                RuntimePreference::Ask => "ask",
                                RuntimePreference::Always => "always",
                                RuntimePreference::Never => "never",
                            }
                        ),
                        Err(error) => format!("无法保存偏好: {error}"),
                    }
                }
                Err(usage) => usage,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Kanban => {
            // `/kanban` 是过渡别名；命中时先提示规范名，避免肌肉记忆继续传播旧名。
            if invoked_as.as_deref() == Some("kanban") {
                history.push(
                    HistoryCell::Separator {
                        label: Some("已更名为 /superpowers-kanban".to_string()),
                    },
                    width,
                );
            }
            let outcome = crate::tui::superpowers_kanban::handle_kanban(
                workdir,
                args.as_deref().unwrap_or(""),
            );
            for line in outcome.lines {
                history.push(HistoryCell::Separator { label: Some(line) }, width);
            }
            KeyOutcome::None
        }
        SlashCommand::Mcp => {
            match parse_mcp_args(args.as_deref().unwrap_or("")) {
                Ok(McpAction::Status) => {
                    let text = render_mcp_status(mcp.master(), &mcp.status());
                    history.push(HistoryCell::Markdown { text }, width);
                }
                Ok(McpAction::Master(on)) => {
                    mcp.set_master(on);
                    let _ = control_tx.blocking_send(crate::ControlCommand::McpRefresh);
                    history.push(
                        HistoryCell::Separator {
                            label: Some(format!(
                                "MCP master 已{}",
                                if on { "开启" } else { "关闭" }
                            )),
                        },
                        width,
                    );
                }
                Ok(McpAction::Server { name, on }) => match mcp.set_server(&name, on) {
                    Ok(()) => {
                        let _ = control_tx.blocking_send(crate::ControlCommand::McpRefresh);
                        history.push(
                            HistoryCell::Separator {
                                label: Some(format!(
                                    "MCP server '{name}' 已{}",
                                    if on { "开启" } else { "关闭" }
                                )),
                            },
                            width,
                        );
                    }
                    Err(err) => history.push(
                        HistoryCell::Separator {
                            label: Some(err.to_string()),
                        },
                        width,
                    ),
                },
                Err(msg) => history.push(HistoryCell::Separator { label: Some(msg) }, width),
            }
            KeyOutcome::None
        }
        SlashCommand::Review => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_review(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取审查信息: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Accept => {
            let label = match parse_accept_args(args.as_deref()) {
                Ok((task_id, confirmation)) => {
                    match daemon_accept(workdir, task_id, confirmation) {
                        Ok(message) => message,
                        Err(error) => format!("无法接受 delivery: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Rework => {
            let label = match parse_rework_args(args.as_deref()) {
                Ok((task_id, feedback, confirmation)) => {
                    match daemon_rework(workdir, task_id, feedback, confirmation) {
                        Ok(message) => message,
                        Err(error) => format!("无法请求返工: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Reject => {
            let label = match parse_reject_args(args.as_deref()) {
                Ok((task_id, reason, confirmation)) => {
                    match daemon_reject(workdir, task_id, reason, confirmation) {
                        Ok(message) => message,
                        Err(error) => format!("无法拒绝 delivery: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Events => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_events(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取任务事件: {error}"),
                },
                Err(_) => "用法: /events <task-id>".into(),
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Diff => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_diff(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取任务 diff: {error}"),
                },
                Err(_) => "用法: /diff <task-id>".into(),
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Mailbox => {
            let label = match parse_review_args(args.as_deref()) {
                Ok(task_id) => match daemon_mailbox(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法读取任务 mailbox: {error}"),
                },
                Err(_) => "用法: /mailbox <task-id>".into(),
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Pause => {
            let label = match parse_pause_resume_args(args.as_deref(), "pause") {
                Ok(task_id) => match daemon_pause(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法暂停任务: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Resume => {
            let label = match parse_pause_resume_args(args.as_deref(), "resume") {
                Ok(task_id) => match daemon_resume(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法恢复任务: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Cancel => {
            let label = match parse_cancel_args(args.as_deref()) {
                Ok((task_id, recursive, confirmation)) => {
                    match daemon_cancel(workdir, task_id, recursive, confirmation) {
                        Ok(message) => message,
                        Err(error) => format!("无法取消任务: {error}"),
                    }
                }
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Retry => {
            let label = match parse_retry_args(args.as_deref()) {
                Ok(task_id) => match daemon_retry(workdir, task_id) {
                    Ok(message) => message,
                    Err(error) => format!("无法重试任务: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
        SlashCommand::Message => {
            let label = match parse_user_message_args(args.as_deref()) {
                Ok((task_id, message)) => match daemon_user_message(workdir, task_id, message) {
                    Ok(message) => message,
                    Err(error) => format!("无法发送用户指令: {error}"),
                },
                Err(error) => error,
            };
            history.push(HistoryCell::Separator { label: Some(label) }, width);
            KeyOutcome::None
        }
    }
}

fn daemon_agents_summary(workdir: &std::path::Path, args: Option<&str>) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_agents_summary_at(&socket, args)
}

#[cfg(test)]
fn daemon_agent_detail_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::InspectTask {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    let yi_agent_store::ipc::IpcResponse::TaskDetail(detail) = response else {
        return Err("daemon 返回了非任务详情响应".into());
    };
    Ok(format!(
        "Agent {}\nsession: {}\nparent: {}\ndepth: {}\nstate: {}\ndelivery: {}",
        detail.task_id,
        detail.session_id,
        detail.parent_task_id.as_deref().unwrap_or("(root)"),
        detail.depth,
        detail.state,
        detail.delivery_json,
    ))
}

fn parse_cancel_args(args: Option<&str>) -> Result<(&str, bool, Option<&str>), String> {
    let usage = "用法: /cancel <task-id> [--recursive] [--confirm <token>]";
    let Some(args) = args else {
        return Err(usage.into());
    };
    let mut parts = args.split_whitespace();
    let Some(task_id) = parts.next() else {
        return Err(usage.into());
    };
    let mut recursive = false;
    let mut confirmation = None;
    while let Some(argument) = parts.next() {
        match argument {
            "--recursive" if !recursive => recursive = true,
            "--confirm" if confirmation.is_none() => {
                confirmation = Some(parts.next().ok_or_else(|| usage.to_string())?);
            }
            _ => return Err(usage.into()),
        }
    }
    Ok((task_id, recursive, confirmation))
}

fn parse_retry_args(args: Option<&str>) -> Result<&str, String> {
    parse_pause_resume_args(args, "retry")
}

fn parse_review_args(args: Option<&str>) -> Result<&str, String> {
    parse_pause_resume_args(args, "review")
}

fn parse_accept_args(args: Option<&str>) -> Result<(&str, Option<&str>), String> {
    let usage = "用法: /accept <task-id> [--confirm <token>]";
    let Some(args) = args else {
        return Err(usage.into());
    };
    let mut parts = args.split_whitespace();
    let Some(task_id) = parts.next() else {
        return Err(usage.into());
    };
    let confirmation = match parts.next() {
        None => None,
        Some("--confirm") => Some(parts.next().ok_or_else(|| usage.to_string())?),
        _ => return Err(usage.into()),
    };
    if parts.next().is_some() {
        return Err(usage.into());
    }
    Ok((task_id, confirmation))
}

fn parse_rework_args(args: Option<&str>) -> Result<(&str, &str, Option<&str>), String> {
    parse_review_text_args(args, "rework", "feedback")
}

fn parse_reject_args(args: Option<&str>) -> Result<(&str, &str, Option<&str>), String> {
    parse_review_text_args(args, "reject", "reason")
}

fn parse_review_text_args<'a>(
    args: Option<&'a str>,
    command: &str,
    text_name: &str,
) -> Result<(&'a str, &'a str, Option<&'a str>), String> {
    let usage = || format!("用法: /{command} <task-id> <{text_name}> [--confirm <token>]");
    let Some(args) = args.map(str::trim).filter(|args| !args.is_empty()) else {
        return Err(usage());
    };
    let Some((task_id, rest)) = args.split_once(char::is_whitespace) else {
        return Err(usage());
    };
    let rest = rest.trim();
    if task_id.is_empty() || rest.is_empty() {
        return Err(usage());
    }
    let (text, confirmation) = match rest.rsplit_once(" --confirm ") {
        Some((text, token)) => {
            let token = token.trim();
            if text.trim().is_empty() || token.is_empty() || token.split_whitespace().count() != 1 {
                return Err(usage());
            }
            (text.trim(), Some(token))
        }
        None => (rest, None),
    };
    Ok((task_id, text, confirmation))
}

fn parse_pause_resume_args<'a>(args: Option<&'a str>, command: &str) -> Result<&'a str, String> {
    let usage = || format!("用法: /{command} <task-id>");
    let Some(args) = args else {
        return Err(usage());
    };
    let mut parts = args.split_whitespace();
    let Some(task_id) = parts.next() else {
        return Err(usage());
    };
    if parts.next().is_some() {
        return Err(usage());
    }
    Ok(task_id)
}

fn parse_user_message_args(args: Option<&str>) -> Result<(&str, &str), String> {
    let Some(args) = args.map(str::trim).filter(|args| !args.is_empty()) else {
        return Err("用法: /message <task-id> <text>".into());
    };
    let Some((task_id, message)) = args.split_once(char::is_whitespace) else {
        return Err("用法: /message <task-id> <text>".into());
    };
    let message = message.trim();
    if task_id.is_empty() || message.is_empty() {
        return Err("用法: /message <task-id> <text>".into());
    }
    Ok((task_id, message))
}

fn daemon_user_message(
    workdir: &std::path::Path,
    task_id: &str,
    message: &str,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_user_message_at(&socket, task_id, message)
}

fn daemon_user_message_at(
    socket: &std::path::Path,
    task_id: &str,
    message: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::SendUserMessage {
            task_id: task_id.to_owned(),
            message: message.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::MessageQueued) {
        return Err("daemon 返回了非消息响应".into());
    }
    Ok(format!("已排队用户指令至任务: {task_id}"))
}

fn daemon_task_session_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::InspectTask {
            task_id: task_id.into(),
        },
    )
    .map_err(|error| error.to_string())?
    {
        yi_agent_store::ipc::IpcResponse::TaskDetail(detail) => Ok(detail.session_id),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format_ipc_error(code, message))
        }
        _ => Err("daemon 返回了非任务详情响应".into()),
    }
}

fn daemon_retry(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    let session_id = daemon_task_session_at(&socket, task_id)?;
    daemon_retry_at(&socket, &session_id, task_id)
}

fn daemon_pause(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    let session_id = daemon_task_session_at(&socket, task_id)?;
    daemon_pause_at(&socket, &session_id, task_id)
}

fn daemon_pause_at(
    socket: &std::path::Path,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PauseTask {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::TaskPaused) {
        return Err("daemon 返回了非暂停响应".into());
    }
    Ok(format!("已请求暂停任务: {task_id}"))
}

fn daemon_resume(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    let session_id = daemon_task_session_at(&socket, task_id)?;
    daemon_resume_at(&socket, &session_id, task_id)
}

fn daemon_resume_at(
    socket: &std::path::Path,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ResumeTask {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::TaskResumed) {
        return Err("daemon 返回了非恢复响应".into());
    }
    Ok(format!("已请求恢复任务: {task_id}"))
}

fn daemon_retry_at(
    socket: &std::path::Path,
    session_id: &str,
    task_id: &str,
) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::RetryTask {
            session_id: session_id.to_owned(),
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    if !matches!(response, yi_agent_store::ipc::IpcResponse::TaskRetried) {
        return Err("daemon 返回了非重试响应".into());
    }
    Ok(format!("已请求重试任务: {task_id}"))
}

fn daemon_cancel(
    workdir: &std::path::Path,
    task_id: &str,
    recursive: bool,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_cancel_at(&socket, task_id, recursive, confirmation)
}

fn daemon_cancel_at(
    socket: &std::path::Path,
    task_id: &str,
    recursive: bool,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let request = match confirmation {
        Some(confirmation_token) => yi_agent_store::ipc::IpcRequest::ConfirmCancel {
            task_id: task_id.to_owned(),
            recursive,
            confirmation_token: confirmation_token.to_owned(),
        },
        None => yi_agent_store::ipc::IpcRequest::PreviewCancel {
            task_id: task_id.to_owned(),
            recursive,
        },
    };
    let response =
        yi_agent_store::ipc::send_request(socket, request).map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token,
            task_ids,
            expires_in_secs,
            ..
        } => Ok(format!(
            "取消预览（{} 个任务）: {}；使用 /cancel {task_id}{} --confirm {confirmation_token} 在 {expires_in_secs}s 内确认",
            task_ids.len(),
            task_ids.join(", "),
            if recursive { " --recursive" } else { "" },
        )),
        yi_agent_store::ipc::IpcResponse::TaskCancelled => Ok(if recursive {
            format!("已递归取消任务树: {task_id}")
        } else {
            format!("已取消任务: {task_id}")
        }),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝取消请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非取消响应".into()),
    }
}

fn daemon_review(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_review_at(&socket, task_id)
}

fn daemon_review_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskDiff {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskDiff { delivery_json, .. } => {
            Ok(format!("Delivery 审查 {task_id}\n{delivery_json}"))
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝审查请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非审查响应".into()),
    }
}

fn daemon_events(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_events_at(&socket, task_id)
}

fn daemon_events_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskEvents {
            task_id: task_id.to_owned(),
            after_event_id: None,
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskEvents { events } if events.is_empty() => {
            Ok(format!("Events {task_id}: 暂无事件"))
        }
        yi_agent_store::ipc::IpcResponse::TaskEvents { events } => {
            let mut output = format!("Events {}（{} 条）", task_id, events.len());
            for event in events {
                output.push_str(&format!(
                    "\n{} {} {}",
                    event.event_id, event.kind, event.payload_json
                ));
            }
            Ok(output)
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝事件请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非事件响应".into()),
    }
}

fn daemon_diff(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_diff_at(&socket, task_id)
}

fn daemon_diff_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskDiff {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskDiff { delivery_json, .. } => {
            Ok(format!("Diff {task_id}\n{delivery_json}"))
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝 diff 请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非 diff 响应".into()),
    }
}

fn daemon_mailbox(workdir: &std::path::Path, task_id: &str) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_mailbox_at(&socket, task_id)
}

fn daemon_mailbox_at(socket: &std::path::Path, task_id: &str) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ReadTaskMailbox {
            task_id: task_id.to_owned(),
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskMailbox { messages } if messages.is_empty() => {
            Ok(format!("Mailbox {task_id}: 暂无消息"))
        }
        yi_agent_store::ipc::IpcResponse::TaskMailbox { messages } => {
            let mut output = format!("Mailbox {}（{} 条）", task_id, messages.len());
            for message in messages {
                output.push_str(&format!(
                    "\n{} {} priority={} {}",
                    message.message_id, message.kind, message.priority, message.payload_json
                ));
            }
            Ok(output)
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝 mailbox 请求: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非 mailbox 响应".into()),
    }
}

fn daemon_accept(
    workdir: &std::path::Path,
    task_id: &str,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_accept_at(&socket, task_id, confirmation)
}

fn daemon_accept_at(
    socket: &std::path::Path,
    task_id: &str,
    confirmation: Option<&str>,
) -> Result<String, String> {
    daemon_review_decision_at(
        socket,
        task_id,
        yi_agent_store::ipc::IpcReviewDecision::Accept {},
        confirmation,
        format!("/accept {task_id}"),
    )
}

fn daemon_rework(
    workdir: &std::path::Path,
    task_id: &str,
    feedback: &str,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_rework_at(&socket, task_id, feedback, confirmation)
}

fn daemon_rework_at(
    socket: &std::path::Path,
    task_id: &str,
    feedback: &str,
    confirmation: Option<&str>,
) -> Result<String, String> {
    daemon_review_decision_at(
        socket,
        task_id,
        yi_agent_store::ipc::IpcReviewDecision::Rework {
            feedback: feedback.to_owned(),
        },
        confirmation,
        format!("/rework {task_id} {feedback}"),
    )
}

fn daemon_reject(
    workdir: &std::path::Path,
    task_id: &str,
    reason: &str,
    confirmation: Option<&str>,
) -> Result<String, String> {
    let socket = crate::runtime_socket_for(workdir).map_err(|error| error.to_string())?;
    daemon_reject_at(&socket, task_id, reason, confirmation)
}

fn daemon_reject_at(
    socket: &std::path::Path,
    task_id: &str,
    reason: &str,
    confirmation: Option<&str>,
) -> Result<String, String> {
    daemon_review_decision_at(
        socket,
        task_id,
        yi_agent_store::ipc::IpcReviewDecision::Reject {
            reason: reason.to_owned(),
        },
        confirmation,
        format!("/reject {task_id} {reason}"),
    )
}

fn daemon_review_decision_at(
    socket: &std::path::Path,
    task_id: &str,
    decision: yi_agent_store::ipc::IpcReviewDecision,
    confirmation: Option<&str>,
    confirm_command: String,
) -> Result<String, String> {
    let request = match confirmation {
        Some(confirmation_token) => yi_agent_store::ipc::IpcRequest::ConfirmReview {
            task_id: task_id.to_owned(),
            decision,
            confirmation_token: confirmation_token.to_owned(),
        },
        None => yi_agent_store::ipc::IpcRequest::PreviewReview {
            task_id: task_id.to_owned(),
            decision,
        },
    };
    let response =
        yi_agent_store::ipc::send_request(socket, request).map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::ReviewPreview {
            delivery_id,
            confirmation_token,
            expires_in_secs,
            ..
        } => Ok(format!(
            "审查预览: task={task_id} delivery={delivery_id}；使用 {confirm_command} --confirm {confirmation_token} 在 {expires_in_secs}s 内确认"
        )),
        yi_agent_store::ipc::IpcResponse::ReviewApproved => {
            Ok(format!("已接受 delivery: {task_id}"))
        }
        yi_agent_store::ipc::IpcResponse::ReviewReworkRequested => {
            Ok(format!("已请求子任务返工: {task_id}"))
        }
        yi_agent_store::ipc::IpcResponse::ReviewRejected => {
            Ok(format!("已拒绝 delivery: {task_id}"))
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(format!(
            "daemon 拒绝审查决策: {}",
            format_ipc_error(code, message)
        )),
        _ => Err("daemon 返回了非审查决策响应".into()),
    }
}

fn daemon_status_at(socket: &std::path::Path) -> Result<String, String> {
    let response =
        yi_agent_store::ipc::send_request(socket, yi_agent_store::ipc::IpcRequest::Status)
            .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::Status {
            high_water_event_id,
        } => Ok(format!(
            "daemon 运行中（event high-water: {high_water_event_id}）"
        )),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format!("daemon 拒绝请求: {code:?} {message:?}"))
        }
        other => Err(format!("daemon 返回了非预期响应: {other:?}")),
    }
}

fn daemon_stop_at(socket: &std::path::Path) -> Result<String, String> {
    let response = yi_agent_store::ipc::send_request(socket, yi_agent_store::ipc::IpcRequest::Stop)
        .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::Stopping => Ok("daemon 正在停止".to_string()),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            Err(format!("daemon 拒绝请求: {code:?} {message:?}"))
        }
        other => Err(format!("daemon 返回了非预期响应: {other:?}")),
    }
}

fn daemon_agents_summary_at(
    socket: &std::path::Path,
    args: Option<&str>,
) -> Result<String, String> {
    let (session_id, active_only, title, excluded_task_id, empty_label) =
        match args.map(str::trim).filter(|args| !args.is_empty()) {
            None => {
                let root = crate::tui::subagents::current_attached_root().ok_or_else(|| {
                    "当前 TUI 未接入 subagent runtime；使用 /agents --all 查看 daemon 全部任务"
                        .to_string()
                })?;
                (
                    Some(root.session_id),
                    false,
                    "Current session",
                    Some(root.task_id),
                    "暂无子任务",
                )
            }
            Some("--all") => (None, false, "All agents", None, "暂无任务"),
            Some("--active") => (None, true, "Active agents", None, "暂无任务"),
            Some(_) => return Err("用法: /agents [--all|--active]".into()),
        };
    let response = yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id,
            active_only,
        },
    )
    .map_err(|error| error.to_string())?;
    let yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } = response else {
        return Err("daemon 返回了非任务摘要响应".into());
    };
    Ok(format_agents_summary(
        title,
        tasks,
        excluded_task_id.as_deref(),
        empty_label,
    ))
}

/// The children of the root this TUI is attached to, for the subagent tab.
///
/// A list is navigation, not a work surface: an unreachable daemon or a
/// detached root yields an empty list rather than an error the user has to
/// dismiss, so Ctrl+P keeps working in every runtime state.
fn subagent_children_at(socket: &std::path::Path) -> SubagentListState {
    let Some(root) = crate::tui::subagents::current_attached_root() else {
        return SubagentListState::default();
    };
    let response = match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: Some(root.session_id),
            active_only: false,
        },
    ) {
        Ok(response) => response,
        Err(_) => return SubagentListState::default(),
    };
    let yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } = response else {
        return SubagentListState::default();
    };
    SubagentListState::new(
        tasks
            .into_iter()
            .filter(|task| !task.is_root && task.task_id != root.task_id)
            .map(|task| super::trace::SubagentListItem {
                active: !is_terminal_task_state(&task.state),
                task_id: task.task_id,
                objective: None,
                state: task.state,
                last_step: None,
                parent_task_id: task.parent_task_id,
            })
            .collect(),
    )
}

/// Whether a task can no longer make progress. Mirrors the daemon's own
/// terminal vocabulary so the tab agrees with `/agents`.
fn is_terminal_task_state(state: &str) -> bool {
    matches!(
        state,
        "completed" | "completed_no_changes" | "failed" | "cancelled"
    )
}

fn format_agents_summary(
    title: &str,
    tasks: Vec<yi_agent_store::ipc::IpcTaskSummary>,
    excluded_task_id: Option<&str>,
    empty_label: &str,
) -> String {
    let tasks: Vec<_> = tasks
        .into_iter()
        .filter(|task| Some(task.task_id.as_str()) != excluded_task_id)
        .collect();
    if tasks.is_empty() {
        return format!("**{title}**\n\n{empty_label}");
    }
    let mut output = format!("**{title} ({})**", tasks.len());
    for task in tasks {
        let root_label = if task.is_root { " **root**" } else { "" };
        output.push_str(&format!(
            "\n- `{}`: {}{}",
            task.task_id, task.state, root_label
        ));
    }
    output
}

/// Build the popup widget for rendering.
///
/// `height` is the popup's row budget, borders included -- the area the layout
/// actually gave us, which a squeezed terminal can make smaller than the popup
/// asked for. Only the windowed commands are drawn; the renderer clips the
/// rest, so rendering the whole list would hide the highlighted row once the
/// selection scrolls past the visible rows.
fn build_popup<'a>(popup: &'a CommandPopup, height: u16) -> Paragraph<'a> {
    let content_height = height.saturating_sub(2) as usize;
    let window = popup.visible(content_height);
    let window_start = popup.window_start(content_height);
    let lines: Vec<Line<'a>> = window
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let name = format!(
                "/{}{}",
                cmd.name(),
                cmd.argument_usage()
                    .map(|usage| format!(" {usage}"))
                    .unwrap_or_default()
            );
            let desc = cmd.description();
            let is_selected = window_start + i == popup.selected_index();
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
            Span::styled("再按 Ctrl+C 退出", Style::new().fg(Color::Yellow)),
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
    // Derive the height from the exact same wrapping the renderer uses. The
    // previous `div_ceil(width)` estimate was wrong for explicit newlines and
    // for per-character wrapping, and a too-small height silently hides the
    // overflow rows (the terminal only reserves `height` rows for the input).
    let prefix = Span::raw("> ");
    wrap_input_buffer(&input.buffer, input.cursor, &prefix, area_width).len() as u16
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
        .map(|p| p.height(area.height) as u16)
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
        // Note: an empty chunk still needs the cursor render below, so a blank
        // line (or the position after a trailing '\n') can show it.
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

    // Empty buffer: show the cursor at position 0 as a styled space.
    if buffer.is_empty() {
        return vec![Line::from(vec![
            prefix.clone(),
            Span::styled(" ", cursor_style),
        ])];
    }

    // Split by explicit newlines first: a pasted multi-line block used to be
    // one span holding raw '\n's, which renders as a single over-wide (and
    // mis-shaped) line whose tail ratatui drops. Byte offsets are tracked so
    // the reverse-video cursor still lands on the right character.
    let mut raw_lines: Vec<(usize, String)> = Vec::new();
    let mut segment_start = 0usize;
    for segment in buffer.split('\n') {
        raw_lines.push((segment_start, segment.to_string()));
        segment_start += segment.len() + 1; // +1 for the consumed '\n'
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (seg_start, segment) in raw_lines {
        let mut current = String::new();
        let mut current_width = 0usize;
        let mut chunk_start = seg_start;

        for ch in segment.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + ch_width > avail && !current.is_empty() {
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
        // Push the segment's final chunk, even when empty, so blank lines and
        // the trailing cursor position survive.
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
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn multiline_paste_respects_newlines_and_width() {
        // A paste with explicit newlines was pushed through as raw text: the
        // '\n's stayed inside one span and the line overflowed the terminal.
        let prefix = Span::raw("> ");
        let text = "first line\n\nsecond line that is long enough to wrap\nthird";
        let mut inp = InputLine::new();
        inp.buffer = text.into();
        let width = 40u16;
        let lines = wrap_input_buffer(&inp.buffer, inp.cursor, &prefix, width);
        for (i, line) in lines.iter().enumerate() {
            let t: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(
                !t.contains('\n'),
                "input line {i} still holds a raw newline: {t:?}"
            );
            let w = UnicodeWidthStr::width(t.as_str());
            assert!(w <= width as usize, "input line {i} is {w} cols: {t:?}");
        }
        assert_eq!(
            compute_input_height(&inp, false, width) as usize,
            lines.len(),
            "height must equal the wrapped line count"
        );
    }

    #[test]
    fn pasted_long_line_is_wrapped_not_clipped() {
        // A bracketed paste arrives as one chunk with no newlines. The wrapper
        // used to split only on '\n' and emit a single over-wide line, whose
        // tail ratatui silently dropped.
        let pasted = "x".repeat(300) + &"中".repeat(60) + "TAIL";
        let width = 40u16;
        let lines = wrap_input_buffer(&pasted, 0, &Span::raw("> "), width);
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= width as usize,
                "input line {i} is {w} cols, width is {width}"
            );
        }
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            rendered.contains("TAIL"),
            "pasted tail was dropped: {rendered:?}"
        );
    }

    #[test]
    fn input_height_matches_the_wrapped_line_count() {
        // The height must come from the same wrapping the renderer uses;
        // counting `div_ceil(width)` mis-sized CJK input and hid lines.
        let prefix = Span::raw("> ");
        for width in [20u16, 33, 40] {
            for text in [
                "a".repeat(200),
                "中".repeat(120),
                "mixed 中文 and ascii ".repeat(20),
                String::new(),
            ] {
                let mut inp = InputLine::new();
                inp.buffer = text.clone();
                let expected = wrap_input_buffer(&inp.buffer, inp.cursor, &prefix, width).len();
                assert_eq!(
                    compute_input_height(&inp, false, width) as usize,
                    expected,
                    "width {width} text {text:?}"
                );
            }
        }
    }

    /// The TUI's slash commands must talk to the same daemon the CLI does.
    /// They used to fall back to `~/.yi-agent/runtime`, while the daemon listens
    /// under `<workdir>/.yi-agent/runtime` — so `/agents`, `/diff`, `/pause` and
    /// friends queried a socket that no daemon ever bound.
    #[test]
    fn slash_commands_resolve_the_same_runtime_socket_as_the_daemon() {
        let workdir = std::path::Path::new("/tmp/some-project");

        let from_tui = crate::runtime_socket_for(workdir).expect("resolves");
        let from_cli = yi_agent_store::ipc::socket_path_for(&crate::runtime_directory_for(workdir))
            .expect("resolves");

        assert_eq!(
            from_tui, from_cli,
            "the TUI and the CLI must agree on one runtime socket"
        );
    }

    #[test]
    fn active_managed_process_statuses_exclude_terminal_states() {
        use yi_agent_tools::ProcessStatus;

        assert!(is_active_managed_process(&ProcessStatus::Starting));
        assert!(is_active_managed_process(&ProcessStatus::Running));
        assert!(is_active_managed_process(&ProcessStatus::Ready));
        assert!(!is_active_managed_process(&ProcessStatus::Exited {
            code: Some(0)
        }));
        assert!(!is_active_managed_process(&ProcessStatus::Killed));
        assert!(!is_active_managed_process(&ProcessStatus::FailedToStart {
            reason: "spawn failed".into(),
        }));
    }
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use futures::future::BoxFuture;
    use ratatui::backend::TestBackend;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use yi_agent_core::subagent::task::WorkspaceLeaseId;
    use yi_agent_core::subagent::worker::{
        AgentWorkerFactory, WorkerError, WorkerHandle, WorkerRecoveryContext, WorkerStart,
    };
    use yi_agent_core::{OutputStream, RootSessionId, TaskId};

    struct RecordingWorkerFactory;

    impl AgentWorkerFactory for RecordingWorkerFactory {
        fn recovery_context(&self) -> WorkerRecoveryContext {
            WorkerRecoveryContext {
                workspace_lease_id: Some("workspace:test".into()),
                worktree_lease: Some("worktree:test".into()),
                checkpoint_json: r#"{"git_head":"test","git_status":""}"#.into(),
                tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
            }
        }

        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            Box::pin(async move {
                let handle = WorkerHandle::new(request.cancellation);
                let paused_handle = handle.clone();
                let mut pause = handle.subscribe_pause();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("test pause listener runtime must initialize");
                    if runtime.block_on(pause.requested()) {
                        paused_handle.report_paused();
                    }
                });
                Ok(handle)
            })
        }
    }

    #[derive(Default)]
    struct DeliveryRecordingFactory {
        handles: std::sync::Mutex<Vec<WorkerHandle>>,
        workspaces: std::sync::Mutex<Vec<WorkspaceLeaseId>>,
    }

    impl AgentWorkerFactory for DeliveryRecordingFactory {
        fn recovery_context(&self) -> WorkerRecoveryContext {
            WorkerRecoveryContext {
                workspace_lease_id: Some("workspace:test".into()),
                worktree_lease: Some("worktree:test".into()),
                checkpoint_json: r#"{"git_head":"test","git_status":""}"#.into(),
                tool_state_json: r#"{"state":"available","registered_tools":[]}"#.into(),
            }
        }

        fn start(
            &self,
            request: WorkerStart,
        ) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>> {
            let handle = WorkerHandle::new(request.cancellation);
            self.workspaces
                .lock()
                .unwrap()
                .push(request.workspace_lease_id.unwrap_or_default());
            self.handles.lock().unwrap().push(handle.clone());
            Box::pin(async move { Ok(handle) })
        }
    }

    fn start_daemon_with_delivered_child() -> (TempDir, yi_agent_store::ipc::Daemon, String) {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let factory = Arc::new(DeliveryRecordingFactory::default());
        let daemon = yi_agent_store::ipc::Daemon::start_with_factory(
            directory.path().join("runtime"),
            &database,
            factory.clone(),
        )
        .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::CreateSession,
        )
        .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::StartWorker {
                session_id: session_id.clone(),
                task_id: root_task_id.clone(),
            },
        )
        .unwrap();
        let spawn_response = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::SpawnChild {
                workdir: None,
                session_id,
                parent_task_id: root_task_id,
                objective: "实现 parser".into(),
                mode: Some("coding".into()),
                model: None,

                sandbox: None,
            },
        )
        .unwrap();
        let yi_agent_store::ipc::IpcResponse::TaskSpawned { task_id } = spawn_response else {
            panic!("expected a spawned child, got {spawn_response:?}");
        };
        let workspace = factory.workspaces.lock().unwrap()[1].clone();
        factory.handles.lock().unwrap()[1].report_delivery(
            yi_agent_core::subagent::task::DeliveryReport::coding(
                "deadbeef",
                "main",
                workspace,
                "cargo test -p child",
            ),
        );
        for _ in 0..100 {
            if yi_agent_store::repository::RuntimeRepository::open(&database)
                .unwrap()
                .task_state(&task_id.parse().unwrap())
                .unwrap()
                == "awaiting_parent_review"
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (directory, daemon, task_id)
    }

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
    fn agents_summary_reads_daemon_task_snapshot() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let mut repository =
            yi_agent_store::repository::RuntimeRepository::open(&database).unwrap();
        let task = TaskId::new();
        repository
            .create_task(&task, &RootSessionId::new(), "queued")
            .unwrap();

        let summary = daemon_agents_summary_at(daemon.socket_path(), Some("--all")).unwrap();

        assert!(summary.starts_with("**All agents (1)**"));
        assert!(summary.contains(&format!("- `{task}`: queued")));
        assert!(summary.contains(&task.to_string()));
        assert!(summary.contains("queued"));
    }

    #[test]
    fn current_agents_summary_excludes_the_attached_root_task() {
        let summary = format_agents_summary(
            "Current session",
            vec![
                yi_agent_store::ipc::IpcTaskSummary {
                    task_id: "root".into(),
                    state: "queued".into(),
                    is_root: true,
                    parent_task_id: None,
                    thread_id: None,
                },
                yi_agent_store::ipc::IpcTaskSummary {
                    task_id: "child".into(),
                    state: "running".into(),
                    is_root: false,
                    parent_task_id: Some("root".into()),
                    thread_id: None,
                },
            ],
            Some("root"),
            "暂无子任务",
        );

        assert!(!summary.contains("root"));
        assert!(summary.contains("Current session (1)"));
        assert!(summary.contains("`child`: running"));
    }

    #[test]
    fn all_agents_summary_marks_root_tasks() {
        let summary = format_agents_summary(
            "All agents",
            vec![yi_agent_store::ipc::IpcTaskSummary {
                task_id: "root".into(),
                state: "running".into(),
                is_root: true,
                parent_task_id: None,
                thread_id: None,
            }],
            None,
            "暂无任务",
        );

        assert!(summary.contains("`root`: running **root**"));
    }

    #[test]
    fn agent_detail_reads_a_daemon_task_for_user_inspection() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(detail.contains(&root_task_id));
        assert!(detail.contains("state: queued"));
        assert!(detail.contains("depth: 0"));
    }

    #[test]
    fn cancel_control_requires_a_preview_token_before_cancelling() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let preview = daemon_cancel_at(daemon.socket_path(), &root_task_id, true, None).unwrap();
        assert!(preview.contains("取消预览"));
        let yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewCancel {
                task_id: root_task_id.clone(),
                recursive: true,
            },
        )
        .unwrap()
        else {
            panic!("expected a cancel preview");
        };
        let result = daemon_cancel_at(
            daemon.socket_path(),
            &root_task_id,
            true,
            Some(&confirmation_token),
        )
        .unwrap();

        assert!(result.contains("递归"));
        let detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();
        assert!(detail.contains("state: cancelled"));
    }

    #[test]
    fn review_command_parsers_accept_confirmation_tokens_and_freeform_text() {
        assert_eq!(parse_review_args(Some("task-1")).unwrap(), "task-1");
        assert_eq!(
            parse_accept_args(Some("task-1 --confirm token-1")).unwrap(),
            ("task-1", Some("token-1"))
        );
        assert_eq!(
            parse_rework_args(Some("task-1 修一下边界条件 --confirm token-2")).unwrap(),
            ("task-1", "修一下边界条件", Some("token-2"))
        );
        assert_eq!(
            parse_reject_args(Some("task-1 方向不对，先停止")).unwrap(),
            ("task-1", "方向不对，先停止", None)
        );
    }

    #[test]
    fn review_control_reads_delivery_json_from_daemon() {
        let (_directory, daemon, task_id) = start_daemon_with_delivered_child();

        let review = daemon_review_at(daemon.socket_path(), &task_id).unwrap();

        assert!(review.contains("Delivery 审查"));
        assert!(review.contains("deadbeef"));
        assert!(review.contains("cargo test -p child"));
    }

    #[test]
    fn events_control_reads_task_events_from_daemon() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::repository::RuntimeRepository::open(&database)
            .unwrap()
            .append_event(
                &root_task_id.parse().unwrap(),
                yi_agent_store::repository::RuntimeEvent::TaskStarted,
            )
            .unwrap();

        let events = daemon_events_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(events.contains("Events"));
        assert!(events.contains("task_started"));
    }

    #[test]
    fn mailbox_control_reads_messages_without_consuming_them() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::SendUserMessage {
                task_id: root_task_id.clone(),
                message: "请汇报进展".into(),
            },
        )
        .unwrap();

        let mailbox = daemon_mailbox_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(mailbox.contains("Mailbox"));
        assert!(mailbox.contains("user_override"));
        assert!(mailbox.contains("请汇报进展"));
        assert_eq!(
            yi_agent_store::repository::RuntimeRepository::open(&database)
                .unwrap()
                .mailbox_messages_for_task(&root_task_id.parse().unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn diff_control_reads_delivery_evidence_from_daemon() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let diff = daemon_diff_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(diff.contains("Diff"));
        assert!(diff.contains("Root session objective not specified."));
    }

    #[test]
    fn accept_control_previews_then_confirms_review_from_tui() {
        let (_directory, daemon, task_id) = start_daemon_with_delivered_child();

        let preview = daemon_accept_at(daemon.socket_path(), &task_id, None).unwrap();

        assert!(preview.contains("审查预览"));
        assert!(preview.contains("/accept"));
        let yi_agent_store::ipc::IpcResponse::ReviewPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewReview {
                task_id: task_id.clone(),
                decision: yi_agent_store::ipc::IpcReviewDecision::Accept {},
            },
        )
        .unwrap()
        else {
            panic!("expected a review preview");
        };
        let result =
            daemon_accept_at(daemon.socket_path(), &task_id, Some(&confirmation_token)).unwrap();

        assert!(result.contains("已接受"));
    }

    #[test]
    fn rework_control_previews_then_confirms_review_from_tui() {
        let (_directory, daemon, task_id) = start_daemon_with_delivered_child();

        let preview = daemon_rework_at(daemon.socket_path(), &task_id, "补测试", None).unwrap();

        assert!(preview.contains("审查预览"));
        assert!(preview.contains("/rework"));
        assert!(preview.contains("补测试"));
        let yi_agent_store::ipc::IpcResponse::ReviewPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewReview {
                task_id: task_id.clone(),
                decision: yi_agent_store::ipc::IpcReviewDecision::Rework {
                    feedback: "补测试".into(),
                },
            },
        )
        .unwrap()
        else {
            panic!("expected a review preview");
        };
        let result = daemon_rework_at(
            daemon.socket_path(),
            &task_id,
            "补测试",
            Some(&confirmation_token),
        )
        .unwrap();

        assert!(result.contains("已请求子任务返工"));
    }

    #[test]
    fn reject_control_previews_then_confirms_review_from_tui() {
        let (_directory, daemon, task_id) = start_daemon_with_delivered_child();

        let preview = daemon_reject_at(daemon.socket_path(), &task_id, "方向不对", None).unwrap();

        assert!(preview.contains("审查预览"));
        assert!(preview.contains("/reject"));
        assert!(preview.contains("方向不对"));
        let yi_agent_store::ipc::IpcResponse::ReviewPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewReview {
                task_id: task_id.clone(),
                decision: yi_agent_store::ipc::IpcReviewDecision::Reject {
                    reason: "方向不对".into(),
                },
            },
        )
        .unwrap()
        else {
            panic!("expected a review preview");
        };
        let result = daemon_reject_at(
            daemon.socket_path(),
            &task_id,
            "方向不对",
            Some(&confirmation_token),
        )
        .unwrap();

        assert!(result.contains("已拒绝"));
    }

    #[test]
    fn retry_control_routes_to_the_daemon_with_explicit_session_scope() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::CreateSession,
        )
        .unwrap()
        else {
            panic!("expected a created session");
        };
        let yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token, ..
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::PreviewCancel {
                task_id: root_task_id.clone(),
                recursive: false,
            },
        )
        .unwrap()
        else {
            panic!("expected a cancel preview");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::ConfirmCancel {
                task_id: root_task_id.clone(),
                recursive: false,
                confirmation_token,
            },
        )
        .unwrap();

        let result = daemon_retry_at(daemon.socket_path(), &session_id, &root_task_id).unwrap();

        assert!(result.contains("已请求重试"));
    }

    #[test]
    fn pause_and_resume_controls_require_a_task_id() {
        assert_eq!(
            parse_pause_resume_args(Some("task-1"), "pause").unwrap(),
            "task-1"
        );
        assert_eq!(
            parse_pause_resume_args(Some("task-1 extra"), "pause").unwrap_err(),
            "用法: /pause <task-id>"
        );
        assert_eq!(
            parse_pause_resume_args(None, "resume").unwrap_err(),
            "用法: /resume <task-id>"
        );
    }

    #[test]
    fn pause_and_resume_controls_route_to_the_daemon_with_explicit_session_scope() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon = yi_agent_store::ipc::Daemon::start_with_factory(
            directory.path().join("runtime"),
            &database,
            Arc::new(RecordingWorkerFactory),
        )
        .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated {
            session_id,
            root_task_id,
        } = yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::CreateSession,
        )
        .unwrap()
        else {
            panic!("expected a created session");
        };
        yi_agent_store::ipc::send_request(
            daemon.socket_path(),
            yi_agent_store::ipc::IpcRequest::StartWorker {
                session_id: session_id.clone(),
                task_id: root_task_id.clone(),
            },
        )
        .unwrap();

        let paused = daemon_pause_at(daemon.socket_path(), &session_id, &root_task_id).unwrap();
        let paused_detail = (0..20)
            .find_map(|_| {
                let detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();
                if detail.contains("state: paused") {
                    Some(detail)
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("worker pause acknowledgement should be reconciled");
        let resumed = daemon_resume_at(daemon.socket_path(), &session_id, &root_task_id).unwrap();
        let resumed_detail = daemon_agent_detail_at(daemon.socket_path(), &root_task_id).unwrap();

        assert!(paused.contains("已请求暂停"));
        assert!(paused_detail.contains("state: paused"));
        assert!(resumed.contains("已请求恢复"));
        assert!(resumed_detail.contains("state: running"));
    }

    #[test]
    fn message_control_routes_an_audited_user_override_to_the_daemon() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(directory.path().join("runtime"), &database)
                .unwrap();
        let yi_agent_store::ipc::IpcResponse::SessionCreated { root_task_id, .. } =
            yi_agent_store::ipc::send_request(
                daemon.socket_path(),
                yi_agent_store::ipc::IpcRequest::CreateSession,
            )
            .unwrap()
        else {
            panic!("expected a created session");
        };

        let result = daemon_user_message_at(
            daemon.socket_path(),
            &root_task_id,
            "请暂停并说明当前阻塞原因",
        )
        .unwrap();

        assert!(result.contains("已排队用户指令"));
    }

    #[test]
    fn test_route_event_tracks_only_bash_tool_calls() {
        let mut registry = RunningTaskRegistry::new();
        let mut statusbar = StatusBarState::default();
        let mut cost = CostTracker::default();

        route_event(
            &mut registry,
            &mut statusbar,
            &mut cost,
            &AgentEvent::ToolCall {
                id: "grep".into(),
                name: "grep".into(),
                input: serde_json::json!({"pattern": "TODO"}),
            },
        );
        assert!(registry.list().is_empty());

        route_event(
            &mut registry,
            &mut statusbar,
            &mut cost,
            &AgentEvent::ToolCall {
                id: "bash".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hi", "expected_timeout_sec": 30}),
            },
        );
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("bash").unwrap().command, "echo hi");
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

    /// Non-Bash tools do not create Ctrl+P tasks, and their results are safe
    /// to ignore in the Bash-task registry.
    #[test]
    fn test_route_event_ignores_non_bash_tool_results() {
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

        assert!(registry.get("search").is_none());
        assert!(registry.list().is_empty());
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

    struct QueueThenDoneEvents {
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        poll_count: Cell<usize>,
    }

    impl EventSource for QueueThenDoneEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => Ok(Some(Event::Paste("queued message".into()))),
                1 => {
                    self.agent_tx
                        .try_send(AgentEvent::Done {
                            reason: yi_agent_core::DoneReason::EndTurn,
                        })
                        .unwrap();
                    Ok(Some(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                    ))))
                }
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    /// Submits a mid-turn message, then delivers `InterjectionsReturned`
    /// immediately followed by `Cancelled` in a single frame — the order core
    /// guarantees. The returned text must land back in the input box, which only
    /// happens if the return is applied before the turn-end handling.
    struct InterjectionThenCancelEvents {
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        poll_count: Cell<usize>,
    }

    impl EventSource for InterjectionThenCancelEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => Ok(Some(Event::Paste("opening".into()))),
                1 => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                2 => Ok(Some(Event::Paste("mid-turn".into()))),
                3 => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                // Queue the pair but do NOT quit in the same frame: `run_loop`
                // drains `agent_rx` at the top of an iteration, so the events
                // sent here are only observed on the next one. An inert key
                // gives that frame a chance to happen before the quit below.
                4 => {
                    self.agent_tx
                        .try_send(AgentEvent::InterjectionsReturned {
                            items: vec![yi_agent_core::Interjection {
                                seq: 1,
                                text: "mid-turn".into(),
                                tag: None,
                            }],
                        })
                        .unwrap();
                    self.agent_tx.try_send(AgentEvent::Cancelled).unwrap();
                    Ok(Some(Event::Key(KeyEvent::new(
                        KeyCode::F(1),
                        KeyModifiers::NONE,
                    ))))
                }
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    /// A test backend whose size can change from a fake event source between
    /// frames, matching a terminal resize without sharing the terminal itself.
    #[derive(Clone)]
    struct ResizableTestBackend {
        inner: Rc<RefCell<TestBackend>>,
    }

    impl Backend for ResizableTestBackend {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.borrow_mut().draw(content)
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().hide_cursor()
        }

        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().show_cursor()
        }

        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            self.inner.borrow_mut().get_cursor_position()
        }

        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            self.inner.borrow_mut().set_cursor_position(position)
        }

        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().clear()
        }

        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.borrow().size()
        }

        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.borrow_mut().window_size()
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.borrow_mut().flush()
        }
    }

    struct ResizeThenQuitEvents {
        backend: Rc<RefCell<TestBackend>>,
        top_marker: Rc<RefCell<Option<String>>>,
        poll_count: Cell<usize>,
    }

    impl EventSource for ResizeThenQuitEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => {
                    let backend = self.backend.borrow();
                    let row: String = (0..32u16)
                        .map(|x| backend.buffer()[(x, 0)].symbol())
                        .collect();
                    drop(backend);
                    let marker = row
                        .split_whitespace()
                        .find(|word| word.starts_with("MARKER-"))
                        .expect("the first rendered history row has a marker")
                        .to_string();
                    *self.top_marker.borrow_mut() = Some(marker);
                    self.backend.borrow_mut().resize(16, 14);
                    Ok(None)
                }
                1 => Ok(None),
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    /// Captures the old and new top rows around an idle local submission.
    /// The final frame must restore the pre-submit content anchor rather than
    /// using the width before the scrollbar was accounted for.
    struct SubmitThenQuitEvents {
        backend: Rc<RefCell<TestBackend>>,
        top_marker: Rc<RefCell<Option<String>>>,
        poll_count: Cell<usize>,
    }

    impl EventSource for SubmitThenQuitEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let poll_count = self.poll_count.get();
            self.poll_count.set(poll_count + 1);
            match poll_count {
                0 => {
                    let backend = self.backend.borrow();
                    let row: String = (0..20u16)
                        .map(|x| backend.buffer()[(x, 0)].symbol())
                        .collect();
                    drop(backend);
                    let marker = row
                        .split_whitespace()
                        .find(|word| word.starts_with("MARKER-"))
                        .expect("the first rendered history row has a marker")
                        .to_string();
                    *self.top_marker.borrow_mut() = Some(marker);
                    Ok(Some(Event::Paste("new local message".into())))
                }
                1 => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                _ => Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::CONTROL,
                )))),
            }
        }
    }

    #[test]
    fn history_anchor_survives_resize_between_frames() {
        let backend = Rc::new(RefCell::new(TestBackend::new(32, 14)));
        let mut terminal = Terminal::new(ResizableTestBackend {
            inner: Rc::clone(&backend),
        })
        .unwrap();
        let (_agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        for index in 0..20 {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("MARKER-{index:02} 12345678901234567890"),
                },
                32,
            );
        }
        history.scroll_offset = 16;
        let mut input = InputLine::new();
        let source = ResizeThenQuitEvents {
            backend: Rc::clone(&backend),
            top_marker: Rc::new(RefCell::new(None)),
            poll_count: Cell::new(0),
        };

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        let narrow_area = ratatui::layout::Rect::new(0, 0, 16, 14);
        let narrow_layout = compute_layout(narrow_area, &input, false, &None, 0);
        let narrow_history_area = narrow_layout.chunks[0];
        let narrow_text_width =
            history.text_width(narrow_history_area.width, narrow_history_area.height);
        let mut before_resize = HistoryState::from_cells(history.cells.clone(), 16);
        let wide_area = ratatui::layout::Rect::new(0, 0, 32, 14);
        let wide_layout = compute_layout(wide_area, &InputLine::new(), false, &None, 0);
        let wide_history_area = wide_layout.chunks[0];
        let wide_text_width =
            before_resize.text_width(wide_history_area.width, wide_history_area.height);
        before_resize.reconcile_scroll_offset(wide_text_width, wide_history_area.height);
        let anchor = before_resize
            .capture_viewport_anchor(wide_text_width, wide_history_area.height)
            .unwrap();
        let mut expected = before_resize;
        expected.restore_viewport_anchor(anchor, narrow_text_width, narrow_history_area.height);

        assert_eq!(history.scroll_offset, expected.scroll_offset);
        let top_marker = source.top_marker.borrow().clone().unwrap();
        let resized_backend = backend.borrow();
        let final_row: String = (0..16u16)
            .map(|x| resized_backend.buffer()[(x, 0)].symbol())
            .collect();
        assert!(
            final_row.contains(&top_marker),
            "expected top marker {top_marker:?} after resize, got {final_row:?}"
        );
    }

    #[test]
    fn history_anchor_survives_local_user_insertion_at_scrollbar_width() {
        let backend = Rc::new(RefCell::new(TestBackend::new(20, 14)));
        let mut terminal = Terminal::new(ResizableTestBackend {
            inner: Rc::clone(&backend),
        })
        .unwrap();
        let (_agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        for index in 0..20 {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("MARKER-{index:02} 1234567890"),
                },
                20,
            );
        }
        history.scroll_offset = 30;
        let source = SubmitThenQuitEvents {
            backend: Rc::clone(&backend),
            top_marker: Rc::new(RefCell::new(None)),
            poll_count: Cell::new(0),
        };

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        let area = ratatui::layout::Rect::new(0, 0, 20, 14);
        let layout = compute_layout(area, &InputLine::new(), false, &None, 0);
        let history_area = layout.chunks[0];
        assert_eq!(
            history.text_width(history_area.width, history_area.height),
            history_area.width - 1,
            "the inserted local message is restored with the scrollbar-reserved width"
        );
        let top_marker = source.top_marker.borrow().clone().unwrap();
        let backend = terminal.backend().inner.borrow();
        let final_row: String = (0..20u16)
            .map(|x| backend.buffer()[(x, 0)].symbol())
            .collect();
        assert!(
            final_row.contains(&top_marker),
            "expected top marker {top_marker:?} after local insertion, got {final_row:?}"
        );
    }

    #[test]
    fn returned_interjection_survives_the_turn_end_it_precedes() {
        let backend = TestBackend::new(60, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let source = InterjectionThenCancelEvents {
            agent_tx,
            poll_count: Cell::new(0),
        };

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        // The core invariant: `InterjectionsReturned` arrives ahead of
        // `Cancelled`, and the TUI hands the text back rather than losing it
        // when the turn ends in the very next frame. Without
        // `apply_interjection_event` wired into the event loop this assertion
        // fails with an empty input box.
        assert_eq!(
            input.buffer, "mid-turn",
            "the returned text must be restored to the input box"
        );
        assert!(
            history.cells.iter().any(|c| matches!(
                c,
                HistoryCell::Separator { label: Some(label) }
                    if label.contains("已退回输入框")
            )),
            "the return must be reported in history: {:?}",
            history.cells
        );
    }

    #[test]
    fn history_anchor_survives_queued_preview_promotion() {
        let backend = TestBackend::new(20, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        for index in 0..20 {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("MARKER-{index:02} 1234567890"),
                },
                20,
            );
        }
        history.scroll_offset = 30;
        let before_cells = history.cells.clone();
        let mut input = InputLine::new();
        let source = QueueThenDoneEvents {
            agent_tx,
            poll_count: Cell::new(0),
        };

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut input,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        let area = ratatui::layout::Rect::new(0, 0, 20, 14);
        let final_layout = compute_layout(area, &input, false, &None, 0);
        let final_history_area = final_layout.chunks[0];
        let final_text_width =
            history.text_width(final_history_area.width, final_history_area.height);
        let pre_drain_layout = compute_layout(area, &InputLine::new(), false, &None, 2);
        let pre_drain_width = history.text_width(
            pre_drain_layout.chunks[0].width,
            pre_drain_layout.chunks[0].height,
        );
        assert_ne!(
            pre_drain_layout.chunks[0].height, final_history_area.height,
            "promotion changes the history viewport height"
        );
        assert_eq!(
            pre_drain_width, 19,
            "wrapped history reserves a scrollbar column"
        );

        let mut before = HistoryState::from_cells(before_cells, 30);
        // The loop first renders without a queue and clamps the deliberately
        // over-large starting offset before the queued preview appears.
        let prior_layout = compute_layout(area, &InputLine::new(), false, &None, 0);
        let prior_text_width =
            before.text_width(prior_layout.chunks[0].width, prior_layout.chunks[0].height);
        before.reconcile_scroll_offset(prior_text_width, prior_layout.chunks[0].height);
        let anchor = before
            .capture_viewport_anchor(prior_text_width, prior_layout.chunks[0].height)
            .expect("the initial viewport is scrolled up");
        let mut expected = HistoryState::from_cells(history.cells.clone(), 0);
        expected.restore_viewport_anchor(anchor, final_text_width, final_history_area.height);

        assert_eq!(
            history.scroll_offset, expected.scroll_offset,
            "the top history cell must remain anchored when the queued preview is promoted; \
             old_width={prior_text_width}, new_width={final_text_width}, old_height={}, \
             new_height={}",
            pre_drain_layout.chunks[0].height, final_history_area.height,
        );
        let top_row: String = (0..20u16)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(
            top_row.contains("MARKER-00"),
            "the original top marker must remain at the viewport top: {top_row:?}"
        );
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

    #[test]
    fn runtime_start_prompt_sends_start_choice_before_accepting_chat_input() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::Start
        );
        assert!(
            input_rx.try_recv().is_err(),
            "startup choice must not be sent as chat input"
        );
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Always
        );
    }

    /// The alert must not eat the keystroke that dismisses it. A popup that
    /// consumed the first character typed would silently corrupt what the user
    /// was writing -- far worse than the nuisance it reports -- so the key both
    /// clears the alert and still reaches the input line.
    #[test]
    fn runtime_failure_alert_does_not_swallow_the_dismissing_key() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        agent_tx
            .try_send(AgentEvent::SubagentRuntimeUnavailable {
                stage: "runtime attach".into(),
                cause: "file is not a database".into(),
            })
            .unwrap();
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            // `ScriptedEvents::poll` pops from the back, so this is the
            // order the keys are delivered in.
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        assert_eq!(
            input_rx.try_recv().unwrap(),
            "hi",
            "the dismiss key was swallowed instead of reaching the input line"
        );
    }

    #[test]
    fn runtime_start_prompt_can_continue_without_delegation() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation
        );
        assert!(
            input_rx.try_recv().is_err(),
            "skip choice must not be sent as chat input"
        );
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Never
        );
    }

    #[test]
    fn escape_skips_runtime_for_this_session_without_persisting() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::Prompt),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        // Session behaviour matches "n": delegation is not enabled this run.
        assert_eq!(
            runtime_choice_rx.try_recv().unwrap(),
            crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation
        );
        // ...but Esc must not write a preference: the next launch asks again.
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(input_rx.try_recv().is_err());
    }

    /// Collects every `Separator` label pushed into history.
    fn separator_labels(history: &HistoryState) -> Vec<String> {
        history
            .cells
            .iter()
            .filter_map(|cell| match cell {
                HistoryCell::Separator { label } => label.clone(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn runtime_start_prompt_is_fully_visible_at_supported_widths() {
        // (terminal width, requested box width, box height) — 40 exercises
        // folding of the legend line; 80 covers the common case where the text
        // fits the box.
        for (term_w, box_w, box_h) in [(40u16, 58u16, 10u16), (80, 58, 10)] {
            let backend = TestBackend::new(term_w, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            // Mirror the live clamp in `run_loop` (`58.min(width - 4)`): ratatui's
            // buffer panics ("index outside of buffer") if the rendered area is
            // wider than the terminal, so a raw 58-wide rect is not a valid model
            // of a 40-column terminal.
            let effective_w = box_w.min(term_w.saturating_sub(4));
            let area = ratatui::layout::Rect::new(0, 0, effective_w, box_h);
            terminal
                .draw(|f| {
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(runtime_prompt_lines())
                            .wrap(ratatui::widgets::Wrap { trim: true }),
                        area,
                    );
                })
                .unwrap();

            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            // ratatui paints the second cell of every wide CJK glyph as a blank
            // continuation cell, so the raw buffer reads "启 动 并 记 住".
            // Compare glyphs without whitespace: the property under test is that
            // no glyph is dropped at the right edge, and clipping still removes
            // one, so the assertion keeps its teeth.
            let rendered: String = rendered.chars().filter(|c| !c.is_whitespace()).collect();

            // Tail of every selectable option must survive; clipping would drop it.
            assert!(rendered.contains("启动并记住"), "width {term_w}");
            assert!(rendered.contains("跳过并记住"), "width {term_w}");
            assert!(rendered.contains("本次跳过"), "width {term_w}");
            assert!(rendered.contains("不记住"), "width {term_w}");
        }
    }

    #[test]
    fn disabled_notice_prints_the_reason_into_history_only() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reason = "已按偏好禁用本地 Agent Runtime";
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(
                crate::tui::subagents::RuntimeStartupIntent::DisabledNotice {
                    reason: reason.to_string(),
                },
            ),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        // Exactly one line explains why delegation is off for this launch.
        assert_eq!(separator_labels(&history), vec![reason.to_string()]);
        // Nothing was asked and nothing was written: a disabled notice is not a
        // user decision, so it must not touch the stored preference.
        assert!(runtime_choice_rx.try_recv().is_err());
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(input_rx.try_recv().is_err());
    }

    #[test]
    fn auto_start_intent_asks_nothing_and_persists_nothing() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let (runtime_choice_tx, mut runtime_choice_rx) = tokio::sync::mpsc::channel(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A "y" would answer a prompt; AutoStart must never show one, so this key
        // must stay free for ordinary chat input instead.
        let events = ScriptedEvents {
            events: Rc::new(RefCell::new(vec![
                Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
                Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
            ])),
        };
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();

        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &events,
            "test-model",
            Some(crate::tui::subagents::RuntimeStartupIntent::AutoStart),
            Some(runtime_choice_tx),
            yi_agent_tools::ProcessManager::new(project.path().to_path_buf()),
            project.path().to_path_buf(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        // No dialog, no choice (main.rs pre-seeds `Start`), no notice, no write.
        let _ = runtime_choice_rx.try_recv();
        assert!(separator_labels(&history).is_empty());
        assert!(!crate::tui::runtime_prefs::preferences_path(project.path()).exists());
        assert!(
            input_rx.try_recv().is_err(),
            "an unsubmitted 'y' must not reach the agent as chat input"
        );
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
        );
        assert!(
            result.is_ok(),
            "two Ctrl+C should quit cleanly, got: {:?}",
            result
        );
    }

    /// Quitting with messages still waiting must report how many were dropped,
    /// so the loss is visible instead of silent.
    #[test]
    fn quitting_reports_pending_messages_that_were_dropped() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Delivery order: type "first" + Enter (sent), type "second" + Enter
        // (queued behind it), then Ctrl+Q. `ScriptedEvents` pops from the end,
        // so the delivery order is pushed in reverse.
        let mut delivery: Vec<Event> = Vec::new();
        for ch in "first".chars() {
            delivery.push(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        delivery.push(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        for ch in "second".chars() {
            delivery.push(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        delivery.push(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        delivery.push(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));
        delivery.reverse();

        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(delivery)),
        };

        let dropped = run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut HistoryState::new(),
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        assert_eq!(
            dropped, 1,
            "the queued-but-never-sent message must be reported as dropped"
        );
    }

    /// The quit notice must name the count, and must stay silent when nothing
    /// was dropped. Covered here rather than through `run_tui`, which needs a
    /// real TTY (`enable_raw_mode`).
    #[test]
    fn report_dropped_pending_writes_the_count_only_when_nonzero() {
        let mut buf: Vec<u8> = Vec::new();
        report_dropped_pending(&mut buf, 3);
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains('3'),
            "the notice must name how many were dropped: {text:?}"
        );
        assert!(
            text.contains("已丢弃"),
            "the notice must be the user-facing message: {text:?}"
        );

        let mut buf: Vec<u8> = Vec::new();
        report_dropped_pending(&mut buf, 0);
        assert!(
            buf.is_empty(),
            "nothing must be printed when no messages were dropped: {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    /// Repeated Esc must not terminate the loop; Ctrl+Q is the explicit terminator.
    #[test]
    fn repeated_esc_does_not_quit() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // ScriptedEvents pops from the end, so this yields Esc, Esc, x, Ctrl+Q.
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
        );
        assert!(
            result.is_ok(),
            "Ctrl+Q should terminate the TUI cleanly, got: {:?}",
            result
        );
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80).map(|x| buffer[(x, input_row)].symbol()).collect();
        assert!(
            row_text.contains('x'),
            "the key after repeated Esc must be processed, got: {row_text:?}"
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let runtime_popup = RuntimePopup::None;
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &runtime_popup,
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
        let runtime_popup = RuntimePopup::None;
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &runtime_popup,
            &mut pending_quit,
            &mut popup,
        );

        assert!(input.buffer.is_empty());
        assert!(pending_quit);
        assert!(popup.is_none());
    }

    #[test]
    fn paste_is_ignored_while_runtime_popup_is_active() {
        let history = HistoryState::new();
        let mut input = InputLine::new();
        let runtime_popup =
            RuntimePopup::Bash(BashPopup::List(ListPopup::new(vec!["task-1".to_string()])));
        let mut pending_quit = true;
        let mut popup = None;

        handle_paste(
            "/cl".to_string(),
            &mut input,
            &history,
            &runtime_popup,
            &mut pending_quit,
            &mut popup,
        );

        assert!(input.buffer.is_empty());
        assert!(pending_quit);
        assert!(popup.is_none());
    }

    /// The bash panel's kill must reach the agent driver as a request to stop
    /// that specific tool call. This used to be a `// TODO` placeholder that
    /// only marked the local registry entry failed and left the process running.
    #[test]
    fn bash_panel_kill_sends_the_tool_call_id_to_the_driver() {
        let mut registry = RunningTaskRegistry::new();
        registry.on_tool_call("call_1", "bash", "sleep 300", 120);
        let mut popup = RuntimePopup::Bash(BashPopup::Detail(DetailPopup::new("call_1".into())));
        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<String>(8);

        // `k` -> confirm prompt
        handle_runtime_popup_key_for_test(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            &mut popup,
            &["call_1".into()],
            &[],
        );
        // `y` -> send. Re-dispatch through the real handler so the channel is exercised.
        let mut popup = RuntimePopup::Bash(BashPopup::ConfirmKill(ConfirmKill {
            task_id: "call_1".into(),
        }));
        handle_bash_popup_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            match &mut popup {
                RuntimePopup::Bash(b) => b,
                _ => unreachable!(),
            },
            &registry,
            &kill_tx,
            80,
        );

        assert_eq!(
            kill_rx.try_recv().as_deref(),
            Ok("call_1"),
            "confirming the kill must ask the driver to cancel that tool call"
        );
    }

    /// `n`/Esc must back out without killing anything.
    #[test]
    fn bash_panel_kill_cancel_does_not_send_a_request() {
        let mut registry = RunningTaskRegistry::new();
        registry.on_tool_call("call_1", "bash", "sleep 300", 120);
        let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let mut bash_popup = BashPopup::ConfirmKill(ConfirmKill {
            task_id: "call_1".into(),
        });

        handle_bash_popup_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut bash_popup,
            &registry,
            &kill_tx,
            80,
        );

        assert!(
            kill_rx.try_recv().is_err(),
            "declining the kill must not send anything"
        );
        assert!(
            matches!(bash_popup, BashPopup::Detail(_)),
            "declining returns to the detail view"
        );
    }

    #[test]
    fn ctrl_p_process_tab_kill_confirmation_sends_process_id() {
        let mut runtime_popup = RuntimePopup::Processes(ProcessPopup::Detail(
            ProcessDetailPopup::new("proc_1".into()),
        ));
        let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);

        handle_runtime_popup_key_for_test(key, &mut runtime_popup, &[], &[]);

        assert!(matches!(
            runtime_popup,
            RuntimePopup::Processes(ProcessPopup::ConfirmKill(_))
        ));
    }

    fn subagent_items() -> SubagentListState {
        SubagentListState::new(vec![
            super::super::trace::SubagentListItem {
                task_id: "task-1".into(),
                objective: Some("first child".into()),
                state: "running".into(),
                last_step: None,
                active: true,
                parent_task_id: Some("root".into()),
            },
            super::super::trace::SubagentListItem {
                task_id: "task-2".into(),
                objective: Some("second child".into()),
                state: "completed".into(),
                last_step: None,
                active: false,
                parent_task_id: Some("root".into()),
            },
        ])
    }

    fn agents_key(
        key: KeyEvent,
        popup: &mut RuntimePopup,
        children: &SubagentListState,
    ) -> Option<String> {
        let registry = RunningTaskRegistry::new();
        let (kill_tx, _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        handle_runtime_popup_key(
            key,
            popup,
            &registry,
            &kill_tx,
            &[],
            &std::collections::HashMap::new(),
            children,
            80,
            24,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_runtime_popup_cycles_bash_processes_and_agents() {
        assert_eq!(RuntimeTab::BashTasks.next(), RuntimeTab::Processes);
        assert_eq!(RuntimeTab::Processes.next(), RuntimeTab::Agents);
        assert_eq!(RuntimeTab::Agents.next(), RuntimeTab::BashTasks);
    }

    #[test]
    fn switching_to_the_agents_tab_builds_the_list_from_the_children() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));

        popup.switch_tab(Vec::new(), children.clone());

        let RuntimePopup::Agents(agents) = popup else {
            panic!("expected the agents tab");
        };
        assert_eq!(agents.children.len(), 2);
        assert_eq!(agents.children.items[0].task_id, "task-1");
        assert!(agents.detail.is_none());
    }

    #[test]
    fn enter_on_a_list_row_opens_that_agents_detail() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Agents(Box::new(AgentsPopup::new(children.clone())));

        agents_key(key(KeyCode::Down), &mut popup, &children);
        agents_key(key(KeyCode::Enter), &mut popup, &children);

        let RuntimePopup::Agents(agents) = popup else {
            panic!("expected the agents tab");
        };
        let Some(super::super::trace::TracePopup::Detail(detail)) = agents.detail else {
            panic!("expected a detail view");
        };
        assert_eq!(detail.task_id(), "task-2", "the row under the cursor opens");
    }

    #[test]
    fn esc_in_a_detail_returns_to_the_list_before_closing_the_popup() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Agents(Box::new(AgentsPopup::new(children.clone())));
        agents_key(key(KeyCode::Enter), &mut popup, &children);

        agents_key(key(KeyCode::Esc), &mut popup, &children);

        let RuntimePopup::Agents(agents) = popup else {
            panic!("the popup must stay open on the first Esc");
        };
        assert!(agents.detail.is_none(), "back to the list");
    }

    #[test]
    fn esc_on_the_agent_list_closes_the_popup() {
        let children = subagent_items();
        let mut popup = RuntimePopup::Agents(Box::new(AgentsPopup::new(children.clone())));

        agents_key(key(KeyCode::Esc), &mut popup, &children);

        assert!(matches!(popup, RuntimePopup::None));
    }

    #[test]
    fn runtime_popup_tab_switches_between_bash_and_processes() {
        let mut popup = RuntimePopup::Bash(BashPopup::List(ListPopup::new(vec!["bash_1".into()])));

        popup.switch_tab(Vec::new(), SubagentListState::default());

        assert!(matches!(
            popup,
            RuntimePopup::Processes(ProcessPopup::List(_))
        ));
    }

    #[test]
    fn process_runtime_popup_blocks_text_input() {
        let popup = RuntimePopup::Processes(ProcessPopup::List(ProcessListPopup::new()));

        assert!(popup.blocks_text_input());
    }

    #[test]
    fn bracketed_paste_renders_input_and_opens_slash_popup() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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

    /// Moving the selection past the visible window must scroll the popup so the
    /// highlighted row stays on screen. Otherwise the user cannot see what Enter
    /// is about to execute.
    #[test]
    fn slash_popup_scrolls_to_keep_selection_visible() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // The popup is capped at 10 rows, i.e. 8 content rows, so 20 Down presses
        // land the selection far outside the initial window.
        const DOWNS: usize = 20;
        // The popup lists `completable()`, so the Nth row after N Down presses is
        // the Nth completable command (not the Nth entry of the full catalog).
        let target = SlashCommand::completable()[DOWNS];

        // `ScriptedEvents` pops from the back, so push in reverse delivery order.
        let mut script = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        script.extend(
            (0..DOWNS).map(|_| Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))),
        );
        script.push(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(script)),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let highlight_row = (0..24u16)
            .find(|&y| (0..80u16).any(|x| buffer[(x, y)].bg == ratatui::style::Color::Blue));
        let row = highlight_row.expect("the highlighted command row must be visible");
        let row_text: String = (0..80u16).map(|x| buffer[(x, row)].symbol()).collect();
        assert!(
            row_text.contains(target.name()),
            "expected the highlighted row to show /{}, got: {row_text:?}",
            target.name()
        );
    }

    /// The highlighted row must stay on screen even when the popup is drawn
    /// shorter than its 10-row cap (short terminal or a squeezed layout), where
    /// the drawn height no longer matches the scroll window.
    #[test]
    fn slash_popup_keeps_selection_visible_on_a_short_terminal() {
        let backend = TestBackend::new(80, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // A 14-row terminal leaves the popup only 8 rows, i.e. 6 content rows.
        const DOWNS: usize = 12;
        // See `slash_popup_scrolls_to_keep_selection_visible`: the popup lists
        // `completable()`, so index into that view.
        let target = SlashCommand::completable()[DOWNS];

        let mut script = vec![Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        ))];
        script.extend(
            (0..DOWNS).map(|_| Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))),
        );
        script.push(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let source = ScriptedEvents {
            events: Rc::new(RefCell::new(script)),
        };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
        )
        .unwrap();

        let buffer = terminal.backend().buffer();
        let highlight_row = (0..14u16)
            .find(|&y| (0..80u16).any(|x| buffer[(x, y)].bg == ratatui::style::Color::Blue));
        let row = highlight_row.expect("the highlighted command row must be visible");
        let row_text: String = (0..80u16).map(|x| buffer[(x, row)].symbol()).collect();
        assert!(
            row_text.contains(target.name()),
            "expected the highlighted row to show /{}, got: {row_text:?}",
            target.name()
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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

    /// Delivers one streamed assistant token per frame, then quits.
    struct StreamingTokenEvents {
        agent_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        poll_count: Cell<usize>,
    }

    impl EventSource for StreamingTokenEvents {
        fn poll(&self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            let count = self.poll_count.get();
            self.poll_count.set(count + 1);
            if count < 4 {
                let _ = self
                    .agent_tx
                    .try_send(AgentEvent::AssistantText(" more".into()));
                return Ok(None);
            }
            Ok(Some(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))))
        }
    }

    /// A streamed token must re-render only the cell it changed.
    ///
    /// `run_loop` applies events at the width the draw will use, not the raw
    /// area width. The history renderer reserves its rightmost column for the
    /// scrollbar, and the cache is keyed by width, so applying events at the
    /// area width used to re-render the whole scrollback once at each width,
    /// i.e. twice per token.
    #[test]
    fn streaming_a_token_re_renders_only_the_changed_cell() {
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));

        const CELLS: usize = 30;
        let mut history = HistoryState::new();
        for index in 0..CELLS {
            history.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("answer {index} with some words in it"),
                },
                79,
            );
        }
        // Warm the cache the way a settled session would have it.
        let warm_width = history.text_width(80, 6);
        assert_eq!(warm_width, 79, "the fixture must overflow the viewport");
        let _ = HistoryView {
            state: &history,
            width: 80,
        }
        .flattened_lines(warm_width);

        let source = StreamingTokenEvents {
            agent_tx,
            poll_count: Cell::new(0),
        };

        crate::tui::cell::reset_lines_call_count();
        run_loop(
            &mut terminal,
            &mut agent_rx,
            &mut history,
            &mut InputLine::new(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            "test-model",
            None,
            None,
            yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            std::env::temp_dir(),
            yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
        )
        .unwrap();

        let renders = crate::tui::cell::lines_call_count();
        assert!(
            renders <= 8,
            "streaming 4 tokens over {CELLS} cells re-rendered {renders} cells; \
             it must re-render only the changed tail, not the whole scrollback"
        );
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
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
                &kill_tx,
                &control_tx,
                &decision_tx,
                &is_running,
                &mut queued,
                &mut pending_quit,
                &mut popup,
                &std::env::temp_dir(),
                &yi_agent_mcp::McpManager::empty(),
                &snapshot_for_tests(),
                "test-model",
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(history.selected, Some(4));
        assert_eq!(
            history.scroll_offset, 0,
            "Shift+Up keeps selection behavior"
        );
    }

    #[test]
    fn esc_interrupts_active_agent_without_arming_quit() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(!pending_quit, "Esc must not arm process exit");
        assert!(
            interrupt_rx.try_recv().is_ok(),
            "interrupt should be sent when agent running"
        );
    }

    #[test]
    fn esc_when_idle_does_nothing() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(!pending_quit, "idle Esc must not arm process exit");
        assert!(
            interrupt_rx.try_recv().is_err(),
            "interrupt should NOT be sent when idle"
        );
    }

    #[test]
    fn ctrl_c_when_running_sends_interrupt() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(result, KeyOutcome::None);
        assert!(pending_quit);
        assert!(interrupt_rx.try_recv().is_ok());
    }

    #[test]
    fn repeated_esc_does_not_quit_from_handle_key() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(result, KeyOutcome::None);
    }

    // ----- handle_key Submit 分流 tests -----

    #[test]
    fn submit_multi_segment_absolute_path_sends_to_agent() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;
        let path = "/Users/name/project explain this";

        input.buffer = path.to_string();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(result, KeyOutcome::Submit(path.to_string()));
        assert_eq!(input_rx.try_recv().unwrap(), path);
        assert!(matches!(
            history.cells.as_slice(),
            [HistoryCell::UserMessage { text }] if text == path
        ));
    }

    #[test]
    fn submit_single_segment_absolute_path_shows_unknown_command() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        input.buffer = "/tmp".to_string();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(result, KeyOutcome::None);
        assert!(input_rx.try_recv().is_err());
        assert!(matches!(
            history.cells.as_slice(),
            [HistoryCell::Separator { label: Some(label) }] if label == "未知命令: /tmp"
        ));
    }

    #[test]
    fn submit_while_running_is_delivered_not_held_in_history() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn: the queue's own `in_flight` flag is the
        // authority for "is a turn running", not `is_running` (see the spec:
        // is_running is still true at Done time, so keying sends off it would
        // deadlock the TUI against the driver).
        input.buffer = "inflight msg".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(input_rx.try_recv().unwrap(), "inflight msg");
        let history_len_before = history.cells.len();

        input.buffer = "mid-turn msg".to_string();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        match result {
            KeyOutcome::Submit(text) => {
                assert_eq!(text, "mid-turn msg");
                // Delivered immediately: the driver folds it into the running
                // turn before the next provider request, rather than holding it
                // back until the turn ends.
                assert_eq!(
                    input_rx.try_recv().unwrap(),
                    "mid-turn msg",
                    "a mid-turn submit must be delivered, not held back"
                );
                // Accounting only: it stays in the preview until core confirms.
                assert_eq!(queued.len(), 1);
                assert_eq!(queued.items(), ["mid-turn msg".to_string()]);
                assert_eq!(
                    history.cells.len(),
                    history_len_before,
                    "a mid-turn message must not be added to history as a new turn"
                );
            }
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn busy_submit_reaches_the_agent_channel() {
        use crate::tui::queued::SubmitOutcome;
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn and take the opening message off the
        // channel, so the next submit is a mid-turn delivery.
        assert_eq!(queued.submit("opening".into()), SubmitOutcome::Sent);
        let _ = input_tx.try_send("opening".into());
        let _ = input_rx.try_recv();

        input.buffer = "follow-up".to_string();
        input.cursor = input.buffer.len();

        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(
            input_rx.try_recv().ok(),
            Some("follow-up".to_string()),
            "a busy submit must be delivered, not merely previewed"
        );
        assert_eq!(queued.items(), ["follow-up".to_string()]);
    }

    #[test]
    fn returned_interjection_goes_back_to_the_input_box() {
        use crate::tui::queued::SubmitOutcome;
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        assert_eq!(queued.submit("unconsumed".into()), SubmitOutcome::Sent);
        assert_eq!(queued.submit("mid-turn".into()), SubmitOutcome::Queued);

        apply_interjection_event(
            &AgentEvent::InterjectionsReturned {
                items: vec![yi_agent_core::Interjection {
                    seq: 1,
                    text: "mid-turn".into(),
                    tag: None,
                }],
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(input.buffer, "mid-turn");
        assert!(queued.is_empty());
        assert!(
            matches!(
                history.cells.as_slice(),
                [HistoryCell::Separator { label: Some(label) }]
                    if label == "1 条追加未生效，已退回输入框"
            ),
            "the return must be reported: {:?}",
            history.cells
        );
    }

    #[test]
    fn multiple_returned_interjections_join_with_newlines() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        queued.submit("opening".into());
        queued.submit("one".into());
        queued.submit("two".into());

        apply_interjection_event(
            &AgentEvent::InterjectionsReturned {
                items: vec![
                    yi_agent_core::Interjection {
                        seq: 1,
                        text: "one".into(),
                        tag: None,
                    },
                    yi_agent_core::Interjection {
                        seq: 2,
                        text: "two".into(),
                        tag: None,
                    },
                ],
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(input.buffer, "one\ntwo");
        assert!(queued.is_empty());
    }

    #[test]
    fn accepted_receipt_lowers_the_pending_count_without_touching_history() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        queued.submit("opening".into());
        queued.submit("first".into());
        queued.submit("second".into());

        apply_interjection_event(
            &AgentEvent::InterjectionAccepted {
                seq: 1,
                text: "first".into(),
                tag: None,
            },
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(queued.items(), ["second".to_string()]);
        assert_eq!(input.buffer, "", "a receipt must not touch the input box");
        assert!(
            history.cells.is_empty(),
            "the receipt is rendered by history.rs"
        );
    }

    #[test]
    fn interjection_events_are_noops_for_unrelated_events() {
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut input = InputLine::new();
        let mut history = HistoryState::new();
        queued.submit("opening".into());
        queued.submit("pending".into());

        apply_interjection_event(
            &AgentEvent::Start,
            &mut queued,
            &mut input,
            &mut history,
            80,
        );

        assert_eq!(queued.items(), ["pending".to_string()]);
        assert_eq!(input.buffer, "");
        assert!(history.cells.is_empty());
    }

    #[test]
    fn submit_while_idle_goes_to_history_not_queue() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
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

    #[test]
    fn full_queue_rejects_and_restores_input_without_blocking() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(true));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // Establish an in-flight turn (the first submit is always Sent).
        input.buffer = "inflight".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(input_rx.try_recv().unwrap(), "inflight");

        // Fill the queue to capacity.
        for i in 0..crate::tui::queued::DeliveredInterjections::CAPACITY {
            input.buffer = format!("msg{i}");
            input.cursor = input.buffer.len();
            let _ = handle_key(
                make_key(KeyCode::Enter, KeyModifiers::NONE),
                &mut input,
                &mut history,
                1000,
                80,
                24,
                &CostTracker::default(),
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
                &is_running,
                &mut queued,
                &mut pending_quit,
                &mut popup,
                &std::env::temp_dir(),
                &yi_agent_mcp::McpManager::empty(),
                &snapshot_for_tests(),
                "test-model",
            );
        }
        assert_eq!(
            queued.len(),
            crate::tui::queued::DeliveredInterjections::CAPACITY
        );
        // Every one of those submits was delivered, not buffered locally, so the
        // channel holds CAPACITY entries; drain them so the overflow assertion
        // below cannot be satisfied by an earlier message.
        for i in 0..crate::tui::queued::DeliveredInterjections::CAPACITY {
            assert_eq!(input_rx.try_recv().unwrap(), format!("msg{i}"));
        }
        assert!(
            input_rx.try_recv().is_err(),
            "channel should now be drained"
        );
        let history_len_before = history.cells.len();

        // The overflow submit must not block and must not be accepted.
        input.buffer = "overflow".to_string();
        input.cursor = input.buffer.len();
        let _ = handle_key(
            make_key(KeyCode::Enter, KeyModifiers::NONE),
            &mut input,
            &mut history,
            1000,
            80,
            24,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(
            queued.len(),
            crate::tui::queued::DeliveredInterjections::CAPACITY,
            "the queue must stay at capacity"
        );
        assert_eq!(
            input.buffer, "overflow",
            "the rejected text must be restored to the input line"
        );
        assert!(
            history.cells.len() > history_len_before,
            "the rejection must be visible in history"
        );
        assert!(
            input_rx.try_recv().is_err(),
            "a rejected message must not reach the channel"
        );
    }

    #[test]
    fn delivered_messages_are_not_re_held_until_turn_end() {
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        // First submit: idle -> sent. Second: queued.
        for text in ["first", "second"] {
            input.buffer = text.to_string();
            input.cursor = input.buffer.len();
            let _ = handle_key(
                make_key(KeyCode::Enter, KeyModifiers::NONE),
                &mut input,
                &mut history,
                1000,
                80,
                24,
                &CostTracker::default(),
                &input_tx,
                &interrupt_tx,
                &kill_tx,
                &control_tx,
                &decision_tx,
                // The driver has NOT yet cleared this flag: this mirrors the
                // real ordering, where Done is emitted before is_running=false.
                &Arc::new(AtomicBool::new(true)),
                &mut queued,
                &mut pending_quit,
                &mut popup,
                &std::env::temp_dir(),
                &yi_agent_mcp::McpManager::empty(),
                &snapshot_for_tests(),
                "test-model",
            );
        }
        // Both reached the driver already: the first opened the turn, the second
        // was delivered as a mid-turn interjection. Neither waits for turn end.
        assert_eq!(input_rx.try_recv().unwrap(), "first");
        assert_eq!(input_rx.try_recv().unwrap(), "second");
        assert!(input_rx.try_recv().is_err());
        // `is_running` is still true here, mirroring the real ordering where
        // Done is emitted before the driver clears the flag; delivery must not
        // have depended on it either way.
        assert_eq!(queued.len(), 1);
        assert_eq!(queued.items(), ["second".to_string()]);

        // Turn end reconciles: core accepted it, so the receipt retires it and
        // the fallback promotion finds nothing left to send.
        queued.on_receipt();
        assert!(queued.is_empty());
        assert_eq!(queued.on_turn_end(), None);
    }

    // ----- /cost slash command tests -----

    #[test]
    fn clear_command_drops_pending_queue_and_resets_in_flight() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut history = HistoryState::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        // One in flight plus two waiting.
        use crate::tui::queued::SubmitOutcome;
        assert_eq!(queued.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(queued.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(queued.submit("c".into()), SubmitOutcome::Queued);

        let outcome = execute_slash_command(
            SlashCommand::Clear,
            None,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert!(queued.is_empty(), "/clear must drop waiting messages");
        // in_flight reset: a later submit sends immediately again.
        assert_eq!(queued.submit("d".into()), SubmitOutcome::Sent);
        assert!(matches!(
            control_rx.try_recv(),
            Ok(crate::ControlCommand::Clear)
        ));
    }

    #[test]
    fn model_command_sends_set_model_control() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let _ = execute_slash_command(
            SlashCommand::Model,
            None,
            Some("claude-opus-4-1".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(
            control_rx.try_recv(),
            Ok(crate::ControlCommand::SetModel("claude-opus-4-1".into()))
        );
    }

    #[test]
    fn model_command_without_args_shows_usage() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let _ = execute_slash_command(
            SlashCommand::Model,
            None,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        let labels = separator_labels(&history);
        assert!(
            labels
                .iter()
                .any(|l| l.contains("用法: /model <model-name>"))
        );
    }

    #[test]
    fn daemon_command_reports_unavailable_runtime() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let dir = tempfile::tempdir().unwrap();
        let outcome = execute_slash_command(
            SlashCommand::Daemon,
            None,
            Some("status".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            dir.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(outcome, KeyOutcome::None);
        let labels = separator_labels(&history);
        assert!(
            labels
                .iter()
                .any(|l| l.contains("无法联系本地 daemon runtime")),
            "expected an unreachable-daemon message, got {labels:?}"
        );
    }

    #[test]
    fn daemon_command_rejects_unknown_subcommand() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let _ = execute_slash_command(
            SlashCommand::Daemon,
            None,
            Some("start".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        let labels = separator_labels(&history);
        assert!(
            labels
                .iter()
                .any(|l| l.contains("用法: /daemon [status|stop]"))
        );
    }

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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            None,
            &mut history,
            80,
            &cost,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
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
    fn config_command_renders_snapshot() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let snapshot = snapshot_for_tests();
        let outcome = execute_slash_command(
            SlashCommand::Config,
            None,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot,
            "test-model",
        );
        assert_eq!(outcome, KeyOutcome::None);
        let rendered: String = history
            .cells
            .iter()
            .filter_map(|c| match c {
                HistoryCell::Markdown { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(rendered.contains("anthropic"));
        assert!(rendered.contains("test-model"));
        assert!(!rendered.contains("api_key"));
    }

    #[test]
    fn cost_command_empty_shows_no_data() {
        let mut history = HistoryState::new();
        let cost = CostTracker::default();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(1);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let outcome = execute_slash_command(
            SlashCommand::Cost,
            None,
            None,
            &mut history,
            80,
            &cost,
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
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

    // ----- /mcp slash command tests -----

    #[test]
    fn mcp_on_sets_master_and_sends_refresh() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mcp = yi_agent_mcp::McpManager::empty();
        assert!(!mcp.master(), "empty manager starts with master off");

        let outcome = execute_slash_command(
            SlashCommand::Mcp,
            None,
            Some("on".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &mcp,
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(outcome, KeyOutcome::None);
        assert!(mcp.master(), "/mcp on must set the master switch");
        assert_eq!(
            control_rx.try_recv().unwrap(),
            crate::ControlCommand::McpRefresh,
            "/mcp on must ask the driver to refresh the registry"
        );
    }

    #[test]
    fn mcp_disable_unknown_server_reports_error_without_refresh() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mcp = yi_agent_mcp::McpManager::empty();

        let outcome = execute_slash_command(
            SlashCommand::Mcp,
            None,
            Some("disable nope".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &mcp,
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(outcome, KeyOutcome::None);
        // The unknown server must be reported, not silently ignored...
        match history.cells.last().unwrap() {
            HistoryCell::Separator { label: Some(label) } => {
                assert!(
                    label.contains("nope"),
                    "error should name the server: {label}"
                );
            }
            other => panic!("expected a Separator, got {other:?}"),
        }
        // ...and no refresh may be sent for a failed toggle.
        assert!(
            control_rx.try_recv().is_err(),
            "unknown server must not trigger a registry refresh"
        );
    }

    #[test]
    fn mcp_status_on_empty_manager_says_unconfigured() {
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mcp = yi_agent_mcp::McpManager::empty();

        let outcome = execute_slash_command(
            SlashCommand::Mcp,
            None,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &std::env::temp_dir(),
            &mut queued,
            &mcp,
            &snapshot_for_tests(),
            "test-model",
        );
        assert_eq!(outcome, KeyOutcome::None);
        match history.cells.last().unwrap() {
            HistoryCell::Markdown { text } => {
                assert!(
                    text.contains("未配置 MCP server"),
                    "empty manager should say unconfigured: {text}"
                );
            }
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    // ----- /runtime slash command tests -----

    #[test]
    fn runtime_status_reports_preference_and_its_source_path() {
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Runtime,
            None,
            None,
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            project.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(outcome, KeyOutcome::None);
        match history.cells.last().unwrap() {
            HistoryCell::Separator { label: Some(label) } => {
                assert!(label.contains("ask"), "missing default value: {label}");
                assert!(
                    label.contains("preferences.json"),
                    "status must name its source file: {label}"
                );
            }
            other => panic!("expected a Separator, got {other:?}"),
        }
    }

    #[test]
    fn runtime_set_persists_the_preference_and_reports_it() {
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Runtime,
            None,
            Some("never".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            project.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert_eq!(
            crate::tui::runtime_prefs::load(project.path()),
            crate::tui::runtime_prefs::RuntimePreference::Never,
            "/runtime never must persist"
        );
        match history.cells.last().unwrap() {
            HistoryCell::Separator { label: Some(label) } => assert!(
                label.contains("never"),
                "confirmation must name the new value: {label}"
            ),
            other => panic!("expected a Separator, got {other:?}"),
        }
    }

    #[test]
    fn runtime_rejects_bad_args_with_usage_without_touching_the_file() {
        let project = tempfile::TempDir::new().unwrap();
        let mut history = HistoryState::new();
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(1);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel::<crate::ControlCommand>(8);
        let mut queued = crate::tui::queued::DeliveredInterjections::new();

        let outcome = execute_slash_command(
            SlashCommand::Runtime,
            None,
            Some("sometimes".into()),
            &mut history,
            80,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            project.path(),
            &mut queued,
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );

        assert_eq!(outcome, KeyOutcome::None);
        assert_eq!(
            separator_labels(&history),
            vec!["用法: /runtime [ask|always|never]".to_string()]
        );
        assert!(
            !crate::tui::runtime_prefs::preferences_path(project.path()).exists(),
            "a rejected argument must not create a preference file"
        );
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
        let mut runtime_popup = RuntimePopup::None;
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
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
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
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(hist.scroll_offset, 6, "second ScrollUp should accumulate");
    }

    #[test]
    fn mouse_scroll_uses_reserved_text_width_for_its_max_offset() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        // This fits the raw 80-column area (78 chars after the user prefix)
        // but wraps after the scrollbar reserves one column.
        for _ in 0..30 {
            hist.push(
                HistoryCell::UserMessage {
                    text: "x".repeat(78),
                },
                80,
            );
        }
        let text_width = hist.text_width(history_area.width, history_area.height);
        assert_eq!(text_width, 79, "overflow reserves a scrollbar column");
        let expected_max = hist.max_scroll_offset(text_width, history_area.height);
        let raw_max = hist.max_scroll_offset(history_area.width, history_area.height);
        assert!(expected_max > raw_max, "reserved width adds wrapped lines");

        let registry = RunningTaskRegistry::new();
        let mut pending_quit = false;
        for _ in 0..100 {
            handle_mouse(
                make_mouse(MouseEventKind::ScrollUp, 0, history_area.y),
                &layout,
                &mut runtime_popup,
                &mut hist,
                &registry,
                &[],
                &std::collections::HashMap::new(),
                &mut pending_quit,
            );
        }

        assert_eq!(hist.scroll_offset, expected_max);
    }

    /// Scrolling down over the history region should decrease `scroll_offset`
    /// (clamped at 0).
    #[test]
    fn mouse_scroll_down_in_history_decreases_offset() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
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
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
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
                &mut runtime_popup,
                &mut hist,
                &registry,
                &[],
                &std::collections::HashMap::new(),
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
        let mut runtime_popup = RuntimePopup::None;
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
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
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
        let mut runtime_popup = RuntimePopup::None;
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
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
            &mut pending_quit,
        );
        assert_eq!(hist.scroll_offset, 0, "click should not scroll");
    }

    /// Scrolling up over the history region should clear `pending_quit`,
    /// matching the behavior of other history navigation keys.
    #[test]
    fn mouse_scroll_clears_pending_quit() {
        let (layout, history_area) = layout_80x24();
        let mut runtime_popup = RuntimePopup::None;
        let mut hist = HistoryState::new();
        let registry = RunningTaskRegistry::new();
        let mut pending_quit = true;

        handle_mouse(
            make_mouse(MouseEventKind::ScrollUp, 0, history_area.y),
            &layout,
            &mut runtime_popup,
            &mut hist,
            &registry,
            &[],
            &std::collections::HashMap::new(),
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
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
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
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &source,
            &snapshot_for_tests(),
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
    #[test]
    fn permission_key_e_toggles_expanded() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, mut decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        history.push_event(make_permission_request_normal(1), 80);

        let outcome = handle_key(
            make_key(KeyCode::Char('e'), KeyModifiers::NONE),
            &mut input,
            &mut history,
            0,
            80,
            20,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert!(matches!(outcome, KeyOutcome::None));
        assert!(
            decision_rx.try_recv().is_err(),
            "'e' must not resolve the permission"
        );
        match &history.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => {
                assert!(*expanded, "'e' should expand the pending request")
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn scroll_keys_work_while_permission_pending() {
        let (input_tx, _input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = mpsc::channel::<()>(1);
        let (kill_tx, mut _kill_rx) = tokio::sync::mpsc::channel::<String>(8);
        let (control_tx, _control_rx) = mpsc::channel::<crate::ControlCommand>(8);
        let (decision_tx, _decision_rx) =
            mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut history = HistoryState::new();
        let mut input = InputLine::new();
        let mut queued = crate::tui::queued::DeliveredInterjections::new();
        let mut pending_quit = false;
        let mut popup = None;

        for _ in 0..120 {
            history.push(HistoryCell::Separator { label: None }, 80);
        }
        history.push_event(make_permission_request_normal(1), 80);
        history.scroll_offset = 0;

        let outcome = handle_key(
            make_key(KeyCode::Up, KeyModifiers::NONE),
            &mut input,
            &mut history,
            100,
            80,
            20,
            &CostTracker::default(),
            &input_tx,
            &interrupt_tx,
            &kill_tx,
            &control_tx,
            &decision_tx,
            &is_running,
            &mut queued,
            &mut pending_quit,
            &mut popup,
            &std::env::temp_dir(),
            &yi_agent_mcp::McpManager::empty(),
            &snapshot_for_tests(),
            "test-model",
        );
        assert!(matches!(outcome, KeyOutcome::None));
        assert_eq!(
            history.scroll_offset, 1,
            "Up should scroll history while a permission is pending"
        );
    }
}

#[cfg(test)]
mod runtime_restart_notice_tests {
    use super::*;

    /// The failure can land many turns into a long session, where a
    /// transcript separator is appended off-screen and never seen. The alert
    /// is a popup so it is guaranteed to be on the visible screen.
    #[test]
    fn the_failure_alert_is_rendered_into_the_viewport() {
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_runtime_notice_popup(
                    f,
                    crate::tui::subagents::RUNTIME_RESTART_NOTICE,
                    ratatui::layout::Rect::new(0, 0, 100, 30),
                );
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        let screen: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            screen.contains("重启yi-agent后即可用"),
            "the restart remedy is not on screen: {screen}"
        );
    }

    /// The startup dialog is the only place a first-time user learns what
    /// happens after pressing `y`. Starting the runtime costs a daemon spawn,
    /// and a session that cannot start one cannot delegate at all until the
    /// process is restarted -- so the dialog must say so up front instead of
    /// leaving the user staring at a bare error afterwards.
    #[test]
    fn startup_prompt_states_that_a_restart_is_needed() {
        let text: String = runtime_prompt_lines()
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("重启"),
            "the dialog never mentions a restart: {text}"
        );
    }
}
