//! Agent loop: think -> act -> observe.

use std::sync::{Arc, Mutex};

use futures::stream::{BoxStream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::message::{ContentBlock, Message};
use crate::provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason, TokenUsage,
};
use crate::tool::{ToolEvent, ToolRegistry, ToolResult};

use tracing::{Instrument, debug, info, info_span, warn};

/// Shared decision channel used to gate tool execution behind user confirmation.
type DecisionRx = Arc<tokio::sync::Mutex<mpsc::Receiver<(u64, crate::permission::Decision)>>>;

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
    /// Max idle time (no provider events) during THINK before the stream is
    /// considered stalled. When elapsed, the agent emits `Done { EndTurn }`
    /// with whatever content was accumulated so far. `None` disables the idle
    /// timeout (stream can hang forever, as before).
    pub think_idle_timeout: Option<std::time::Duration>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5".to_string(),
            system_prompt: Some(Self::default_system_prompt()),
            max_turns: Some(100),
            gen_params: Default::default(),
            compact_threshold: Some(100_000),
            compact_keep_turns: Some(4),
            // Default idle timeout: 60s between provider events. This is
            // intentionally generous (LLMs can pause between deltas while
            // thinking) but bounded so a stalled connection eventually
            // resolves instead of hanging forever.
            think_idle_timeout: Some(std::time::Duration::from_secs(60)),
        }
    }
}

impl AgentConfig {
    /// Built-in system prompt encouraging batch tool calls and persistent
    /// task execution. Used when the user does not provide a custom
    /// `system_prompt`.
    pub fn default_system_prompt() -> String {
        r#"You are yi-agent. You are a helpful general purpose agent designed by Gong Yichen (宫一尘). You have logical thinking, aim for the best, execute perfectly and always speak with evidence.

You work efficiently by minimizing round-trips. Tool use strategy:
- Independent operations (reading multiple files, parallel searches): issue
  MULTIPLE tool calls in a single response. They will be executed in parallel.
- Dependent operations that must run in sequence (create dir → write file →
  run tests): combine them into ONE bash call using && so the whole sequence
  completes in a single step.
- Only split work across turns when a later step genuinely depends on the
  RESULT of an earlier step.

Example: instead of 3 turns (mkdir, write, test), use one bash call:
  mkdir -p src/utils && echo '...' > src/utils/mod.rs && cargo test

Style: Never use emoji in any response. All communication must be plain text only.

Task execution:
- Keep working until the user's request is fully resolved. Only end your
  turn when you are confident the task is complete.
- Verify your work before declaring done: for code changes, run the
  relevant build/test commands; for factual claims, cite the source.
- After writing or editing a file, inspect the changed result and run the
  most relevant check before giving a final answer. Prefer write/edit tools
  for file changes; use bash primarily for checks and batch operations.
- If a tool call fails, diagnose the error and retry with a fix rather
  than reporting failure and stopping.
- When information is missing, make a reasonable assumption, state it
  briefly, and continue. Do not stop to ask unless the assumption would
  be risky or irreversible.
- Do not substitute a narrower or easier task for the one requested."#
            .to_string()
    }
}

/// Agent runtime.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
    permission_checker: Option<Arc<crate::permission::PermissionChecker>>,
    decision_rx: Option<DecisionRx>,
}

