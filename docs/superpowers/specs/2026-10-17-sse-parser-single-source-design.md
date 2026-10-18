# 设计：SSE 行解析收敛为单一实现（`yi-agent-llm::sse`）

日期：2026-10-17
状态：待评审
关系：本文来自 2026-10-17 的仓库冗余审计（P0-A）。审计结论：SSE 行解析在三个
provider 流实现中逐字重复三份，且非法 UTF-8 的处理已出现三种逃逸写法。

## 1. 背景与问题

`SseFrame` + `SseLineParser` + `feed()` 这一套 SSE 字节流按行切分逻辑，在
`yi-agent-llm` 内被复制了三份：

| 文件 | 定义位置 | 非法 UTF-8 的处理 |
|---|---|---|
| `crates/yi-agent-llm/src/anthropic/stream.rs` | 18-93 | 推入合成 JSON 帧 `{"type":"__parse_error",...}`，之后在 `parse_frame` 里（146-152）重新解码并匹配 |
| `crates/yi-agent-llm/src/openai/stream.rs` | 17-73 | 推入合成 JSON 字符串 `{"__parse_error":...}`，之后在 `parse_frame` 里（225-228）用 `strip_prefix` 抠字符串 |
| `crates/yi-agent-llm/src/openai/responses/stream.rs` | 18-80 | 给 `SseFrame` 加了一个 `parse_error: Option<String>` 字段，之后在 `parse_frame` 里（112-114）先检查该字段 |

三份的 `feed()` 主体（`buf.extend_from_slice` → `while let Some(nl) = position(b'\n')`
→ `drain(..=nl)` → `saturating_sub(1)` 去换行 → CRLF 去 `\r` → 空行代表帧边界
→ `data:` 前缀累积 → `:` 注释忽略）逐字相同。`responses/stream.rs:3` 的模块注释
自认「与 `openai/stream.rs` 同形：自带私有的 `SseLineParser` + `SseFrame`」。

### 真实危害（不是纯风格问题）

三份副本对**同一个错误**用了三种编码方式。当前外部行为**恰好等价**（三条路径最终都
产出 `Err(ProviderError::Stream(msg))`），但这种「等价」是靠三套互不相干的机制巧合达成的：

- 任何对切分逻辑的修复（例如 UTF-8 跨 TCP 分片的边界 bug）必须改三处；
- 新增一个 provider（例如 Gemini 流）会复制出第四份，并第四次发明一种错误编码；
- 三个 `parse_frame` 里各有一段「还原合成错误」的样板代码，它们不表达任何业务语义，
  只是三份副本各自的解码器。

同时，三个流类型的 `poll_next` 骨架（drain `pending_events` → 逐帧 `parse_frame` →
`inner.poll_next_unpin` → chunk 喂 `line_parser.feed`）以及传输错误分类

```rust
let error = if e.is_timeout() {
    ProviderError::Network(format!("stream error: timed out: {e}"))
} else {
    ProviderError::Stream(format!("stream error: {e}"))
};
```

也是逐字重复（`anthropic/stream.rs:400-404`、`openai/stream.rs:412-416`、
`responses/stream.rs:349-353`）。

## 2. 目标与非目标

### 目标

1. **SSE 行解析只有一份实现**：新建 `yi-agent-llm::sse` 模块，拥有
   `SseFrame` / `SseLineParser` / `feed`。
2. **非法 UTF-8 的错误语义只定义一次**：以帧级错误（`Result`）表达，取代三种合成编码。
3. **传输错误分类（timeout→`Network`，其余→`Stream`）只定义一次**。
4. 三个 provider 流的**对外行为不变**：同样的字节序列产出同样的
   `Stream<Item = Result<ProviderEvent, ProviderError>>`。

### 非目标

- 不合并三个 `parse_frame`（它们解析的 JSON 事件类型确实不同：Anthropic 的
  `message_start`/`content_block_*`、chat 的 `choices[].delta`、responses 的
  `output_item.*`）。这是**合理地不同**，保留。
- 不合并三个 provider 的 client/retry/transport（`openai/client.rs` 的退避、
  channel 解析等是 OpenAI 专有逻辑）。
- 不改 `ProviderEvent` / `ProviderError` 的公开定义。
- 不改 SSE 对外可观测语义（见 §3 的等价性论证）。

## 3. 设计

### 3.1 错误语义收敛规则（唯一行为变更点，且已论证等价）

统一后 `SseLineParser::feed` 返回 `Vec<Result<SseFrame, ProviderError>>`：

```rust
pub struct SseFrame {
    /// 该帧累积的 `data:` 负载（多个 data 行以 '\n' 连接）。
    pub data: String,
}

pub struct SseLineParser { /* buf, current_data_lines */ }

impl SseLineParser {
    pub fn new() -> Self;
    /// 喂入一段字节，返回本次新完成的帧。非法 UTF-8 的行产出一个 Err 帧。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Result<SseFrame, ProviderError>>;
}
```

非法 UTF-8 的规则（三份现状的统一）：

- 该帧产出 `Err(ProviderError::Stream(format!("invalid UTF-8 in SSE line: {e}")))`。
- **清空** `current_data_lines`（沿用 responses 的现行写法：损坏行之后不应把此前
  累积的 data 行与新帧混在一起）。注意 anthropic/openai 的现状是**不清空**，但它们的
  合成错误帧会被 `parse_frame` 立即转成 `Err` 返回，`poll_next` 随即终止整个流——
  清空与不清空在「流就此结束」的语义下不可观测。统一取清空这一更审慎的写法。
