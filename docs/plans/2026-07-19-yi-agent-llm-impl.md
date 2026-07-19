# yi-agent-llm AnthropicProvider Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement `AnthropicProvider` in `yi-agent-llm` that talks to Anthropic Messages API (streaming SSE) via the `Provider` trait defined in `yi-agent-core`.

**Architecture:** Pre-change `yi-agent-core` to add `model` field on `ProviderRequest`/`AgentConfig`. Then build `yi-agent-llm` with an `anthropic/` submodule containing `types.rs` (request types + `From<ProviderRequest>`), `error.rs` (HTTP status → `ProviderError`), `stream.rs` (SSE parser implementing `Stream<Item = Result<ProviderEvent>>`), and `client.rs` (`AnthropicProvider` + `Provider` impl). All HTTP responses are mocked via `wiremock` in tests — no real API calls.

**Tech Stack:** Rust 2024, `reqwest` 0.12 (rustls-tls, stream, json), `futures` 0.3, `async-trait` 0.1, `serde`/`serde_json`, `bytes` 1, `wiremock` 0.6 (dev).

**Reference design:** [`docs/plans/2026-07-19-yi-agent-llm-design.md`](./2026-07-19-yi-agent-llm-design.md)

**Working directory:** `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/yi-agent-llm`

All paths below are relative to the worktree root unless noted. Cargo commands run in `yi-agent-rs/`.

---

## Task 1: Add `model` field to `ProviderRequest` (core)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs` (struct `ProviderRequest` around line 13)
- Modify: `yi-agent-rs/crates/yi-agent-core/src/provider.rs` (4 test sites that build `ProviderRequest { ... }`)

**Step 1: Edit the struct**

Add a `pub model: String` field as the first field of `ProviderRequest`:

```rust
/// Request to a provider.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    /// Model identifier (e.g. "claude-sonnet-4-5"). Interpreted by the provider.
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenParams,
}
```

**Step 2: Update the 4 test sites in `provider.rs`**

In the `tests` module there are 4 places that construct `ProviderRequest { system: None, messages: vec![], tools: vec![], params: GenParams::default() }`. Add `model: "claude-sonnet-4-5".to_string(),` as the first field to each.

The 4 tests are:
- `call_accumulates_text_only`
- `call_accumulates_text_and_tool_use`
- `call_handles_stop_reason`
- `call_stream_yields_events`

**Step 3: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core`
Expected: all tests PASS (24 tests in workspace, 0 failed)

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/provider.rs
git commit -m "feat(core): add model field to ProviderRequest"
```

---

## Task 2: Add `model` field to `AgentConfig` (core)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (struct `AgentConfig` around line 49, `Default` impl around line 55, `ProviderRequest` construction around line 181)

**Step 1: Add `model` to `AgentConfig` struct**

```rust
pub struct AgentConfig {
    /// Model identifier passed to the provider (e.g. "claude-sonnet-4-5").
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub gen_params: GenParams,
}
```

**Step 2: Update `Default` impl**

```rust
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5".to_string(),
            system_prompt: None,
            max_turns: Some(100),
            gen_params: Default::default(),
        }
    }
}
```

**Step 3: Pass `model` when constructing `ProviderRequest`**

Find the `// 1. THINK` comment around line 180 and update the `ProviderRequest` construction:

```rust
// 1. THINK
let req = ProviderRequest {
    model: config.model.clone(),
    system: config.system_prompt.clone(),
    messages: messages.clone(),
    tools: tools.schemas(),
    params: config.gen_params.clone(),
};
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core`
Expected: all tests PASS

(Note: the test at line ~531 uses `AgentConfig { max_turns: Some(1), ..Default::default() }` — it inherits `model` from `Default`, no edit needed.)

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): add model field to AgentConfig, thread through to ProviderRequest"
```

---

## Task 3: Set up `yi-agent-llm` dependencies and module skeleton

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/mod.rs`
- Create: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/client.rs` (empty stub)
- Create: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs` (empty stub)
- Create: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs` (empty stub)
- Create: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/error.rs` (empty stub)