/// Events emitted during agent loop.
#[derive(Debug, Clone, Serialize)]
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
    ToolOutputDelta {
        id: String,
        stream: crate::tool::OutputStream,
        text: String,
    },
    ToolExit {
        id: String,
        code: Option<i32>,
    },
    ToolTimeout {
        id: String,
    },
    Usage {
        model: String,
        usage: TokenUsage,
    },
    /// Heuristic estimate of prefill (input) tokens, emitted before the
    /// provider returns real usage. Lets the status bar show activity.
    EstimatedPrefill(u32),
    /// Streamed delta that counts toward decode (output) tokens but is not
    /// assistant-visible text (e.g. tool-call argument JSON). Used by the
    /// status bar for flow-style decode estimation during tool-call turns.
    DecodeDelta(String),
    Done {
        reason: DoneReason,
    },
    /// Auto-compact 完成事件。old_msg_count 是 compact 前的消息数,
    /// new_msg_count 是 compact 后(含 summary + 保留轮)。
    AutoCompacting {
        old_msg_count: usize,
        new_msg_count: usize,
    },
    Cancelled,
    Error(AgentError),
    PermissionRequest {
        request_id: u64,
        tool_name: String,
        tool_input: Value,
        prefix_suggestion: Option<String>,
        kind: crate::permission::PermissionKind,
    },
    PermissionResolved {
        request_id: u64,
        decision: crate::permission::Decision,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DoneReason {
    EndTurn,
    MaxTurns,
    Interrupted { reason: String },
}

const CONTINUE_AFTER_TRUNCATION: &str =
    "Continue the interrupted task from where you stopped. Do not repeat completed work.";
const COMPLETION_AUDIT_PROMPT: &str =
    "Before you finish, verify the changed result using an appropriate read, diff, build, or test.";

#[derive(Debug, Clone, thiserror::Error, Serialize)]
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
            permission_checker: None,
            decision_rx: None,
        }
    }

    pub fn with_session(self, session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            ..self
        }
    }

    /// Attach a permission checker and decision channel for tool gating.
    ///
    /// `decision_rx` is shared via `Arc<Mutex<Receiver>>` so that the same
    /// receiver can be re-attached when the agent is reconstructed (e.g. on
    /// `/clear` or `/model` in inline mode).
    pub fn with_permission(
        mut self,
        checker: Arc<crate::permission::PermissionChecker>,
        decision_rx: DecisionRx,
    ) -> Self {
        self.permission_checker = Some(checker);
        self.decision_rx = Some(decision_rx);
        self
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
        // 每次运行使用新的 cancel token,避免上一次 cancel 留下的状态
        // 卡死后续运行(inline 模式的 Interrupt/ctrl_c/新 prompt 只 cancel
        // 不重建 agent,如果不重置,后续 run() 会在 run_loop 开头立刻返回
        // Cancelled)。
        self.cancel_token = CancellationToken::new();
        self.session
            .lock()
            .unwrap()
            .push(Message::user(user_prompt));

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let session = self.session.clone();
        let cancel_token = self.cancel_token.clone();
        let permission_checker = self.permission_checker.clone();
        let decision_rx = self.decision_rx.clone();

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            if tx.send(AgentEvent::Start).await.is_err() {
                return; // Receiver dropped, stop the loop
            }
            run_loop(
                tx,
                provider,
                tools,
                session,
                config,
                cancel_token,
                permission_checker,
                decision_rx,
            )
            .await;
        });

        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    tx: mpsc::Sender<AgentEvent>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
    permission_checker: Option<Arc<crate::permission::PermissionChecker>>,
    decision_rx: Option<DecisionRx>,
) {
    let mut messages = session.lock().unwrap().messages().to_vec();
    // 记录进入 run_loop 时的 session 长度(含 Agent::run push 的 user 消息,
    // 不含任何 assistant 回复)。cancel 时 truncate 到此长度,回滚悬空的
    // assistant(tool_use),避免下次 run 被 Anthropic API 拒绝(tool_use
    // 必须跟 tool_result)。
    let session_len = session.lock().unwrap().len();
    let mut last_input_tokens: Option<u32> = None;
    let mut turn = 0u32;
    let mut verification_pending = false;
    let mut audit_attempted = false;
    // Cursor for incremental request logging: only log messages[last_logged..] each turn.
    let mut last_logged = 0usize;

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

        // auto-compact: 每轮 THINK 前用上次 input_tokens 判断
        if let (Some(threshold), Some(tokens)) = (
            config.compact_threshold.filter(|&t| t > 0),
            last_input_tokens,
        ) {
            if tokens >= threshold && messages.len() > 4 {
                let old_count = messages.len();
                let keep_turns = config.compact_keep_turns.unwrap_or(4);
                let session_snapshot = session.lock().unwrap().clone();
                match crate::compact::compact_session(
                    &provider,
                    &config,
                    &session_snapshot,
                    keep_turns,
                )
                .await
                {
                    Ok(new_session) => {
                        messages = new_session.messages().to_vec();
                        *session.lock().unwrap() = new_session;
                        // Reset logging cursor: compact replaced the entire
                        // message list, so last_logged is now stale.
                        last_logged = 0;
                        let _ = tx
                            .send(AgentEvent::AutoCompacting {
                                old_msg_count: old_count,
                                new_msg_count: messages.len(),
                            })
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "auto-compact failed, will retry next turn");
                    }
                }
            }
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

        // Log the request delta (only new messages since last turn) at debug level.
        // Avoids O(N^2) duplication of conversation history across turns.
        debug!(
            turn,
            system = ?config.system_prompt,
            new_msgs = ?&messages[last_logged..],
            "think: request delta"
        );
        last_logged = messages.len();

        // 1. THINK
        let req = ProviderRequest {
            model: config.model.clone(),
            system: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: tools.schemas(),
            params: config.gen_params.clone(),
        };
        // Emit a heuristic prefill estimate so the status bar shows activity
        // before the provider returns real usage (OpenAI-compatible APIs only
        // send usage at stream end).
        let prefill_estimate = estimate_prefill_tokens(&req);
        let _ = tx.try_send(AgentEvent::EstimatedPrefill(prefill_estimate));

        let stream = match provider.call_stream(req).await {
            Ok(s) => {
                tracing::info!(
                    turn,
                    "provider call_stream returned Ok, entering accumulate"
                );
                s
            }
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
        let (content, stop_reason, last_usage) = tokio::select! {
            result = accumulate_provider_stream(stream, &tx, &model, config.think_idle_timeout) => match result {
                Ok(v) => {
                    tracing::info!(turn, stop_reason = ?v.1, content_blocks = v.0.len(), "accumulate returned Ok");
                    v
                }
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
                // THINK 阶段 cancel: session 里只有 user 消息(无 assistant
                // 回复),truncate 到 session_len 保留 user 消息(可接受,
                // Anthropic 允许 user 无 assistant 回复)。
                session.lock().unwrap().truncate(session_len);
                let _ = tx.send(AgentEvent::Cancelled).await;
                return;
            }
        };

        last_input_tokens = last_usage.map(|u| u.input_tokens);

        // Log the full accumulated response content at debug level (never repeats across turns).
        debug!(turn, content = ?content, "think: response");

        // Detect idle stall: accumulate_stream synthesizes this stop reason
        // when no provider event arrives within the idle timeout.
        if let StopReason::Other(ref s) = stop_reason {
            if s == "idle timeout" {
                tracing::warn!(
                    turn,
                    "think phase ended due to idle timeout (stalled stream)"
                );
            } else if let Some(message) = s.strip_prefix("stream error: ") {
                let _ = tx
                    .send(AgentEvent::Error(AgentError::Provider(
                        ProviderError::Stream(message.to_string()),
                    )))
                    .await;
                return;
            }
        }

        messages.push(Message::assistant(content.clone()));
        session
            .lock()
            .unwrap()
            .push(Message::assistant(content.clone()));

        match stop_reason {
            StopReason::EndTurn => {}
            StopReason::MaxTokens => {
                messages.push(Message::user(CONTINUE_AFTER_TRUNCATION));
                session
                    .lock()
                    .unwrap()
                    .push(Message::user(CONTINUE_AFTER_TRUNCATION));
                continue;
            }
            StopReason::StopSequence => {
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted {
                            reason: "stop sequence".into(),
                        },
                    })
                    .await;
                return;
            }
            StopReason::Other(reason) => {
                let _ = tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::Interrupted { reason },
                    })
                    .await;
                return;
            }
        }

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
            if verification_pending && !audit_attempted {
                audit_attempted = true;
                messages.push(Message::user(COMPLETION_AUDIT_PROMPT));
                session
                    .lock()
                    .unwrap()
                    .push(Message::user(COMPLETION_AUDIT_PROMPT));
                continue;
            }
            info!(turn, "agent loop done: end_turn");
            tracing::info!(turn, "emitting AgentEvent::Done(EndTurn)");
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

        // 3. ACT - permission check + parallel execution
        info!(turn, tool_count = tool_uses.len(), tools = ?tool_uses.iter().map(|(_, n, _)| n.as_str()).collect::<Vec<_>>(), "act: executing tools");

        // 权限检查阶段: 在并行执行前逐个检查,过滤被拒绝的工具
        let mut checked_uses: Vec<(String, String, Value)> = Vec::new();
        let mut denied_results: Vec<(String, ToolResult)> = Vec::new();
        for (id, name, input) in tool_uses {
            if let Some(checker) = &permission_checker {
                let check_result = checker.check(&name, &input);
                match check_result {
                    crate::permission::CheckResult::Allow => {
                        checked_uses.push((id, name, input));
                    }
                    crate::permission::CheckResult::Deny => {
                        let _ = tx
                            .send(AgentEvent::ToolResult {
                                id: id.clone(),
                                result: ToolResult::error("permission denied"),
                            })
                            .await;
                        denied_results.push((id.clone(), ToolResult::error("permission denied")));
                    }
                    crate::permission::CheckResult::NeedConfirm(req) => {
                        if let Some(decision_rx) = &decision_rx {
                            let id_clone = id.clone();
                            match handle_confirmation(
                                &tx,
                                checker,
                                decision_rx,
                                &cancel_token,
                                id,
                                name,
                                input,
                                req,
                                "user denied",
                            )
                            .await
                            {
                                Some((id, name, input)) => checked_uses.push((id, name, input)),
                                None => denied_results
                                    .push((id_clone, ToolResult::error("user denied"))),
                            }
                        } else {
                            // No decision channel - deny by default
                            let _ = tx
                                .send(AgentEvent::ToolResult {
                                    id: id.clone(),
                                    result: ToolResult::error(
                                        "permission required but no decision channel",
                                    ),
                                })
                                .await;
                            denied_results.push((
                                id.clone(),
                                ToolResult::error("permission required but no decision channel"),
                            ));
                        }
                    }
                    crate::permission::CheckResult::Blacklisted(req) => {
                        if let Some(decision_rx) = &decision_rx {
                            let id_clone = id.clone();
                            match handle_confirmation(
                                &tx,
                                checker,
                                decision_rx,
                                &cancel_token,
                                id,
                                name,
                                input,
                                req,
                                "user denied blacklisted command",
                            )
                            .await
                            {
                                Some((id, name, input)) => checked_uses.push((id, name, input)),
                                None => denied_results.push((
                                    id_clone,
                                    ToolResult::error("user denied blacklisted command"),
                                )),
                            }
                        } else {
                            let _ = tx
                                .send(AgentEvent::ToolResult {
                                    id: id.clone(),
                                    result: ToolResult::error(
                                        "blacklisted command requires confirmation",
                                    ),
                                })
                                .await;
                            denied_results.push((
                                id.clone(),
                                ToolResult::error("blacklisted command requires confirmation"),
                            ));
                        }
                    }
                }
            } else {
                // No permission checker - allow all (backward compatible)
                checked_uses.push((id, name, input));
            }
        }

        let futures: Vec<_> = checked_uses
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

                    let tool = match tools.get(name) {
                        Some(t) => t,
                        None => {
                            let result = ToolResult::error(format!("tool not found: {}", name));
                            let _ = tx
                                .send(AgentEvent::ToolResult {
                                    id: id.clone(),
                                    result: result.clone(),
                                })
                                .await;
                            return (id.clone(), Some(result));
                        }
                    };

                    // Set up streaming channel + forwarder
                    let (event_tx, mut event_rx) = mpsc::channel::<ToolEvent>(64);
                    let fwd_tx = tx.clone();
                    let fwd_id = id.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = event_rx.recv().await {
                            let agent_ev = match ev {
                                ToolEvent::OutputDelta { stream, text } => {
                                    AgentEvent::ToolOutputDelta {
                                        id: fwd_id.clone(),
                                        stream,
                                        text,
                                    }
                                }
                                ToolEvent::Exit { code } => AgentEvent::ToolExit {
                                    id: fwd_id.clone(),
                                    code,
                                },
                                ToolEvent::Timeout => {
                                    AgentEvent::ToolTimeout { id: fwd_id.clone() }
                                }
                                ToolEvent::Truncated { .. } => continue,
                            };
                            let _ = fwd_tx.send(agent_ev).await;
                        }
                    });

                    let result = tool.call_stream(input.clone(), event_tx).await;

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

        let mutating_tool_ids: std::collections::HashSet<String> = checked_uses
            .iter()
            .filter(|(_, name, _)| {
                tools
                    .get(name)
                    .is_some_and(|tool| !tool.metadata().read_only)
            })
            .map(|(id, _, _)| id.clone())
            .collect();

        // Check 3: ACT 中 — select! between join_all and cancel
        let results = tokio::select! {
            r = futures::future::join_all(futures) => r,
            _ = cancel_token.cancelled() => {
                info!(turn, "agent loop cancelled during act");
                // ACT 阶段 cancel: session 里有 user + assistant(tool_use),
                // 但无对应 tool_result。truncate 到 session_len(含 user,
                // 不含 assistant)回滚悬空的 tool_use,避免下次 run 被
                // Anthropic API 拒绝。
                session.lock().unwrap().truncate(session_len);
                let _ = tx.send(AgentEvent::Cancelled).await;
                return;
            }
        };

        // 4. OBSERVE - feed results back in tool_use_id order
        let mut tool_results: Vec<ContentBlock> = results
            .into_iter()
            .filter_map(|(id, result)| {
                result.map(|r| {
                    if !r.is_error && mutating_tool_ids.contains(&id) {
                        verification_pending = true;
                    }
                    ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: r.content,
                        is_error: r.is_error,
                    }
                })
            })
            .collect();
        // Add denied tool results so LLM sees them
        for (id, result) in denied_results {
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: result.content,
                is_error: result.is_error,
            });
        }
        let tool_results_msg = Message::tool_results(tool_results);
        messages.push(tool_results_msg.clone());
        session.lock().unwrap().push(tool_results_msg);
    }
}

