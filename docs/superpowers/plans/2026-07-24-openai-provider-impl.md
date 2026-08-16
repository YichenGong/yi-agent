# OpenAI Provider Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `OpenaiProvider` to `yi-agent-llm` that talks to OpenAI Chat Completions API (streaming SSE + tool calling) via the existing `Provider` trait, and wire up provider selection in `yi-agent` CLI via `--provider` flag.

**Architecture:** Mirror the existing `anthropic/` module layout with a new `openai/` submodule in `yi-agent-llm` (`types.rs`, `error.rs`, `stream.rs`, `client.rs`). Add `provider` field to `Config` in `yi-agent`. `main.rs` dispatches on `config.provider` to construct either `AnthropicProvider` or `OpenaiProvider`. All HTTP responses mocked via `wiremock` in tests — no real API calls.

**Tech Stack:** Rust 2024, `reqwest` 0.12 (rustls-tls, stream, json), `futures` 0.3, `async-trait` 0.1, `serde`/`serde_json`, `bytes` 1, `wiremock` 0.6 (dev).

**Reference design:** [`docs/plans/2026-07-24-openai-provider-design.md`](./2026-07-24-openai-provider-design.md)

**Working directory:** `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/openai-provider`

All paths below are relative to the worktree root unless noted. Cargo commands run in `yi-agent-rs/`.

---

## Task 1: Add `provider` field to `Config` (yi-agent)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs` (struct `Config` around line 9, `Cli` struct around line 22, `load()` around line 60)
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs` (tests module)

**Step 1: Add `provider` to `Config` struct**

```rust
pub struct Config {
    pub provider: String,
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: u32,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub compact_threshold: u32,
    pub compact_keep_turns: u32,
}
```

**Step 2: Add `--provider` to `Cli` struct**

Add as the first field:

```rust
pub struct Cli {
    /// LLM provider: "anthropic" or "openai" (overrides YI_AGENT_PROVIDER)
    #[arg(long)]
    pub provider: Option<String>,

    /// API endpoint URL (overrides MODEL_API_URL)
    #[arg(long)]
    pub api_url: Option<String>,
    // ... rest unchanged
}
```

**Step 3: Resolve `provider` in `load()`**

Add at the top of `load()`, before `api_key`:

```rust
let provider = cli
    .provider
    .clone()
    .or_else(|| std::env::var("YI_AGENT_PROVIDER").ok())
    .unwrap_or_else(|| "anthropic".to_string());
```

Then in the `Ok(Config { ... })` block, add `provider,` as the first field.

**Step 4: Update existing tests**

Every test constructs `Cli { ... }` and some construct `Config` or check its fields. Add `provider: None` to every `Cli { ... }` literal in the tests module. The tests are:
- `load_requires_api_key` — add `provider: None,`
- `load_loads_from_cli_args` — add `provider: Some("openai".into()),` and optionally `assert_eq!(config.provider, "openai");`
- `load_defaults_api_url_and_model` — add `provider: None,`
- `load_includes_compact_defaults` — add `provider: None,`
- `load_rejects_nonexistent_workdir` — add `provider: None,`

**Step 5: Add a new test for provider default and override**

```rust
#[test]
fn load_defaults_provider_to_anthropic() {
    let cli = Cli {
        provider: None,
        api_url: None,
        api_key: Some("test-key".into()),
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        compact_threshold: None,
        compact_keep_turns: None,
    };
    let config = load(&cli).unwrap();
    assert_eq!(config.provider, "anthropic");
}
```

**Step 6: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib config`
Expected: all tests PASS

**Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "feat(yi-agent): add provider field to Config with --provider flag"
```

---

## Task 2: Create `openai` module skeleton (yi-agent-llm)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/src/openai/mod.rs`
- Create: `yi-agent-rs/crates/yi-agent-llm/src/openai/error.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/lib.rs`

**Step 1: Create `openai/mod.rs`**

```rust
//! OpenAI Chat Completions API provider.

pub mod client;
pub mod error;
pub mod stream;
pub mod types;
```

**Step 2: Create `openai/error.rs`**

```rust
//! Error mapping from OpenAI HTTP responses to ProviderError.

use yi_agent_core::ProviderError;

/// Map a `reqwest::Response` into a `ProviderError` based on its HTTP status code.
///
/// | Status            | Variant                      |
/// |-------------------|------------------------------|
/// | 401, 403          | `Auth`                       |
/// | 429               | `RateLimited`                |
/// | 400, 422          | `InvalidRequest`             |
/// | 500..=599         | `Server`                     |
/// | other             | `Server` (unexpected status) |
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
```

**Step 3: Create empty placeholder files**

Create `openai/client.rs`, `openai/stream.rs`, `openai/types.rs` each with just a module doc comment:

```rust
//! OpenAI provider client — populated in later tasks.
```

(`stream.rs` gets `//! SSE stream parser for OpenAI streaming responses.` etc.)

**Step 4: Update `lib.rs`**

```rust
//! yi-agent-llm: LLM provider implementations.
//!
//! 依赖 `yi-agent-core` 的 `Provider` trait,实现 Anthropic Claude 和 OpenAI provider,
//! 架构上预留多 provider 扩展能力。

pub mod anthropic;
pub mod openai;

pub use anthropic::client::AnthropicProvider;
pub use anthropic::client::AnthropicProviderOpts;
```

**Step 5: Run build to verify it compiles**

Run: `cd yi-agent-rs && cargo build -p yi-agent-llm`
Expected: PASS (compiles with empty placeholder modules)

**Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/openai/ yi-agent-rs/crates/yi-agent-llm/src/lib.rs
git commit -m "feat(yi-agent-llm): scaffold openai provider module"
```

---

## Task 3: Implement OpenAI request types and `From<ProviderRequest>`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`

**Step 1: Write the failing tests**

Add at the bottom of `types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::{GenParams, Message};

    #[test]
    fn converts_simple_user_text_request() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::user("hello")],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.model, "gpt-4o");
        assert_eq!(o.messages.len(), 1);
        assert_eq!(o.messages[0].role, "user");
        assert!(o.tools.is_empty());
        assert!(o.stream);
    }

    #[test]
    fn system_message_becomes_role_system() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::system("be helpful"), Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 2);
        assert_eq!(o.messages[0].role, "system");
        match &o.messages[0].content {
            OpenaiContent::Text(t) => assert_eq!(t, "be helpful"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn merges_provider_request_system_field() {
        // ProviderRequest.system is a separate field; it should be prepended
        // as a system message before the messages array.
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: Some("base prompt".to_string()),
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 2);
        assert_eq!(o.messages[0].role, "system");
        match &o.messages[0].content {
            OpenaiContent::Text(t) => assert_eq!(t, "base prompt"),
            _ => panic!("expected Text content"),
        }
    }

    #[test]
    fn tool_use_block_maps_to_tool_call() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::assistant(vec![ContentBlock::ToolUse {
                id: "call_01".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/a"}),
            }])],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        assert_eq!(o.messages.len(), 1);
        assert_eq!(o.messages[0].role, "assistant");
        match &o.messages[0].content {
            OpenaiContent::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_01");
                assert_eq!(calls[0].function.name, "read");
                assert_eq!(calls[0].function.arguments, r#"{"path":"/a"}"#);
            }
            _ => panic!("expected ToolCalls content"),
        }
    }

    #[test]
    fn tool_result_maps_to_role_tool_message() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "call_01".into(),
            content: vec![ContentBlock::Text("ok".into())],
            is_error: false,
        };
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::tool_results(vec![result])],
            tools: vec![],
            params: GenParams::default(),
        };
        let o: OpenaiRequest = req.into();
        // Tool results become separate "tool" role messages.
        assert_eq!(o.messages.len(), 1);
        assert_eq!(o.messages[0].role, "tool");
        match &o.messages[0].content {
            OpenaiContent::ToolResult { tool_call_id, content } => {
                assert_eq!(tool_call_id, "call_01");
                assert!(content.contains("ok"));
            }
            _ => panic!("expected ToolResult content"),
        }
    }

    #[test]
    fn serializes_request_json_correctly() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![ToolSchema {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            params: GenParams {
                temperature: Some(0.5),
                max_tokens: Some(1024),
                ..Default::default()
            },
        };
        let o: OpenaiRequest = req.into();
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["stream"], true);
        assert_eq!(json["temperature"], 0.5);
        assert_eq!(json["max_tokens"], 1024);
        // stop_sequences should serialize as "stop"
        assert!(json.get("stop").is_none() || json["stop"].is_null());
        // tools should be present
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "read");
        // stream_options should request usage
        assert_eq!(json["stream_options"]["include_usage"], true);
    }

    #[test]
    fn stop_sequences_serialize_as_stop() {
        let req = ProviderRequest {
            model: "gpt-4o".to_string(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: vec![],
            params: GenParams {
                stop_sequences: Some(vec!["END".into()]),
                ..Default::default()
            },
        };
        let o: OpenaiRequest = req.into();
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["stop"], serde_json::json!(["END"]));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::types`
Expected: FAIL — types not defined yet

**Step 3: Write the type definitions**

Replace the placeholder content in `types.rs` with:

```rust
//! OpenAI Chat Completions API request types and conversion from core types.

use serde::Serialize;
use serde_json::Value;

use yi_agent_core::{ContentBlock, ProviderRequest, Role, ToolSchema};

/// OpenAI /v1/chat/completions request body.
#[derive(Serialize)]
pub struct OpenaiRequest {
    pub model: String,
    pub messages: Vec<OpenaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OpenaiTool>,
    /// Always true — we always stream.
    pub stream: bool,
    /// Request usage in the final stream chunk.
    pub stream_options: OpenaiStreamOptions,
    #[serde(flatten)]
    pub params: OpenaiGenParams,
}

#[derive(Serialize)]
pub struct OpenaiStreamOptions {
    pub include_usage: bool,
}

#[derive(Serialize)]
pub struct OpenaiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<OpenaiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum OpenaiContent {
    Text(String),
    ToolCalls(Vec<OpenaiToolCall>),
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Serialize)]
pub struct OpenaiToolCall {
    pub id: String,
    pub r#type: String, // "function"
    pub function: OpenaiToolCallFunction,
}

#[derive(Serialize)]
pub struct OpenaiToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize, Default)]
pub struct OpenaiGenParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop")]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct OpenaiTool {
    pub r#type: String, // "function"
    pub function: OpenaiToolFunction,
}

#[derive(Serialize)]
pub struct OpenaiToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl From<ToolSchema> for OpenaiTool {
    fn from(t: ToolSchema) -> Self {
        Self {
            r#type: "function".to_string(),
            function: OpenaiToolFunction {
                name: t.name,
                description: t.description,
                parameters: t.input_schema,
            },
        }
    }
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

/// Extract text from content blocks, concatenating all Text blocks.
fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

impl From<ProviderRequest> for OpenaiRequest {
    fn from(req: ProviderRequest) -> Self {
        let mut messages: Vec<OpenaiMessage> = Vec::new();

        // ProviderRequest.system -> prepend as system message
        if let Some(s) = req.system {
            messages.push(OpenaiMessage {
                role: "system".to_string(),
                name: None,
                content: Some(OpenaiContent::Text(s)),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        for m in req.messages {
            match m.role {
                Role::System => {
                    for block in &m.content {
                        if let ContentBlock::Text(t) = block {
                            messages.push(OpenaiMessage {
                                role: "system".to_string(),
                                name: None,
                                content: Some(OpenaiContent::Text(t.clone())),
                                tool_calls: None,
                                tool_call_id: None,
                            });
                        }
                    }
                }
                Role::User => {
                    let text = extract_text(&m.content);
                    messages.push(OpenaiMessage {
                        role: "user".to_string(),
                        name: None,
                        content: Some(OpenaiContent::Text(text)),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                Role::Assistant => {
                    // Split into text + tool_calls
                    let text_parts: Vec<String> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect();
                    let tool_calls: Vec<OpenaiToolCall> = m
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => Some(OpenaiToolCall {
                                id: id.clone(),
                                r#type: "function".to_string(),
                                function: OpenaiToolCallFunction {
                                    name: name.clone(),
                                    arguments: input.to_string(),
                                },
                            }),
                            _ => None,
                        })
                        .collect();

                    let content = if !text_parts.is_empty() {
                        Some(OpenaiContent::Text(text_parts.join("")))
                    } else {
                        None
                    };

                    messages.push(OpenaiMessage {
                        role: "assistant".to_string(),
                        name: None,
                        content,
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                    });
                }
                Role::Tool => {
                    // Each ToolResult block becomes a separate tool-role message
                    for block in m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = block
                        {
                            let text = extract_text(&content);
                            let body = if is_error {
                                format!("error: {}", text)
                            } else {
                                text
                            };
                            messages.push(OpenaiMessage {
                                role: "tool".to_string(),
                                name: None,
                                content: Some(OpenaiContent::ToolResult {
                                    tool_call_id: tool_use_id,
                                    content: body,
                                }),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id.clone()),
                            });
                        }
                    }
                }
            }
        }

        Self {
            model: req.model,
            messages,
            tools: req.tools.into_iter().map(Into::into).collect(),
            stream: true,
            stream_options: OpenaiStreamOptions { include_usage: true },
            params: OpenaiGenParams {
                temperature: req.params.temperature,
                max_tokens: req.params.max_tokens,
                top_p: req.params.top_p,
                stop_sequences: req.params.stop_sequences,
            },
        }
    }
}
```

