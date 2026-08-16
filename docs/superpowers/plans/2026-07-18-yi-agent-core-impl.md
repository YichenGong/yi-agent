# yi-agent-core Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the `yi-agent-core` crate with message model, Tool trait, Provider trait, and Agent event-stream loop per the design document.

**Architecture:** Four modules (`message` ← `tool` ← `provider` ← `agent`) with single-direction dependencies. Agent loop runs as a spawned task emitting `AgentEvent` through an mpsc channel. Tool execution is parallel via `futures::join_all`. Errors are fed back to LLM as `ToolResult { is_error: true }`.

**Tech Stack:** Rust 2024 edition, `async-trait`, `futures`, `serde`/`serde_json`, `thiserror`, `tokio` (full), `tokio-stream`.

**Design doc:** `docs/plans/2026-07-18-yi-agent-core-design.md`

---

## Implementation Order

1. **Dependencies** — update `Cargo.toml`
2. **`message.rs`** — pure data types, no deps
3. **`tool.rs`** — depends on `message`
4. **`provider.rs`** — depends on `message` + `tool`
5. **`agent.rs`** — depends on all above
6. **`lib.rs`** — re-exports
7. **Integration smoke test** — verify Agent compiles with mock provider

Each task follows TDD: write failing test → verify fail → implement → verify pass → commit.

---

## Task 1: Add Dependencies

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/Cargo.toml`

**Step 1: Update Cargo.toml with dependencies**

Replace the entire `[lib]` section and add dependencies:

```toml
[package]
name = "yi-agent-core"
description = "Core agent loop, session management, and trait definitions"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[lib]
path = "src/lib.rs"

[dependencies]
async-trait = "0.1"
futures = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tokio-stream = "0.1"
```

**Step 2: Verify it builds**

Run: `cargo build -p yi-agent-core`
Expected: BUILD SUCCESS (no compile errors, dependencies resolve)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/Cargo.toml
git commit -m "Add yi-agent-core dependencies"
```

---

## Task 2: Implement `message.rs`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/message.rs`
- Create: `yi-agent-rs/crates/yi-agent-core/src/lib.rs` (temporary, just `mod message;`)
- Test: `yi-agent-rs/crates/yi-agent-core/src/message.rs` (inline `#[cfg(test)]` module)

**Step 1: Write failing tests in message.rs**

Create `src/message.rs` with:

```rust
//! Message model for agent communication.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    /// Tool result message (serialized as "user" by provider impls).
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),

    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },

    Image {
        source: ImageSource,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url(String),
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content: blocks,
        }
    }

    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Tool,
            content: results,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::Text(text.into())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_constructor() {
        let m = Message::user("hello");
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content, vec![ContentBlock::Text("hello".to_string())]);
    }

    #[test]
    fn assistant_message_constructor() {
        let m = Message::assistant(vec![ContentBlock::Text("hi".into())]);
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn tool_results_message_has_tool_role() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let m = Message::tool_results(vec![result]);
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn system_message_constructor() {
        let m = Message::system("be helpful");
        assert_eq!(m.role, Role::System);
    }

    #[test]
    fn content_block_serde_roundtrip() {
        let block = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "/tmp"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn nested_tool_result_content() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![
                ContentBlock::Text("summary".into()),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "base64data".into(),
                    },
                },
            ],
            is_error: false,
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }
}
```

**Step 2: Wire up lib.rs temporarily**

Create `src/lib.rs`:

```rust
//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod message;
```

**Step 3: Run tests to verify they pass**

Run: `cargo test -p yi-agent-core`
Expected: 6 tests pass

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/message.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "Implement message model (Role, Message, ContentBlock)"
```

---

## Task 3: Implement `tool.rs`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs` (add `pub mod tool;`)

**Step 1: Write failing tests in tool.rs**

Create `src/tool.rs` with tests first:

```rust
//! Tool trait and registry.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::message::ContentBlock;

/// Result of tool execution, fed back to the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

impl ToolResult {
    /// Success: single text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(text.into())],
            is_error: false,
        }
    }

    /// Error: text with "error: " prefix + is_error=true.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(format!("error: {}", text.into()))],
            is_error: true,
        }
    }

    /// Multiple content blocks, not an error.
    pub fn with_content(content: Vec<ContentBlock>) -> Self {
        Self { content, is_error: false }
    }
}

/// Metadata describing a tool's non-behavioral properties.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolMetadata {
    pub source: ToolSource,
    pub requires_confirmation: bool,
    pub read_only: bool,
    pub version: Option<String>,
}

/// Where a tool comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolSource {
    #[default]
    Builtin,
    Mcp { server_name: String },
    Plugin { name: String },
}

/// Tool schema passed to the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// All tools (builtin, MCP, plugins) implement this.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    fn description(&self) -> &str;
    async fn call(&self, args: Value) -> ToolResult;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::default()
    }
}

/// Registry of tools keyed by name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.schema(),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"msg": {"type": "string"}}})
        }
        fn description(&self) -> &str { "Echoes input" }
        async fn call(&self, args: Value) -> ToolResult {
            ToolResult::text(args.to_string())
        }
    }

    #[test]
    fn tool_result_text_constructor() {
        let r = ToolResult::text("hello");
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 1);
    }

    #[test]
    fn tool_result_error_constructor() {
        let r = ToolResult::error("boom");
        assert!(r.is_error);
        match &r.content[0] {
            ContentBlock::Text(s) => assert!(s.starts_with("error:")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn tool_result_with_content() {
        let blocks = vec![ContentBlock::Text("a".into()), ContentBlock::Text("b".into())];
        let r = ToolResult::with_content(blocks);
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 2);
    }

    #[test]
    fn registry_register_and_get() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        assert_eq!(reg.len(), 1);
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn registry_schemas() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let schemas = reg.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "echo");
        assert_eq!(schemas[0].description, "Echoes input");
    }

    #[tokio::test]
    async fn tool_call_returns_result() {
        let tool = EchoTool;
        let result = tool.call(serde_json::json!({"msg": "hi"})).await;
        assert!(!result.is_error);
    }

    #[test]
    fn tool_metadata_default() {
        let tool = EchoTool;
        let meta = tool.metadata();
        assert_eq!(meta.source, ToolSource::Builtin);
        assert!(!meta.requires_confirmation);
        assert!(!meta.read_only);
        assert!(meta.version.is_none());
    }
}
```

**Step 2: Update lib.rs**

```rust
//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod message;
pub mod tool;
```

**Step 3: Run tests**

Run: `cargo test -p yi-agent-core`
Expected: all tests pass (message + tool tests)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/tool.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "Implement Tool trait, ToolResult, ToolRegistry, ToolMetadata"
```

---

## Task 4: Implement `provider.rs`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/provider.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`

**Step 1: Write failing tests and implementation in provider.rs**

Create `src/provider.rs`:

```rust
//! Provider trait for LLM backends.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;

use crate::message::ContentBlock;
use crate::message::Message;
use crate::tool::ToolSchema;

/// Request to a provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenParams,
}

/// Generation parameters (all optional, provider uses its defaults if None).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
}

/// Streaming event from a provider.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseDelta { id: String, partial_json: String },
    ToolUseEnd { id: String },
    Stop { reason: StopReason },
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    Other(String),
}

/// Accumulated provider response.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
}

/// Errors from a provider.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("network error: {0}")]
    Network(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("rate limited")]
    RateLimited,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("server error: {0}")]
    Server(String),
    #[error("stream error: {0}")]
    Stream(String),
}

/// LLM provider trait.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Main method: stream events.
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;

    /// Convenience: accumulate stream into full response.
    async fn call(
        &self,
        req: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let mut stream = self.call_stream(req).await?;
        let mut content = Vec::new();
        let mut current_text = String::new();
        let mut tool_uses: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        let mut stop_reason = StopReason::EndTurn;

        while let Some(event) = stream.next().await {
            match event {
                ProviderEvent::TextDelta(s) => current_text.push_str(&s),
                ProviderEvent::ToolUseStart { id, name } => {
                    if !current_text.is_empty() {
                        content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
                    }
                    tool_uses.insert(id, (name, String::new()));
                }
                ProviderEvent::ToolUseDelta { id, partial_json } => {
                    if let Some((_, json)) = tool_uses.get_mut(&id) {
                        json.push_str(&partial_json);
                    }
                }
                ProviderEvent::ToolUseEnd { id } => {
                    if let Some((name, json)) = tool_uses.remove(&id) {
                        let input: serde_json::Value =
                            serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
                        content.push(ContentBlock::ToolUse { id, name, input });
                    }
                }
                ProviderEvent::Stop { reason } => {
                    stop_reason = reason;
                }
            }
        }
        if !current_text.is_empty() {
            content.push(ContentBlock::Text(current_text));
        }
        Ok(ProviderResponse { content, stop_reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::StreamExt;

    /// Mock provider that emits a predefined sequence of events.
    struct MockProvider {
        events: Vec<ProviderEvent>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let events = self.events.clone();
            Ok(futures::stream::iter(events).boxed())
        }
    }

    fn text_event(s: &str) -> ProviderEvent {
        ProviderEvent::TextDelta(s.to_string())
    }

    fn tool_call_events(id: &str, name: &str, json: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolUseStart { id: id.into(), name: name.into() },
            ProviderEvent::ToolUseDelta { id: id.into(), partial_json: json.into() },
            ProviderEvent::ToolUseEnd { id: id.into() },
        ]
    }

    #[tokio::test]
    async fn call_accumulates_text_only() {
        let provider = MockProvider {
            events: vec![
                text_event("Hello"),
                text_event(" world"),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        };
        let resp = provider.call(ProviderRequest {
            system: None,
            messages: vec![],
            tools: vec![],
            params: GenParams::default(),
        }).await.unwrap();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        assert_eq!(resp.content, vec![ContentBlock::Text("Hello world".into())]);
    }

    #[tokio::test]
    async fn call_accumulates_text_and_tool_use() {
        let provider = MockProvider {
            events: vec![
                text_event("I'll read the file"),
                ProviderEvent::ToolUseStart { id: "t1".into(), name: "read".into() },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"path":"# },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#""main.rs"}"# },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        };
        let resp = provider.call(ProviderRequest {
            system: None,
            messages: vec![],
            tools: vec![],
            params: GenParams::default(),
        }).await.unwrap();
        assert_eq!(resp.content.len(), 2);
        match &resp.content[0] {
            ContentBlock::Text(s) => assert_eq!(s, "I'll read the file"),
            _ => panic!("expected Text"),
        }
        match &resp.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "read");
                assert_eq!(input, &serde_json::json!({"path": "main.rs"}));
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[tokio::test]
    async fn call_handles_stop_reason() {
        let provider = MockProvider {
            events: vec![
                text_event("truncated"),
                ProviderEvent::Stop { reason: StopReason::MaxTokens },
            ],
        };
        let resp = provider.call(ProviderRequest {
            system: None,
            messages: vec![],
            tools: vec![],
            params: GenParams::default(),
        }).await.unwrap();
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn call_stream_yields_events() {
        let provider = MockProvider {
            events: vec![
                text_event("a"),
                text_event("b"),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        };
        let mut stream = provider.call_stream(ProviderRequest {
            system: None,
            messages: vec![],
            tools: vec![],
            params: GenParams::default(),
        }).await.unwrap();
        let mut collected = Vec::new();
        while let Some(e) = stream.next().await {
            collected.push(e);
        }
        assert_eq!(collected.len(), 3);
    }

    #[test]
    fn gen_params_default() {
        let p = GenParams::default();
        assert!(p.temperature.is_none());
        assert!(p.max_tokens.is_none());
    }
}
```

**Step 2: Update lib.rs**

```rust
//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod message;
pub mod tool;
pub mod provider;
```

**Step 3: Run tests**

Run: `cargo test -p yi-agent-core`
Expected: all tests pass (message + tool + provider tests)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/provider.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "Implement Provider trait, ProviderEvent, ProviderRequest, ProviderError"
```

---

## Task 5: Implement `agent.rs`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`