**Step 1: Replace `Cargo.toml` content**

```toml
[package]
name = "yi-agent-llm"
description = "LLM provider implementations"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[lib]
path = "src/lib.rs"

[dependencies]
yi-agent-core = { workspace = true }

# HTTP + 流式
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }
bytes = "1"
futures = "0.3"
async-trait = "0.1"
tokio = { version = "1", default-features = false }

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
wiremock = "0.6"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

**Step 2: Replace `src/lib.rs` content**

```rust
//! yi-agent-llm: LLM provider implementations.
//!
//! 依赖 `yi-agent-core` 的 `Provider` trait,初期实现 Anthropic Claude provider,
//! 架构上预留多 provider 扩展能力。

pub mod anthropic;

pub use anthropic::client::AnthropicProvider;
pub use anthropic::client::AnthropicProviderOpts;
```

**Step 3: Create `src/anthropic/mod.rs`**

```rust
//! Anthropic Messages API provider.

pub mod client;
pub mod error;
pub mod stream;
pub mod types;
```

**Step 4: Create empty stubs for the 4 submodules**

Each file gets only a doc comment placeholder:

`src/anthropic/client.rs`:
```rust
//! AnthropicProvider and Provider trait implementation.
```

`src/anthropic/types.rs`:
```rust
//! Anthropic API request/response types and conversion from core types.
```

`src/anthropic/stream.rs`:
```rust
//! SSE stream parser for Anthropic streaming responses.
```

`src/anthropic/error.rs`:
```rust
//! Error mapping from Anthropic HTTP responses to ProviderError.
```

**Step 5: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-llm`
Expected: compiles with no errors (warnings about unused imports are fine for now)

**Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/
git commit -m "feat(llm): set up dependencies and module skeleton for AnthropicProvider"
```

---

## Task 4: Implement `types.rs` — Anthropic request types

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs`

**Step 1: Write the types**

Replace the file content with:

```rust
//! Anthropic API request/response types and conversion from core types.

use serde::Serialize;
use serde_json::Value;

use yi_agent_core::{ContentBlock, ImageSource, Message, ProviderRequest, Role, ToolSchema};

/// Anthropic /v1/messages request body.
#[derive(Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<AnthropicTool>,
    #[serde(flatten)]
    pub params: AnthropicGenParams,
    /// Always true — we always stream.
    pub stream: bool,
}

#[derive(Serialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text { text: String },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<AnthropicContentBlock>,
        is_error: bool,
    },
    Image { source: AnthropicImageSource },
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum AnthropicImageSource {
    Base64 {
        r#type: String, // "base64"
        media_type: String,
        data: String,
    },
    Url {
        r#type: String, // "url"
        url: String,
    },
}

#[derive(Serialize, Default)]
pub struct AnthropicGenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl From<ToolSchema> for AnthropicTool {
    fn from(t: ToolSchema) -> Self {
        Self {
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        }
    }
}

impl From<ImageSource> for AnthropicImageSource {
    fn from(s: ImageSource) -> Self {
        match s {
            ImageSource::Base64 { media_type, data } => AnthropicImageSource::Base64 {
                r#type: "base64".to_string(),
                media_type,
                data,
            },
            ImageSource::Url(url) => AnthropicImageSource::Url {
                r#type: "url".to_string(),
                url,
            },
        }
    }
}

impl From<ContentBlock> for AnthropicContentBlock {
    fn from(b: ContentBlock) -> Self {
        match b {
            ContentBlock::Text(text) => AnthropicContentBlock::Text { text },
            ContentBlock::ToolUse { id, name, input } => AnthropicContentBlock::ToolUse { id, name, input },
            ContentBlock::ToolResult { tool_use_id, content, is_error } => AnthropicContentBlock::ToolResult {
                tool_use_id,
                content: content.into_iter().map(Into::into).collect(),
                is_error,
            },
            ContentBlock::Image { source } => AnthropicContentBlock::Image { source: source.into() },
        }
    }
}

impl From<AnthropicGenParams> for yi_agent_core::GenParams {
    fn from(p: AnthropicGenParams) -> Self {
        Self {
            temperature: p.temperature,
            max_tokens: p.max_tokens,
            top_p: p.top_p,
            stop_sequences: p.stop_sequences,
        }
    }
}

impl From<yi_agent_core::GenParams> for AnthropicGenParams {
    fn from(p: yi_agent_core::GenParams) -> Self {
        Self {
            temperature: p.temperature,
            max_tokens: p.max_tokens,
            top_p: p.top_p,
            stop_sequences: p.stop_sequences,
        }
    }
}

/// Role label mapping. `Role::Tool` and `Role::System` are special-cased elsewhere.
fn role_label(role: Role) -> &'static str {
    match role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

impl From<ProviderRequest> for AnthropicRequest {
    fn from(req: ProviderRequest) -> Self {
        // System messages are pulled out to the top-level `system` field.
        // All other messages (including Role::Tool) become `role: "user"` entries.
        let mut system_parts: Vec<String> = Vec::new();
        let mut messages: Vec<AnthropicMessage> = Vec::new();

        for m in req.messages {
            match m.role {
                Role::System => {
                    for block in m.content {
                        if let ContentBlock::Text(t) = block {
                            system_parts.push(t);
                        }
                    }
                }
                _ => {
                    let role = role_label(m.role).to_string();
                    let content: Vec<AnthropicContentBlock> =
                        m.content.into_iter().map(Into::into).collect();
                    messages.push(AnthropicMessage { role, content });
                }
            }
        }

        // If ProviderRequest.system was explicitly set, prepend it.
        if let Some(s) = req.system {
            system_parts.insert(0, s);
        }

        let system = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        };

        Self {
            model: req.model,
            messages,
            system,
            tools: req.tools.into_iter().map(Into::into).collect(),
            params: req.params.into(),
            stream: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::{GenParams, Message, ToolSchema};

    #[test]
    fn converts_simple_user_text_request() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![Message::user("hello")],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.model, "claude-sonnet-4-5");
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].role, "user");
        assert!(a.system.is_none());
        assert_eq!(a.tools.len(), 0);
        assert!(a.stream);
    }

    #[test]
    fn pulls_system_message_to_top_level() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![
                Message::system("be helpful"),
                Message::user("hi"),
            ],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.system.as_deref(), Some("be helpful"));
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].role, "user");
    }

    #[test]
    fn merges_system_field_and_system_message() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: Some("base prompt".to_string()),
            messages: vec![
                Message::system("extra instructions"),
                Message::user("hi"),
            ],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.system.as_deref(), Some("base prompt\n\nextra instructions"));
    }

    #[test]
    fn tool_role_serializes_as_user() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let a: AnthropicRequest = req.into();
        assert_eq!(a.messages[0].role, "user");
    }

    #[test]
    fn serializes_request_json_correctly() {
        let req = ProviderRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams {
                temperature: Some(0.5),
                max_tokens: Some(1024),
                ..Default::default()
            },
        };
        let a: AnthropicRequest = req.into();
        let json = serde_json::to_value(&a).unwrap();
        assert_eq!(json["model"], "claude-sonnet-4-5");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.5);
        assert_eq!(json["max_tokens"], 1024);
        // system should be absent (skip_serializing_if)
        assert!(json.get("system").is_none() || json["system"].is_null());
        // tools should be absent
        assert!(json.get("tools").is_none() || json["tools"].is_null());
    }
}
```

**Step 2: Run tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm types::tests`
Expected: 5 tests PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs
git commit -m "feat(llm): implement Anthropic request types and From<ProviderRequest>"
```

---

## Task 5: Implement `error.rs` — HTTP status mapping

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/error.rs`

**Step 1: Write the error mapping**

```rust
//! Error mapping from Anthropic HTTP responses to ProviderError.

