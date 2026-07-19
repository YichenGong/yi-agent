# yi-agent-llm Implementation Design

**Date:** 2026-07-19
**Status:** Approved (brainstormed 2026-07-19)
**Scope:** `yi-agent-rs/crates/yi-agent-llm`

## Goal

在 `yi-agent-llm` 中实现首个 LLM provider — `AnthropicProvider`,通过 `yi-agent-core` 的 `Provider` trait 接入 Claude Messages API (流式 SSE)。架构上预留多 provider 扩展能力。

## Decisions (brainstorming 结论)

| # | 决策点 | 选择 | 理由 |
|---|---|---|---|
| 1 | 首个 provider | Anthropic Claude | 消息模型已对齐,映射层最薄,SSE 成熟 |
| 2 | API key 来源 | 多来源(优先级) | 显式参数 > 环境变量 > 默认值;兼顾测试与生产 |
| 3 | HTTP 客户端 | reqwest + hyper | 主流,流式支持好,生态丰富 |
| 4 | SSE 解析 | 自己写(最小实现) | Anthropic SSE 格式简单,~30 行可解析,避免依赖 |
| 5 | API 端点 | base_url 可配 | 方便测试 + 后续接 Bedrock/Vertex |
| 6 | 模型名传递 | 请求级指定 | `ProviderRequest.model` 字段,每次调用可切换模型 |
| 7 | 超时/重试 | 只设 timeout(60s),不重试 | YAGNI;重试逻辑由调用方决定 |
| 8 | 错误映射 | 复用 core 已有 ProviderError variants | `Auth/RateLimited/InvalidRequest/Server/Network/Stream` 已覆盖 |
| 9 | 生成参数 | core 传 `GenParams`,Provider 转换 | 保持 provider-agnostic |
| 10 | 测试策略 | wiremock mock server | CI 可跑,不消耗 token |
| 11 | 代码结构 | `anthropic/` 子目录 | 为未来多 provider 留结构 |
| 12 | 流断连 | 不做重连 | YAGNI;调用方处理 |
| 13 | 依赖范围 | 最小依赖集 | 不引入 thiserror/tracing,复用 core 的 |
| 14 | 请求构造 | `From<ProviderRequest> for AnthropicRequest` | 职责清晰,转换可测试 |
| 15 | 认证头 | `x-api-key` + `anthropic-version` | Anthropic 官方格式 |
| 16 | ContentBlock 转换 | Provider 负责转换 | 隔离 Anthropic 格式变化 |

## Architecture

### 模块结构

```
yi-agent-rs/crates/yi-agent-llm/
├── Cargo.toml
└── src/
    ├── lib.rs                          # 模块导出 + pub use
    └── anthropic/
        ├── mod.rs                      # pub mod client/types/stream/error;
        ├── client.rs                   # AnthropicProvider + Provider impl
        ├── types.rs                    # AnthropicRequest/Message/ContentBlock/Tool + From<ProviderRequest>
        ├── stream.rs                   # SseParser + Stream<Item=Result<ProviderEvent>>
        └── error.rs                   # map_status_error(reqwest::Response) -> ProviderError
```

### 数据流

```
ProviderRequest
  → types.rs::From → AnthropicRequest(serde 序列化)
  → client.rs(reqwest POST /v1/messages, stream=true)
  → reqwest::Response::bytes_stream()
  → stream.rs(逐块拼 SSE event)
  → ProviderEvent(TextDelta/ToolUseStart/Delta/End/Stop)
  → 返回 BoxStream<ProviderEvent> 给 agent loop
```

## Core 侧的必要改动

`yi-agent-llm` 实现前,需先改 `yi-agent-core`(已恢复原状,待实现阶段再动):

- `ProviderRequest` 加 `pub model: String`
- `AgentConfig` 加 `pub model: String`(default = `"claude-sonnet-4-5"`)
- `agent.rs` 透传 `model` 到 `ProviderRequest`
- core 测试构造处加 `model` 字段

## Types & Conversion (`types.rs`)

