//! Agent loop: think -> act -> observe.

use std::sync::{Arc, Mutex};

use futures::stream::{BoxStream, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::message::{ContentBlock, Message};
use crate::provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason, TokenUsage,
};
use crate::tool::{ToolRegistry, ToolResult};

use tracing::{Instrument, info, info_span, warn};

/// In-memory message container. No persistence.
#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn truncate(&mut self, len: usize) {
        self.messages.truncate(len);
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Model identifier passed to the provider (e.g. "claude-sonnet-4-5").
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub gen_params: GenParams,
    /// Token count threshold to trigger auto-compact.
    pub compact_threshold: Option<u32>,
    /// Number of recent turns to keep during compact.
    pub compact_keep_turns: Option<u32>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5".to_string(),
            system_prompt: None,
            max_turns: Some(100),
            gen_params: Default::default(),
            compact_threshold: Some(100_000),
            compact_keep_turns: Some(4),
        }
    }
}

/// Agent runtime.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
}

/// Events emitted during agent loop.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Start,
    AssistantText(String),
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        result: ToolResult,
    },
    Usage(TokenUsage),
    Done {
        reason: DoneReason,
    },
    Cancelled,
    Error(AgentError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneReason {
    EndTurn,
    MaxTurns,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, config: AgentConfig) -> Self {
        Self {
            provider,
            tools,
            session: Arc::new(Mutex::new(Session::new())),
            config,
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn with_session(self, session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            ..self
        }
    }

    pub fn session(&self) -> Session {
        self.session.lock().unwrap().clone()
    }

    /// Trigger cancellation. The run loop will exit at the nearest check point.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Get a clone of the cancellation token.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Run the agent loop, returning a stream of events.
    pub async fn run(
        &mut self,
        user_prompt: String,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        self.session
            .lock()
            .unwrap()
            .push(Message::user(user_prompt));

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let session = self.session.clone();
        let cancel_token = self.cancel_token.clone();

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            if tx.send(AgentEvent::Start).await.is_err() {
                return; // Receiver dropped, stop the loop
            }
            run_loop(tx, provider, tools, session, config, cancel_token).await;
        });

        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }
}

