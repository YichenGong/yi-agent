# yi-agent-core 流式输出与中断 + Token 计数设计

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 为 yi-agent-core 添加取消机制(双路:cancel() + drop receiver)和 token 计数(每轮独立 Usage 事件)。

**Architecture:** Agent 持有 tokio_util::CancellationToken,run_loop 在三个 check 点(THINK 前、THINK 中 select!、ACT 中 select!)响应取消,发 AgentEvent::Cancelled。Provider 层新增 ProviderEvent::Usage(TokenUsage),AnthropicStream 解析 message_start/message_delta 的 usage 字段并透传到 agent 层发 AgentEvent::Usage。

**Tech Stack:** tokio-util (CancellationToken), serde (TokenUsage 序列化)

---

## 决策总结

| # | 决策点 | 选择 |
|---|--------|------|
| 1 | 取消机制暴露方式 | C:双重 — `Agent::cancel()` + `CancellationToken`,drop receiver 兜底 |
| 2 | 取消在 AgentEvent 里的表示 | C:新增 `AgentEvent::Cancelled`,与 `Done` 互斥 |
| 3 | Token 计数传递方式 | A:新增 `AgentEvent::Usage(TokenUsage)` 独立事件,每轮发 |
| 4 | TokenUsage 字段集 | B:input/output + cache 字段(Option) |
| 5 | Usage 从 provider 到 agent 的传递 | A:新增 `ProviderEvent::Usage(TokenUsage)` |
| 6 | CancellationToken 到 provider 的传递 | A:只改 agent 层,drop stream 即 abort,不改 Provider trait |
| 7 | ACT 中取消处理 | B:select! 竞争 join_all 和 cancel,取消时 drop 未完成 futures |

---

## 1. 取消机制架构

### Agent 结构体改动

```rust
pub struct Agent {
    session: Arc<Mutex<Session>>,
    cancel_token: CancellationToken,  // 新增
}
```

`Agent::new()` 和 `Agent::with_session()` 内部创建 `CancellationToken`。新增方法:

```rust
impl Agent {
    /// 触发取消,run_loop 会在最近的 check 点退出
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// 拿到 token clone(供需要监听取消的调用方使用)
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}
```

### run_loop 三个 check 点

1. **THINK 前**:轮迭代开始时,`if cancel_token.is_cancelled() { return; }`
2. **THINK 中**:`tokio::select!` 竞争 provider stream 和 `cancel_token.cancelled()`,取消时 drop stream
3. **ACT 中**:`tokio::select!` 竞争 `join_all(tool_futures)` 和 `cancel_token.cancelled()`,取消时 drop 未完成的工具 futures

取消时发射 `AgentEvent::Cancelled` 后 return。drop receiver 兜底不变(现有 `tx.send().await.is_err()` 逻辑保留)。

### 依赖变更

`yi-agent-core/Cargo.toml` 加:
```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

---

## 2. Token 计数数据流

### TokenUsage 结构体(放 provider.rs)

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
}
```

`Default` 让 provider 不支持缓存时 cache 字段默认 `None`。

### ProviderEvent 新增变体

```rust
pub enum ProviderEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseDelta { id: String, partial_json: String },
    ToolUseEnd { id: String },
    Stop { reason: StopReason },
    Usage(TokenUsage),  // 新增
}
```

### accumulate_stream 转发

`accumulate_stream` 回调收到 `ProviderEvent::Usage(u)` 时转发 `AgentEvent::Usage(u)`。直接透传分次事件(message_start 的 input、message_delta 的 output 各发一次),累计由消费方做。

### AnthropicStream 解析改动(yi-agent-llm 侧)

**message_start 事件:**

API 返回:
```json
{ "type": "message_start", "message": { "usage": { "input_tokens": 120, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 } } }
```

解析为:
```rust
ProviderEvent::Usage(TokenUsage {
    input_tokens: usage.input_tokens,
    output_tokens: 0,
    cache_creation_input_tokens: usage.cache_creation_input_tokens,
    cache_read_input_tokens: usage.cache_read_input_tokens,
})
```