use yi_agent_core::ProviderError;

/// Convert a non-2xx `reqwest::Response` into a `ProviderError`.
pub async fn map_status_error(resp: reqwest::Response) -> ProviderError {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    match status {
        401 | 403 => ProviderError::Auth(format!("{}: {}", status, body)),
        429 => ProviderError::RateLimited,
        400 | 422 => ProviderError::InvalidRequest(format!("{}: {}", status, body)),
        500..=599 => ProviderError::Server(format!("{}: {}", status, body)),
        _ => ProviderError::Server(format!("unexpected status {}: {}", status, body)),
    }
}

#[cfg(test)]
mod tests {
    // Unit tests for map_status_error require constructing a reqwest::Response
    // from a status + body. Use wiremock for end-to-end coverage instead.
    // See integration tests in client.rs.
}
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-llm`
Expected: compiles cleanly

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/error.rs
git commit -m "feat(llm): implement HTTP status -> ProviderError mapping"
```

---

## Task 6: Implement `stream.rs` — SSE parser (core logic)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs`

**Step 1: Write the SSE parser**

```rust
//! SSE stream parser for Anthropic streaming responses.
//!
//! Reads raw bytes from a `reqwest` response stream and emits `ProviderEvent`s.
//! Handles the standard SSE wire format: `event: <name>\n` / `data: <json>\n` / `\n` (event boundary).

use std::collections::HashMap;

use bytes::Bytes;
use futures::{Stream, StreamExt, stream::BoxStream};
use serde::Deserialize;
use serde_json::Value;

use yi_agent_core::{ProviderError, ProviderEvent, StopReason};

/// One SSE frame parsed from the byte stream.
struct SseFrame {
    event: String,
    data: String,
}

/// Parses raw byte chunks into SSE frames.
struct SseLineParser {
    buf: String,
    current_event: String,
    current_data_lines: Vec<String>,
}

impl SseLineParser {
    fn new() -> Self {
        Self {
            buf: String::new(),
            current_event: String::new(),
            current_data_lines: Vec::new(),
        }
    }

    /// Feed a chunk of bytes. Returns completed SSE frames.
    fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        let mut frames = Vec::new();

        loop {
            let Some(nl) = self.buf.find('\n') else {
                break;
            };
            let mut line = self.buf[..nl].to_string();
            self.buf = self.buf[nl + 1..].to_string();

            // Strip trailing \r if present (CRLF)
            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
                // Empty line = event boundary. Emit a frame if we have data.
                if !self.current_data_lines.is_empty() {
                    let event = std::mem::take(&mut self.current_event);
                    let data = std::mem::take(&mut self.current_data_lines).join("\n");
                    frames.push(SseFrame { event, data });
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("event:") {
                self.current_event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.current_data_lines.push(rest.trim().to_string());
            } else if line.starts_with(':') {
                // Comment, ignore.
            } else {
                // Unknown field — ignore per SSE spec.
            }
        }

        frames
    }
}

/// Converts a `Stream<Item = Result<Bytes, reqwest::Error>>` into a
/// `Stream<Item = Result<ProviderEvent, ProviderError>>`.
pub struct AnthropicStream<S> {
    line_parser: SseLineParser,
    inner: S,
    /// Maps content block index → tool_use_id (set on content_block_start, read on delta/stop).
    block_ids: HashMap<usize, String>,
}

impl<S> AnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            line_parser: SseLineParser::new(),
            inner,
            block_ids: HashMap::new(),
        }
    }

    /// Try to parse one SSE frame into a `ProviderEvent`.
    /// Returns `Ok(None)` for frames that don't emit an event (ping, message_start, etc).
    fn parse_frame(&mut self, frame: SseFrame) -> Result<Option<ProviderEvent>, ProviderError> {
        let data: Value = serde_json::from_str(&frame.data).map_err(|e| {
            ProviderError::Stream(format!("invalid SSE JSON: {e}; data: {}", frame.data))
        })?;

        let event_type = data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");

        match event_type {
            "content_block_start" => {
                let index = data
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ProviderError::Stream("content_block_start missing index".into()))?
                    as usize;
                let block = data.get("content_block").cloned().unwrap_or(Value::Null);
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");

                if block_type == "tool_use" {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.block_ids.insert(index, id.clone());
                    return Ok(Some(ProviderEvent::ToolUseStart { id, name }));
                }
                Ok(None)
            }
            "content_block_delta" => {
                let index = data
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ProviderError::Stream("content_block_delta missing index".into()))?
                    as usize;
                let delta = data.get("delta").cloned().unwrap_or(Value::Null);
                let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");

                match delta_type {
                    "text_delta" => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        Ok(Some(ProviderEvent::TextDelta(text)))
                    }
                    "input_json_delta" => {
                        let partial_json = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let id = self
                            .block_ids
                            .get(&index)
                            .cloned()
                            .ok_or_else(|| {
                                ProviderError::Stream(format!(
                                    "input_json_delta for unknown block index {index}"
                                ))
                            })?;
                        Ok(Some(ProviderEvent::ToolUseDelta { id, partial_json }))
                    }
                    _ => Ok(None),
                }
            }
            "content_block_stop" => {
                let index = data
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| ProviderError::Stream("content_block_stop missing index".into()))?
                    as usize;
                if let Some(id) = self.block_ids.remove(&index) {
                    Ok(Some(ProviderEvent::ToolUseEnd { id }))
                } else {
                    Ok(None)
                }
            }
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
            "message_start" | "message_stop" | "ping" => Ok(None),
            "error" => {
                let msg = data
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                Err(ProviderError::Stream(msg.to_string()))
            }
            _ => Ok(None),
        }
    }
}

impl<S> Stream for AnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;

        loop {
            // First: try to emit any pending frame from the current buffer (already handled by feed
            // when it returns frames — but here we need to re-check if buf has a full frame).
            // Actually our feed() returns frames immediately, so we drive the inner stream.

            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ProviderError::Network(e.to_string()))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let frames = self.line_parser.feed(&chunk);
                    for frame in frames {
                        match self.parse_frame(frame) {
                            Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                            Ok(None) => continue,
                            Err(e) => return Poll::Ready(Some(Err(e))),
                        }
                    }
                    // No event emitted from this chunk, keep polling.
                }
            }
        }
    }
}

/// Convenience: box a stream for return from `Provider::call_stream`.
pub fn boxed<S>(stream: AnthropicStream<S>) -> BoxStream<'static, Result<ProviderEvent, ProviderError>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    stream.boxed()
}

#[derive(Deserialize, Debug)]
struct SseEventEnvelope {
    #[serde(default)]
    event: String,
    #[serde(default)]
    data: String,
}
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-llm`
Expected: compiles (may have dead-code warnings for `boxed` and `AnthropicStream::new` until used)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs
git commit -m "feat(llm): implement SSE parser for Anthropic streaming responses"
```

---

## Task 7: Write unit tests for SSE parser

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs` (add tests module)

