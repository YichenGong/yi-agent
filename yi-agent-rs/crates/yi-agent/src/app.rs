//! App 主循环：并发协调输入、agent 事件流、中断信号。

use std::sync::Arc;

use anyhow::Result;
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;
use yi_agent_core::{Agent, AgentConfig, AgentEvent, Provider, Session, ToolRegistry};

use crate::input::{self, UserCommand, help_text};
use crate::render::Renderer;

/// 应用运行时状态。
///
/// 额外持有 provider/tools/config 的 Arc，用于 `/clear` 时重建 Agent。
pub struct App {
    agent: Agent,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    renderer: Box<dyn Renderer>,
}

impl App {
    pub fn new(
        agent: Agent,
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
        renderer: Box<dyn Renderer>,
    ) -> Self {
        Self {
            agent,
            provider,
            tools,
            config,
            renderer,
        }
    }

    /// 运行 App 主循环。
    ///
    /// 三个并发源通过 tokio::select! 协调：
    /// 1. 用户输入（reedline via spawn_blocking → mpsc channel）
    /// 2. agent 事件流（BoxStream<AgentEvent>）
    /// 3. Ctrl+C / ESC 中断信号
    ///
    /// 中断通过 drop stream 实现——Agent 内部 task 在 channel send
    /// 失败时自动退出。
    pub async fn run(mut self) -> Result<()> {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<UserCommand>(16);

        // Task 1: 输入循环（reedline 是同步阻塞的，放到 spawn_blocking）
        let cmd_tx_clone = cmd_tx.clone();
        tokio::task::spawn_blocking(move || {
            run_input_loop(cmd_tx_clone);
        });

        // ESC 监听 task（仅在 agent 运行时生效）
        let (esc_tx, mut esc_rx) = mpsc::channel::<()>(1);
        tokio::task::spawn_blocking(move || {
            run_esc_listener(esc_tx);
        });

        let mut current_stream: Option<BoxStream<'static, AgentEvent>> = None;

        loop {
            tokio::select! {
                // 用户输入了新命令
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        UserCommand::Prompt(text) => {
                            // drop 旧 stream（如果有），中断旧 agent 运行
                            current_stream = None;

                            self.renderer.render_user_input(&text);
                            match self.agent.run(text).await {
                                Ok(stream) => {
                                    current_stream = Some(stream);
                                }
                                Err(e) => {
                                    self.renderer.render_error(&e);
                                }
                            }
                        }
                        UserCommand::Quit => {
                            drop(current_stream.take());
                            break;
                        }
                        UserCommand::Clear => {
                            current_stream = None;
                            self.agent = Agent::new(
                                Arc::clone(&self.provider),
                                Arc::clone(&self.tools),
                                self.config.clone(),
                            ).with_session(Session::new());
                            self.renderer.render_system("对话已清空");
                        }
                        UserCommand::Help => {
                            self.renderer.render_system(help_text());
                        }
                    }
                }
                // ESC 键（仅在 agent 运行时作为中断）
                Some(()) = esc_rx.recv(), if current_stream.is_some() => {
                    current_stream = None;
                    self.renderer.render_system("已中断");
                }
                // Ctrl+C 信号（仅在 agent 运行时中断，不退出程序）
                _ = tokio::signal::ctrl_c(), if current_stream.is_some() => {
                    current_stream = None;
                    self.renderer.render_system("已中断");
                }
                // agent 事件流有新事件
                event = async {
                    match &mut current_stream {
                        Some(s) => s.next().await,
                        None => None,
                    }
                }, if current_stream.is_some() => {
                    match event {
                        Some(AgentEvent::Done { .. }) => {
                            current_stream = None;
                        }
                        Some(e) => {
                            self.renderer.render_agent_event(&e);
                        }
                        None => {
                            // stream 意外结束
                            current_stream = None;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// reedline 输入循环（运行在 spawn_blocking 中）。
fn run_input_loop(cmd_tx: mpsc::Sender<UserCommand>) {
    use reedline::{DefaultPrompt, Reedline, Signal};

    let mut line_editor = Reedline::create();
    let prompt = DefaultPrompt::default();

    loop {
        let sig = line_editor.read_line(&prompt);
        match sig {
            Ok(Signal::Success(line)) => {
                if let Some(cmd) = input::parse_user_input(&line) {
                    if cmd_tx.blocking_send(cmd).is_err() {
                        break; // receiver dropped, exit
                    }
                }
            }
            Ok(Signal::CtrlC) => {
                // reedline 的 CtrlC 默认清空当前行，不退出
                // 我们在主循环里单独监听 tokio::signal::ctrl_c()
            }
            Ok(Signal::CtrlD) => {
                // EOF: 退出
                let _ = cmd_tx.blocking_send(UserCommand::Quit);
                break;
            }
            Err(_) => {
                // 读取出错，尝试继续
                eprintln!("输入读取错误，请重试");
            }
        }
    }
}

/// ESC 键监听器（运行在 spawn_blocking 中）。
///
/// 使用 crossterm 的 event poll 监听 ESC 键。
/// 只在 agent 运行时由主循环消费（主循环用 `if current_stream.is_some()` 守卫）。
fn run_esc_listener(esc_tx: mpsc::Sender<()>) {
    use crossterm::event::{self, Event, KeyCode};
    use std::time::Duration;

    loop {
        // 每 100ms 轮询一次，避免持续阻塞导致 task 无法退出
        if event::poll(Duration::from_millis(100)).is_err() {
            break;
        }
        if let Ok(true) = event::poll(Duration::from_millis(0)) {
            if let Ok(Event::Key(key)) = event::read() {
                if key.code == KeyCode::Esc && esc_tx.blocking_send(()).is_err() {
                    break; // receiver dropped
                }
            }
        }
    }
}