```rust
/// Anthropic /v1/messages 请求体
#[derive(Serialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<AnthropicTool>,
    #[serde(flatten)]
    pub params: AnthropicGenParams,
    pub stream: bool,  // 固定 true
}

#[derive(Serialize)]
pub struct AnthropicMessage {
    pub role: String,           // "user" / "assistant"
    pub content: Vec<AnthropicContentBlock>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: Vec<AnthropicContentBlock>, is_error: bool },
    Image { source: AnthropicImageSource },
}

#[derive(Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
```

### 转换规则 (`From<ProviderRequest> for AnthropicRequest`)

- `req.params.temperature/max_tokens/top_p/stop_sequences` → 透传,`None` 用 `skip_serializing_if` 跳过
- `Message::role == Role::Tool` → 序列化成 `"user"` (Anthropic 要求 tool result 放在 user 消息里)
- `Message::role == Role::System` → 不进 `messages`,挪到顶层 `system` 字段(若多条 system 拼接)
- `ContentBlock` 字段对齐,1:1 映射
- `ToolSchema` → `AnthropicTool` 1:1

## AnthropicProvider (`client.rs`)

```rust
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,      // 默认 "https://api.anthropic.com"
    api_key: String,
    api_version: String,   // 默认 "2023-06-01"
    timeout: Duration,     // 默认 60s
}

impl AnthropicProvider {
    /// 优先级:显式参数 > 环境变量 > 默认值
    /// base_url: opts.base_url OR ANTHROPIC_BASE_URL OR "https://api.anthropic.com"
    /// api_key: opts.api_key OR ANTHROPIC_API_KEY (缺则返回 Err)
    /// api_version: opts.api_version OR "2023-06-01"
    /// timeout: opts.timeout OR 60s
    pub fn new(opts: AnthropicProviderOpts) -> Result<Self, ProviderError> { ... }
}

#[derive(Default)]
pub struct AnthropicProviderOpts {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_version: Option<String>,
    pub timeout: Option<Duration>,
}
```

### Provider trait 实现

```rust
#[async_trait]
impl Provider for AnthropicProvider {
    async fn call_stream(&self, req: ProviderRequest) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let body: AnthropicRequest = AnthropicRequest::from(req);
        let resp = self.client
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

        let event_stream = SseParser::new(resp.bytes_stream())
            .map(|item| item.map_err(|e| ProviderError::Stream(e.to_string())));
        Ok(event_stream.boxed())
    }
}
```

### 设计要点

- `api_version` 可配:Anthropic 偶尔升级 version,写死就要改代码
- `timeout` 默认 60s (保守值)
- `api_key` 缺失返回 `ProviderError::Auth("ANTHROPIC_API_KEY not set")`
- `reqwest::Client` 内部连接池复用,单例即可

## SSE Parsing (`stream.rs`)

### Anthropic SSE 格式

```
event: message_start
data: {"type":"message_start","message":{...}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"main.rs\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

event: message_stop
data: {"type":"message_stop"}
```

### SseParser 设计

```rust
pub struct SseParser {
    buf: String,           // 行缓冲(拼接未完成的行)
    current_event: String, // 当前 event 字段值
    current_data: String,  // 当前 data 字段值(可能多行)
}

impl SseParser {
    pub fn new<S>(byte_stream: S) -> Self
    where S: Stream<Item = Result<Bytes, reqwest::Error>>
    {
        Self { buf: String::new(), current_event: String::new(), current_data: String::new() }
    }
}

// 核心:实现 Stream<Item = Result<ProviderEvent, ParseError>>
// 每次 bytes_stream 来一块数据 → 追加到 buf → 按行扫描
//   - "event: xxx"  → 记到 current_event
//   - "data: xxx"   → 记到 current_data
//   - 空行          → 一个完整 SSE event,parse current_data 为 JSON,
//                     根据 type 字段决定映射成哪个 ProviderEvent
```

### event → ProviderEvent 映射

| Anthropic event | ProviderEvent |
|---|---|
| `content_block_delta` + `delta.type == "text_delta"` | `TextDelta(delta.text)` |
| `content_block_start` + `content_block.type == "tool_use"` | `ToolUseStart { id, name }` |
| `content_block_delta` + `delta.type == "input_json_delta"` | `ToolUseDelta { id, partial_json }` (id 从 content_block_start 缓存) |
| `content_block_stop` | `ToolUseEnd { id }` (id 从 start 缓存) |
| `message_delta` + `delta.stop_reason` 存在 | `Stop { reason }` (映射 end_turn/max_tokens/stop_sequence → StopReason) |
| `message_stop` | (无,流自然结束) |
| `ping` / `message_start` / `error`(SSE 层) | (忽略或转 ProviderError::Stream) |