**Step 1: Append tests module**

Add at the end of `stream.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use futures::stream::StreamExt;

    /// Helper: feed bytes through an AnthropicStream and collect all events.
    async fn collect_events(chunks: Vec<&[u8]>) -> Vec<Result<ProviderEvent, ProviderError>> {
        let inner = stream::iter(chunks.into_iter().map(|c| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(c))));
        let mut s = AnthropicStream::new(inner);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item);
        }
        out
    }

    fn sse(body: &str) -> String {
        body.to_string()
    }

    #[tokio::test]
    async fn parses_text_delta_sequence() {
        let body = sse(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
             event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
             event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let bytes = body.into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;

        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
        assert!(matches!(&events[1], ProviderEvent::TextDelta(t) if t == " world"));
        assert!(matches!(&events[2], ProviderEvent::ToolUseEnd { .. }) == false); // no tool use
        assert!(matches!(&events[3], ProviderEvent::Stop { reason: StopReason::EndTurn }));
    }

    #[tokio::test]
    async fn parses_tool_use_with_partial_json() {
        let body = sse(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read\",\"input\":{}}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"main.rs\\\"}\"}}\n\n\
             event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
        );
        let bytes = body.into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;

        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 4);
        match &events[0] {
            ProviderEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(name, "read");
            }
            _ => panic!("expected ToolUseStart"),
        }
        match &events[1] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(partial_json, "{\"path\":");
            }
            _ => panic!("expected ToolUseDelta"),
        }
        match &events[2] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "toolu_01");
                assert_eq!(partial_json, "\"main.rs\"}");
            }
            _ => panic!("expected ToolUseDelta"),
        }
        match &events[3] {
            ProviderEvent::ToolUseEnd { id } => assert_eq!(id, "toolu_01"),
            _ => panic!("expected ToolUseEnd"),
        }
    }

    #[tokio::test]
    async fn handles_chunk_splits_inside_line() {
        // Simulate the case where a SSE event is split across multiple byte chunks.
        let full = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".to_string();
        // Split into "event: content_block_delta\nda" and the rest.
        let split_at = "event: content_block_delta\nda".len();
        let part1 = &full.as_bytes()[..split_at];
        let part2 = &full.as_bytes()[split_at..];

        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
    }

    #[tokio::test]
    async fn maps_stop_reasons_correctly() {
        for (input, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("tool_use", StopReason::Other("tool_use".to_string())),
        ] {
            let body = format!(
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{}\"}}}}\n\n",
                input
            );
            let bytes = body.into_bytes();
            let events = collect_events(vec![bytes.as_slice()]).await;
            let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
            assert_eq!(events.len(), 1, "input: {}", input);
            match &events[0] {
                ProviderEvent::Stop { reason } => assert_eq!(*reason, expected),
                _ => panic!("expected Stop for input {}", input),
            }
        }
    }

    #[tokio::test]
    async fn ignores_ping_and_message_start() {
        let body = sse(
            "event: ping\ndata: {\"type\":\"ping\"}\n\n\
             event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n",
        );
        let bytes = body.into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn surfaces_sse_error_event() {
        let body = sse("event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n");
        let bytes = body.into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ProviderError::Stream(msg)) => assert_eq!(msg, "overloaded"),
            _ => panic!("expected Stream error"),
        }
    }
}
```