Note: the `role_label` function is used above — keep it for clarity even though the `From` impl uses string literals directly (it's clearer to read the match arms). Actually remove the `role_label` function since we inline the strings. Wait — I'll keep the code as written above since each arm uses explicit string literals and `role_label` is unused. Let me remove it to avoid dead code warnings.

Actually, re-reading my impl: I do NOT use `role_label` in the `From` impl (I use string literals). So remove `role_label` entirely. The tests don't reference it.

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::types`
Expected: all tests PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs
git commit -m "feat(yi-agent-llm): add OpenAI request types with From<ProviderRequest>"
```

---

## Task 4: Implement OpenAI SSE stream parser

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs`

This is the most complex piece. OpenAI's streaming format differs from Anthropic's: tool calls arrive as `delta.tool_calls[N]` where `N` is an index, arguments arrive as incremental JSON fragments, and termination is signaled by `data: [DONE]` or `finish_reason` in the last chunk.

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use futures::stream::StreamExt;

    /// Helper: feed bytes through an OpenaiStream and collect all events.
    async fn collect_events(chunks: Vec<&[u8]>) -> Vec<Result<ProviderEvent, ProviderError>> {
        let inner = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, reqwest::Error>(Bytes::copy_from_slice(c))),
        );
        let mut s = OpenaiStream::new(inner);
        let mut out = Vec::new();
        while let Some(item) = s.next().await {
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn parses_text_delta_sequence() {
        let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // 3 events: TextDelta("Hello"), TextDelta(" world"), Stop{EndTurn}
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
        assert!(matches!(&events[1], ProviderEvent::TextDelta(t) if t == " world"));
        assert!(matches!(&events[2], ProviderEvent::Stop { reason: StopReason::EndTurn }));
    }

    #[tokio::test]
    async fn parses_tool_call_with_incremental_arguments() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"main.rs\\\"}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // Expected: ToolUseStart, 2x ToolUseDelta, ToolUseEnd, Stop{EndTurn}
        // (finish_reason "tool_calls" maps to EndTurn since the agent loop continues)
        assert_eq!(events.len(), 5, "events: {:?}", events);
        match &events[0] {
            ProviderEvent::ToolUseStart { id, name } => {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "read");
            }
            _ => panic!("expected ToolUseStart"),
        }
        match &events[1] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "call_abc");
                assert_eq!(partial_json, "{\"path\":");
            }
            _ => panic!("expected ToolUseDelta 1"),
        }
        match &events[2] {
            ProviderEvent::ToolUseDelta { id, partial_json } => {
                assert_eq!(id, "call_abc");
                assert_eq!(partial_json, "\"main.rs\"}");
            }
            _ => panic!("expected ToolUseDelta 2"),
        }
        match &events[3] {
            ProviderEvent::ToolUseEnd { id } => assert_eq!(id, "call_abc"),
            _ => panic!("expected ToolUseEnd"),
        }
        assert!(matches!(&events[4], ProviderEvent::Stop { reason: StopReason::EndTurn }));
    }

    #[tokio::test]
    async fn handles_chunk_splits_inside_line() {
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n".to_string();
        let split_at = "data: {\"cho".len();
        let part1 = &full.as_bytes()[..split_at];
        let part2 = &full.as_bytes()[split_at..];
        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "Hello"));
    }

    #[tokio::test]
    async fn handles_chunk_split_inside_multibyte_utf8() {
        // "你好" is 6 bytes in UTF-8. Split inside the first character.
        let text = "你好";
        let full = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
            text
        );
        let full_bytes = full.into_bytes();
        let text_bytes = text.as_bytes();
        let text_start = full_bytes.windows(text_bytes.len())
            .position(|w| w == text_bytes)
            .expect("text bytes should be present");
        let split_at = text_start + 3;
        let part1 = &full_bytes[..split_at];
        let part2 = &full_bytes[split_at..];
        let events = collect_events(vec![part1, part2]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ProviderEvent::TextDelta(t) => assert_eq!(t, "你好"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[tokio::test]
    async fn maps_finish_reasons_correctly() {
        for (input, expected) in [
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            ("tool_calls", StopReason::EndTurn),
        ] {
            let body = format!(
                "data: {{\"choices\":[{{\"finish_reason\":\"{}\"}}]}}\n\ndata: [DONE]\n\n",
                input
            );
            let bytes = body.into_bytes();
            let events = collect_events(vec![bytes.as_slice()]).await;
            let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
            // For "tool_calls" we also get a ToolUseEnd, so find the Stop event.
            let stop = events.iter().find(|e| matches!(e, ProviderEvent::Stop { .. }));
            assert!(stop.is_some(), "input: {}, events: {:?}", input, events);
            match stop.unwrap() {
                ProviderEvent::Stop { reason } => assert_eq!(*reason, expected, "input: {}", input),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn parses_done_marker_without_finish_reason() {
        // Some API responses end with [DONE] without a finish_reason chunk.
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // TextDelta + Stop{EndTurn}
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "hi"));
        assert!(matches!(&events[1], ProviderEvent::Stop { reason: StopReason::EndTurn }));
    }

    #[tokio::test]
    async fn parses_usage_in_final_chunk() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // TextDelta, Usage, Stop
        assert_eq!(events.len(), 3);
        match &events[1] {
            ProviderEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
                assert_eq!(u.cache_creation_input_tokens, None);
                assert_eq!(u.cache_read_input_tokens, None);
            }
            _ => panic!("expected Usage event, got: {:?}", events[1]),
        }
    }

    #[tokio::test]
    async fn ignores_empty_delta_chunks() {
        // Some chunks have an empty delta object (role-only on first chunk).
        let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // Only TextDelta + Stop (role-only chunk emits nothing)
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn multiple_tool_calls_in_one_response() {
        // Two tool calls with different indices in one response.
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"write\",\"arguments\":\"\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n\
             data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        let events: Vec<ProviderEvent> = events.into_iter().filter_map(|r| r.ok()).collect();
        // Expected: StartA, StartB, DeltaA, DeltaB, EndA, EndB, Stop
        // (On finish_reason "tool_calls", we emit ToolUseEnd for all active tool calls)
        assert_eq!(events.len(), 7, "events: {:?}", events);
        assert!(matches!(&events[0], ProviderEvent::ToolUseStart { id, name } if id == "call_a" && name == "read"));
        assert!(matches!(&events[1], ProviderEvent::ToolUseStart { id, name } if id == "call_b" && name == "write"));
        assert!(matches!(&events[2], ProviderEvent::ToolUseDelta { id, .. } if id == "call_a"));
        assert!(matches!(&events[3], ProviderEvent::ToolUseDelta { id, .. } if id == "call_b"));
        assert!(matches!(&events[4], ProviderEvent::ToolUseEnd { id } if id == "call_a"));
        assert!(matches!(&events[5], ProviderEvent::ToolUseEnd { id } if id == "call_b"));
        assert!(matches!(&events[6], ProviderEvent::Stop { reason: StopReason::EndTurn }));
    }

    #[tokio::test]
    async fn surfaces_invalid_json_as_error() {
        let body = "data: not valid json\n\n";
        let bytes = body.to_string().into_bytes();
        let events = collect_events(vec![bytes.as_slice()]).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Err(ProviderError::Stream(_))));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::stream`
Expected: FAIL — `OpenaiStream` not defined

**Step 3: Implement `OpenaiStream`**

The key insight: we reuse the `SseLineParser` byte-buffering approach from `anthropic/stream.rs` (buffer raw bytes, decode at line boundaries). But the frame parsing logic is different: OpenAI has no `event:` field, only `data: <json>` and `data: [DONE]`.

```rust
//! SSE stream parser for OpenAI Chat Completions streaming responses.
//!
//! Reads raw bytes from a `reqwest` response stream and emits `ProviderEvent`s.
//! Handles SSE wire format: `data: <json>\n\n` with `data: [DONE]` terminator.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use yi_agent_core::provider::TokenUsage;
use yi_agent_core::{ProviderError, ProviderEvent, StopReason};

/// One SSE frame parsed from the byte stream.
struct SseFrame {
    data: String,
}

/// Parses raw byte chunks into SSE frames.
/// Buffers bytes (not String) so multi-byte UTF-8 split across chunks is safe.
struct SseLineParser {
    buf: Vec<u8>,
    current_data_lines: Vec<String>,
}

impl SseLineParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            current_data_lines: Vec::new(),
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();

        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            let line_bytes = &line_bytes[..line_bytes.len().saturating_sub(1)];

            let mut line = match std::str::from_utf8(line_bytes) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    frames.push(SseFrame {
                        data: format!("{{\"__parse_error\":\"invalid UTF-8 in SSE line: {e}\"}}"),
                    });
                    continue;
                }
            };

            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
                if !self.current_data_lines.is_empty() {
                    let data = std::mem::take(&mut self.current_data_lines).join("\n");
                    frames.push(SseFrame { data });
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                self.current_data_lines.push(rest.trim().to_string());
            } else if line.starts_with(':') {
                // Comment, ignore.
            }
            // Other fields (event:, id:) — not used by OpenAI streaming.
        }

        frames
    }
}

/// Converts a byte stream into `Result<ProviderEvent, ProviderError>`.
pub struct OpenaiStream<S> {
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<SseFrame>,
    /// Maps tool_calls index -> (tool_call_id, name). Set on first appearance,
    /// used to attach the id to subsequent argument deltas.
    tool_calls: HashMap<usize, (String, String)>,
    /// Tracks which tool call indices have already had ToolUseStart emitted.
    started_indices: std::collections::HashSet<usize>,
}

impl<S> OpenaiStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            line_parser: SseLineParser::new(),
            inner,
            pending_frames: VecDeque::new(),
            tool_calls: HashMap::new(),
            started_indices: std::collections::HashSet::new(),
        }
    }

    /// Parse one SSE frame's data into zero or more ProviderEvents.
    /// Returns Ok(None) for frames that produce no event (empty delta, etc).
    /// Returns Ok(Some(events)) — note: a single frame can produce multiple
    /// events (e.g. finish_reason "tool_calls" emits ToolUseEnd for all active
    /// calls + a Stop event).
    fn parse_frame(&mut self, frame: SseFrame) -> Result<Vec<ProviderEvent>, ProviderError> {
        // Check for [DONE] marker
        if frame.data == "[DONE]" {
            return Ok(vec![ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            }]);
        }

        // Check for synthetic parse error
        if let Some(rest) = frame.data.strip_prefix("{\"__parse_error\":") {
            let msg = rest.trim_end_matches('}');
            return Err(ProviderError::Stream(msg.to_string()));
        }

        let data: Value = serde_json::from_str(&frame.data).map_err(|e| {
            ProviderError::Stream(format!("invalid SSE JSON: {e}; data: {}", frame.data))
        })?;

        let mut events = Vec::new();

        // Check for usage (final chunk before [DONE])
        if let Some(usage) = data.get("usage") {
            if !usage.is_null() {
                let prompt_tokens = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
                let completion_tokens = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0) as u32;
                events.push(ProviderEvent::Usage(TokenUsage {
                    input_tokens: prompt_tokens,
                    output_tokens: completion_tokens,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }));
            }
        }

        // Process choices
        if let Some(choices) = data.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                // Check finish_reason first — if tool_calls, emit ToolUseEnd for all active calls.
                if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                    match finish {
                        "stop" => {
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::EndTurn,
                            });
                        }
                        "length" => {
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::MaxTokens,
                            });
                        }
                        "tool_calls" => {
                            // Emit ToolUseEnd for all active tool calls, then Stop.
                            let ids: Vec<String> = self
                                .tool_calls
                                .values()
                                .map(|(id, _)| id.clone())
                                .collect();
                            // Sort by index to preserve order
                            // Actually we need to sort by index — but we only have the map.
                            // Collect indices sorted, then get ids.
                            let mut indices: Vec<usize> = self.tool_calls.keys().copied().collect();
                            indices.sort();
                            for idx in indices {
                                if let Some((id, _)) = self.tool_calls.remove(&idx) {
                                    events.push(ProviderEvent::ToolUseEnd { id });
                                }
                            }
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::EndTurn,
                            });
                        }
                        other => {
                            events.push(ProviderEvent::Stop {
                                reason: StopReason::Other(other.to_string()),
                            });
                        }
                    }
                }

                // Process delta
                if let Some(delta) = choice.get("delta") {
                    // Text content
                    if let Some(content) = delta.get("content").and_then(Value::as_str) {
                        if !content.is_empty() {
                            events.push(ProviderEvent::TextDelta(content.to_string()));
                        }
                    }

                    // Tool calls
                    if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tool_calls {
                            let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;

                            // First appearance: has id + function.name
                            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                                let name = tc
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                self.tool_calls.insert(index, (id.to_string(), name.clone()));
                                events.push(ProviderEvent::ToolUseStart {
                                    id: id.to_string(),
                                    name,
                                });
                                self.started_indices.insert(index);
                            }

                            // Argument delta
                            if let Some(args) = tc
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(Value::as_str)
                            {
                                if !args.is_empty() {
                                    if let Some((id, _)) = self.tool_calls.get(&index) {
                                        events.push(ProviderEvent::ToolUseDelta {
                                            id: id.clone(),
                                            partial_json: args.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(events)
    }
}

impl<S> Stream for OpenaiStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<ProviderEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Drain pending frames. Each frame may produce multiple events,
            // so we buffer them in a sub-queue.
            while let Some(frame) = self.pending_frames.pop_front() {
                match self.parse_frame(frame) {
                    Ok(events) => {
                        // If multiple events, we need to buffer them.
                        // We'll push them back as individual items by
                        // re-queuing. But pending_frames is VecDeque<SseFrame>,
                        // not events. So we use a separate event buffer.
                        // Simpler: drain events one at a time using a field.
                        // For now, if events.len() == 1, return it.
                        // If > 1, we need a different approach.
                        // Let's add a pending_events buffer to the struct.
                        // Actually, to avoid restructuring, let's reverse and
                        // push the remaining events as synthetic frames.
                        // No — that's hacky. Let me restructure to use
                        // pending_events: VecDeque<ProviderEvent>.
                        // I'll fix this in the implementation.
                        if events.is_empty() {
                            continue;
                        }
                        // Return first, buffer rest
                        // TODO: add pending_events field
                        // For now, this won't compile correctly — see fix below
                        let mut iter = events.into_iter();
                        let first = iter.next().unwrap();
                        // We can't store the rest without a field. This is a design
                        // issue. Let me add the field in the struct definition.
                        // This is why we test first — the test failure will guide us.
                        return Poll::Ready(Some(Ok(first)));
                        // BUG: remaining events are lost!
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                }
            }

            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ProviderError::Network(e.to_string()))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let frames = self.line_parser.feed(&chunk);
                    self.pending_frames.extend(frames);
                }
            }
        }
    }
}
```

**Important — design fix:** The `parse_frame` can return multiple events (e.g. `finish_reason: "tool_calls"` emits multiple `ToolUseEnd` + `Stop`). The `Stream` impl needs a `pending_events: VecDeque<ProviderEvent>` buffer to emit them one at a time. Let me correct the struct:

Add to the struct:
```rust
pending_events: VecDeque<ProviderEvent>,
```

And in the `poll_next` loop, BEFORE draining `pending_frames`, drain `pending_events`:

```rust
if let Some(event) = self.pending_events.pop_front() {
    return Poll::Ready(Some(Ok(event)));
}
```

And when `parse_frame` returns multiple events, push all of them to `pending_events` and `continue` the loop (the next iteration will pop the first one).

And for errors from `parse_frame`, return `Some(Err(e))` directly.

Here's the corrected `poll_next`:

```rust
fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    loop {
        // 1. Drain pending events first (from a multi-event frame)
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        // 2. Drain pending frames
        while let Some(frame) = self.pending_frames.pop_front() {
            match self.parse_frame(frame) {
                Ok(events) => {
                    if events.is_empty() {
                        continue;
                    }
                    // Buffer all events, loop will drain them one at a time.
                    for ev in events {
                        self.pending_events.push_back(ev);
                    }
                    break; // go back to top of loop to drain pending_events
                }
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
        }

        // 3. Poll inner stream for more bytes
        match self.inner.poll_next_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(Err(e))) => {
                return Poll::Ready(Some(Err(ProviderError::Network(e.to_string()))));
            }
            Poll::Ready(Some(Ok(chunk))) => {
                let frames = self.line_parser.feed(&chunk);
                self.pending_frames.extend(frames);
            }
        }
    }
}
```

And `OpenaiStream::new` must initialize `pending_events: VecDeque::new()`.

Make sure to include `use std::collections::HashSet;` (already available via `std::collections::HashSet`).

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::stream`
Expected: all tests PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs
git commit -m "feat(yi-agent-llm): add OpenAI SSE stream parser with tool call support"
```

---

## Task 5: Implement `OpenaiProvider` client and `Provider` trait impl

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs`

**Step 1: Write the failing test (integration-level, but we'll do it in Task 6)**

This task only implements the client struct + `Provider` impl. We'll test it via wiremock in Task 6. For now, just implement and verify it compiles.

**Step 2: Implement `OpenaiProvider`**

```rust
//! OpenaiProvider and Provider trait implementation.

use std::env;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use yi_agent_core::{Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason};

use crate::openai::error::map_status_error;
use crate::openai::stream::OpenaiStream;
use crate::openai::types::OpenaiRequest;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Configuration for constructing an [`OpenaiProvider`].
///
/// All fields optional — resolved with the following priority:
/// 1. Explicit value here
/// 2. Environment variable (`OPENAI_BASE_URL`, `OPENAI_API_KEY`)
/// 3. Built-in default
#[derive(Default)]
pub struct OpenaiProviderOpts {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: Option<Duration>,
}

pub struct OpenaiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    timeout: Duration,
}

