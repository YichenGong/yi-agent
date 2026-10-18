# SSE 行解析收敛 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `yi-agent-llm` 内三份重复的 SSE 行解析（`SseFrame` / `SseLineParser` / `feed`）与重复的传输错误分类收敛成一份 `sse` 模块，且三个 provider 流的对外行为不变。

**Architecture:** 新建 `crates/yi-agent-llm/src/sse.rs`，拥有 `SseFrame`、`SseLineParser`（`feed` 返回 `Vec<Result<SseFrame, ProviderError>>`）与 `SseStreamCore<S>`（持有缓冲区 + 内部流 + 统一的 `is_timeout → Network` 错误分类）。三个 provider 流改为内嵌 `SseStreamCore`，各自只保留 `parse_frame`（它们解析的 JSON 事件类型确实不同）与 provider 特有状态。三处合成 `__parse_error` 编码全部删除。

**Tech Stack:** Rust（`yi-agent-llm`：`bytes`、`futures`、`reqwest`、`serde_json`；测试用 `tokio`）。

**Spec:** `docs/superpowers/specs/2026-10-17-sse-parser-single-source-design.md`

## Global Constraints

- 行号以基线 `f5dd3376` 为准；改动前用 `grep -n` 复核锚点（分支会前移）。
- 环境：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`，所有 cargo 命令加 `--offline`。
- 只显式 `git add <path>`；每个 Task 结束前 `cargo fmt -p yi-agent-llm`。
- **不改** `ProviderEvent` / `ProviderError` 的公开定义。
- **不改**三个 provider 的 `parse_frame` 事件解析逻辑（只删其中的合成错误解码分支）。
- **不改** provider 的 client/retry/transport 层。
- 对外行为不变：`anthropic/stream.rs`、`openai/stream.rs`、`responses/stream.rs` 的既有 `#[tokio::test]` 必须原样通过。
- `sse.rs` **不引入任何新依赖**：允许引用本 crate 既有的 crate（`std`/`bytes`/`futures`/`yi_agent_core`，以及 `SseStreamCore` 约束里的 `reqwest::Error`——`reqwest`/`serde_json` 本就是 `yi-agent-llm` 的既有依赖）；不新增任何 crate。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-llm/src/sse.rs` — **新建**。`SseFrame`、`SseLineParser`、`SseStreamCore<S>` 及其单元测试。
- `yi-agent-rs/crates/yi-agent-llm/src/lib.rs` — 增加 `pub mod sse;`。
- `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs` — 删本地 `SseFrame`/`SseLineParser`；`AnthropicStream` 改为内嵌 `SseStreamCore`；删 `__parse_error` 分支。
- `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs` — 同上（其 `parse_frame` 返回 `Vec<ProviderEvent>`）。
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs` — 同上（其 `SseFrame.parse_error` 字段删除）。

---
### Task 1: 新建 `sse` 模块（解析器 + 骨架 + 单元测试）

> 本 Task 的文件内容已在实现前**实测编译并通过 10/10 测试**（含分片、CRLF、多行 data、
> 非法 UTF-8、UTF-8 字符跨 chunk、`[DONE]` 透传）。整份照抄即可。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-llm/src/sse.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct SseFrame { pub data: String }`
  - `pub struct SseLineParser` + `pub fn new() -> Self`
  - `impl SseLineParser { pub fn feed(&mut self, chunk: &[u8]) -> Vec<Result<SseFrame, ProviderError>> }`
  - `pub struct SseStreamCore<S>` + `pub fn new(inner: S) -> Self`
  - `impl<S> SseStreamCore<S> where S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin { pub fn poll_next_frame(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<SseFrame, ProviderError>>> }`

- [ ] **Step 1: 创建 `sse.rs`**

创建 `yi-agent-rs/crates/yi-agent-llm/src/sse.rs`，内容如下（整份照抄）：

```rust
//! SSE 字节流的行解析与传输骨架。
//!
//! 三个 provider 流（Anthropic、OpenAI chat、OpenAI responses）共用这里的
//! `SseLineParser`（按行切分、还原 `data:` 负载）与 `SseStreamCore`（缓冲、内部流
//! 轮询、传输错误分类）。provider 各自只保留把一帧 JSON 解析成 `ProviderEvent` 的
//! `parse_frame`。

use std::collections::VecDeque;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::{Stream, StreamExt};

use yi_agent_core::ProviderError;

/// 一个已完成的 SSE 帧：其 `data:` 负载（多个 data 行以 `\n` 连接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub data: String,
}