**Step 2: Run tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm stream::tests`
Expected: 6 tests PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs
git commit -m "test(llm): add SSE parser unit tests"
```

---

## Task 8: Implement `client.rs` — AnthropicProvider

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/client.rs`

**Step 1: Write the provider**

```rust
//! AnthropicProvider and Provider trait implementation.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::env;

use yi_agent_core::{Provider, ProviderError, ProviderEvent, ProviderRequest};

use crate::anthropic::error::map_status_error;
use crate::anthropic::stream::AnthropicStream;
use crate::anthropic::stream::boxed as box_stream;
use crate::anthropic::types::AnthropicRequest;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Configuration for constructing an [`AnthropicProvider`].
///
/// All fields optional — resolved with the following priority:
/// 1. Explicit value here
/// 2. Environment variable (`ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`)
/// 3. Built-in default
#[derive(Default)]
pub struct AnthropicProviderOpts {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_version: Option<String>,
    pub timeout: Option<Duration>,
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    api_version: String,
    timeout: Duration,
}

impl AnthropicProvider {
    /// Construct a provider.
    ///
    /// `api_key` resolution: `opts.api_key` > `ANTHROPIC_API_KEY` env var > error.
    pub fn new(opts: AnthropicProviderOpts) -> Result<Self, ProviderError> {
        let api_key = opts
            .api_key
            .or_else(|| env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| {
                ProviderError::Auth("ANTHROPIC_API_KEY not set and no api_key provided".into())
            })?;

        let base_url = opts
            .base_url
            .or_else(|| env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let api_version = opts
            .api_version
            .unwrap_or_else(|| DEFAULT_API_VERSION.to_string());

        let timeout = opts.timeout.unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url,
            api_key,
            api_version,
            timeout,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body: AnthropicRequest = req.into();

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.api_version)
            .header("content-type", "application/json")
            .json(&body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(map_status_error(resp).await);
        }

        let byte_stream = resp.bytes_stream();
        let event_stream = AnthropicStream::new(byte_stream);
        Ok(box_stream(event_stream))
    }
}
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-llm`
Expected: compiles cleanly

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/client.rs
git commit -m "feat(llm): implement AnthropicProvider with streaming"
```

