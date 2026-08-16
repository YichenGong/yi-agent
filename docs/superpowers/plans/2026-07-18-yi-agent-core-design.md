# yi-agent-core 设计文档

**日期**: 2026-07-18
**状态**: 已确认，待实现
**范围**: `yi-agent-rs/crates/yi-agent-core` 的模块划分、核心类型、trait 定义

---

## 1. 目标与定位

`yi-agent-core` 是 yi-agent 的核心库，定义 agent 循环的数据模型和抽象 trait，不包含任何具体 provider 实现、工具实现或持久化实现。

**对外契约**：
- `yi-agent-llm` 实现 `Provider` trait
- `yi-agent-tools` / `yi-agent-mcp` 实现 `Tool` trait
- `yi-agent-store` 基于 `Session` 类型实现持久化
- `yi-agent` CLI 组装 `Agent` 并消费 `AgentEvent` 流

**设计原则**：
- 自定义中立消息模型，core 不依赖任何 provider SDK
- 依赖单向流动：`message ← tool ← provider ← agent`，无环
- 流式优先，一次性调用是流式的累积便捷方法
- 错误当结果喂回 LLM，循环不因工具错误中断

---

## 2. 模块划分

```
yi-agent-core/src/
├── lib.rs          // 重导出
├── message.rs      // Message / ContentBlock / Role（纯数据，无依赖）
├── tool.rs         // Tool trait / ToolResult / ToolRegistry / ToolMetadata（依赖 message）
├── provider.rs     // Provider trait / ProviderEvent / ProviderRequest（依赖 message + tool）
└── agent.rs        // Agent / AgentEvent / Session / AgentConfig（依赖前三者）
```

---

## 3. 核心决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 循环驱动模型 | 事件流驱动 | CLI 可实时渲染，类似 Claude Code 体验 |
| Provider 抽象 | 多 provider + tool calling | 设计上支持切换，不绑死一家 |
| Tool 参数传递 | JSON Value 在边界，强类型在工具内部 | 与 LLM 原生格式对齐；开闭原则；MCP 兼容 |
| ToolResult 形状 | 结构化 content blocks | 支持文本/图片/多段，对齐 Anthropic 格式 |
| 消息模型 | 自定义中立 Message | core 中立，provider 实现负责翻译 |
| Session 职责 | 纯内存消息容器 | 持久化由 `yi-agent-store` 单独处理 |
| 循环终止条件 | LLM 不再调用工具 | 最自然的"任务完成"信号 |
| Provider 流式 | 流式优先，一次性是累积便捷方法 | 两类 provider 都能接入 |
| 多工具执行 | 并行执行 | 性能优势，结果按 id 顺序存回 |
| 工具错误处理 | 错误包成 ToolResult 喂回 LLM | 让 LLM 自我修复，循环不中断 |
| max_turns 上限 | 默认 100 | 安全网，防止无限循环 |

---

## 4. `message.rs` — 消息模型

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    /// 工具结果消息（序列化时映射回 "user"）
    Tool,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),

    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,  // 嵌套：支持返回文本/图片/多段
        is_error: bool,
    },

    /// 图片（未来扩展，类型留好）
    Image {
        source: ImageSource,
    },
}

#[derive(Debug, Clone)]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url(String),
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self;
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self;
    pub fn tool_results(results: Vec<ContentBlock>) -> Self;
    pub fn system(text: impl Into<String>) -> Self;
}
```

**关键点**：
- `ToolResult.content` 嵌套 `Vec<ContentBlock>`，对齐 Anthropic 格式
- `Role::Tool` 独立变体，序列化层负责映射到具体 provider 格式
- `Image` 占位，第一版不实现图片工具

**不加**：`name` 字段、`tool_calls` 独立字段、`refusal` / `annotations`

---

## 5. `tool.rs` — Tool trait 与注册表

```rust
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self;
    pub fn error(text: impl Into<String>) -> Self;
    pub fn with_content(content: Vec<ContentBlock>) -> Self;
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn description(&self) -> &str;
    async fn call(&self, args: serde_json::Value) -> ToolResult;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolMetadata {
    pub source: ToolSource,
    pub requires_confirmation: bool,
    pub read_only: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolSource {
    #[default]
    Builtin,
    Mcp { server_name: String },
    Plugin { name: String },
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, tool: Arc<dyn Tool>);
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    pub fn schemas(&self) -> Vec<ToolSchema>;
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
```

**关键点**：
- `Tool::call` 返回 `ToolResult` 而非 `Result<ToolResult>` —— 错误也是结果（`is_error: true`），和"错误喂回 LLM"决策一致
- `Arc<dyn Tool>` —— 并行执行需要共享 + `Send + Sync`
- `ToolMetadata` 三个字段都有明确消费者：`source`（日志/UI）、`requires_confirmation`（CLI 确认）、`read_only`（权限分组）、`version`（MCP 漂移检测/日志）
- `metadata()` 默认实现 —— 简单工具不碰元数据

**不加**：`validate_args()`、权限/确认机制（在 tools 层）、`category`、`rate_limit`、`timeout`（工具内部管）

---

## 6. `provider.rs` — Provider trait 与流式事件

```rust
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub params: GenParams,
}