impl SseFrame {
    fn new(data: String) -> Self {
        Self { data }
    }
}

/// 把原始字节块切分成 SSE 帧。
///
/// 缓冲的是**字节**（不是 `String`），因此 TCP 分片把一个多字节 UTF-8 字符切成两半
/// 时不会丢数据：只有在一整行（以 `\n` 结尾）到齐后才解码 UTF-8。
#[derive(Default)]
pub struct SseLineParser {
    buf: Vec<u8>,
    current_data_lines: Vec<String>,
}

impl SseLineParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回本次新完成的帧。
    ///
    /// 非法 UTF-8 的行产出一个 `Err` 帧，并清空已累积的 `data:` 行。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Result<SseFrame, ProviderError>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();

        // 逐行处理，末尾不完整的行留在缓冲里。
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            let line_bytes = &line_bytes[..line_bytes.len().saturating_sub(1)];

            let mut line = match std::str::from_utf8(line_bytes) {
                Ok(s) => s.to_string(),
                Err(e) => {
                    // 丢弃损坏行，清掉已累积的 data 行，以帧级错误上报。
                    self.current_data_lines.clear();
                    frames.push(Err(ProviderError::Stream(format!(
                        "invalid UTF-8 in SSE line: {e}"
                    ))));
                    continue;
                }
            };

            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
                // 空行 = 帧边界。有 data 就产出一帧。
                if !self.current_data_lines.is_empty() {
                    let data = std::mem::take(&mut self.current_data_lines).join("\n");
                    frames.push(Ok(SseFrame::new(data)));
                }
                continue;
            }

            if let Some(rest) = line.strip_prefix("data:") {
                self.current_data_lines.push(rest.trim().to_string());
            }
            // `event:` / `id:` / `retry:` 与 `:` 注释行一律忽略：分派只看 JSON 的
            // `type` 字段。
        }

        frames
    }
}

/// SSE 流共用的传输骨架。
///
/// 持有行解析器、内部字节流与待发帧队列，并把传输层错误分类唯一化：
/// 只有**超时**是可重试的 `Network`，其余传输中断归 `Stream`。
///
/// 分类必须在 `is_timeout()` 上做：一个 reqwest deadline 的 `Display` 是
/// "error decoding response body"，不含 "timed out" 文本，只有 `is_timeout()`
/// 会回溯 source 链。因此这里**禁止**改用 `is_decode()`（或任何解码分支判定）来分类：
/// 请求 deadline 的 `Display` 同样会匹配它，会把可重试的超时误判成不可重试的传输中断。
pub struct SseStreamCore<S> {
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<Result<SseFrame, ProviderError>>,
    /// 内层流是否已结束（一旦结束就永远结束）。
    input_ended: bool,
}