/// Wait for a user decision matching `expected_id` on the decision channel.
/// Discards any mismatched messages (defensive) and returns `Deny` if the
/// channel is closed or the cancel token is triggered.
async fn wait_for_decision(
    decision_rx: &DecisionRx,
    expected_id: u64,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> crate::permission::Decision {
    let mut rx = decision_rx.lock().await;
    loop {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return crate::permission::Decision::Deny,
            msg = rx.recv() => match msg {
                Some((id, d)) if id == expected_id => return d,
                Some(_) => continue,
                None => return crate::permission::Decision::Deny,
            }
        }
    }
}

/// Handles a permission request that needs user confirmation (NeedConfirm or Blacklisted).
/// Sends PermissionRequest event, waits for decision, sends PermissionResolved event.
/// Returns Some((id, name, input)) if user allows execution, None if user denies.
#[allow(clippy::too_many_arguments)]
async fn handle_confirmation(
    tx: &mpsc::Sender<AgentEvent>,
    checker: &Arc<crate::permission::PermissionChecker>,
    decision_rx: &DecisionRx,
    cancel_token: &tokio_util::sync::CancellationToken,
    id: String,
    name: String,
    input: Value,
    req: crate::permission::PermissionRequest,
    deny_message: &str,
) -> Option<(String, String, Value)> {
    let _ = tx
        .send(AgentEvent::PermissionRequest {
            request_id: req.request_id,
            tool_name: req.tool_name.clone(),
            tool_input: req.tool_input.clone(),
            prefix_suggestion: req.prefix_suggestion.clone(),
            kind: req.kind.clone(),
        })
        .await;

    let decision = wait_for_decision(decision_rx, req.request_id, cancel_token).await;

    let _ = tx
        .send(AgentEvent::PermissionResolved {
            request_id: req.request_id,
            decision: decision.clone(),
        })
        .await;

    match decision {
        crate::permission::Decision::AllowOnce
        | crate::permission::Decision::AlwaysAllowTool
        | crate::permission::Decision::AlwaysAllowPrefix(_) => {
            if let Err(e) = checker.apply_decision(&name, &decision, &req.kind).await {
                tracing::warn!("failed to persist permission decision: {e}");
            }
            Some((id, name, input))
        }
        crate::permission::Decision::Deny => {
            let _ = tx
                .send(AgentEvent::ToolResult {
                    id: id.clone(),
                    result: ToolResult::error(deny_message),
                })
                .await;
            None
        }
    }
}