async fn run_loop(
    tx: mpsc::Sender<AgentEvent>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
) {
    let mut messages = session.lock().unwrap().messages().to_vec();
    let mut turn = 0u32;

    let model = config.model.clone();
    let loop_span = info_span!("agent_loop", model = %model, msg_count = messages.len());
    let _loop_enter = loop_span.enter();

    loop {
        // Check 1: THINK 前
        if cancel_token.is_cancelled() {
            info!(turn, "agent loop cancelled before think");
            let _ = tx.send(AgentEvent::Cancelled).await;
            return;
        }

        turn += 1;
        if let Some(max) = config.max_turns {
            if turn > max {
                info!(turn, max, "agent loop reached max turns");
                if tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::MaxTurns,
                    })
                    .await
                    .is_err()
                {
                    return; // Receiver dropped, stop the loop
                }
                return;
            }
        }

        info!(turn, msg_count = messages.len(), "think: calling provider");

        // 1. THINK
        let req = ProviderRequest {
            model: config.model.clone(),
            system: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: tools.schemas(),
            params: config.gen_params.clone(),
        };

        let stream = match provider.call_stream(req).await {
            Ok(s) => s,
            Err(e) => {
                warn!(turn, error = %e, "provider call failed");
                if tx
                    .send(AgentEvent::Error(AgentError::Provider(e)))
                    .await
                    .is_err()
                {
                    return; // Receiver dropped, stop the loop
                }
                return;
            }
        };

        // Check 2: THINK 中 — select! between accumulate and cancel
        let (content, _stop_reason) = tokio::select! {
            result = accumulate_provider_stream(stream, &tx) => match result {
                Ok(v) => v,
                Err(e) => {
                    warn!(turn, error = %e, "provider stream error");
                    if tx.send(AgentEvent::Error(e)).await.is_err() {
                        return;
                    }
                    return;
                }
            },
            _ = cancel_token.cancelled() => {
                info!(turn, "agent loop cancelled during think");
                let _ = tx.send(AgentEvent::Cancelled).await;
                return;
            }
        };

        messages.push(Message::assistant(content.clone()));
        session
            .lock()
            .unwrap()
            .push(Message::assistant(content.clone()));

        // 2. Termination check
        let tool_uses: Vec<(String, String, Value)> = content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolUse { id, name, input } = b {
                    Some((id.clone(), name.clone(), input.clone()))
                } else {
                    None
                }
            })
            .collect();

        if tool_uses.is_empty() {
            info!(turn, "agent loop done: end_turn");
            if tx
                .send(AgentEvent::Done {
                    reason: DoneReason::EndTurn,
                })
                .await
                .is_err()
            {
                return; // Receiver dropped, stop the loop
            }
            return;
        }

        // 3. ACT - parallel execution
        info!(turn, tool_count = tool_uses.len(), tools = ?tool_uses.iter().map(|(_, n, _)| n.as_str()).collect::<Vec<_>>(), "act: executing tools");
        let futures: Vec<_> = tool_uses
            .iter()
            .map(|(id, name, input)| {
                let tools = tools.clone();
                let tx = tx.clone();
                async move {
                    let tool_span = info_span!("tool_call", tool = %name, id = %id);
                    let _enter = tool_span.enter();
                    info!(input = %input, "tool call start");

                    if tx
                        .send(AgentEvent::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return (id.clone(), None);
                    }

                    let result = match tools.get(name) {
                        Some(tool) => tool.call(input.clone()).await,
                        None => ToolResult::error(format!("tool not found: {}", name)),
                    };

                    info!(is_error = result.is_error, "tool call done");

                    if tx
                        .send(AgentEvent::ToolResult {
                            id: id.clone(),
                            result: result.clone(),
                        })
                        .await
                        .is_err()
                    {
                        return (id.clone(), None);
                    }

                    (id.clone(), Some(result))
                }
                .instrument(info_span!("tool", name = %name, id = %id))
            })
            .collect();

        // Check 3: ACT 中 — select! between join_all and cancel
        let results = tokio::select! {
            r = futures::future::join_all(futures) => r,
            _ = cancel_token.cancelled() => {
                info!(turn, "agent loop cancelled during act");
                let _ = tx.send(AgentEvent::Cancelled).await;
                return;
            }
        };

        // 4. OBSERVE - feed results back in tool_use_id order
        let tool_results: Vec<ContentBlock> = results
            .into_iter()
            .filter_map(|(id, result)| {
                result.map(|r| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: r.content,
                    is_error: r.is_error,
                })
            })
            .collect();
        let tool_results_msg = Message::tool_results(tool_results);
        messages.push(tool_results_msg.clone());
        session.lock().unwrap().push(tool_results_msg);
    }
}

