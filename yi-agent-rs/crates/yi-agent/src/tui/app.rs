use std::io::stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use yi_agent_core::AgentEvent;

use super::history::{HistoryState, HistoryView};
use super::input::{InputAction, InputLine};

/// Run the ratatui TUI main loop.
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

fn run_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
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

            // History area
            let history_view = HistoryView {
                state: history,
                width: chunks[0].width,
            };
            f.render_widget(history_view, chunks[0]);

            // Input area (gray bg, "> " prefix)
            let input_line = build_input_line(input);
            f.render_widget(input_line, chunks[2]);
        })?;

        // Poll for key events with timeout
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match handle_key(key, input, history, input_tx, interrupt_tx) {
                    KeyOutcome::Quit => break,
                    KeyOutcome::Submit(text) => {
                        // Show user message in history before sending to agent
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