**Step 1: Write implementation and tests in agent.rs**

Create `src/agent.rs`:

```rust
//! Agent loop: think → act → observe.

use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::message::{ContentBlock, Message};
use crate::provider::{GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason};
use crate::tool::{ToolRegistry, ToolResult};

/// In-memory message container. No persistence.
#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self { Self::default() }

    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn truncate(&mut self, len: usize) {
        self.messages.truncate(len);
    }

    pub fn len(&self) -> usize { self.messages.len() }

    pub fn is_empty(&self) -> bool { self.messages.is_empty() }
}

/// Agent configuration.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub gen_params: GenParams,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_turns: Some(100),
            gen_params: Default::default(),
        }
    }
}

/// Agent runtime.
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Session,
    config: AgentConfig,
}

/// Events emitted during agent loop.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Start,
    AssistantText(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, result: ToolResult },
    Done { reason: DoneReason },
    Error(AgentError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneReason {
    EndTurn,
    MaxTurns,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("max turns exceeded")]
    MaxTurnsExceeded,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            session: Session::new(),
            config,
        }
    }

    pub fn with_session(mut self, session: Session) -> Self {
        self.session = session;
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Run the agent loop, returning a stream of events.
    pub async fn run(
        &mut self,
        user_prompt: String,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        self.session.push(Message::user(user_prompt));

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let config = self.config.clone();
        let messages = self.session.messages().to_vec();

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let _ = tx.send(AgentEvent::Start).await;
            run_loop(tx, provider, tools, messages, config).await;
        });

        Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
    }
}

async fn run_loop(
    tx: mpsc::Sender<AgentEvent>,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    mut messages: Vec<Message>,
    config: AgentConfig,
) {
    let mut turn = 0u32;

    loop {
        turn += 1;
        if let Some(max) = config.max_turns {
            if turn > max {
                let _ = tx.send(AgentEvent::Done { reason: DoneReason::MaxTurns }).await;
                return;
            }
        }

        // 1. THINK
        let req = ProviderRequest {
            system: config.system_prompt.clone(),
            messages: messages.clone(),
            tools: tools.schemas(),
            params: config.gen_params.clone(),
        };

        let stream = match provider.call_stream(req).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(AgentError::Provider(e))).await;
                return;
            }
        };

        let (content, stop_reason) = match accumulate_provider_stream(stream, &tx).await {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e)).await;
                return;
            }
        };

        messages.push(Message::assistant(content.clone()));

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

        if tool_uses.is_empty() || stop_reason != StopReason::EndTurn {
            let _ = tx.send(AgentEvent::Done { reason: DoneReason::EndTurn }).await;
            return;
        }

        // 3. ACT — parallel execution
        let futures: Vec<_> = tool_uses
            .iter()
            .map(|(id, name, input)| {
                let tools = tools.clone();
                let tx = tx.clone();
                async move {
                    let _ = tx.send(AgentEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }).await;

                    let result = match tools.get(name) {
                        Some(tool) => tool.call(input.clone()).await,
                        None => ToolResult::error(format!("tool not found: {}", name)),
                    };

                    let _ = tx.send(AgentEvent::ToolResult {
                        id: id.clone(),
                        result: result.clone(),
                    }).await;

                    (id.clone(), result)
                }
            })
            .collect();
        let results = futures::future::join_all(futures).await;

        // 4. OBSERVE — feed results back in tool_use_id order
        let tool_results: Vec<ContentBlock> = results
            .into_iter()
            .map(|(id, result)| ContentBlock::ToolResult {
                tool_use_id: id,
                content: result.content,
                is_error: result.is_error,
            })
            .collect();
        messages.push(Message::tool_results(tool_results));
    }
}

async fn accumulate_provider_stream(
    mut stream: BoxStream<'static, ProviderEvent>,
    tx: &mpsc::Sender<AgentEvent>,
) -> Result<(Vec<ContentBlock>, StopReason), AgentError> {
    let mut content = Vec::new();
    let mut current_text = String::new();
    let mut tool_uses: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut stop_reason = StopReason::EndTurn;

    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::TextDelta(s) => {
                current_text.push_str(&s);
                let _ = tx.send(AgentEvent::AssistantText(s)).await;
            }
            ProviderEvent::ToolUseStart { id, name } => {
                if !current_text.is_empty() {
                    content.push(ContentBlock::Text(std::mem::take(&mut current_text)));
                }
                tool_uses.insert(id, (name, String::new()));
            }
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                if let Some((_, json)) = tool_uses.get_mut(&id) {
                    json.push_str(&partial_json);
                }
            }
            ProviderEvent::ToolUseEnd { id } => {
                if let Some((name, json)) = tool_uses.remove(&id) {
                    let input: Value =
                        serde_json::from_str(&json).unwrap_or(Value::Null);
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
            }
            ProviderEvent::Stop { reason } => {
                stop_reason = reason;
            }
        }
    }
    if !current_text.is_empty() {
        content.push(ContentBlock::Text(current_text));
    }
    Ok((content, stop_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Message};
    use crate::provider::{GenParams, Provider, ProviderEvent, ProviderError, ProviderRequest, StopReason};
    use crate::tool::{Tool, ToolRegistry, ToolResult};
    use async_trait::async_trait;
    use futures::stream::BoxStream;

    /// Provider that returns a fixed sequence of events, looping tool calls if needed.
    struct ScriptedProvider {
        scripts: Vec<Vec<ProviderEvent>>,
        call_index: std::sync::Mutex<usize>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Self {
            Self { scripts, call_index: std::sync::Mutex::new(0) }
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let mut idx = self.call_index.lock().unwrap();
            let script = self.scripts.get(*idx).cloned().unwrap_or_default();
            *idx += 1;
            Ok(futures::stream::iter(script).boxed())
        }
    }

    struct UpperEchoTool;

    #[async_trait]
    impl Tool for UpperEchoTool {
        fn name(&self) -> &str { "upper" }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn description(&self) -> &str { "Uppercases text" }
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

    #[tokio::test]
    async fn agent_terminates_on_end_turn_no_tools() {
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("Hello".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(matches!(events.first(), Some(AgentEvent::Start)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::AssistantText(t) if t == "Hello")));
        assert!(matches!(events.last(), Some(AgentEvent::Done { reason: DoneReason::EndTurn })));
    }

    #[tokio::test]
    async fn agent_executes_tool_and_loops() {
        // Turn 1: assistant asks to call "upper"
        // Turn 2: assistant responds with result, no more tool calls
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::TextDelta("Let me uppercase".into()),
                ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"# },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#""hi"}"# },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
            vec![
                ProviderEvent::TextDelta("Result: HI".into()),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), AgentConfig::default());

        let stream = agent.run("uppercase hi".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "upper")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if !result.is_error)));
        assert!(matches!(events.last(), Some(AgentEvent::Done { reason: DoneReason::EndTurn })));
    }

    #[tokio::test]
    async fn agent_handles_tool_not_found() {
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart { id: "t1".into(), name: "ghost".into() },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: "{}".into() },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
            vec![
                ProviderEvent::TextDelta("ok".into()),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        ]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default());

        let stream = agent.run("call ghost".into()).await.unwrap();
        let events = collect_events(stream);

        assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolResult { result, .. } if result.is_error)));
    }

    #[tokio::test]
    async fn agent_respects_max_turns() {
        // Provider always emits a tool call → infinite loop if no cap
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"x"}"# },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ];
            20  // repeated script won't work, but we set max_turns=2 so loop stops early
        ]);
        // Note: ScriptedProvider returns empty vec after scripts exhausted, which has no tool uses
        // → terminates via empty tool_uses. So max_turns test uses small cap.
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            max_turns: Some(2),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

        let stream = agent.run("loop".into()).await.unwrap();
        let events = collect_events(stream);

        // Should terminate (either via MaxTurns or EndTurn after scripts exhaust)
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { .. })));
    }

    #[tokio::test]
    async fn agent_with_session_restores_history() {
        let mut session = Session::new();
        session.push(Message::user("previous"));
        let provider = ScriptedProvider::new(vec![vec![
            ProviderEvent::TextDelta("ok".into()),
            ProviderEvent::Stop { reason: StopReason::EndTurn },
        ]]);
        let tools = Arc::new(ToolRegistry::new());
        let mut agent = Agent::new(Arc::new(provider), tools, AgentConfig::default())
            .with_session(session);

        assert_eq!(agent.session().len(), 1); // restored
        let _stream = agent.run("next".into()).await.unwrap();
        assert_eq!(agent.session().len(), 2); // restored + new user prompt
    }
}
```