impl std::fmt::Debug for OpenaiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenaiProvider")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl OpenaiProvider {
    /// Construct a provider.
    ///
    /// `api_key` resolution: `opts.api_key` > `OPENAI_API_KEY` env var > error.
    pub fn new(opts: OpenaiProviderOpts) -> Result<Self, ProviderError> {
        let api_key = opts
            .api_key
            .or_else(|| env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| {
                ProviderError::Auth("OPENAI_API_KEY not set and no api_key provided".into())
            })?;

        let base_url = opts
            .base_url
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let timeout = opts
            .timeout
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url,
            api_key,
            timeout,
        })
    }
}

#[async_trait]
impl Provider for OpenaiProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body: OpenaiRequest = req.into();

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
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
        let event_stream = OpenaiStream::new(byte_stream);

        let mapped = event_stream
            .map(|item| match item {
                Ok(event) => event,
                Err(e) => ProviderEvent::Stop {
                    reason: StopReason::Other(format!("stream error: {e}")),
                },
            })
            .scan(Some(()), |state, event| {
                let yield_event = state.is_some();
                if matches!(event, ProviderEvent::Stop { .. }) {
                    *state = None;
                }
                std::future::ready(if yield_event { Some(event) } else { None })
            });
        Ok(mapped.boxed())
    }
}
```

**Step 3: Re-export from `lib.rs`**

```rust
pub mod anthropic;
pub mod openai;

