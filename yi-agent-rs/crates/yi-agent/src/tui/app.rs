use std::io::stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use yi_agent_core::AgentEvent;

use super::history::{HistoryState, HistoryView};
use super::input::{InputAction, InputLine};

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
    run_loop(terminal, agent_rx, &mut history, &mut input, input_tx, interrupt_tx, decision_tx, is_running, &CrosstermEventSource)
}

/// Testable variant: accepts a custom EventSource for injecting fake key events.
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
    run_loop(terminal, agent_rx, &mut history, &mut input, input_tx, interrupt_tx, decision_tx, is_running, events)
}

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

    loop {
        // Drain all pending agent events
        let width = terminal.size()?.width;
        while let Ok(event) = agent_rx.try_recv() {
            history.push_event(event, width);
        }

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),    // history
                    Constraint::Length(1), // blank gap
                    Constraint::Length(1), // input
                ])
                .split(f.area());

            let history_view = HistoryView {
                state: history,
                width: chunks[0].width,
            };
            f.render_widget(history_view, chunks[0]);

            let input_line = build_input_line(input, pending_quit);
            f.render_widget(input_line, chunks[2]);
        })?;

        // Poll for key events with timeout
        if let Some(Event::Key(key)) = events.poll(Duration::from_millis(50))? {
            match handle_key(key, input, history, input_tx, interrupt_tx, decision_tx, &mut pending_quit) {
                KeyOutcome::Quit => break,
                KeyOutcome::Submit(text) => {
                    pending_quit = false;
                    history.push(super::cell::HistoryCell::UserMessage { text: text.clone() });
                    let _ = input_tx.blocking_send(text);
                }
                KeyOutcome::None => {}
            }
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

    // Input handling
    match input.handle_key(key) {
        InputAction::Submit => {
            let text = input.take_submitted();
            KeyOutcome::Submit(text)
        }
        _ => KeyOutcome::None,
    }
}

fn build_input_line(input: &InputLine, pending_quit: bool) -> Paragraph<'static> {
    let prefix = Span::styled("> ", Style::new().add_modifier(Modifier::BOLD | Modifier::DIM));
    let line = if pending_quit {
        Line::from(vec![
            prefix,
            Span::styled(
                "再按 Ctrl+C 或 Esc 退出",
                Style::new().fg(Color::Yellow),
            ),
        ])
    } else {
        Line::from(vec![prefix, Span::raw(input.buffer.clone())])
    };
    Paragraph::new(line).style(Style::new().bg(Color::Indexed(240)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::cell::HistoryCell;
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
            &mut terminal, &mut agent_rx, &input_tx, &interrupt_tx, &decision_tx, &is_running, &source,
        ).unwrap();

        // After Ctrl+C, the input row should show the confirm message
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(row_text.contains("Ctrl") || row_text.contains("退出"),
            "expected confirm prompt, got: {row_text:?}");
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
            &mut terminal, &mut agent_rx, &input_tx, &interrupt_tx, &decision_tx, &is_running, &source,
        );
        assert!(result.is_ok(), "two Ctrl+C should quit cleanly, got: {:?}", result);
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
            &mut terminal, &mut agent_rx, &input_tx, &interrupt_tx, &decision_tx, &is_running, &source,
        );
        assert!(result.is_ok(), "two Esc should quit cleanly, got: {:?}", result);
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

        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]));
        let source = ScriptedEvents { events };

        let result = run_tui_with_backend_and_events(
            &mut terminal, &mut agent_rx, &input_tx, &interrupt_tx, &decision_tx, &is_running, &source,
        );
        assert!(result.is_ok(), "Ctrl+Q should quit directly, got: {:?}", result);
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
        let source = ScriptedEvents { events: events.clone() };

        run_tui_with_backend_and_events(
            &mut terminal,
            &mut agent_rx,
            &input_tx,
            &interrupt_tx,
            &decision_tx,
            &is_running,
            &source,
        ).unwrap();

        // No submit happened, so input_tx should be empty
        assert!(input_rx.try_recv().is_err(), "no submit expected, but got a message");

        // The terminal buffer should contain "hi" in the input row (last row)
        let buffer = terminal.backend().buffer();
        let input_row = 23u16;
        let row_text: String = (0..80)
            .map(|x| buffer[(x, input_row)].symbol())
            .collect();
        assert!(row_text.contains("hi"), "expected 'hi' in input row, got: {row_text:?}");
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
        agent_tx.try_send(AgentEvent::AssistantText("hello world".into())).unwrap();

        // Script: Ctrl+Q to quit after one frame
        let events = Rc::new(RefCell::new(vec![
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
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
        ).unwrap();

        // The history area should contain "hello world"
        let buffer = terminal.backend().buffer();
        let history_text: String = (0..23u16).flat_map(|y| {
            (0..80u16).map(move |x| buffer[(x, y)].symbol())
        }).collect();
        assert!(history_text.contains("hello world"), "expected 'hello world' in history, got: {history_text:?}");
    }
}