---

## Task 9: Write integration tests with wiremock

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/tests/integration.rs`

**Step 1: Write the integration tests**

```rust
//! Integration tests for AnthropicProvider against a wiremock mock server.

use std::time::Duration;

use futures::stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{ContentBlock, GenParams, Message, Provider, ProviderError, ProviderEvent, ProviderRequest, Role};
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts};

const SSE_TEXT_STREAM: &str = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

const SSE_TOOL_USE_STREAM: &str = "\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"main.rs\\\"}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n";

const SSE_MAX_TOKENS_STREAM: &str = "\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"truncated\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n";

fn provider_for(server: &MockServer) -> AnthropicProvider {
    AnthropicProvider::new(AnthropicProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("test-key".to_string()),
        api_version: None,
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction")
}

fn simple_request() -> ProviderRequest {
    ProviderRequest {
        model: "claude-sonnet-4-5".to_string(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    }
}

async fn collect_events(stream: futures::stream::BoxStream<'static, ProviderEvent>) -> Vec<ProviderEvent> {
    let mut s = stream;
    let mut out = Vec::new();
    while let Some(e) = s.next().await {
        out.push(e);
    }
    out
}

#[tokio::test]
async fn streams_text_deltas_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(SSE_TEXT_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| if let ProviderEvent::TextDelta(t) = e { Some(t.clone()) } else { None })
        .collect();
    assert_eq!(text, "Hello world");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Stop { .. })));
}

#[tokio::test]
async fn streams_tool_use_deltas_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(SSE_TOOL_USE_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(e, ProviderEvent::ToolUseStart { id, name } if id == "toolu_01" && name == "read")));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ProviderEvent::ToolUseDelta { .. }))
            .count(),
        2
    );
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::ToolUseEnd { id } if id == "toolu_01")));
}

#[tokio::test]
async fn maps_stop_reason_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(SSE_MAX_TOKENS_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Stop { reason: yi_agent_core::StopReason::MaxTokens })));
}

#[tokio::test]
async fn returns_auth_error_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let err = provider.call_stream(simple_request()).await.unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)));
}

#[tokio::test]
async fn returns_rate_limited_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let err = provider.call_stream(simple_request()).await.unwrap_err();
    assert!(matches!(err, ProviderError::RateLimited));
}

#[tokio::test]
async fn returns_server_error_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let err = provider.call_stream(simple_request()).await.unwrap_err();
    assert!(matches!(err, ProviderError::Server(_)));
}

#[tokio::test]
async fn returns_auth_error_when_no_api_key() {
    // Make sure the env var isn't picked up by accident.
    std::env::remove_var("ANTHROPIC_API_KEY");
    let err = AnthropicProvider::new(AnthropicProviderOpts {
        api_key: None,
        ..Default::default()
    })
    .unwrap_err();
    assert!(matches!(err, ProviderError::Auth(_)));
}

#[tokio::test]
async fn reads_api_key_from_env() {
    std::env::set_var("ANTHROPIC_API_KEY", "env-key");
    let provider = AnthropicProvider::new(AnthropicProviderOpts::default()).expect("env key picked up");
    std::env::remove_var("ANTHROPIC_API_KEY");
    // Sanity check: we just verify construction succeeded; the actual key is private.
    let _ = format!("{}", provider.api_key.len() > 0);
}