**message_delta 事件:**

API 返回:
```json
{ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 45 } }
```

解析为两个事件:
- `ProviderEvent::Usage(TokenUsage { input_tokens: 0, output_tokens: 45, cache_creation_input_tokens: None, cache_read_input_tokens: None })`
- `ProviderEvent::Stop { reason: StopReason::EndTurn }`

需要给 Anthropic SSE 类型加 `usage` 字段(用 serde rename 对齐 API 字段名)。`cache_creation_input_tokens` 和 `cache_read_input_tokens` 可能不存在(无缓存时),用 `serde(default)` + `Option<u32>`。

---

## 3. AgentEvent 变更与事件时序

### AgentEvent 新增变体

```rust
pub enum AgentEvent {
    Start,
    AssistantText(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, result: ToolResult },
    Usage(TokenUsage),      // 新增
    Done { reason: DoneReason },
    Cancelled,              // 新增,与 Done 互斥
    Error(AgentError),
}
```

### 事件时序示例

**正常两轮:**
```
Start
AssistantText("Let me check...")
Usage { input: 120, output: 8, ... }       // message_start
ToolCall { id: "tool1", name: "read", ... }
ToolResult { id: "tool1", result: ... }
AssistantText("The file contains...")
Usage { input: 350, output: 15, ... }      // 第二轮 message_start
Done { reason: EndTurn }
```

**THINK 中取消:**
```
Start
AssistantText("Let me...")
Cancelled
```

**ACT 中取消:**
```
Start
AssistantText("Let me check...")
ToolCall { id: "tool1", name: "bash", ... }
Cancelled          // tool 还没执行完就取消
```

### 关键规则

- `Cancelled` 发出后不再发任何事件(循环直接 return)
- `Done` 和 `Cancelled` 互斥,一次运行只发其中一个
- `Usage` 可以在 `Cancelled` 之前出现(取消发生在 THINK 中时,可能已收到 input_tokens 的 Usage)
- `DoneReason` 保持不变,只有 `EndTurn` 和 `MaxTurns`

---

## 4. 测试策略与边界情况

### yi-agent-core 测试(mock provider)

1. **正常 Usage 转发** — mock provider 发 `ProviderEvent::Usage`,断言 agent 层发 `AgentEvent::Usage`
2. **THINK 中取消** — mock provider stream 无限延迟,调用 `cancel()`,断言发 `AgentEvent::Cancelled`
3. **ACT 中取消** — mock provider 正常返回 tool_use,mock tool 无限延迟,调用 `cancel()`,断言发 `AgentEvent::Cancelled` 且未发 `ToolResult`
4. **MaxTurns 仍然发 Done** — 确认取消机制不影响 MaxTurns 路径
5. **drop receiver 兜底** — 不调用 `cancel()`,直接 drop receiver,run_loop 在下次 send 失败时退出,无 panic

### yi-agent-llm 测试(AnthropicStream)

1. **message_start 解析 usage** — 含 input_tokens + cache 字段,断言 `ProviderEvent::Usage`
2. **message_delta 解析 usage** — 含 output_tokens,断言 `ProviderEvent::Usage`
3. **无 cache 字段** — `cache_*` 为 `None`
4. **usage 后接 Stop** — 验证事件顺序:`Usage` → `Stop`

### 边界情况处理

- **Usage 在取消后**:run_loop 取消后直接 return,不再处理后续 stream 事件(因为 stream 已被 drop)
- **Usage input=0 output=0**:仍正常发事件(provider 异常但不该过滤)
- **多轮累计**:agent 层不累计,消费方自己加。每轮的 `Usage` 是独立的
- **CancellationToken clone**:每次 `Agent::run()` 内部 clone token 传入 run_loop,原始 token 留在 Agent 上供 `cancel()` 调用

---

## 依赖变更总结

| Crate | 变更 |
|-------|------|
| yi-agent-core | 加 `tokio-util = { version = "0.7", features = ["rt"] }` |
| yi-agent-llm | 无新依赖(已有 reqwest + serde) |
