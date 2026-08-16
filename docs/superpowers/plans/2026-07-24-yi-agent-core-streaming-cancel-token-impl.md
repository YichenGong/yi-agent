# yi-agent-core 流式输出与中断 + Token 计数实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 为 yi-agent-core 添加取消机制(CancellationToken + drop receiver 兜底)和 token 计数(ProviderEvent::Usage → AgentEvent::Usage)。

**Architecture:** Agent 持有 CancellationToken,run_loop 在三个 check 点响应取消。Provider 层新增 ProviderEvent::Usage,AnthropicStream 解析 message_start/message_delta 的 usage 字段并透传。

**Tech Stack:** tokio-util (CancellationToken), serde (TokenUsage)

**设计文档:** [2026-07-24-yi-agent-core-streaming-cancel-token-design.md](./2026-07-24-yi-agent-core-streaming-cancel-token-design.md)

---

### Task 1: 添加 tokio-util 依赖 + TokenUsage 结构体 + ProviderEvent::Usage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`

**Step 1: Write the failing test**

在 `yi-agent-rs/crates/yi-agent-core/src/provider.rs` 的 `#[cfg(test)] mod tests` 末尾加:

```rust
    #[test]
    fn token_usage_default_has_no_cache() {
        let u = TokenUsage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_creation_input_tokens, None);
        assert_eq!(u.cache_read_input_tokens, None);
    }

    #[test]
    fn provider_event_usage_variant_exists() {
        let u = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(10),
            cache_read_input_tokens: None,
        };
        let e = ProviderEvent::Usage(u.clone());
        assert!(matches!(e, ProviderEvent::Usage(_)));
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core -- token_usage_default provider_event_usage`
Expected: FAIL — `TokenUsage` not found / `ProviderEvent::Usage` variant doesn't exist

**Step 3: Write minimal implementation**

`yi-agent-rs/crates/yi-agent-core/Cargo.toml` 在 `[dependencies]` 末尾加:
```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

`yi-agent-rs/crates/yi-agent-core/src/provider.rs` 在 `ProviderEvent` enum 之前加:
```rust
/// Token usage from a provider response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}
```

在 `ProviderEvent` enum 中 `Stop` 变体后加:
```rust
    Usage(TokenUsage),
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-core -- token_usage_default provider_event_usage`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/Cargo.toml yi-agent-rs/crates/yi-agent-core/src/provider.rs
git commit -m "feat: add TokenUsage struct and ProviderEvent::Usage variant"
```

---

### Task 2: 改 accumulate_stream 回调签名为 FnMut(ProviderEvent) 并转发 Usage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`

**Step 1: Write the failing test**

在 `provider.rs` 的 `mod tests` 末尾加:

```rust
    #[tokio::test]
    async fn accumulate_stream_forwards_usage_via_callback() {
        let events = vec![
            text_event("hi"),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        let provider = MockProvider { events };
        let stream = provider
            .call_stream(ProviderRequest {
                model: "test".into(),
                system: None,
                messages: vec![],
                tools: vec![],
                params: GenParams::default(),
            })
            .await
            .unwrap();

        let mut received_text = Vec::new();
        let mut received_usage = Vec::new();
        let (content, stop) = accumulate_stream(stream, |ev| {
            match ev {
                ProviderEvent::TextDelta(s) => received_text.push(s),
                ProviderEvent::Usage(u) => received_usage.push(u),
                _ => {}
            }
        })
        .await
        .unwrap();

        assert_eq!(content, vec![ContentBlock::Text("hi".into())]);
        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(received_text, vec!["hi".to_string()]);
        assert_eq!(received_usage.len(), 1);
        assert_eq!(received_usage[0].input_tokens, 10);
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core -- accumulate_stream_forwards_usage`
Expected: FAIL — `accumulate_stream` callback expects `String` not `ProviderEvent`

**Step 3: Write minimal implementation**

在 `provider.rs` 中修改 `accumulate_stream` 签名和实现:

```rust
pub async fn accumulate_stream<F>(
    mut stream: BoxStream<'static, ProviderEvent>,
    mut on_event: F,
) -> Result<(Vec<ContentBlock>, StopReason), ProviderError>
where
    F: FnMut(ProviderEvent),
{
    let mut content = Vec::new();
    let mut current_text = String::new();
    let mut tool_uses: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut stop_reason = StopReason::EndTurn;

    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(s) => {
                current_text.push_str(&s);
                on_event(ProviderEvent::TextDelta(s));
            }
            ProviderEvent::ToolUseStart { id, name } => {
                if !current_text.is_empty() {
                    content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
                }
                tool_uses.insert(id.clone(), (name, String::new()));
            }
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                if let Some((_, json)) = tool_uses.get_mut(&id) {
                    json.push_str(&partial_json);
                }
            }
            ProviderEvent::ToolUseEnd { id } => {
                if let Some((name, json)) = tool_uses.remove(&id) {
                    let input: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                        ProviderError::Stream(format!("malformed tool use JSON for id {id}: {e}"))
                    })?;
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
            }
            ProviderEvent::Stop { reason } => {
                stop_reason = reason;
            }
            ProviderEvent::Usage(u) => {
                on_event(ProviderEvent::Usage(u));
            }
        }
    }
    if !current_text.is_empty() {
        content.push(ContentBlock::Text(current_text));
    }
    Ok((content, stop_reason))
}
```

同时修改 `call()` 方法中的回调(从 `|_| {}` 改为 `|_| {}`,签名变了但空闭包仍然兼容):

```rust
    async fn call(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let stream = self.call_stream(req).await?;
        let (content, stop_reason) = accumulate_stream(stream, |_| {}).await?;
        Ok(ProviderResponse {
            content,
            stop_reason,
        })
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-core -- accumulate_stream_forwards_usage`
Expected: PASS

同时验证已有测试不回归:
Run: `cargo test -p yi-agent-core`
Expected: 全部 PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/provider.rs
git commit -m "refactor: accumulate_stream callback to FnMut(ProviderEvent), forward Usage"
```

---

### Task 3: 添加 AgentEvent::Usage 和 AgentEvent::Cancelled + 转发 Usage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

**Step 1: Write the failing test**

在 `agent.rs` 的 `mod tests` 末尾加:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_forwards_usage_events() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Usage(yi_agent_core::TokenUsage {
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core -- agent_forwards_usage`
Expected: FAIL — `AgentEvent::Usage` variant doesn't exist

**Step 3: Write minimal implementation**

在 `agent.rs` 的 `use` 块中,从 `provider` 模块加 `TokenUsage`:

```rust
use crate::provider::{
    GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason, TokenUsage,
};
```

在 `AgentEvent` enum 中加两个变体:

```rust
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
```

修改 `accumulate_provider_stream` 转发 Usage:

```rust
async fn accumulate_provider_stream(
    stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(Vec<ContentBlock>, StopReason), AgentError> {
    let tx = tx.clone();
    let (content, stop_reason) = crate::provider::accumulate_stream(stream, move |event| {
        match event {
            ProviderEvent::TextDelta(s) => {
                let _ = tx.try_send(AgentEvent::AssistantText(s));
            }
            ProviderEvent::Usage(u) => {
                let _ = tx.try_send(AgentEvent::Usage(u));
            }
            _ => {}
        }
    })
    .await?;
    Ok((content, stop_reason))
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-core -- agent_forwards_usage`
Expected: PASS

验证已有测试不回归:
Run: `cargo test -p yi-agent-core`
Expected: 全部 PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat: add AgentEvent::Usage and AgentEvent::Cancelled variants"
```

---

### Task 4: 给 Agent 加 CancellationToken + cancel()/cancel_token() 方法

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

**Step 1: Write the failing test**

在 `agent.rs` 的 `mod tests` 末尾加:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_cancel_token_is_cancellable() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("hi".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let token = agent.cancel_token();
        assert!(!token.is_cancelled());
        agent.cancel();
        assert!(token.is_cancelled());
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core -- agent_cancel_token`
Expected: FAIL — no method `cancel_token` / `cancel` on Agent

**Step 3: Write minimal implementation**

在 `agent.rs` 顶部加 import:

```rust
use tokio_util::sync::CancellationToken;
```

修改 `Agent` 结构体:

```rust
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Arc<Mutex<Session>>,
    config: AgentConfig,
    cancel_token: CancellationToken,
}
```

修改 `Agent::new()`:

```rust
pub fn new(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, config: AgentConfig) -> Self {
    Self {
        provider,
        tools,
        session: Arc::new(Mutex::new(Session::new())),
        config,
        cancel_token: CancellationToken::new(),
    }
}
```

在 `impl Agent` 块中 `session()` 方法后加:

```rust
    /// Trigger cancellation. The run loop will exit at the nearest check point.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Get a clone of the cancellation token.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-core -- agent_cancel_token`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat: add CancellationToken to Agent with cancel()/cancel_token()"