- 不再使用 `__parse_error` 字符串，也不再有 `SseFrame.parse_error` 字段。

三个 `parse_frame` 里删除各自的合成错误解码分支（`anthropic/stream.rs:146-152`、
`openai/stream.rs:225-228`、`responses/stream.rs:112-114`），改为在 `poll_next`
里直接处理 `Err` 帧。

**等价性论证**：三个 `poll_next` 现状都是「拿到错误帧 → 返回
`Poll::Ready(Some(Err(_)))` → 流结束」。统一后错误帧本身就是 `Err`，`poll_next`
直接返回它。对调用方（`agent.rs` 的 provider 循环）而言，错误类型、错误文本前缀
（`"invalid UTF-8 in SSE line: "`）与「流终止」三者均不变。

### 3.2 共用流骨架（`SseStreamCore`）

新增一个持有公共状态的骨架，供三个 provider 流内嵌：

```rust
/// SSE 流共用的传输骨架：缓冲区、待发帧、内部流轮询与错误分类。
pub struct SseStreamCore<S> {
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<Result<SseFrame, ProviderError>>,
}

impl<S> SseStreamCore<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self;

    /// 取下一个帧：
    /// - `Some(Ok(frame))` 有新帧；
    /// - `Some(Err(e))` 帧级/传输级错误，调用方应原样上抛并结束流；
    /// - `None` 表示内部流结束。
    /// `Pending` 表示当前无数据，调用方应返回 `Poll::Pending`。
    pub fn poll_next_frame(&mut self, cx: &mut Context<'_>)
        -> Poll<Option<Result<SseFrame, ProviderError>>>;
}
```

`poll_next_frame` 内部：`pending_frames` 先出；否则 `inner.poll_next_unpin(cx)`，
`Err(e)` 走统一分类（`e.is_timeout()` → `Network`，否则 `Stream`），
`Ok(chunk)` 喂 `line_parser.feed`。

每个 provider 流只保留自己的 `parse_frame` 与 provider 特有状态：

```rust
pub struct AnthropicStream<S> {
    core: SseStreamCore<S>,
    pending_events: VecDeque<ProviderEvent>,
    block_ids: HashMap<usize, String>,
    pending_stop: Option<StopReason>,
    thinking_buffers: HashMap<usize, ThinkingBuffer>,
}
```

**关于 `poll_next` 的差异边界**：anthropic 一帧最多产一个事件（`Ok(Some(ev))` 立即
返回），openai/responses 一帧可产多个事件（批量塞入 `pending_events`）。这个差异
保留在各自的 `poll_next` 里——骨架只统一「怎么拿下一帧」，不统一「一帧怎么变成事件」。

因此 anthropic 的 `poll_next` 结构为：

1. `pending_stop` 先出（必须早于 drain，见其注释）；
2. `pending_events` 出队；
3. `core.poll_next_frame(cx)`：`Some(Ok(frame))` → `parse_frame`；
   `Some(Err(e))` → `Poll::Ready(Some(Err(e)))`；`None` → `Poll::Ready(None)`；
   `Pending` → `Poll::Pending`。

openai/responses 的结构：先 `pending_events`，再 `pending_frames`（各自 `parse_frame`
可返回 `Vec<ProviderEvent>`），最后 `core.poll_next_frame`。

> 注：由于三处 `poll_next` 的帧→事件映射不同，本次**不**强行把 `poll_next` 合并成
> 一个函数；只收敛「取帧 + 错误分类」这半边。这已消除全部重复，且不引入回调式
> 复杂度。

### 3.3 模块边界

- 新建 `crates/yi-agent-llm/src/sse.rs`，在 `src/lib.rs` 声明 `pub mod sse;`
  （供 `anthropic/` 与 `openai/` 子模块使用；对外 `pub` 便于将来第四个 provider 复用）。
- `sse.rs` 只依赖 `bytes`、`futures`、`std`，以及 `yi_agent_core::ProviderError`。

## 4. 测试策略

1. **新增 `sse.rs` 单元测试**（迁移性质的回归）：把三个 `feed` 的现行行为固化为
   `sse::SseLineParser` 的测试：
   - 单帧（`data: {...}\n\n`）；
   - 分片跨行（chunk 在行中间切断）；
   - 跨 chunk 切在 UTF-8 字符中间；
   - CRLF；
   - `:` 注释行忽略；
   - 多行 `data:` 以 `\n` 连接；
   - 非法 UTF-8 → `Err`；
   - `[DONE]` 作为普通 data 帧透传（终止语义由 openai 的 `parse_frame` 负责，不在
     解析器里）。
2. **三个 provider 流的既有测试必须原样通过**（`anthropic/stream.rs`、
   `openai/stream.rs`、`responses/stream.rs` 的既有 `#[tokio::test]`），作为
   「对外行为不变」的验收证据。
3. 新增一条针对 anthropic/openai 的**非法 UTF-8 端到端**测试（现状无覆盖），锁定
   §3.1 的统一语义。

## 5. 验收标准

- `cargo test -p yi-agent-llm` 全绿。
- `cargo clippy -p yi-agent-llm --all-targets -- -D warnings` 无警告。
- `grep -rn "SseLineParser\|struct SseFrame" crates/yi-agent-llm/src/` 只命中 `sse.rs`
  （即三处副本已删）。
- `grep -rn "__parse_error\|parse_error" crates/yi-agent-llm/src/` 无命中。
- `cargo test --workspace` 全绿（确认无跨 crate 影响）。