**Note on the `max_turns` test:** The `vec![...; 20]` syntax won't work with non-Clone `ProviderEvent` easily. Simplify that test to use a 2-script provider and verify termination. Replace the test body:

```rust
    #[tokio::test]
    async fn agent_respects_max_turns() {
        // Provider emits tool call, then empty (no tool uses) → loop stops naturally.
        // With max_turns=1, loop stops after first turn regardless.
        let provider = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::ToolUseStart { id: "t1".into(), name: "upper".into() },
                ProviderEvent::ToolUseDelta { id: "t1".into(), partial_json: r#"{"text":"x"}"# },
                ProviderEvent::ToolUseEnd { id: "t1".into() },
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ],
        ]);
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(UpperEchoTool));
        let config = AgentConfig {
            max_turns: Some(1),
            ..Default::default()
        };
        let mut agent = Agent::new(Arc::new(provider), Arc::new(tools), config);

        let stream = agent.run("loop".into()).await.unwrap();
        let events = collect_events(stream);

        // With max_turns=1: turn 1 executes tool, turn 2 > max → MaxTurns
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done { reason: DoneReason::MaxTurns })));
    }
```

**Step 2: Update lib.rs**

```rust
//! yi-agent-core: agent loop, session management, and core trait definitions.

pub mod message;
pub mod tool;
pub mod provider;
pub mod agent;

// Re-export most-used types at crate root.
pub use agent::{Agent, AgentConfig, AgentError, AgentEvent, DoneReason, Session};
pub use message::{ContentBlock, ImageSource, Message, Role};
pub use provider::{GenParams, Provider, ProviderError, ProviderEvent, ProviderRequest, ProviderResponse, StopReason};
pub use tool::{Tool, ToolMetadata, ToolRegistry, ToolResult, ToolSchema, ToolSource};
```