```

---

### Task 5: run_loop 传入 cancel_token + THINK 前 check + THINK 中 select! 取消

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

**Step 1: Write the failing test**

在 `agent.rs` 的 `mod tests` 末尾加:

```rust
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

        let stream = agent.run("hi".into()).await.unwrap();
        // Cancel after a short delay to let the loop start.
        let agent_cancel = async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // We can't call agent.cancel() after moving stream. Use cancel_token.
        };
        // We need the cancel handle. Re-structure: get token before run.
        // Actually, let's cancel via a spawned task.
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core -- agent_cancel_during_think`
Expected: FAIL — test hangs or times out (no cancel check in run_loop)

**Step 3: Write minimal implementation**

修改 `Agent::run()` 传入 cancel_token:

```rust
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
                return;
            }
            run_loop(tx, provider, tools, session, config, cancel_token).await;
        });

        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }
```

修改 `run_loop` 签名并加取消逻辑:

```rust
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

    loop {
        // Check 1: THINK 前
        if cancel_token.is_cancelled() {
            let _ = tx.send(AgentEvent::Cancelled).await;
            return;
        }

        turn += 1;
        if let Some(max) = config.max_turns {
            if turn > max {
                if tx
                    .send(AgentEvent::Done {
                        reason: DoneReason::MaxTurns,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                return;
            }
        }

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
                if tx
                    .send(AgentEvent::Error(AgentError::Provider(e)))
                    .await
                    .is_err()
                {
                    return;
                }
                return;
            }
        };

        // Check 2: THINK 中 — select! between accumulate and cancel
        let (content, _stop_reason) = tokio::select! {
            result = accumulate_provider_stream(stream, &tx) => match result {
                Ok(v) => v,
                Err(e) => {
                    if tx.send(AgentEvent::Error(e)).await.is_err() {
                        return;
                    }
                    return;
                }
            },
            _ = cancel_token.cancelled() => {
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
            if tx
                .send(AgentEvent::Done {
                    reason: DoneReason::EndTurn,
                })
                .await
                .is_err()
            {
                return;
            }
            return;
        }

        // 3. ACT - parallel execution
        let futures: Vec<_> = tool_uses
            .iter()
            .map(|(id, name, input)| {
                let tools = tools.clone();
                let tx = tx.clone();
                async move {
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
            })
            .collect();

        // Check 3: ACT 中 — select! between join_all and cancel
        let results = tokio::select! {
            r = futures::future::join_all(futures) => r,
            _ = cancel_token.cancelled() => {
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
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-core -- agent_cancel_during_think`
Expected: PASS

验证已有测试不回归:
Run: `cargo test -p yi-agent-core`
Expected: 全部 PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat: run_loop cancel checks (pre-THINK, THINK select!, ACT select!)"
```

---

### Task 6: ACT 中取消测试 + drop receiver 兜底测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`

**Step 1: Write the failing tests**

在 `agent.rs` 的 `mod tests` 末尾加:

```rust
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-core -- agent_cancel_during_act agent_drop_receiver`
Expected: `agent_drop_receiver` should PASS (drop receiver already works). `agent_cancel_during_act` should PASS (ACT select! already implemented in Task 5).

Note: These tests verify the implementation from Task 5 is correct. If they pass immediately, that's expected — they're confirmation tests.

**Step 3: Run test to verify it passes**

Run: `cargo test -p yi-agent-core -- agent_cancel_during_act agent_drop_receiver`
Expected: PASS

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "test: ACT cancel and drop receiver safety tests"
```

---

### Task 7: AnthropicStream 解析 message_start usage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs`

**Step 1: Write the failing test**

在 `stream.rs` 的 `mod tests` 末尾加:

```rust
    #[tokio::test]
    async fn parses_message_start_usage() {
        let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":5}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 120);
                assert_eq!(u.output_tokens, 0);
                assert_eq!(u.cache_creation_input_tokens, Some(10));
                assert_eq!(u.cache_read_input_tokens, Some(5));
            }
            _ => panic!("expected Usage event"),
        }
    }

    #[tokio::test]
    async fn parses_message_start_usage_no_cache_fields() {
        let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":50}}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 50);
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            _ => panic!("expected Usage event"),
        }
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-llm -- parses_message_start_usage`
Expected: FAIL — message_start returns Ok(None), no Usage event emitted

**Step 3: Write minimal implementation**

在 `stream.rs` 的 `parse_frame` 方法中,修改 `"message_start"` 分支:

当前(line 214):
```rust
            "message_start" | "message_stop" | "ping" => Ok(None),
```

改为:
```rust
            "message_start" => {
                let usage = data
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if usage.is_null() {
                    return Ok(None);
                }
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u32;
                let cache_creation_input_tokens = usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                let cache_read_input_tokens = usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                Ok(Some(ProviderEvent::Usage(TokenUsage {
                    input_tokens,
                    output_tokens: 0,
                    cache_creation_input_tokens,
                    cache_read_input_tokens,
                })))
            }
            "message_stop" | "ping" => Ok(None),
```

在 `stream.rs` 顶部的 `use` 块中,从 `yi_agent_core` 加 `TokenUsage`:

```rust
use yi_agent_core::{ProviderError, ProviderEvent, StopReason, TokenUsage};
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-llm -- parses_message_start_usage`
Expected: PASS

验证已有测试不回归:
Run: `cargo test -p yi-agent-llm`
Expected: 全部 PASS(注意 `ignores_ping_and_message_start` 测试现在可能需要更新,因为它期望 message_start 不产生事件)

如果 `ignores_ping_and_message_start` 测试失败(因为现在 message_start 产生 Usage),更新它:
- 测试中的 `message_start` data 是 `{"type":"message_start","message":{}}` — `message` 没有 `usage` 字段,`usage.is_null()` 为 true,返回 `Ok(None)`,不产生事件。所以该测试应该仍然 PASS。

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs
git commit -m "feat: parse message_start usage into ProviderEvent::Usage"
```

---

### Task 8: AnthropicStream 解析 message_delta usage

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs`

**Step 1: Write the failing test**

在 `stream.rs` 的 `mod tests` 末尾加:

```rust
    #[tokio::test]
    async fn parses_message_delta_usage() {
        let body = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":45}}\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 2);
        match &events[0] {
            ProviderEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 0);
                assert_eq!(u.output_tokens, 45);
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            _ => panic!("expected Usage event first"),
        }
        assert!(matches!(
            &events[1],
            ProviderEvent::Stop {
                reason: StopReason::EndTurn
            }
        ));
    }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-llm -- parses_message_delta_usage`
Expected: FAIL — message_delta only emits Stop, no Usage event

**Step 3: Write minimal implementation**

在 `stream.rs` 的 `parse_frame` 方法中,修改 `"message_delta"` 分支:

当前(line 200-213):
```rust
            "message_delta" => {
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(reason_str) = delta.get("stop_reason").and_then(Value::as_str) {
                    let reason = match reason_str {
                        "end_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "stop_sequence" => StopReason::StopSequence,
                        other => StopReason::Other(other.to_string()),
                    };
                    Ok(Some(ProviderEvent::Stop { reason }))
                } else {
                    Ok(None)
                }
            }
```

改为(在 Stop 之前先发 Usage):
```rust
            "message_delta" => {
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                let usage = data.get("usage").cloned();
                let stop_reason = delta
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(|reason_str| match reason_str {
                        "end_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "stop_sequence" => StopReason::StopSequence,
                        other => StopReason::Other(other.to_string()),
                    });

                // Emit Usage first (if present), then Stop (if present).
                if let Some(u) = usage {
                    let output_tokens = u
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    // Return Usage; we can only return one event per frame.
                    // But we need to emit BOTH Usage and Stop.
                    // Since parse_frame returns Option<ProviderEvent>, we can only return one.
                    // Solution: store stop_reason in a field for next call, or return Usage
                    // and let the caller see Stop on the next frame.
                    // But there's no next frame — message_delta is the last frame before message_stop.
                    //
                    // Better: change parse_frame to return Vec<ProviderEvent>.
                    // Or: emit Usage as a synthetic pending frame followed by Stop.
                    //
                    // Simplest: store stop_reason in self, return Usage now, emit Stop next call.
                    // But that requires adding state.
                    //
                    // Actually, the cleanest is to return Usage and queue Stop.
                    // Let's use pending_frames to queue the Stop.
                    let stop = stop_reason.clone();
                    let _ = stop; // will use below
                    return Ok(Some(ProviderEvent::Usage(TokenUsage {
                        input_tokens: 0,
                        output_tokens,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    })));
                }

                if let Some(reason) = stop_reason {
                    Ok(Some(ProviderEvent::Stop { reason }))
                } else {
                    Ok(None)
                }
            }
```

Wait — `parse_frame` returns `Option<ProviderEvent>`, but we need to emit TWO events (Usage + Stop). The `poll_next` loop processes one frame at a time. We need to either:
- Change `parse_frame` to return `Vec<ProviderEvent>` (breaks API)
- Queue the Stop event in `pending_frames` (hacky, SseFrame is private)
- Store stop_reason in a field on `AnthropicStream`, emit Usage now, emit Stop on next `poll_next`

The cleanest is to add a `pending_stop: Option<StopReason>` field to `AnthropicStream`:

In `stream.rs`, modify `AnthropicStream` struct:

```rust
pub struct AnthropicStream<S> {
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<SseFrame>,
    block_ids: HashMap<usize, String>,
    pending_stop: Option<StopReason>,
}
```

`new()`:
```rust
    pub fn new(inner: S) -> Self {
        Self {
            line_parser: SseLineParser::new(),
            inner,
            pending_frames: VecDeque::new(),
            block_ids: HashMap::new(),
            pending_stop: None,
        }
    }
```

`parse_frame` for `message_delta`:
```rust
            "message_delta" => {
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                let usage = data.get("usage").cloned();
                let stop_reason = delta
                    .get("stop_reason")
                    .and_then(Value::as_str)
                    .map(|reason_str| match reason_str {
                        "end_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::MaxTokens,
                        "stop_sequence" => StopReason::StopSequence,
                        other => StopReason::Other(other.to_string()),
                    });

                // If we have both usage and stop_reason, emit Usage first and queue Stop.
                if let Some(u) = usage {
                    let output_tokens = u
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    if let Some(reason) = stop_reason {
                        self.pending_stop = Some(reason);
                    }
                    return Ok(Some(ProviderEvent::Usage(TokenUsage {
                        input_tokens: 0,
                        output_tokens,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                    })));
                }

                if let Some(reason) = stop_reason {
                    Ok(Some(ProviderEvent::Stop { reason }))
                } else {
                    Ok(None)
                }
            }
```

In `poll_next`, before draining pending_frames, check `pending_stop`:

```rust
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // First, emit any pending Stop event (from a message_delta that had both usage + stop).
            if let Some(reason) = self.pending_stop.take() {
                return Poll::Ready(Some(Ok(ProviderEvent::Stop { reason })));
            }
            // Then, drain any pending frames from a previous chunk.
            while let Some(frame) = self.pending_frames.pop_front() {
                // ... same as before
            }
            // ... same as before
        }
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-llm -- parses_message_delta_usage`
Expected: PASS

验证已有测试不回归:
Run: `cargo test -p yi-agent-llm`
Expected: 全部 PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs
git commit -m "feat: parse message_delta usage into ProviderEvent::Usage (emit before Stop)"
```

---

### Task 9: 验证全量测试 + clippy + fmt

**Files:** None (verification only)

**Step 1: Run full verification**

```bash
cd yi-agent-rs
just ci
```

Expected: fmt-check, lint (clippy -D warnings), test (all features, workspace), build — all pass.

如果 clippy 有警告,修复后重新验证。

**Step 2: Commit (if any fixes needed)**

如有修复:
```bash
git add -A
git commit -m "fix: clippy/fmt fixes after streaming+cancel+token implementation"
```

如无修复,无需 commit。

---

## 依赖变更总结

| Crate | 变更 |
|-------|------|
| yi-agent-core | 加 `tokio-util = { version = "0.7", features = ["rt"] }` |
| yi-agent-llm | 无新依赖 |

## 测试覆盖

| 测试 | 验证点 |
|------|--------|
| `token_usage_default_has_no_cache` | TokenUsage::default() 的 cache 字段为 None |
| `provider_event_usage_variant_exists` | ProviderEvent::Usage 变体存在 |
| `accumulate_stream_forwards_usage_via_callback` | accumulate_stream 回调收到 Usage |
| `agent_forwards_usage_events` | Agent 转发 ProviderEvent::Usage 为 AgentEvent::Usage |
| `agent_cancel_token_is_cancellable` | Agent::cancel() / cancel_token() 工作 |
| `agent_cancel_during_think_emits_cancelled` | THINK 中取消发 Cancelled,不发 Done |
| `agent_cancel_during_act_emits_cancelled` | ACT 中取消发 Cancelled,不发 ToolResult |
| `agent_drop_receiver_does_not_panic` | drop receiver 不 panic |
| `parses_message_start_usage` | message_start 的 usage 解析为 Usage 事件 |
| `parses_message_start_usage_no_cache_fields` | 无 cache 字段时 cache_* 为 None |
| `parses_message_delta_usage` | message_delta 的 usage 解析,Usage 在 Stop 之前 |