### 关键设计

- `tool_use_id` 在 `content_block_start` 收到后要**缓存**,因为后续 `content_block_delta` 和 `content_block_stop` 不带 id(只带 `index`)
- `ProviderEvent::ToolUseDelta` 要求 `id` — 用 `index` 反查缓存的 id
- `input_json_delta` 的 `partial_json` 是片段,直接透传,core 的 `accumulate_stream` 已经会拼接

## Error Handling (`error.rs`)

```rust
/// 把 reqwest::Response (非 2xx) 转成 ProviderError
async fn map_status_error(resp: reqwest::Response) -> ProviderError {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    match status {
        401 | 403 => ProviderError::Auth(format!("{}: {}", status, body)),
        429       => ProviderError::RateLimited,
        400 | 422 => ProviderError::InvalidRequest(format!("{}: {}", status, body)),
        500..=599 => ProviderError::Server(format!("{}: {}", status, body)),
        _         => ProviderError::Server(format!("unexpected status {}: {}", status, body)),
    }
}
```

## Testing

用 `wiremock` mock HTTP server,不联真实 API。

### 测试覆盖

- `streams_text_deltas_correctly` — 纯文本流
- `streams_tool_use_deltas_correctly` — 工具调用流
- `returns_auth_error_on_401` — 401 → Auth
- `returns_rate_limited_on_429` — 429 → RateLimited
- `returns_server_error_on_500` — 500 → Server
- `parses_stop_reasons_end_turn_max_tokens_stop_sequence` — 三种 stop_reason 映射
- `streams_partial_json_deltas_for_tool_input` — 工具 input 分片
- `handles_multiple_content_blocks_in_one_response` — text + tool_use 混合
- SSE 断行测试:把 SSE 切成 `["event: xxx\nda", "ta: {...}\n\n"]` 这种,parsed 结果应跟整块一致
- Message 转换:`Role::Tool` → `"user"`;多条 system 合并到顶层 `system`;嵌套 `ContentBlock::ToolResult`

## Dependencies

### `yi-agent-llm/Cargo.toml`

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

[dependencies]
yi-agent-core = { workspace = true }

# HTTP + 流式
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls"] }
bytes = "1"
futures = "0.3"
async-trait = "0.1"
tokio = { version = "1", default-features = false }   # 不拉 features,core 已启用

# 序列化
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
wiremock = "0.6"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### 设计要点

- `reqwest` 关 default features,启用 `json` + `stream` + `rustls-tls`(避免 OpenSSL 依赖,简化交叉编译)
- `tokio` 在 `[dependencies]` 里不拉 features(运行时由调用方提供),只在 `[dev-dependencies]` 里为测试拉 macros + rt-multi-thread
- 不引入 `thiserror` — `ProviderError` 已经在 core 用 thiserror 定义,这里只复用
- 不引入 `tracing` — YAGNI
- `bytes` 是 `reqwest::Response::bytes_stream()` 的返回类型需要的

## Implementation Order

(供 writing-plans 参考,非本设计文档约束)

1. `types.rs` — 定义类型 + `From<ProviderRequest>` (无依赖,先做)
2. `error.rs` — 错误映射 (无依赖)
3. `stream.rs` — SSE 解析器 (依赖 `ProviderEvent`)
4. `client.rs` — Provider 构造 + impl (依赖前三者)
5. `lib.rs` — 导出
6. tests — wiremock 集成测试

## Out of Scope (YAGNI)

- 重试逻辑(timeout 后不自动重试)
- 流断连重连
- `tracing` 日志
- Bedrock / Vertex AI 适配
- OpenAI / Ollama provider(后续)
- Token 计数(待 core 扩展 `AgentEvent::Done`)
- 图片工具实际调用(只确保 `ContentBlock::Image` 序列化正确)
