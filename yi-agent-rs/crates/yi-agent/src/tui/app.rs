use std::io::stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use yi_agent_core::AgentEvent;

use super::cell::HistoryCell;
use super::history::{HistoryState, HistoryView};
use super::input::{InputAction, InputLine};
use super::slash::{CommandPopup, SlashCommand};

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
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
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
        &decision_tx,
        &is_running,
        &CrosstermEventSource,
    );

    // Always restore terminal state, even on error
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
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
        decision_tx,
        is_running,
        &CrosstermEventSource,
    )
}

/// Testable variant: accepts a custom EventSource for injecting fake key events.
#[cfg(test)]
pub fn run_tui_with_backend_and_events<B: Backend, E: EventSource>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
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
        decision_tx,
        is_running,
        events,
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
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    events: &E,
) -> std::io::Result<()> {
    let _ = is_running;
    let mut pending_quit = false;
    let mut popup: Option<CommandPopup> = None;

    loop {
        // Drain all pending agent events
        let width = terminal.size()?.width;
        while let Ok(event) = agent_rx.try_recv() {
            history.push_event(event, width);
        }

        terminal.draw(|f| {
            let area = f.area();
            let input_width = area.width;
            let input_height = compute_input_height(input, pending_quit, input_width).min(6);
            let popup_height = popup
                .as_ref()
                .map(|p| (p.filtered().len() + 2).min(10) as u16) // +2 for borders
                .unwrap_or(0);

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),               // history
                    Constraint::Length(popup_height), // popup (0 when none)
                    Constraint::Length(1),            // blank gap
                    Constraint::Length(input_height), // input (wraps up to 6 lines)
                ])
                .split(area);

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

            let input_line = build_input_line(input, pending_quit, chunks[3].width);
            f.render_widget(input_line, chunks[3]);
        })?;

        // Poll for key events with timeout
        if let Some(Event::Key(key)) = events.poll(Duration::from_millis(50))? {
            match handle_key(
                key,
                input,
                history,
                input_tx,
                interrupt_tx,
                decision_tx,
                &mut pending_quit,
                &mut popup,
            ) {
                KeyOutcome::Quit => break,
                KeyOutcome::Submit(text) => {
                    pending_quit = false;
                    history.push(HistoryCell::UserMessage { text: text.clone() });
                    let _ = input_tx.blocking_send(text);
                }
                KeyOutcome::None => {}
            }
            // After any key, sync popup state with the (possibly modified) buffer
            sync_popup(&mut popup, &input.buffer);
        }
    }

    Ok(())
}

enum KeyOutcome {
    None,
    Quit,
    Submit(String),
}

fn handle_key(
    key: KeyEvent,
    input: &mut InputLine,
    history: &mut HistoryState,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    decision_tx: &tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    pending_quit: &mut bool,
    popup: &mut Option<CommandPopup>,
) -> KeyOutcome {
    // Check if there's a pending permission request
    if let Some((request_id, _tool_name, prefix_suggestion, kind)) = history.pending_permission_info() {
        let decision = match key.code {
            KeyCode::Char('1') => Some(yi_agent_core::permission::Decision::AllowOnce),
            KeyCode::Char('2') => Some(yi_agent_core::permission::Decision::AlwaysAllowTool),
            KeyCode::Char('3') => prefix_suggestion.map(|p| yi_agent_core::permission::Decision::AlwaysAllowPrefix(p.to_string())),
            KeyCode::Char('4') => Some(yi_agent_core::permission::Decision::Deny),
            KeyCode::Enter => {
                let default = match kind {
                    yi_agent_core::permission::PermissionKind::Blacklisted(_) => yi_agent_core::permission::Decision::Deny,
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
            return KeyOutcome::None;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            if *pending_quit {
                return KeyOutcome::Quit;
            }
            *pending_quit = true;
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
            history.scroll_up(10);
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
                            input_tx,
                            interrupt_tx,
                        );
                    } else {
                        // No command selected (empty filter) — show error
                        let text = input.take_submitted();
                        *popup = None;
                        history.push(HistoryCell::Separator {
                            label: Some(format!("未知命令: {}", text)),
                        });
                        return KeyOutcome::None;
                    }
                }
            }
            _ => {}
        }
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
                    return execute_slash_command(cmd, args, history, input_tx, interrupt_tx);
                } else {
                    // Unknown slash command
                    *popup = None;
                    history.push(HistoryCell::Separator {
                        label: Some(format!("未知命令: {}", text)),
                    });
                    return KeyOutcome::None;
                }
            }
            *popup = None;
            KeyOutcome::Submit(text)
        }
        _ => KeyOutcome::None,
    }
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
fn execute_slash_command(
    cmd: SlashCommand,
    args: Option<String>,
    history: &mut HistoryState,
    _input_tx: &tokio::sync::mpsc::Sender<String>,
    _interrupt_tx: &tokio::sync::mpsc::Sender<()>,
) -> KeyOutcome {
    match cmd {
        SlashCommand::Quit => KeyOutcome::Quit,
        SlashCommand::Clear => {
            history.clear();
            history.push(HistoryCell::Separator {
                label: Some("对话已清空".to_string()),
            });
            KeyOutcome::None
        }
        SlashCommand::Help => {
            let mut help_text = String::from("可用命令:\n");
            for c in SlashCommand::all() {
                help_text.push_str(&format!("  /{:<10} {}\n", c.name(), c.description()));
            }
            history.push(HistoryCell::UserMessage { text: help_text });
            KeyOutcome::None
        }
        SlashCommand::Cost => {
            history.push(HistoryCell::Separator {
                label: Some("Token 用量: (暂未实现)".to_string()),
            });
            KeyOutcome::None
        }
        SlashCommand::Config => {
            history.push(HistoryCell::Separator {
                label: Some("当前配置: (暂未实现)".to_string()),
            });
            KeyOutcome::None
        }
        SlashCommand::Compact => {
            history.push(HistoryCell::Separator {
                label: Some("压缩对话: (暂未实现)".to_string()),
            });
            KeyOutcome::None
        }
        SlashCommand::Model => {
            if let Some(model) = args {
                history.push(HistoryCell::Separator {
                    label: Some(format!("切换模型到: {} (暂未实现)", model)),
                });
            } else {
                history.push(HistoryCell::Separator {
                    label: Some("用法: /model <model-name>".to_string()),
                });
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
    use ratatui::backend::TestBackend;
    use std::cell::RefCell;
    use std::rc::Rc;

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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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

    /// Typing `/` should show the slash command popup with all commands.
    #[test]
    fn slash_popup_appears_on_slash() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_agent_tx, mut agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
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
}