pub use anthropic::client::AnthropicProvider;
pub use anthropic::client::AnthropicProviderOpts;
pub use openai::client::OpenaiProvider;
pub use openai::client::OpenaiProviderOpts;
```

**Step 4: Run build to verify it compiles**

Run: `cd yi-agent-rs && cargo build -p yi-agent-llm`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/src/openai/client.rs yi-agent-rs/crates/yi-agent-llm/src/lib.rs
git commit -m "feat(yi-agent-llm): add OpenaiProvider with Provider trait impl"
```

---

## Task 6: Integration tests for `OpenaiProvider`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/tests/openai_integration.rs`

**Step 1: Write the integration tests**

```rust
//! Integration tests for OpenaiProvider against a wiremock mock server.

use std::sync::Mutex;
use std::time::Duration;

use futures::stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use yi_agent_core::{
    ContentBlock, GenParams, Message, Provider, ProviderError, ProviderEvent, ProviderRequest,
    ProviderResponse, StopReason,
};
use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

/// Serialize tests that touch the `OPENAI_API_KEY` env var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const SSE_TEXT_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";

const SSE_TOOL_USE_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_01\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"main.rs\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";

const SSE_MAX_TOKENS_STREAM: &str = "\
data: {\"choices\":[{\"delta\":{\"content\":\"truncated\"}}]}\n\n\
data: {\"choices\":[{\"finish_reason\":\"length\"}]}\n\n\
data: [DONE]\n\n";

fn provider_for(server: &MockServer) -> OpenaiProvider {
    OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(server.uri()),
        api_key: Some("test-key".to_string()),
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction")
}

fn simple_request() -> ProviderRequest {
    ProviderRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    }
}

async fn collect_events(
    stream: futures::stream::BoxStream<'static, ProviderEvent>,
) -> Vec<ProviderEvent> {
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
        .and(path("/v1/chat/completions"))
        .and(header("Authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_TEXT_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    let text: String = events.iter().filter_map(|e| {
        if let ProviderEvent::TextDelta(t) = e { Some(t.clone()) } else { None }
    }).collect();
    assert_eq!(text, "Hello world");
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::Stop { .. })));
}

#[tokio::test]
async fn streams_tool_use_deltas_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_TOOL_USE_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::ToolUseStart { id, name } if id == "call_01" && name == "read"
    )));
    assert_eq!(
        events.iter().filter(|e| matches!(e, ProviderEvent::ToolUseDelta { .. })).count(),
        2
    );
    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::ToolUseEnd { id } if id == "call_01"
    )));
    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::Stop { reason: StopReason::EndTurn }
    )));
}

#[tokio::test]
async fn maps_stop_reason_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_MAX_TOKENS_STREAM),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;
    assert!(events.iter().any(|e| matches!(
        e, ProviderEvent::Stop { reason: StopReason::MaxTokens }
    )));
}

#[tokio::test]
async fn returns_auth_error_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}

#[tokio::test]
async fn returns_rate_limited_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::RateLimited)));
}

#[tokio::test]
async fn returns_server_error_on_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::Server(_))));
}

#[tokio::test]
async fn returns_auth_error_when_no_api_key() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let result = OpenaiProvider::new(OpenaiProviderOpts {
        api_key: None,
        ..Default::default()
    });
    assert!(matches!(result, Err(ProviderError::Auth(_))));
}

#[tokio::test]
async fn reads_api_key_from_env() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "env-key");
    }
    let provider = OpenaiProvider::new(OpenaiProviderOpts::default()).expect("env key picked up");
    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }
    let _ = provider;
}

#[tokio::test]
async fn returns_invalid_request_error_on_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let result = provider.call_stream(simple_request()).await;
    assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
}

#[tokio::test]
async fn trims_trailing_slash_in_base_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let provider = OpenaiProvider::new(OpenaiProviderOpts {
        base_url: Some(format!("{}/", server.uri())),
        api_key: Some("test-key".to_string()),
        timeout: Some(Duration::from_secs(5)),
    })
    .expect("provider construction");
    let _ = provider.call_stream(simple_request()).await.expect("stream ok");
}

#[tokio::test]
async fn call_accumulates_full_response_end_to_end() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello \"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\n\
                data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_42\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\\\"rust\\\"}\"}}]}}]}\n\n\
                data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}]}\n\n\
                data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let resp: ProviderResponse = provider.call(simple_request()).await.expect("call ok");

    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.content.len(), 2, "expected text + tool_use");
    match &resp.content[0] {
        ContentBlock::Text(t) => assert_eq!(t, "Hello world"),
        other => panic!("expected Text, got {other:?}"),
    }
    match &resp.content[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_42");
            assert_eq!(name, "search");
            assert_eq!(input, &serde_json::json!({"q":"rust"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[tokio::test]
async fn sends_system_message_as_role_system() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::body_string_contains("\"role\":\"system\""))
        .and(wiremock::matchers::body_string_contains("be helpful"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n"),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let req = ProviderRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message::system("be helpful"), Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    };
    let _ = provider.call_stream(req).await.expect("stream ok");
}

#[tokio::test]
async fn mid_stream_error_becomes_terminal_stop() {
    let server = MockServer::start().await;
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n\
                data: not valid json\n\n\
                data: {\"choices\":[{\"delta\":{\"content\":\"should-not-arrive\"}}]}\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server);
    let stream = provider.call_stream(simple_request()).await.expect("stream ok");
    let events = collect_events(stream).await;

    // First: text delta
    assert!(matches!(&events[0], ProviderEvent::TextDelta(t) if t == "partial"));
    // Second: error -> terminal Stop
    match &events[1] {
        ProviderEvent::Stop { reason: StopReason::Other(msg) } => {
            assert!(msg.contains("invalid SSE JSON") || msg.contains("stream error"));
        }
        _ => panic!("expected Stop{{Other}}, got: {:?}", events[1]),
    }
    assert_eq!(events.len(), 2, "stream should terminate after Stop");
}
```