impl<S> SseStreamCore<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            line_parser: SseLineParser::new(),
            inner,
            pending_frames: VecDeque::new(),
            input_ended: false,
        }
    }

    /// 取下一个帧（一次一个，调用方消费完再取）。
    ///
    /// - `Some(Ok(frame))`：有新帧；
    /// - `Some(Err(e))`：帧级或传输级错误，调用方应原样上抛并结束流；
    /// - `None`：内部流结束且缓冲已排空；
    /// - `Pending`：当前无数据，调用方应返回 `Poll::Pending`。
    pub fn poll_next_frame(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<SseFrame, ProviderError>>> {
        loop {
            if let Some(frame) = self.pending_frames.pop_front() {
                return Poll::Ready(Some(frame));
            }

            if self.input_ended {
                return Poll::Ready(None);
            }

            match self.inner.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    self.input_ended = true;
                    // 回到循环：若这一批 feed 还有残留帧则先发完，否则返回 None。
                }
                Poll::Ready(Some(Err(e))) => {
                    // `Network`（超时）分支由 tests/integration.rs 的 request_timeout_is_a_total_deadline_not_an_idle_timeout 端到端覆盖。
                    let error = if e.is_timeout() {
                        ProviderError::Network(format!("stream error: timed out: {e}"))
                    } else {
                        ProviderError::Stream(format!("stream error: {e}"))
                    };
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    let frames = self.line_parser.feed(&chunk);
                    self.pending_frames.extend(frames);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> Vec<Result<SseFrame, ProviderError>> {
        let mut parser = SseLineParser::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(parser.feed(c));
        }
        out
    }

    #[test]
    fn emits_one_frame_for_a_single_data_line() {
        let frames = feed_all(&[b"data: {\"a\":1}\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "{\"a\":1}");
    }

    #[test]
    fn buffers_a_partial_trailing_line() {
        let frames = feed_all(&[b"data: {\"a\"", b":1}\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "{\"a\":1}");
    }

    #[test]
    fn handles_a_chunk_split_inside_a_utf8_char() {
        // "中" = E4 B8 AD；在第二个字节后切断。
        let full = "data: 中\n\n".as_bytes();
        let frames = feed_all(&[&full[..7], &full[7..]]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "中");
    }

    #[test]
    fn strips_crlf_line_endings() {
        let frames = feed_all(&[b"data: {\"a\":1}\r\n\r\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "{\"a\":1}");
    }

    #[test]
    fn ignores_comment_lines() {
        let frames = feed_all(&[b": ping\ndata: {\"a\":1}\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "{\"a\":1}");
    }

    #[test]
    fn joins_multiple_data_lines_with_newline() {
        let frames = feed_all(&[b"data: line1\ndata: line2\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "line1\nline2");
    }

    #[test]
    fn passes_a_done_sentinel_through_as_a_plain_frame() {
        let frames = feed_all(&[b"data: [DONE]\n\n"]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].as_ref().unwrap().data, "[DONE]");
    }

    #[test]
    fn surfaces_invalid_utf8_as_an_error_frame() {
        let frames = feed_all(&[b"data: \xff\n\n"]);
        assert_eq!(frames.len(), 1);
        match frames[0].as_ref().unwrap_err() {
            ProviderError::Stream(msg) => {
                assert!(msg.starts_with("invalid UTF-8 in SSE line: "), "got: {msg}")
            }
            other => panic!("expected Stream error, got {other:?}"),
        }
    }

    #[test]
    fn core_returns_none_when_the_inner_stream_ends_empty() {
        let w = std::task::Waker::noop();
        let mut cx = Context::from_waker(w);
        let inner = futures::stream::empty::<Result<Bytes, reqwest::Error>>();
        let mut core = SseStreamCore::new(inner);
        assert!(matches!(core.poll_next_frame(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn core_yields_frames_from_an_inner_chunk_one_at_a_time() {
        let w = std::task::Waker::noop();
        let mut cx = Context::from_waker(w);
        let inner = futures::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n",
        ))]);
        let mut core = SseStreamCore::new(inner);
        match core.poll_next_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => assert_eq!(frame.data, "{\"a\":1}"),
            other => panic!("expected first frame, got {other:?}"),
        }
        match core.poll_next_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => assert_eq!(frame.data, "{\"b\":2}"),
            other => panic!("expected second frame, got {other:?}"),
        }
        assert!(matches!(core.poll_next_frame(&mut cx), Poll::Ready(None)));
    }
}
```

> 注意：`std::task::Waker::noop()` 返回的是 `&'static Waker`，所以写
> `Context::from_waker(w)`（**不要**加 `&`）。本仓 rustc 1.98 已支持。

- [ ] **Step 2: 在 `lib.rs` 声明模块**

修改 `yi-agent-rs/crates/yi-agent-llm/src/lib.rs`，在 `pub mod anthropic;` 之前插入
`pub mod sse;`。**必须放在模块文档注释（`//!` 块）之后**，即模块声明区变为：

```rust
//! yi-agent-llm: LLM provider implementations.
//!
//! 依赖 `yi-agent-core` 的 `Provider` trait,实现 Anthropic Claude 和 OpenAI provider,
//! 架构上预留多 provider 扩展能力。

pub mod sse;
pub mod anthropic;
pub mod openai;
```

- [ ] **Step 3: 运行测试确认通过**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline sse:: 2>&1 | tail -20`

Expected: `test result: ok. 10 passed; 0 failed`。

- [ ] **Step 4: fmt + clippy + commit**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo fmt -p yi-agent-llm
cargo clippy -p yi-agent-llm --offline --all-targets -- -D warnings 2>&1 | tail -5
cd ..
git add yi-agent-rs/crates/yi-agent-llm/src/sse.rs yi-agent-rs/crates/yi-agent-llm/src/lib.rs
git commit -m "refactor(llm): add a shared sse module with the line parser and stream core"
```

Expected: clippy 无警告；commit 成功。
---

### Task 2: Anthropic 流改用共享模块

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs`

**Interfaces:**
- Consumes: `crate::sse::{SseFrame, SseStreamCore}`（Task 1）。
- Produces: 对外不变的 `AnthropicStream<S>`。

- [ ] **Step 1: 记录基线行为**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline anthropic:: 2>&1 | tail -8`

Expected: 既有测试全绿。记下通过数量。

- [ ] **Step 2: 删除本地的 `SseFrame` / `SseLineParser`**

在 `crates/yi-agent-llm/src/anthropic/stream.rs` 中删除：

- `struct SseFrame { data: String }` 及其上方 doc（基线行 17-19）；
- `struct SseLineParser { … }`、`impl SseLineParser { … }`（含 `new` 与 `feed`，基线行 21-93）。

删除后，该文件首个结构体定义应直接是 `pub struct AnthropicStream<S>`（基线行 95 附近）。

- [ ] **Step 3: 引入共享类型并改 `AnthropicStream` 字段**

在该文件顶部 `use` 区加入：

```rust
use crate::sse::{SseFrame, SseStreamCore};
```

把 `AnthropicStream` 的两处字段

```rust
    line_parser: SseLineParser,
    inner: S,
    /// Frames parsed from the inner stream but not yet emitted as events.
    pending_frames: VecDeque<SseFrame>,
```

替换为：

```rust
    core: SseStreamCore<S>,
```

并在 `AnthropicStream::new` 里把

```rust
        Self {
            line_parser: SseLineParser::new(),
            inner,
            pending_frames: VecDeque::new(),
```

替换为：

```rust
        Self {
            core: SseStreamCore::new(inner),
```

（其余字段 `block_ids` / `pending_stop` / `thinking_buffers` 不变。）

- [ ] **Step 4: 删除 `parse_frame` 里的 `__parse_error` 分支**

删除 `match event_type {` 中的这一段（基线 146-152）：

```rust
            "__parse_error" => {
                let msg = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid UTF-8 in SSE line");
                Err(ProviderError::Stream(msg.to_string()))
            }
```

- [ ] **Step 5: 重写 `poll_next` 的取帧部分**

把 `poll_next` 里的「drain pending_frames」与「poll inner」两段替换为对 `core` 的调用。完成后 `poll_next` 应为：

```rust
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // 先发挂起的 Stop（来自同时带 usage + stop_reason 的 message_delta），
            // 必须早于取帧，才能保证 Stop 排在 Usage 之后。
            if let Some(reason) = self.pending_stop.take() {
                return Poll::Ready(Some(Ok(ProviderEvent::Stop { reason })));
            }

            match self.core.poll_next_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(frame))) => match self.parse_frame(frame) {
                    Ok(Some(event)) => return Poll::Ready(Some(Ok(event))),
                    Ok(None) => continue,
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
            }
        }
    }
```

- [ ] **Step 6: 清理不再使用的 import**

删掉因本次改动而不再使用的 import（通常是 `std::collections::VecDeque`、`bytes::Bytes`、`futures::StreamExt`）。保留 `Pin`、`Context`、`Poll`、`Stream`、`HashMap`、`Value` 等仍在用的。

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-llm --offline 2>&1 | grep -E "^(warning|error)" | head -20`

Expected: 若报 `unused import` 就按提示删；直到无输出。

- [ ] **Step 7: 运行测试确认行为不变**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline anthropic:: 2>&1 | tail -8`

Expected: 与 Step 1 相同数量（或更多）的测试全部 PASS。

- [ ] **Step 8: 新增非法 UTF-8 端到端测试**

在 `anthropic/stream.rs` 的 `mod tests` 末尾追加：

```rust
    #[tokio::test]
    async fn surfaces_invalid_utf8_as_a_stream_error() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from_static(b"data: \xff\n\n"))];
        let stream = AnthropicStream::new(stream::iter(chunks));
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ProviderError::Stream(msg)) => {
                assert!(msg.starts_with("invalid UTF-8 in SSE line: "), "got: {msg}")
            }
            other => panic!("expected Stream error, got {other:?}"),
        }
    }
```

- [ ] **Step 9: 运行新测试确认通过**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline --lib surfaces_invalid_utf8 2>&1 | tail -8`

Expected: PASS。注意 `cargo test` 的过滤串是**全路径子串**匹配，而测试的全路径是
`anthropic::stream::tests::surfaces_invalid_utf8_as_a_stream_error`，所以过滤串必须用有区分度的叶子名
`surfaces_invalid_utf8`（写成 `anthropic::surfaces_invalid_utf8` 会切掉中间的 `stream::tests::` 段、匹配到 0 个测试而打印
`test result: ok. 0 passed` 的假绿）。该叶子名会一并覆盖 Task 1 的 `sse::tests::surfaces_invalid_utf8_as_an_error_frame`；
Task 3 的 openai 同名测试此时尚未创建。

- [ ] **Step 10: fmt + commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-llm && cd ..
git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/stream.rs
git commit -m "refactor(llm): anthropic stream uses the shared sse core"
```

---

### Task 3: OpenAI chat 流改用共享模块

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs`

**Interfaces:**
- Consumes: `crate::sse::{SseFrame, SseStreamCore}`（Task 1）。注意本流的 `parse_frame`
  返回 `Vec<ProviderEvent>`，因此 `poll_next` 需保留自己的 `pending_events` 批量drain。

- [ ] **Step 1: 记录基线行为**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline openai::stream 2>&1 | tail -8`

Expected: 全绿。记下数量。

- [ ] **Step 2: 删除本地 `SseFrame` / `SseLineParser`**

删除 `struct SseFrame { data: String }`（基线 17-19）与 `struct SseLineParser` +
`impl SseLineParser`（基线 21-73）。

- [ ] **Step 3: 改 `OpenaiStream` 字段与构造**

顶部加 `use crate::sse::{SseFrame, SseStreamCore};`。

把字段

```rust
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<SseFrame>,
    pending_events: VecDeque<ProviderEvent>,
```

替换为

```rust
    core: SseStreamCore<S>,
    pending_events: VecDeque<ProviderEvent>,
```

`OpenaiStream::new` 里对应把 `line_parser: SseLineParser::new(), inner, pending_frames: VecDeque::new(),` 换成 `core: SseStreamCore::new(inner),`（保留 `pending_events` / `pending_tool_calls` / `stopped` / `reasoning_buffer`）。

- [ ] **Step 4: 删除 `parse_frame` 的 `__parse_error` 分支**

删除 `parse_frame` 里的（基线 225-228）：

```rust
        if let Some(rest) = frame.data.strip_prefix("{\"__parse_error\":") {
            let msg = rest.trim_end_matches('}');
            return Err(ProviderError::Stream(msg.to_string()));
        }
```

- [ ] **Step 5: 重写 `poll_next`**

替换为：

```rust
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }

            match self.core.poll_next_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(frame))) => match self.parse_frame(frame) {
                    Ok(events) => {
                        if events.is_empty() {
                            continue;
                        }
                        self.pending_events.extend(events);
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
            }
        }
    }
```

- [ ] **Step 6: 清理 import 并确认编译**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-llm --offline 2>&1 | grep -E "^(warning|error)" | head -20`

Expected: 无输出（按提示删掉 unused import）。

- [ ] **Step 7: 运行测试确认行为不变**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline openai::stream 2>&1 | tail -8`

Expected: 与 Step 1 相同数量（或更多）的测试全部 PASS。

- [ ] **Step 8: 新增非法 UTF-8 端到端测试**

在 `openai/stream.rs` 的 `mod tests` 末尾追加：

```rust
    #[tokio::test]
    async fn surfaces_invalid_utf8_as_a_stream_error() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from_static(b"data: \xff\n\n"))];
        let stream = OpenaiStream::new(stream::iter(chunks));
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            Err(ProviderError::Stream(msg)) => {
                assert!(msg.starts_with("invalid UTF-8 in SSE line: "), "got: {msg}")
            }
            other => panic!("expected Stream error, got {other:?}"),
        }
    }
```

- [ ] **Step 9: 运行新测试**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline --lib surfaces_invalid_utf8 2>&1 | tail -8`

Expected: PASS。理由同 Task 2 Step 9：过滤串是全路径子串匹配，测试全路径为
`openai::stream::tests::surfaces_invalid_utf8_as_a_stream_error`，因此必须用有区分度的叶子名 `surfaces_invalid_utf8`
（`openai::stream::surfaces_invalid_utf8` 会切掉 `tests::` 段，匹配 0 个测试）。该叶子名会覆盖三处同类测试：本 Task 新增的
openai 测试、Task 1 的 `sse::tests::surfaces_invalid_utf8_as_an_error_frame` 与 Task 2 的 anthropic 测试。

- [ ] **Step 10: fmt + commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-llm && cd ..
git add yi-agent-rs/crates/yi-agent-llm/src/openai/stream.rs
git commit -m "refactor(llm): openai chat stream uses the shared sse core"
```

---

### Task 4: OpenAI responses 流改用共享模块并全量验证

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs`

**Interfaces:**
- Consumes: `crate::sse::{SseFrame, SseStreamCore}`（Task 1）。
- Produces: 对外不变的 `ResponsesStream<S>`（本任务的 `SseFrame.parse_error` 字段必须消失）。

- [ ] **Step 1: 记录基线行为**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline responses 2>&1 | tail -8`

Expected: 全绿。记下数量。

- [ ] **Step 2: 删除本地 `SseFrame` / `SseLineParser`**

删除该文件里的 `struct SseFrame { data, parse_error }`（基线 18-22）与
`struct SseLineParser` + `impl SseLineParser`（基线 24-80）。

- [ ] **Step 3: 改 `ResponsesStream` 字段与构造**

顶部加 `use crate::sse::{SseFrame, SseStreamCore};`。

字段

```rust
    line_parser: SseLineParser,
    inner: S,
    pending_frames: VecDeque<SseFrame>,
    pending_events: VecDeque<ProviderEvent>,
```

替换为

```rust
    core: SseStreamCore<S>,
    pending_events: VecDeque<ProviderEvent>,
```

`ResponsesStream::new` 里把 `line_parser: SseLineParser::new(), inner, pending_frames: VecDeque::new(),` 换成 `core: SseStreamCore::new(inner),`（保留 `call_ids` / `stopped`）。

- [ ] **Step 4: 删除 `parse_frame` 的 `parse_error` 分支**

删除 `parse_frame` 开头的（基线 112-114）：

```rust
        if let Some(msg) = frame.parse_error {
            return Err(ProviderError::Stream(msg));
        }
```

- [ ] **Step 5: 重写 `poll_next`**

替换为：

```rust
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }

            match self.core.poll_next_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Some(Ok(frame))) => match self.parse_frame(frame) {
                    Ok(events) => {
                        if events.is_empty() {
                            continue;
                        }
                        self.pending_events.extend(events);
                    }
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
            }
        }
    }
```

- [ ] **Step 6: 编译并清理 import**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo build -p yi-agent-llm --offline 2>&1 | grep -E "^(warning|error)" | head -20`

Expected: 无输出。

- [ ] **Step 7: 运行测试确认行为不变**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-llm --offline responses 2>&1 | tail -8`

Expected: 与 Step 1 相同数量（或更多）的测试全部 PASS。

- [ ] **Step 8: 全量验收（本 P0 的最终门槛）**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp

# 1) 三处副本已删：只应命中 sse.rs
grep -rn "struct SseLineParser\|struct SseFrame" crates/yi-agent-llm/src/

# 2) 合成错误编码已消失：应无输出
grep -rn "__parse_error\|parse_error" crates/yi-agent-llm/src/

# 3) 全量测试与 lint
cargo test -p yi-agent-llm --offline 2>&1 | tail -6
cargo clippy -p yi-agent-llm --offline --all-targets -- -D warnings 2>&1 | tail -3
```

Expected:
- 命令 1 只打印 `sse.rs` 的两行定义；
- 命令 2 无输出；
- `cargo test` 全绿；
- clippy 无警告。

- [ ] **Step 9: 工作区级回归**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test --workspace --offline 2>&1 | tail -15`

Expected: 全绿（确认无跨 crate 影响）。

- [ ] **Step 10: fmt + commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-llm && cd ..
git add yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs
git commit -m "refactor(llm): responses stream uses the shared sse core; drop the parse_error field"
```
