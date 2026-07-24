//! App 主循环：并发协调输入、agent 事件流、中断信号。

use std::sync::Arc;

use anyhow::Result;
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;
use yi_agent_core::{Agent, AgentConfig, AgentEvent, Provider, Session, ToolRegistry};

use crate::compact::compact_session;
use crate::file_ref::expand_file_refs;
use crate::input::{self, UserCommand, help_text};
use crate::render::Renderer;

/// Tracks token usage: cumulative for /cost, last context size for auto-compact.
#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub last_input_tokens: u64,
}

impl UsageStats {
    pub fn add_usage(&mut self, usage: yi_agent_core::TokenUsage) {
        self.total_input_tokens += usage.input_tokens as u64;
        self.total_output_tokens += usage.output_tokens as u64;
        self.last_input_tokens = usage.input_tokens as u64;
    }

    pub fn reset_session(&mut self) {
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.last_input_tokens = 0;
    }

    /// Last API call's input token count — approximates current context size.
    pub fn last_context_tokens(&self) -> u64 {
        self.last_input_tokens
    }
}

/// Format AgentConfig for /config display.
fn format_config(config: &AgentConfig, workdir: &std::path::Path) -> String {
    let max_turns = config
        .max_turns
        .map_or("无限制".to_string(), |n| n.to_string());
    let threshold = config.compact_threshold.unwrap_or(100_000);
    let keep_turns = config.compact_keep_turns.unwrap_or(4);
    format!(
        "模型: {}\n工作目录: {}\n最大轮数: {}\nCompact 阈值: {} tokens\nCompact 保留轮数: {}",
        config.model,
        workdir.display(),
        max_turns,
        threshold,
        keep_turns
    )
}

/// 应用运行时状态。
///
/// 额外持有 provider/tools/config 的 Arc，用于 `/clear` 时重建 Agent。
pub struct App {
    agent: Agent,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    workdir: std::path::PathBuf,
    renderer: Box<dyn Renderer>,
    usage_stats: UsageStats,
}

impl App {
    pub fn new(
        agent: Agent,
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
        workdir: std::path::PathBuf,
        renderer: Box<dyn Renderer>,
    ) -> Self {
        Self {
            agent,
            provider,
            tools,
            config,
            workdir,
            renderer,
            usage_stats: UsageStats::default(),
        }
    }

    /// 运行 App 主循环。
    ///
    /// 三个并发源通过 tokio::select! 协调：
    /// 1. 用户输入（reedline via spawn_blocking → mpsc channel）
    /// 2. agent 事件流（BoxStream<AgentEvent>）
    /// 3. Ctrl+C / ESC 中断信号
    ///
    /// 中断通过 `agent.cancel()` 触发 CancellationToken，Agent 在
    /// 下一个检查点退出并发出 `AgentEvent::Cancelled`，由 renderer 渲染。
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
                            // 如果有正在运行的 agent，先中断
                            if current_stream.is_some() {
                                self.agent.cancel();
                                current_stream = None;
                            }

                            // 自动 compact 检查：用最近一次 API 调用的 input_tokens
                            // 近似当前上下文大小（而非累计值，累计值会二次增长导致过早触发）
                            let threshold = self.config.compact_threshold.unwrap_or(100_000) as u64;
                            if self.usage_stats.last_context_tokens() > threshold {
                                self.renderer.render_system("上下文接近上限，正在自动压缩...");
                                let keep_turns = self.config.compact_keep_turns.unwrap_or(4);
                                let session = self.agent.session();
                                match compact_session(&self.provider, &self.config, &session, keep_turns).await {
                                    Ok(new_session) => {
                                        self.agent = Agent::new(
                                            Arc::clone(&self.provider),
                                            Arc::clone(&self.tools),
                                            self.config.clone(),
                                        )
                                        .with_session(new_session);
                                        self.usage_stats.reset_session();
                                    }
                                    Err(e) => {
                                        self.renderer.render_error(&e);
                                    }
                                }
                            }