async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
    idle_timeout: Option<std::time::Duration>,
) -> Result<(Vec<ContentBlock>, StopReason, Option<TokenUsage>), AgentError> {
    let tx = tx.clone();
    let model = model.to_string();
    let (content, stop_reason, last_usage) = crate::provider::accumulate_stream(
        stream,
        move |event| match event {
            ProviderEvent::TextDelta(s) => {
                let _ = tx.try_send(AgentEvent::AssistantText(s));
            }
            ProviderEvent::Usage(u) => {
                let _ = tx.try_send(AgentEvent::Usage {
                    model: model.clone(),
                    usage: u,
                });
            }
            ProviderEvent::ToolUseDelta { partial_json, .. } => {
                let _ = tx.try_send(AgentEvent::DecodeDelta(partial_json));
            }
            _ => {}
        },
        idle_timeout,
    )
    .await?;
    Ok((content, stop_reason, last_usage))
}

/// Heuristic token estimate: ASCII ~4 chars/token, non-ASCII (CJK etc.) ~1.5 chars/token.
fn estimate_tokens(text: &str) -> u32 {
    let mut ascii = 0u32;
    let mut non_ascii = 0u32;
    for c in text.chars() {
        if (c as u32) < 0x80 {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    (ascii as f32 / 4.0 + non_ascii as f32 / 1.5) as u32
}

/// Estimate prefill tokens from the full request (system + tools + all messages).
fn estimate_prefill_tokens(req: &crate::provider::ProviderRequest) -> u32 {
    let mut total = req.system.as_deref().map(estimate_tokens).unwrap_or(0);
    // Tools schema (name + description + input_schema JSON) is part of prefill.
    for tool in &req.tools {
        total += estimate_tokens(&tool.name);
        total += estimate_tokens(&tool.description);
        total += estimate_tokens(&tool.input_schema.to_string());
    }
    for msg in &req.messages {
        for block in &msg.content {
            match block {
                crate::message::ContentBlock::Text(t) => total += estimate_tokens(t),
                crate::message::ContentBlock::ToolUse { name, input, .. } => {
                    total += estimate_tokens(name);
                    total += estimate_tokens(&input.to_string());
                }
                crate::message::ContentBlock::ToolResult { content, .. } => {
                    for b in content {
                        if let crate::message::ContentBlock::Text(t) = b {
                            total += estimate_tokens(t);
                        }
                    }
                }
                crate::message::ContentBlock::Image { .. } => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, Role};
    use crate::permission::{Decision, PermissionKind};
    use crate::provider::{
        GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason,
    };
    use crate::tool::{Tool, ToolMetadata, ToolRegistry, ToolResult};
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
        fn metadata(&self) -> ToolMetadata {
            ToolMetadata {
                read_only: true,
                ..Default::default()
            }
        }
    }

    struct MutatingTool;

    #[async_trait]
    impl Tool for MutatingTool {
        fn name(&self) -> &str {
            "mutate"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn description(&self) -> &str {
            "Mutates a file"
        }
        async fn call(&self, _args: serde_json::Value) -> ToolResult {
            ToolResult::text("changed")
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
    async fn agent_continues_after_max_tokens() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("partial".into()),
                ProviderEvent::Stop {
                    reason: StopReason::MaxTokens,
                },
            ],
            vec![
                ProviderEvent::TextDelta(" complete".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let events = collect_events(agent.run("write a file".into()).await.unwrap());

        assert!(
            events.iter().any(
                |event| matches!(event, AgentEvent::AssistantText(text) if text == " complete")
            )
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_does_not_report_abnormal_stop_as_end_turn() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("partial".into()),
            ProviderEvent::Stop {
                reason: StopReason::Other("idle timeout".into()),
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let events = collect_events(agent.run("write a file".into()).await.unwrap());

        assert!(!matches!(
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
    async fn agent_audits_unverified_mutation_before_end_turn() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "mutate".into(),
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
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("verified".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(MutatingTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let events = collect_events(agent.run("change file".into()).await.unwrap());

        assert!(agent.session().messages().iter().any(|message| matches!(
            message.content.first(),
            Some(ContentBlock::Text(text)) if text.contains("verify the changed result")
        )));
        assert!(
            events.iter().any(
                |event| matches!(event, AgentEvent::AssistantText(text) if text == "verified")
            )
        );
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
        let session = agent.session();
        let tool_result_message = session
            .messages()
            .iter()
            .find(|message| message.role == Role::Tool)
            .unwrap();
        assert!(matches!(
            tool_result_message.content.as_slice(),
            [ContentBlock::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "t1"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_reports_provider_stream_stop_as_error() {
        let provider = ScriptedProvider::new(vec![vec![ProviderEvent::Stop {
            reason: StopReason::Other("stream error: invalid SSE payload".into()),
        }]]);
        let mut agent = Agent::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );

        let events = collect_events(agent.run("hello".into()).await.unwrap());
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::Error(AgentError::Provider(ProviderError::Stream(message)))
                if message.contains("invalid SSE payload")
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::Done { .. }))
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
                AgentEvent::Usage { usage, .. } => Some(usage.clone()),
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

    /// Provider that emits one TextDelta then stalls forever (no Stop, no None).
    /// Simulates a real-world stall where the server sends partial text then
    /// the connection goes silent without a proper terminal event.
    struct StallAfterDeltaProvider;

    #[async_trait]
    impl Provider for StallAfterDeltaProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            // Emit one TextDelta, then pending forever.
            let stream = futures::stream::iter(vec![ProviderEvent::TextDelta("partial".into())])
                .chain(futures::stream::pending());
            Ok(stream.boxed())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_think_emits_cancelled() {
        let provider = Arc::new(HangingProvider);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(provider, tools, AgentConfig::default());

        // run() resets the cancel token (Fix 1), so we must capture the
        // token AFTER run() returns to cancel the correct (new) token.
        let stream = agent.run("hi".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });
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

    /// When the provider stream emits a TextDelta then stalls (no Stop event),
    /// the agent must not hang forever. It should detect the idle stall and
    /// emit a terminal event (Done or Error) within a bounded time.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_think_stream_stall_emits_terminal_within_timeout() {
        let provider = Arc::new(StallAfterDeltaProvider);
        let tools = Arc::new(ToolRegistry::new());
        // Short idle timeout so the test runs fast (the stall is detected
        // quickly instead of waiting for the 60s default).
        let config = AgentConfig {
            think_idle_timeout: Some(std::time::Duration::from_millis(500)),
            ..Default::default()
        };
        let mut agent = Agent::new(provider, tools, config);

        let stream = agent.run("hi".into()).await.unwrap();

        // Wrap in a timeout: if the agent hangs forever (the bug), this fails.
        // The idle timeout under test should be much shorter than this.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            collect_events_async(stream),
        )
        .await;

        let events = result
            .expect("agent hung forever — idle stall not detected (no terminal event within 10s)");

        // Must have received the partial text.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::AssistantText(t) if t == "partial")),
            "should have the partial AssistantText"
        );
        // Must have a terminal event (Done or Error), not just hang.
        let has_terminal = events
            .iter()
            .any(|e| matches!(e, AgentEvent::Done { .. } | AgentEvent::Error(_)));
        assert!(
            has_terminal,
            "should emit a terminal event (Done or Error) after stall, got: {:?}",
            events
        );
    }

    /// Async version of collect_events for use with tokio::time::timeout.
    async fn collect_events_async(mut stream: BoxStream<'static, AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        use futures::StreamExt;
        while let Some(ev) = stream.next().await {
            out.push(ev);
        }
        out
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

        // run() resets the cancel token (Fix 1), so capture AFTER run().
        let stream = agent.run("hang".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });
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

        // Fix 2: ACT 阶段 cancel 后,session 应回滚到 cancel 前长度
        // (只含 user 消息,不含悬空的 assistant(tool_use))。
        let session = agent.session();
        assert_eq!(
            session.len(),
            1,
            "session should be rolled back to only the user message"
        );
        assert_eq!(session.messages()[0].role, Role::User);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_during_act_rolls_back_session() {
        // Fix 2 专项测试:ACT 阶段 cancel 后,session 中不能留下悬空的
        // assistant(tool_use)(否则下次 run 会被 Anthropic API 拒绝,
        // 因为 tool_use 必须跟 tool_result)。
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

        let session_before = agent.session().len();
        assert_eq!(session_before, 0, "fresh agent has empty session");

        let stream = agent.run("hang".into()).await.unwrap();
        let cancel_token = agent.cancel_token();
        let _handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel_token.cancel();
        });
        let _ = collect_events(stream);

        let session = agent.session();
        // session 应只含 user 消息(1 条),不含 assistant(tool_use)。
        assert_eq!(
            session.len(),
            1,
            "session should contain only the user message after ACT cancel"
        );
        assert_eq!(session.messages()[0].role, Role::User);
        // 确认没有任何 Assistant 消息残留(tool_use 悬空)。
        assert!(
            !session.messages().iter().any(|m| m.role == Role::Assistant),
            "no Assistant message should remain after ACT cancel"
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
    async fn agent_cancel_then_run_works() {
        // Fix 1: cancel() was permanent. After Fix 1, run() resets the
        // cancel token, so a previously cancelled agent can still run to
        // completion. This regression test guards against reintroducing
        // the "agent permanently stuck after cancel" bug.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        // Cancel before run starts. With Fix 1, run() resets the token,
        // so this cancel must NOT cause the upcoming run to emit Cancelled.
        agent.cancel();
        assert!(
            agent.cancel_token().is_cancelled(),
            "precondition: token cancelled"
        );

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        // Should complete normally, ending with Done(EndTurn), NOT Cancelled.
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::Cancelled)),
            "run after cancel should not be cancelled (Fix 1 reset token)"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[test]
    fn agent_config_default_model() {
        let config = AgentConfig::default();
        assert_eq!(config.model, "claude-sonnet-4-5");
        assert_eq!(config.max_turns, Some(100));
    }

    #[test]
    fn default_system_prompt_contains_identity_and_strategy() {
        let prompt = AgentConfig::default_system_prompt();
        assert!(prompt.contains("yi-agent"));
        assert!(prompt.contains("Gong Yichen"));
        assert!(prompt.contains("minimizing round-trips"));
        assert!(prompt.contains("MULTIPLE tool calls"));
        assert!(prompt.contains("&&"));
    }

    #[test]
    fn agent_config_default_uses_default_system_prompt() {
        let config = AgentConfig::default();
        assert_eq!(
            config.system_prompt.as_deref(),
            Some(AgentConfig::default_system_prompt().as_str())
        );
    }

    #[test]
    fn agent_config_custom_system_prompt_overrides_default() {
        let config = AgentConfig {
            system_prompt: Some("be brief".into()),
            ..Default::default()
        };
        assert_eq!(config.system_prompt.as_deref(), Some("be brief"));
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
            think_idle_timeout: None,
        };
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.max_turns, Some(50));
        assert_eq!(config.gen_params.temperature, Some(0.7));
    }

    #[test]
    fn permission_request_event_constructs() {
        let ev = AgentEvent::PermissionRequest {
            request_id: 1,
            tool_name: "bash".to_string(),
            tool_input: serde_json::json!({"command": "ls"}),
            prefix_suggestion: Some("ls".to_string()),
            kind: PermissionKind::Normal,
        };
        match ev {
            AgentEvent::PermissionRequest {
                request_id,
                tool_name,
                ..
            } => {
                assert_eq!(request_id, 1);
                assert_eq!(tool_name, "bash");
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn permission_resolved_event_constructs() {
        let ev = AgentEvent::PermissionResolved {
            request_id: 1,
            decision: Decision::AllowOnce,
        };
        match ev {
            AgentEvent::PermissionResolved {
                request_id,
                decision,
            } => {
                assert_eq!(request_id, 1);
                assert_eq!(decision, Decision::AllowOnce);
            }
            _ => panic!("expected PermissionResolved"),
        }
    }

    #[test]
    fn agent_with_permission_builder_sets_fields() {
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            false,
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (_tx, rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let provider = ScriptedProvider::new(vec![]);
        let tools = Arc::new(ToolRegistry::new());
        let agent = Agent::new(Arc::new(provider), tools, AgentConfig::default())
            .with_permission(checker, rx);
        assert!(agent.permission_checker.is_some());
        assert!(agent.decision_rx.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_permission_allow_executes_tool() {
        // PermissionChecker with yolo=true allows all bash commands (no blacklist match).
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            true, // yolo: allow all (except blacklist, which is empty)
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (_decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "upper".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"text":"hi"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
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
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // "upper" tool is not bash/write/edit, so check() returns Allow immediately.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error)),
            "tool should execute and return success"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_permission_need_confirm_user_denies() {
        // No whitelist, no yolo -> bash tool call triggers NeedConfirm.
        // User sends Deny -> tool result is an error.
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            false, // not yolo
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"command":"ls"}"#.into(),
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
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        // Spawn a task to respond with Deny when a PermissionRequest arrives.
        let _handle = tokio::spawn(async move {
            // Wait for the request_id from the channel, then send Deny.
            // The receiver is in the agent, so we just send a decision for any request_id.
            // We need to know the request_id. It starts at 1.
            decision_tx
                .send((1, crate::permission::Decision::Deny))
                .await
                .ok();
        });

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // Should have a PermissionRequest event
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest { .. })),
            "should emit PermissionRequest event"
        );
        // Should have a PermissionResolved event with Deny
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::PermissionResolved {
                    decision: crate::permission::Decision::Deny,
                    ..
                }
            )),
            "should emit PermissionResolved with Deny"
        );
        // Should have a ToolResult with error (denied)
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if result.is_error)),
            "should have an error ToolResult for denied tool"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_with_permission_need_confirm_user_allows() {
        // No whitelist, no yolo -> bash tool call triggers NeedConfirm.
        // User sends AllowOnce -> tool executes normally.
        let blocklist: crate::permission::BlocklistFn = std::sync::Arc::new(|_| None);
        let checker = std::sync::Arc::new(crate::permission::PermissionChecker::new(
            crate::permission::PermissionsConfig::default(),
            false,
            std::path::PathBuf::from("/tmp"),
            blocklist,
        ));
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, crate::permission::Decision)>(16);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));

        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart {
                    id: "t1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: r#"{"command":"ls"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
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
        // Register a tool named "bash" that just echoes.
        struct BashEchoTool;
        #[async_trait]
        impl Tool for BashEchoTool {
            fn name(&self) -> &str {
                "bash"
            }
            fn schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}})
            }
            fn description(&self) -> &str {
                "Echoes the command back"
            }
            async fn call(&self, args: serde_json::Value) -> ToolResult {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                ToolResult::text(format!("ran: {}", cmd))
            }
        }
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(BashEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default())
            .with_permission(checker, decision_rx);

        let _handle = tokio::spawn(async move {
            decision_tx
                .send((1, crate::permission::Decision::AllowOnce))
                .await
                .ok();
        });

        let stream = agent.run("test".into()).await.unwrap();
        let events = collect_events(stream);

        // Should have PermissionRequest and PermissionResolved events
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::PermissionRequest { .. })),
            "should emit PermissionRequest event"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::PermissionResolved {
                    decision: crate::permission::Decision::AllowOnce,
                    ..
                }
            )),
            "should emit PermissionResolved with AllowOnce"
        );
        // Should have a successful ToolResult
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error)),
            "should have a successful ToolResult for allowed tool"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_triggers_when_threshold_exceeded() {
        // Pre-populate session with 4 messages (2 user/assistant pairs) so that
        // after turn 1 the session has 7 messages — enough for compact_session
        // to find a non-zero split point with keep_turns=1.
        // Turn 1: tool_use + Usage(input=200). After turn 1:
        //   [user1, asst1, user2, asst2, user_prompt, asst_tool_use, tool_results] = 7
        // Turn 2 THINK前: last_input_tokens=200 >= threshold=100, 7 > 4 → compact.
        // compact_session 调 provider.call() → Script[1]: "summary text".
        // session 替换为 [summary, recent...]. emit AutoCompacting.
        // Turn 2 THINK → Script[2]: "done" + EndTurn.
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
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("summary text".into()),
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
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::AutoCompacting {
                    old_msg_count,
                    new_msg_count
                } if *old_msg_count > *new_msg_count
            )),
            "should emit AutoCompacting with old > new, events: {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_below_threshold() {
        // Usage(input=50) < threshold=100 → no compact.
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
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 50,
                    ..Default::default()
                }),
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
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when below threshold"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_when_threshold_none() {
        // threshold=None → no compact even if Usage claims 200.
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
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
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
        let config = AgentConfig {
            compact_threshold: None,
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when threshold is None"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_when_threshold_zero() {
        // threshold=Some(0) → filtered out by `filter(|&t| t > 0)`, no compact.
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
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
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
        let config = AgentConfig {
            compact_threshold: Some(0),
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when threshold is zero"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_skipped_on_first_turn() {
        // First turn: last_input_tokens is None → no compact even if
        // Usage claims 999. No with_session needed — the pre-check
        // happens before any Usage is captured.
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 999,
                ..Default::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting on first turn"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    /// Provider that returns scripted events for the first N calls,
    /// then returns an error on the (N)-th call, then resumes scripted events.
    struct ScriptThenFail {
        scripts: Vec<Vec<ProviderEvent>>,
        fail_at: usize,
        call_index: std::sync::Mutex<usize>,
        fail_error: ProviderError,
    }

    impl ScriptThenFail {
        fn new(scripts: Vec<Vec<ProviderEvent>>, fail_at: usize, error: ProviderError) -> Self {
            Self {
                scripts,
                fail_at,
                call_index: std::sync::Mutex::new(0),
                fail_error: error,
            }
        }
    }

    #[async_trait]
    impl Provider for ScriptThenFail {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut idx = self.call_index.lock().unwrap();
            let current = *idx;
            *idx += 1;
            if current == self.fail_at {
                return Err(self.fail_error.clone());
            }
            let script = self.scripts.get(current).cloned().unwrap_or_else(|| {
                vec![ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                }]
            });
            Ok(futures::stream::iter(script).boxed())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_failure_continues_loop() {
        // compact fails (provider error on compact call), run_loop continues.
        // Call 0 (turn 1 THINK): tool_use + Usage(input=200) + Stop
        // Call 1 (compact_session): Err(Auth)
        // Call 2 (turn 2 THINK): "done" + EndTurn
        let provider = ScriptThenFail::new(
            vec![
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
                    ProviderEvent::Usage(TokenUsage {
                        input_tokens: 200,
                        ..Default::default()
                    }),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ],
                // Call 1 is consumed by compact_session → will fail
                vec![],
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ],
            ],
            1,
            ProviderError::Auth("compact auth failed".into()),
        );
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompacting { .. })),
            "should not emit AutoCompacting when compact fails"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_compact_resets_baseline() {
        // Verify compact → next THINK → compact again works.
        // Scripts:
        // 0: turn 1 — tool_use + Usage(200) + Stop
        // 1: compact call — "summary1" + Stop
        // 2: turn 2 THINK — tool_use + Usage(200) + Stop
        // 3: compact call — "summary2" + Stop
        // 4: turn 3 THINK — "done" + EndTurn
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
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("summary1".into()),
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
                    partial_json: r#"{"text":"b"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "t2".into() },
                ProviderEvent::Usage(TokenUsage {
                    input_tokens: 200,
                    ..Default::default()
                }),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ],
            vec![
                ProviderEvent::TextDelta("summary2".into()),
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
        let config = AgentConfig {
            compact_threshold: Some(100),
            compact_keep_turns: Some(1),
            ..Default::default()
        };
        let mut session = Session::new();
        session.push(Message::user("old1"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply1".into(),
        )]));
        session.push(Message::user("old2"));
        session.push(Message::assistant(vec![ContentBlock::Text(
            "reply2".into(),
        )]));
        let mut agent =
            Agent::new(Arc::new(provider), Arc::new(tools), config).with_session(session);

        let stream = agent.run("prompt".into()).await.unwrap();
        let events = collect_events(stream);

        let compact_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AutoCompacting { .. }))
            .count();
        assert_eq!(
            compact_count, 2,
            "should emit exactly 2 AutoCompacting events, got {compact_count}"
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::EndTurn
            })
        ));
    }

    #[test]
    fn estimate_tokens_ascii() {
        // 8 ASCII chars → 8/4 = 2 tokens
        assert_eq!(estimate_tokens("hello!!!"), 2);
    }

    #[test]
    fn estimate_tokens_cjk() {
        // 3 CJK chars → 3/1.5 = 2 tokens
        assert_eq!(estimate_tokens("你好吗"), 2);
    }

    #[test]
    fn estimate_tokens_mixed() {
        // 4 ASCII + 3 CJK → 1 + 2 = 3 tokens
        assert_eq!(estimate_tokens("hi!!你好吗"), 3);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }
}