#[tokio::test]
async fn sends_system_message_as_top_level_system() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains("\"system\":\"be helpful\""))
        .respond_with(
            ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = ProviderRequest {
        model: "claude-sonnet-4-5".to_string(),
        system: None,
        messages: vec![Message::system("be helpful"), Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    };
    let _ = provider.call_stream(req).await.expect("stream ok");
    // The mock only responds 200 if the body contained `"system":"be helpful"`.
}
```

**Step 2: Run tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test integration`
Expected: all tests PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/tests/integration.rs
git commit -m "test(llm): add wiremock integration tests for AnthropicProvider"
```

---

## Task 10: Final verification — full workspace test + clippy

**Files:** None modified.

**Step 1: Run full workspace tests**

Run: `cd yi-agent-rs && cargo test --workspace`
Expected: all tests PASS (core 24 + llm types tests 5 + stream tests 6 + integration tests 9 = 44+)

**Step 2: Run clippy**

Run: `cd yi-agent-rs && cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings (fix any that appear)

**Step 3: Verify no unused code**

Run: `cd yi-agent-rs && cargo check -p yi-agent-llm --all-targets`
Expected: clean

**Step 4: Commit any cleanup (if needed)**

```bash
git add -A
git commit -m "chore(llm): cleanup"
```

(If no cleanup needed, skip this step.)

---

## Task 11: Update progress docs

**Files:**
- Modify: `docs/project-management/yi-agent-core.md` (or create if not exists; add "AnthropicProvider 实现" feature)
- Modify: `docs/project-management/README.md` (update index if a new module row is needed)

**Step 1: Check current progress docs**

Run: `ls docs/project-management/`

**Step 2: Add/update entries**

If `yi-agent-llm.md` does not exist, create it with:

```markdown
# yi-agent-llm

## 模块说明

LLM provider 实现。基于 `yi-agent-core` 的 `Provider` trait,初期实现 Anthropic Claude provider,架构上预留多 provider 扩展能力。

## 范围边界

**做什么:**
- Anthropic Messages API (streaming SSE) 接入
- Provider 配置(base_url / api_key / api_version / timeout 多来源优先级)
- SSE 流解析 + ProviderEvent 映射
- HTTP 错误码到 ProviderError 的映射

**不做什么:**
- 不做重试逻辑(YAGNI)
- 不做流断连重连(YAGNI)
- 不做 OpenAI / Ollama provider(后续)
- 不做 Bedrock / Vertex AI 适配(后续)
- 不做 tracing 日志(YAGNI)

## Features

- [x] AnthropicProvider 设计 — [设计](../plans/2026-07-19-yi-agent-llm-design.md)
- [x] AnthropicProvider 实现(core `model` 字段 + types + stream + client + 测试)
```

Update `docs/project-management/README.md` to add `yi-agent-llm` to the module index:

```markdown
- **yi-agent-llm** → [详情](./yi-agent-llm.md)
  - [x] AnthropicProvider 设计
  - [x] AnthropicProvider 实现
```

**Step 3: Commit**

```bash
git add docs/project-management/
git commit -m "docs: update progress for yi-agent-llm"
```

---

## Summary

| Task | What |
|---|---|
| 1 | Add `model` field to `ProviderRequest` (core) |
| 2 | Add `model` field to `AgentConfig` (core) |
| 3 | Set up llm deps + module skeleton |
| 4 | Implement `types.rs` (Anthropic types + From conversion) |
| 5 | Implement `error.rs` (HTTP status → ProviderError) |
| 6 | Implement `stream.rs` (SSE parser) |
| 7 | SSE parser unit tests |
| 8 | Implement `client.rs` (AnthropicProvider + Provider impl) |
| 9 | wiremock integration tests |
| 10 | Full workspace test + clippy |
| 11 | Update progress docs |