**Step 2: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test openai_integration`
Expected: all tests PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-llm/tests/openai_integration.rs
git commit -m "test(yi-agent-llm): add OpenaiProvider integration tests with wiremock"
```

---

## Task 7: Wire up provider selection in `main.rs`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Step 1: Update `main.rs` to dispatch on `config.provider`**

Replace the provider construction block (lines 24-30) with:

```rust
let provider: Arc<dyn Provider> = match config.provider.as_str() {
    "anthropic" => Arc::new(yi_agent_llm::AnthropicProvider::new(
        yi_agent_llm::AnthropicProviderOpts {
            base_url: Some(config.api_url.clone()),
            api_key: Some(config.api_key.clone()),
            ..Default::default()
        },
    )?),
    "openai" => Arc::new(yi_agent_llm::OpenaiProvider::new(
        yi_agent_llm::OpenaiProviderOpts {
            base_url: Some(config.api_url.clone()),
            api_key: Some(config.api_key.clone()),
            ..Default::default()
        },
    )?),
    other => anyhow::bail!(
        "unknown provider '{}': expected 'anthropic' or 'openai'",
        other
    ),
};
```

**Step 2: Update the default `api_url` and `model` in `config.rs`**

Currently the defaults are Anthropic-specific. Make them provider-aware:

In `load()`, replace the `api_url` resolution with:

```rust
let provider = cli.provider.clone()
    .or_else(|| std::env::var("YI_AGENT_PROVIDER").ok())
    .unwrap_or_else(|| "anthropic".to_string());

let default_api_url = match provider.as_str() {
    "openai" => "https://api.openai.com",
    _ => "https://api.anthropic.com",
};
let default_model = match provider.as_str() {
    "openai" => "gpt-4o",
    _ => "claude-sonnet-4-20250514",
};

let api_url = cli.api_url.clone()
    .or_else(|| std::env::var("MODEL_API_URL").ok())
    .unwrap_or_else(|| default_api_url.to_string());

let model = cli.model.clone()
    .or_else(|| std::env::var("YI_AGENT_MODEL").ok())
    .unwrap_or_else(|| default_model.to_string());
```

Note: `provider` must be resolved BEFORE `api_url` and `model` since their defaults depend on it. Move the `provider` resolution to the top of `load()`.

**Step 3: Update the `load_defaults_api_url_and_model` test**

```rust
#[test]
fn load_defaults_api_url_and_model() {
    let cli = Cli {
        provider: None,
        api_url: None,
        api_key: Some("test-key".into()),
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        compact_threshold: None,
        compact_keep_turns: None,
    };
    let config = load(&cli).unwrap();
    assert_eq!(config.provider, "anthropic");
    assert_eq!(config.api_url, "https://api.anthropic.com");
    assert_eq!(config.model, "claude-sonnet-4-20250514");
    assert_eq!(config.max_turns, 20);
}

#[test]
fn load_defaults_openai_provider() {
    let cli = Cli {
        provider: Some("openai".into()),
        api_url: None,
        api_key: Some("test-key".into()),
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        compact_threshold: None,
        compact_keep_turns: None,
    };
    let config = load(&cli).unwrap();
    assert_eq!(config.provider, "openai");
    assert_eq!(config.api_url, "https://api.openai.com");
    assert_eq!(config.model, "gpt-4o");
}
```

**Step 4: Run all tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent`
Expected: all tests PASS

**Step 5: Run full workspace tests**

Run: `cd yi-agent-rs && cargo test`
Expected: all tests PASS across all crates

**Step 6: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "feat(yi-agent): wire up provider selection in main with --provider flag"
```

---

## Task 8: Final verification and cleanup

**Step 1: Run clippy**

Run: `cd yi-agent-rs && cargo clippy --all-targets -- -D warnings`
Expected: no warnings

**Step 2: Run formatter**

Run: `cd yi-agent-rs && cargo fmt --all`

**Step 3: Check for formatting changes**

Run: `cd yi-agent-rs && git diff --exit-code`
Expected: no diff (or only formatting fixes)

**Step 4: Run full test suite one final time**

Run: `cd yi-agent-rs && cargo test`
Expected: all tests PASS

**Step 5: Commit any formatting fixes**

```bash
git add -A
git commit -m "style: cargo fmt"
```

**Step 6: Move the design doc**

The design doc was created at `yi-agent-rs/docs/plans/2026-07-24-openai-provider-design.md`. Move it to the canonical location:

```bash
mv yi-agent-rs/docs/plans/2026-07-24-openai-provider-design.md docs/plans/
rmdir yi-agent-rs/docs/plans 2>/dev/null; rmdir yi-agent-rs/docs 2>/dev/null
```

(Note: the worktree's `docs/plans/` may not exist; create it if needed.)

---

## Summary

| Task | Description |
|------|-------------|
| 1 | Add `provider` field to `Config` + `--provider` CLI flag |
| 2 | Create `openai` module skeleton in `yi-agent-llm` |
| 3 | Implement OpenAI request types + `From<ProviderRequest>` (TDD) |
| 4 | Implement OpenAI SSE stream parser with tool call support (TDD) |
| 5 | Implement `OpenaiProvider` + `Provider` trait impl |
| 6 | Integration tests with `wiremock` (TDD) |
| 7 | Wire up provider selection in `main.rs` + provider-aware defaults |
| 8 | Final verification: clippy, fmt, full test suite |

**Key design decisions:**
- OpenAI `finish_reason: "tool_calls"` maps to `StopReason::EndTurn` (the agent loop continues after tool calls; `EndTurn` signals "model yielded, now execute tools")
- `stream_options: { include_usage: true }` to get token usage in the final chunk
- Tool results mapped to `role: "tool"` messages (OpenAI format), each as a separate message
- Provider-aware defaults: `--provider openai` changes default `api_url` and `model`
