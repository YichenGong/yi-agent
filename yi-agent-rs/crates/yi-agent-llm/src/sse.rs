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
