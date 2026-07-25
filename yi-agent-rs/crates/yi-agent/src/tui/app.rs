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
        &is_running,
    );

    // Always restore terminal state, even on error
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    result
}

/// Run the TUI loop with any ratatui backend (used by tests with TestBackend).
/// Does NOT call enable_raw_mode / EnterAlternateScreen.
pub fn run_tui_with_backend<B: Backend>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    let mut history = HistoryState::new();
    let mut input = InputLine::new();
    run_loop(terminal, agent_rx, &mut history, &mut input, input_tx, interrupt_tx, is_running)
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    history: &mut HistoryState,
    input: &mut InputLine,
    input_tx: &tokio::sync::mpsc::Sender<String>,
    interrupt_tx: &tokio::sync::mpsc::Sender<()>,
    is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    let _ = is_running;

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

            let input_line = build_input_line(input);
            f.render_widget(input_line, chunks[2]);
        })?;

        // Poll for key events with timeout
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match handle_key(key, input, history, input_tx, interrupt_tx) {
                    KeyOutcome::Quit => break,
                    KeyOutcome::Submit(text) => {
                        history.push(super::cell::HistoryCell::UserMessage { text: text.clone() });
                        let _ = input_tx.blocking_send(text);
                    }
                    KeyOutcome::None => {}
                }
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
) -> KeyOutcome {
    // Global keys first
    match key.code {
        KeyCode::Esc => {
            let _ = interrupt_tx.blocking_send(());
            return KeyOutcome::None;
        }
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            let _ = interrupt_tx.blocking_send(());
            return KeyOutcome::None;
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return KeyOutcome::Quit;
        }
        KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
            history.toggle_fold_selected();
            return KeyOutcome::None;
        }
        KeyCode::Up if key.modifiers == KeyModifiers::SHIFT => {
            history.select_up();
            return KeyOutcome::None;
        }
        KeyCode::Down if key.modifiers == KeyModifiers::SHIFT => {
            history.select_down();
            return KeyOutcome::None;
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            history.scroll_up(10);
            return KeyOutcome::None;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            history.scroll_down(10);
            return KeyOutcome::None;
        }
        _ => {}
    }

    // Input handling
    match input.handle_key(key) {
        InputAction::Submit => {
            let text = input.take_submitted();
            KeyOutcome::Submit(text)
        }
        _ => KeyOutcome::None,
    }
}

fn build_input_line(input: &InputLine) -> Paragraph<'static> {
    let prefix = Span::styled("> ", Style::new().add_modifier(Modifier::BOLD | Modifier::DIM));
    let text = Span::raw(input.buffer.clone());
    let line = Line::from(vec![prefix, text]);
    Paragraph::new(line).style(Style::new().bg(Color::Indexed(240)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

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

        // Simulate the production calling stack: block_on spawns a
        // spawn_blocking task that does blocking_send (exactly what run_loop
        // does on Submit). Before the fix this panicked.
        rt.block_on(async move {
            let tx = input_tx.clone();
            let handle = tokio::task::spawn_blocking(move || {
                // This is what run_loop does on Submit:
                tx.blocking_send("hello".to_string()).unwrap();
            });
            handle.await.unwrap();

            // Verify the message arrived
            assert_eq!(input_rx.recv().await.unwrap(), "hello");
        });
    }

    /// Verify run_tui_with_backend compiles and can construct a TestBackend-based
    /// terminal without touching the real terminal (no enable_raw_mode).
    #[test]
    fn run_tui_with_backend_accepts_test_backend() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel::<AgentEvent>(16);
        let (input_tx, _input_rx) = tokio::sync::mpsc::channel::<String>(16);
        let (interrupt_tx, _interrupt_rx) = tokio::sync::mpsc::channel::<()>(1);
        let is_running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Send a Start event so drain has something to do
        agent_tx.try_send(AgentEvent::Start).unwrap();

        // We can't easily test the full loop (event::poll needs a real tty),
        // but we verify the function is callable with a TestBackend and
        // returns an error (event::poll fails in test env) rather than panicking.
        let result = run_tui_with_backend(
            &mut terminal,
            &mut agent_rx.into(),
            &input_tx,
            &interrupt_tx,
            &is_running,
        );
        // event::poll returns Err in non-tty environment -> io::Error, not panic
        assert!(result.is_err(), "expected io::Error from event::poll in non-tty, got {:?}", result);
    }
}