**Step 3: Run tests**

Run: `cargo test -p yi-agent-core`
Expected: all tests pass

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "Implement Agent loop, Session, AgentEvent with parallel tool execution"
```

---

## Task 6: Verify Workspace Builds

**Files:** none modified

**Step 1: Build entire workspace**

Run: `cargo build`
Expected: BUILD SUCCESS for all 6 crates

**Step 2: Run all tests**

Run: `cargo test`
Expected: all tests pass, 0 failures

**Step 3: Run clippy**

Run: `cargo clippy -p yi-agent-core -- -D warnings`
Expected: no warnings

**Step 4: Commit (if any cleanup)**

If clippy produced warnings, fix them and commit:

```bash
git add -A
git commit -m "Fix clippy warnings in yi-agent-core"
```

---

## Task 7: Merge to Main

**Step 1: Switch to main**

```bash
git checkout main
git merge --no-ff feature/yi-agent-core-impl -m "Implement yi-agent-core: message/tool/provider/agent modules"
```

**Step 2: Verify main builds**

```bash
cargo build && cargo test
```

**Step 3: Push**

```bash
git push origin main
```

**Step 4: Clean up worktree**

```bash
git worktree remove .worktrees/yi-agent-core
git branch -d feature/yi-agent-core-impl
```

---

## Summary

| Task | Module | Tests |
|---|---|---|
| 1 | Cargo.toml deps | build only |
| 2 | message.rs | 6 tests |
| 3 | tool.rs | 7 tests |
| 4 | provider.rs | 5 tests |
| 5 | agent.rs | 6 tests |
| 6 | workspace verification | clippy + build |
| 7 | merge to main | — |

**Total: ~24 unit tests across 4 modules.**

Each task is self-contained, produces a commit, and follows TDD.