#[derive(Debug, Clone, Default)]
pub struct GenParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseDelta { id: String, partial_json: String },
    ToolUseEnd { id: String },
    Stop { reason: StopReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;

    /// 默认实现：累积 stream 成完整 response
    async fn call(
        &self,
        req: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    Network(String),
    Auth(String),
    RateLimited,
    InvalidRequest(String),
    Server(String),
    Stream(String),
}
```

**关键点**：
- `system` 独立字段，不混在 messages 里
- `call` 默认实现做 delta 累积，provider 只实现 `call_stream`
- `StopReason::EndTurn` 对应 agent 循环终止条件
- `ProviderError` 用 `thiserror`，分类清晰便于重试决策

**不加**：`Provider::name()`、`Provider::models()`、token 用量统计

---

## 7. `agent.rs` — Agent 循环与 Session

```rust
#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self;
    pub fn push(&mut self, msg: Message);
    pub fn messages(&self) -> &[Message];
    pub fn truncate(&mut self, len: usize);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,           // 默认 100
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

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    session: Session,
    config: AgentConfig,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Start,
    AssistantText(String),                // 透传 TextDelta
    ToolCall { id: String, name: String, input: serde_json::Value },
    ToolResult { id: String, result: ToolResult },
    Done { reason: DoneReason },
    Error(Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneReason {
    EndTurn,                               // LLM 主动停止
    MaxTurns,                              // 达到安全上限
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    Provider(#[from] ProviderError),
    ToolNotFound(String),
    MaxTurnsExceeded,
}

impl Agent {
    pub fn new(provider: Arc<dyn Provider>, tools: Arc<ToolRegistry>, config: AgentConfig) -> Self;
    pub fn with_session(self, session: Session) -> Self;
    pub fn session(&self) -> &Session;
    pub async fn run(&mut self, user_prompt: String) -> Result<BoxStream<'static, AgentEvent>, Error>;
}
```

### Agent 循环流程

```
agent.run(user_prompt) -> Stream<AgentEvent>

0. 初始化
   Session.push(Message::user(user_prompt))
   emit AgentEvent::Start

1. THINK — 调用 Provider
   req = ProviderRequest { system, messages, tools, params }
   stream = provider.call_stream(req)
   accumulate stream → content_blocks（透传 TextDelta 为 AgentEvent::AssistantText）
   Session.push(Message::assistant(content))
   emit AgentEvent::AssistantText(...)

2. 判断终止
   tool_uses = content.filter(ToolUse)
   if tool_uses.is_empty() || stop_reason != EndTurn:
       emit AgentEvent::Done { reason: EndTurn }
       return

   if turn > max_turns:
       emit AgentEvent::Done { reason: MaxTurns }
       return

3. ACT — 并行执行工具
   for each tool_use (并行 via futures::join_all):
       emit AgentEvent::ToolCall { id, name, input }
       result = registry.get(name).call(input) 或 ToolResult::error("not found")
       emit AgentEvent::ToolResult { id, result }

4. OBSERVE — 喂回工具结果
   tool_results = results 按 tool_use_id 顺序构造
   Session.push(Message::tool_results(tool_results))

   → 回到步骤 1
```

**关键点**：
- `run` 用 `mpsc::channel` + `tokio::spawn` 把同步签名变事件流
- `run_loop` 是自由函数，避免 `&mut self` 跨 await 借用问题
- `max_turns` 默认 100，安全网
- `Stop { reason }` 参与终止判断，`MaxTokens` 等异常停止即使有 tool_uses 也终止
- 并行执行 + 有序存储（results 按 `tool_uses` 原顺序构造）
- `AgentEvent::ToolCall` 发完整 input，CLI 不处理 delta

**不加**：`Agent::stop()`（drop stream 即可）、`pause/resume`（用 session 保存恢复）、token 计数（后续加在 `Done` 事件）

### Session 持久化挂钩

```rust
// yi-agent-store 未来这样用：
let session = agent.session();
store.save(session).await?;
let restored = store.load(id).await?;
let agent = Agent::new(...).with_session(restored);
```

---

## 8. 依赖清单

`yi-agent-core` 的 `Cargo.toml`：

```toml
[dependencies]
async-trait = "0.1"
futures = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tokio-stream = "0.1"
```

---

## 9. 未来扩展点（当前不实现）

- **Token 计数**：扩展 `AgentEvent::Done` 携带 usage 信息
- **Agent 取消**：当前 drop stream 即可，未来如需显式取消可加 `CancellationToken`
- **插件系统**：基于 `ToolSource::Plugin` 和动态加载
- **图片工具**：`ContentBlock::Image` 已留好类型
- **权限分组**：基于 `ToolMetadata.read_only` 做策略
- **会话压缩**：`Session::truncate` 已留口子，压缩策略在 `yi-agent-store`