async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(Vec<ContentBlock>, StopReason), AgentError> {
    let tx = tx.clone();
    let (content, stop_reason) =
        crate::provider::accumulate_stream(stream, move |event| match event {
            ProviderEvent::TextDelta(s) => {
                let _ = tx.try_send(AgentEvent::AssistantText(s));
            }
            ProviderEvent::Usage(u) => {
                let _ = tx.try_send(AgentEvent::Usage(u));
            }
            _ => {}
        })
        .await?;
    Ok((content, stop_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, Role};
    use crate::provider::{
        GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason,
    };
    use crate::tool::{Tool, ToolRegistry, ToolResult};
    use async_trait::async_trait;
    use futures::stream::BoxStream;

    /// Provider that returns a fixed sequence of events.
    /// Each call returns the next script; if scripts exhausted, returns empty (EndTurn).
    struct ScriptedProvider {
        scripts: Vec<Vec<ProviderEvent>>,
        call_index: std::sync::Mutex<usize>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
            Self {
                scripts,
                call_index: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut idx = self.call_index.lock().unwrap();
            let script = self.scripts.get(*idx).cloned().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }]
            });
            *idx += 1;
            Ok(futures::stream::iter(script).boxed())
        }
    }

    struct UpperEchoTool;

    #[async_trait]
    impl Tool for UpperEchoTool {
        fn name(&self) -> &str {
            "upper"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn description(&self) -> &str {
            "Uppercases text"
        }
        async fn call(&self, args: serde_json::Value) -> ToolResult {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            ToolResult::text(text.to_uppercase())
        }
    }

    fn collect_events(stream: BoxStream<'static, AgentEvent>) -> Vec<AgentEvent> {
        futures::executor::block_on_stream(stream).collect()
    }

    #[tokio::test]
    async fn session_basic_ops() {
        let mut s = Session::new();
        assert!(s.is_empty());
        s.push(Message::user("hi"));
        assert_eq!(s.len(), 1);
        s.truncate(0);
        assert!(s.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_terminates_on_end_turn_no_tools() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(matches!(events.first(), Some(AgentEvent::Start)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(t) if t == "Hello"))
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_executes_tool_and_loops() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("Let me uppercase".into()),
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"#.to_string(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#""hi"}"#.to_string(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("Result: HI".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("uppercase hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "upper"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error))
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_handles_tool_not_found() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "ghost".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: "{}".into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("call ghost".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if result.is_error))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_respects_max_turns() {
        // Provider always emits a tool call -> would infinite loop without cap.
        // With max_turns=1: turn 1 executes tool, turn 2 > max -> MaxTurns.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "upper".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: r#"{"text":"x"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            max_turns: Some(1),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

        let stream = agent.run("loop".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                reason: DoneReason::MaxTurns
            }
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_session_restores_history() {
        let mut session = Session::new();
        session.push(Message::user("previous"));
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("ok".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent =
            Agent::new(Arc::new(provider), tools, AgentConfig::default()).with_session(session);

        assert_eq!(agent.session().len(), 1); // restored
        let stream = agent.run("next".into()).await.unwrap();
        // Consume all events to ensure the spawned task completes.
        let events = collect_events(stream);
        // restored(1) + user_prompt(1) + assistant(1) = 3
        assert_eq!(agent.session().len(), 3);
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_forwards_usage_events() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Usage(crate::provider::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        let usage_events: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usage_events.len(), 1);
        assert_eq!(usage_events[0].input_tokens, 10);
        assert_eq!(usage_events[0].output_tokens, 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_token_is_cancellable() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let token = agent.cancel_token();
        assert!(!token.is_cancelled());
        agent.cancel();
        assert!(token.is_cancelled());
    }

    /// Provider whose stream never produces events (simulates a long LLM call).
    struct HangingProvider;

    #[async_trait]
    impl Provider for HangingProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            // A stream that never yields — pending forever.
            let pending = futures::stream::pending();
            Ok(pending.boxed())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_think_emits_cancelled() {
        let provider = Arc::new(HangingProvider);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, AgentConfig::default());

        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "should have Cancelled event"
        );
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "should NOT have Done event"
        );
    }

    /// Tool that never completes (simulates a long-running tool).
    struct HangingTool;

    #[async_trait]
    impl Tool for HangingTool {
        fn name(&self) -> &str {
            "hang"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn description(&self) -> &str {
            "A tool that hangs forever"
        }
        async fn call(&self, _args: serde_json::Value) -> ToolResult {
            // Never returns
            std::future::pending::<()>().await;
            ToolResult::text("unreachable")
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_act_emits_cancelled() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::ToolUseStart {
                id: "t1".into(),
                name: "hang".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "t1".into(),
                partial_json: "{}".into(),
            },
            ProviderEvent::ToolUseEnd { id: "t1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(HangingTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });

        let stream = agent.run("hang".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "should have Cancelled event"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { .. })),
            "should NOT have ToolResult (tool was still running)"
        );
    }

    #[test]
    fn agent_config_has_compact_fields() {
        let config = AgentConfig::default();
        assert!(config.compact_threshold.is_some());
        assert!(config.compact_keep_turns.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_drop_receiver_does_not_panic() {
        let provider = HangingProvider;
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        // Drop the stream immediately without consuming.
        drop(stream);
        // Give the spawned task time to notice the dropped receiver.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // No panic means success.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_executes_parallel_tools_in_single_turn() {
        // Provider emits two tool calls in one turn; both should execute.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::ToolUseStart {
                    id: "t2".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t2".into(),
                    partial_json: r#"{"text":"b"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t2".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("parallel".into()).await.unwrap();
        let events = collect_events(stream);

        let tool_calls: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 2);

        let tool_results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResult { id, result } => Some((id.clone(), result.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 2);
        // Both results should be successful
        assert!(tool_results.iter().all(|(_, r)| !r.is_error));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_multi_turn_loop_three_turns() {
        // Turn 1: tool call -> Turn 2: tool call -> Turn 3: final text
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"first"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t2".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t2".into(),
                    partial_json: r#"{"text":"second"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t2".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("final answer".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("multi".into()).await.unwrap();
        let events = collect_events(stream);

        // Should have 2 ToolCall events
        let tool_calls = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
            .count();
        assert_eq!(tool_calls, 2);

        // Should end with Done(EndTurn)
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_propagates_provider_error() {
        struct ErrorProvider;
        #[async_trait]
        impl Provider for ErrorProvider {
            async fn call_stream(
                &self,
                _req: ProviderRequest,
            ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
                Err(ProviderError::Auth("invalid key".into()))
            }
        }

        let provider = ErrorProvider;
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Error(AgentError::Provider(ProviderError::Auth(_)))
            )),
            "should have Provider Auth error event"
        );
        // Should NOT have a Done event
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Done { .. })),
            "should NOT have Done event after error"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_session_history_after_multi_turn() {
        // After 2 tool turns + final text:
        // user(1) + assistant_turn1(1) + tool_results(1) + assistant_turn2(1) + tool_results(1) + assistant_final(1) = 6
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"a"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("final".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("start".into()).await.unwrap();
        let _ = collect_events(stream);

        let session = agent.session();
        // user(1) + assistant(1) + tool_results(1) + assistant(1) = 4
        assert_eq!(session.len(), 4);
        assert_eq!(session.messages()[0].role, Role::User);
        assert_eq!(session.messages()[1].role, Role::Assistant);
        assert_eq!(session.messages()[2].role, Role::Tool);
        assert_eq!(session.messages()[3].role, Role::Assistant);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_sequential_runs_accumulate_session() {
        // First run: text only -> user + assistant = 2 messages
        // Second run: text only -> + user + assistant = 4 messages
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("first".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("second".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        // First run
        let stream1 = agent.run("prompt1".into()).await.unwrap();
        let _ = collect_events(stream1);
        assert_eq!(agent.session().len(), 2);

        // Second run — session should accumulate
        let stream2 = agent.run("prompt2".into()).await.unwrap();
        let _ = collect_events(stream2);
        assert_eq!(agent.session().len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_assistant_text_event_preserves_content() {
        // Verify AssistantText events carry the full text from provider.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Hello ".into()),
            ProviderEvent::TextDelta("World".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::AssistantText(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello World");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_max_turns_zero_immediate_done() {
        // With max_turns=0: turn 1 > 0 immediately -> MaxTurns before any provider call.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("unreachable".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let config = AgentConfig {
            max_turns: Some(0),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), tools, config);

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                reason: DoneReason::MaxTurns
            }
        )));
        // Should not have any assistant text since provider was never called
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(_))),
            "should NOT have AssistantText with max_turns=0"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_before_run_emits_cancelled() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        // Cancel before run starts
        agent.cancel();
        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "should have Cancelled event when pre-cancelled"
        );
    }

    #[test]
    fn agent_config_default_model() {
        let config = AgentConfig::default();
        assert_eq!(config.model, "claude-sonnet-4-5");
        assert_eq!(config.max_turns, Some(100));
        assert!(config.system_prompt.is_none());
    }

    #[test]
    fn agent_config_custom_values() {
        let config = AgentConfig {
            model: "custom-model".into(),
            system_prompt: Some("be brief".into()),
            max_turns: Some(50),
            gen_params: GenParams {
                temperature: Some(0.7),
                max_tokens: Some(4096),
                ..Default::default()
            },
            compact_threshold: Some(50_000),
            compact_keep_turns: Some(2),
        };
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.max_turns, Some(50));
        assert_eq!(config.gen_params.temperature, Some(0.7));
    }
}