                            // 展开 @path 文件引用
                            let expanded = match expand_file_refs(&text, &self.workdir) {
                                Ok(text) => text,
                                Err(e) => {
                                    self.renderer.render_error(
                                        &yi_agent_core::AgentError::Provider(
                                            yi_agent_core::ProviderError::InvalidRequest(
                                                e.to_string(),
                                            ),
                                        ),
                                    );
                                    continue;
                                }
                            };

                            self.renderer.render_user_input(&text);
                            match self.agent.run(expanded).await {
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
                            self.usage_stats.reset_session();
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
                        UserCommand::Model(name) => {
                            current_stream = None;
                            let session = self.agent.session();
                            self.config.model = name.clone();
                            self.agent = Agent::new(
                                Arc::clone(&self.provider),
                                Arc::clone(&self.tools),
                                self.config.clone(),
                            )
                            .with_session(session);
                            self.renderer
                                .render_system(&format!("模型已切换为 {name}"));
                        }
                        UserCommand::Cost => {
                            let input = self.usage_stats.total_input_tokens;
                            let output = self.usage_stats.total_output_tokens;
                            let ctx = self.usage_stats.last_input_tokens;
                            self.renderer.render_system(
                                &format!("累计用量：input {input} tokens / output {output} tokens\n当前上下文：{ctx} tokens")
                            );
                        }
                        UserCommand::Compact => {
                            if current_stream.is_some() {
                                self.agent.cancel();
                                current_stream = None;
                            }
                            let before_msgs = self.agent.session().len();
                            let keep_turns = self.config.compact_keep_turns.unwrap_or(4);
                            let session = self.agent.session();
                            match compact_session(&self.provider, &self.config, &session, keep_turns).await {
                                Ok(new_session) => {
                                    let after_msgs = new_session.len();
                                    self.agent = Agent::new(
                                        Arc::clone(&self.provider),
                                        Arc::clone(&self.tools),
                                        self.config.clone(),
                                    )
                                    .with_session(new_session);
                                    self.usage_stats.reset_session();
                                    self.renderer
                                        .render_system(&format!("对话已压缩：{before_msgs} 条消息 → {after_msgs} 条消息"));
                                }
                                Err(e) => {
                                    self.renderer.render_error(&e);
                                }
                            }
                        }
                        UserCommand::Config => {
                            self.renderer.render_system(&format_config(&self.config, &self.workdir));
                        }
                    }
                }
                // ESC 键（仅在 agent 运行时作为中断）
                Some(()) = esc_rx.recv(), if current_stream.is_some() => {
                    self.agent.cancel();
                    // Cancelled 事件会通过 stream 流出，由下方事件分支渲染
                }
                // Ctrl+C 信号（仅在 agent 运行时中断，不退出程序）
                _ = tokio::signal::ctrl_c(), if current_stream.is_some() => {
                    self.agent.cancel();
                    // Cancelled 事件会通过 stream 流出，由下方事件分支渲染
                }
                // agent 事件流有新事件
                event = async {
                    match &mut current_stream {
                        Some(s) => s.next().await,
                        None => None,
                    }
                }, if current_stream.is_some() => {
                    match event {
                        Some(AgentEvent::Done { .. }) | Some(AgentEvent::Cancelled) => {
                            current_stream = None;
                        }
                        Some(e) => {
                            if let AgentEvent::Usage(u) = &e {
                                self.usage_stats.add_usage(u.clone());
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op provider for testing Agent construction without API calls.
    struct NoopProvider;
    #[async_trait::async_trait]
    impl yi_agent_core::Provider for NoopProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::ProviderEvent>,
            yi_agent_core::ProviderError,
        > {
            unimplemented!("NoopProvider is for construction-only tests")
        }
    }

    #[test]
    fn app_tracks_token_usage() {
        let mut stats = UsageStats::default();
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 200,
            output_tokens: 75,
            ..Default::default()
        });
        assert_eq!(stats.total_input_tokens, 300);
        assert_eq!(stats.total_output_tokens, 125);
        assert_eq!(stats.last_input_tokens, 200);
    }

    #[test]
    fn usage_stats_session_reset() {
        let mut stats = UsageStats::default();
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 500,
            output_tokens: 100,
            ..Default::default()
        });
        stats.reset_session();
        assert_eq!(stats.total_input_tokens, 0);
        assert_eq!(stats.total_output_tokens, 0);
        assert_eq!(stats.last_input_tokens, 0);
    }

    #[test]
    fn last_context_tokens_tracks_most_recent_api_call() {
        let mut stats = UsageStats::default();
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 10_000,
            output_tokens: 500,
            ..Default::default()
        });
        // First call: context is 10K
        assert_eq!(stats.last_context_tokens(), 10_000);

        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 15_000,
            output_tokens: 800,
            ..Default::default()
        });
        // Second call: context grew to 15K (not 25K cumulative)
        assert_eq!(stats.last_context_tokens(), 15_000);
        assert_eq!(stats.total_input_tokens, 25_000); // cumulative still tracks for /cost
    }

    #[test]
    fn format_config_display() {
        let config = AgentConfig {
            model: "test-model".to_string(),
            max_turns: Some(42),
            compact_threshold: Some(80_000),
            compact_keep_turns: Some(6),
            ..Default::default()
        };
        let workdir = std::path::Path::new("/tmp/project");
        let s = format_config(&config, workdir);
        assert!(s.contains("test-model"));
        assert!(s.contains("42"));
        assert!(s.contains("/tmp/project"));
        assert!(s.contains("80000"));
        assert!(s.contains("6"));
    }

    #[test]
    fn format_config_unlimited_turns() {
        let config = AgentConfig {
            model: "m".to_string(),
            max_turns: None,
            ..Default::default()
        };
        let s = format_config(&config, std::path::Path::new("/tmp"));
        assert!(s.contains("无限制"));
    }

    #[test]
    fn model_swap_preserves_session() {
        // Verify Agent::new().with_session(session) actually retains messages.
        // This tests the real code path used by /model hot-swap, not just
        // Session::clone().
        use yi_agent_core::{Agent, Message, ToolRegistry};

        let provider: Arc<dyn Provider> = Arc::new(NoopProvider);
        let tools = Arc::new(ToolRegistry::new());
        let config = AgentConfig {
            model: "model-a".to_string(),
            ..Default::default()
        };

        // Build a session with real messages
        let mut session = Session::new();
        session.push(Message::user("hello"));
        session.push(Message::assistant(vec![yi_agent_core::ContentBlock::Text(
            "hi".into(),
        )]));
        assert_eq!(session.len(), 2);

        // Simulate /model hot-swap: rebuild agent with new model, preserve session
        let mut new_config = config.clone();
        new_config.model = "model-b".to_string();
        let agent =
            Agent::new(Arc::clone(&provider), Arc::clone(&tools), new_config).with_session(session);

        // The new agent must have the same messages
        let restored = agent.session();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.messages()[0].role, yi_agent_core::Role::User);
        assert_eq!(restored.messages()[1].role, yi_agent_core::Role::Assistant);
    }

    #[test]
    fn auto_compact_triggers_when_context_exceeds_threshold() {
        // Test the actual trigger logic: last_context_tokens > threshold
        let mut stats = UsageStats::default();
        let threshold = 100_000u64;

        // Below threshold — no compact
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 50_000,
            output_tokens: 1_000,
            ..Default::default()
        });
        assert!(stats.last_context_tokens() <= threshold);

        // Above threshold — should trigger
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 120_000,
            output_tokens: 2_000,
            ..Default::default()
        });
        assert!(stats.last_context_tokens() > threshold);
    }
}
