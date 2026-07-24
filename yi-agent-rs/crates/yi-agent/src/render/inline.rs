//! InlineRenderer：通过 reedline external_printer 通道输出文本，用 ANSI 颜色区分角色。
//!
//! 核心约束：reedline 的 `external_messages()` 会对每条消息按 `\n` 拆行，
//! 每行单独打印在 prompt 上方。因此流式文本必须缓冲，只在遇到完整行
//! （以 `\n` 结尾）时才发送，避免每个 chunk 各占一行。

use crossbeam_channel::Sender;

use yi_agent_core::{AgentError, AgentEvent, ContentBlock, DoneReason, ToolResult};

use super::Renderer;

/// ANSI 颜色码
const COLOR_USER_BG: &str = "\x1b[48;5;240m"; // 浅灰背景
const COLOR_RESET: &str = "\x1b[0m";
const COLOR_YELLOW: &str = "\x1b[33m";
const COLOR_GREEN: &str = "\x1b[32m";
const COLOR_RED: &str = "\x1b[31m";
const COLOR_RED_BOLD: &str = "\x1b[1;31m";
const COLOR_DIM: &str = "\x1b[2m";

/// 最大摘要长度
const SUMMARY_MAX_LEN: usize = 80;

/// 内联流式渲染器。
///
/// 输出通过 reedline 的 `external_printer` 通道发送，由 reedline 事件循环
/// 负责在 prompt 上方安全地打印，保持光标跟踪一致。
///
/// 流式文本（`AssistantText`）会在 `streaming_buffer` 中缓冲，只在遇到
/// 完整行时发送，确保多个 chunk 不会被拆成多行。
pub struct InlineRenderer {
    /// reedline external_printer 的发送端；None 时回退到直接 stdout（测试用）
    printer_sender: Option<Sender<String>>,
    /// true 表示上一行是助手流式文本，可能还有缓冲未发送
    streaming_text_in_progress: bool,
    /// 流式文本缓冲区：累积未遇到 `\n` 的文本
    streaming_buffer: String,
}

impl InlineRenderer {
    /// 创建一个直接输出到 stdout 的渲染器（无 reedline 通道）。
    ///
    /// 仅用于测试或不使用 reedline 的场景。生产代码应使用 [`with_printer`](Self::with_printer)。
    pub fn new() -> Self {
        Self {
            printer_sender: None,
            streaming_text_in_progress: false,
            streaming_buffer: String::new(),
        }
    }

    /// 创建一个通过 reedline external_printer 通道输出的渲染器。
    pub fn with_printer(sender: Sender<String>) -> Self {
        Self {
            printer_sender: Some(sender),
            streaming_text_in_progress: false,
            streaming_buffer: String::new(),
        }
    }

    /// 如果上一行是未完成的流式文本，通过通道发送剩余缓冲并补换行。
    fn finish_streaming_line(&mut self) {
        if self.streaming_text_in_progress {
            if let Some(sender) = &self.printer_sender {
                if !self.streaming_buffer.is_empty() {
                    let _ = sender.send(format!("{}\n", self.streaming_buffer));
                    self.streaming_buffer.clear();
                }
            } else {
                // 回退：直接 stdout 模式
                println!();
            }
            self.streaming_text_in_progress = false;
        }
    }

    /// 发送一条完整消息行（非流式）通过通道，或回退到 println!。
    fn send_line(&self, text: &str) {
        if let Some(sender) = &self.printer_sender {
            let _ = sender.send(text.to_string());
        } else {
            println!("{text}");
        }
    }

    /// 处理流式文本 chunk：缓冲并按完整行发送。
    fn send_streaming_chunk(&mut self, text: &str) {
        if let Some(sender) = &self.printer_sender {
            self.streaming_buffer.push_str(text);
            // 发送所有完整行，保留最后一个 `\n` 之后的残余
            if let Some(last_newline) = self.streaming_buffer.rfind('\n') {
                let complete: String = self.streaming_buffer[..=last_newline].to_string();
                let remainder: String = self.streaming_buffer[last_newline + 1..].to_string();
                let _ = sender.send(complete);
                self.streaming_buffer = remainder;
            }
        } else {
            // 回退：直接 stdout 流式打印
            use std::io::Write;
            print!("{text}");
            std::io::stdout().flush().ok();
        }
    }

    /// 将工具输入 JSON 截断为摘要
    fn summarize_input(input: &serde_json::Value) -> String {
        let s = input.to_string();
        truncate(&s, SUMMARY_MAX_LEN)
    }

    /// 将工具结果内容截断为摘要
    fn summarize_result(result: &ToolResult) -> String {
        let text = extract_text(&result.content);
        truncate(&text, SUMMARY_MAX_LEN)
    }
}

impl Default for InlineRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer for InlineRenderer {
    fn render_user_input(&mut self, text: &str) {
        self.finish_streaming_line();
        self.send_line(&format!("{COLOR_USER_BG} 你: {text} {COLOR_RESET}"));
    }

    fn render_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Start => {
                // 不打印
            }
            AgentEvent::AssistantText(text) => {
                // 流式追加：缓冲并按完整行发送
                self.send_streaming_chunk(text);
                self.streaming_text_in_progress = true;
            }
            AgentEvent::ToolCall { name, input, .. } => {
                self.finish_streaming_line();
                let summary = Self::summarize_input(input);
                self.send_line(&format!("  {COLOR_YELLOW}⚙{COLOR_RESET} {name}({summary})"));
            }
            AgentEvent::ToolResult { result, .. } => {
                self.finish_streaming_line();
                let summary = Self::summarize_result(result);
                if result.is_error {
                    self.send_line(&format!(
                        "  {COLOR_RED}↳{COLOR_RESET} {COLOR_DIM}{summary}{COLOR_RESET}"
                    ));
                } else {
                    self.send_line(&format!(
                        "  {COLOR_GREEN}↳{COLOR_RESET} {COLOR_DIM}{summary}{COLOR_RESET}"
                    ));
                }
            }
            AgentEvent::Done { reason } => {
                self.finish_streaming_line();
                match reason {
                    DoneReason::EndTurn => {
                        // 正常完成，不额外打印
                    }
                    DoneReason::MaxTurns => {
                        self.send_line(&format!("{COLOR_DIM}· 达到最大轮数限制{COLOR_RESET}"));
                    }
                }
            }
            AgentEvent::Usage(_) => {
                // token 计数事件不打印
            }
            AgentEvent::Cancelled => {
                self.finish_streaming_line();
                self.send_line(&format!("{COLOR_DIM}· 已中断{COLOR_RESET}"));
            }
            AgentEvent::Error(err) => {
                self.finish_streaming_line();
                self.render_error(err);
            }
        }
    }

    fn render_error(&mut self, err: &AgentError) {
        self.finish_streaming_line();
        self.send_line(&format!("{COLOR_RED_BOLD}✗ {err}{COLOR_RESET}"));
    }

    fn render_system(&mut self, msg: &str) {
        self.finish_streaming_line();
        self.send_line(&format!("{COLOR_DIM}· {msg}{COLOR_RESET}"));
    }
}

/// 从 ContentBlock 列表提取纯文本
fn extract_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => out.push_str(t),
            ContentBlock::ToolResult { content, .. } => {
                out.push_str(&extract_text(content));
            }
            ContentBlock::ToolUse { name, input, .. } => {
                out.push_str(&format!("[{name}: {input}]"));
            }
            ContentBlock::Image { .. } => {
                out.push_str("[image]");
            }
        }
    }
    out
}

/// 截断字符串到 max_len，超长加 "..."
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    /// 构建一个带通道的渲染器，返回 (renderer, receiver)
    fn renderer_with_channel() -> (InlineRenderer, crossbeam_channel::Receiver<String>) {
        let (tx, rx) = unbounded();
        (InlineRenderer::with_printer(tx), rx)
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate("hello world this is a long string", 5);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn extract_text_from_text_block() {
        let blocks = vec![ContentBlock::Text("hello".into())];
        assert_eq!(extract_text(&blocks), "hello");
    }

    #[test]
    fn extract_text_from_nested_tool_result() {
        let inner = vec![ContentBlock::Text("result text".into())];
        let blocks = vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: inner,
            is_error: false,
        }];
        assert_eq!(extract_text(&blocks), "result text");
    }

    #[test]
    fn summarize_input_truncates_long_json() {
        let long_value = serde_json::json!({
            "path": "this/is/a/very/long/path/that/exceeds/the/limit/and/should/be/truncated"
        });
        let summary = InlineRenderer::summarize_input(&long_value);
        assert!(summary.len() <= SUMMARY_MAX_LEN + 3); // +3 for "..."
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn summarize_result_extracts_text() {
        let result = ToolResult::text("operation succeeded");
        let summary = InlineRenderer::summarize_result(&result);
        assert!(summary.contains("operation succeeded"));
    }

    #[test]
    fn render_cancelled_resets_streaming_state() {
        let mut renderer = InlineRenderer::new();
        renderer.streaming_text_in_progress = true;
        renderer.render_agent_event(&AgentEvent::Cancelled);
        assert!(!renderer.streaming_text_in_progress);
    }

    #[test]
    fn render_cancelled_after_streaming_resets_state() {
        let mut renderer = InlineRenderer::new();
        renderer.render_agent_event(&AgentEvent::AssistantText("partial".into()));
        assert!(renderer.streaming_text_in_progress);
        renderer.render_agent_event(&AgentEvent::Cancelled);
        assert!(!renderer.streaming_text_in_progress);
    }

    #[test]
    fn render_usage_event_does_not_print() {
        use yi_agent_core::provider::TokenUsage;
        let mut renderer = InlineRenderer::new();
        renderer.streaming_text_in_progress = true;
        renderer.render_agent_event(&AgentEvent::Usage(TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        }));
        assert!(renderer.streaming_text_in_progress);
    }

    // --- 通道模式测试 ---

    #[test]
    fn assistant_text_sends_through_channel() {
        let (mut renderer, rx) = renderer_with_channel();
        // 完整行（含换行）应立即发送
        renderer.render_agent_event(&AgentEvent::AssistantText("hello\n".into()));
        assert_eq!(rx.try_recv(), Ok("hello\n".to_string()));
        // 通道中不应有多余消息
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn assistant_text_partial_line_buffers_until_finish() {
        let (mut renderer, rx) = renderer_with_channel();
        // 不含换行的 chunk 应缓冲，不发送
        renderer.render_agent_event(&AgentEvent::AssistantText("partial".into()));
        assert!(rx.try_recv().is_err());
        assert!(renderer.streaming_text_in_progress);
        // finish_streaming_line（由下一个事件触发）应发送缓冲 + 换行
        renderer.render_agent_event(&AgentEvent::Cancelled);
        assert_eq!(rx.try_recv(), Ok("partial\n".to_string()));
        // Cancelled 本身也发送消息
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn assistant_text_multiple_chunks_concatenate() {
        let (mut renderer, rx) = renderer_with_channel();
        // 多个不含换行的 chunk 应拼接，最终作为一条消息发送
        renderer.render_agent_event(&AgentEvent::AssistantText("Hello".into()));
        renderer.render_agent_event(&AgentEvent::AssistantText(", ".into()));
        renderer.render_agent_event(&AgentEvent::AssistantText("world!".into()));
        // 都应缓冲，不发送
        assert!(rx.try_recv().is_err());
        // 触发 flush
        renderer.finish_streaming_line();
        assert_eq!(rx.try_recv(), Ok("Hello, world!\n".to_string()));
    }

    #[test]
    fn assistant_text_multi_line_chunk_splits_correctly() {
        let (mut renderer, rx) = renderer_with_channel();
        // 含多个换行的 chunk：完整行立即发送，残余缓冲
        renderer.render_agent_event(&AgentEvent::AssistantText("line1\nline2\npartial".into()));
        assert_eq!(rx.try_recv(), Ok("line1\nline2\n".to_string()));
        // partial 仍在缓冲中
        assert!(rx.try_recv().is_err());
        // flush
        renderer.finish_streaming_line();
        assert_eq!(rx.try_recv(), Ok("partial\n".to_string()));
    }

    #[test]
    fn tool_call_sends_formatted_line_through_channel() {
        let (mut renderer, rx) = renderer_with_channel();
        let input = serde_json::json!({"path": "/tmp/test"});
        renderer.render_agent_event(&AgentEvent::ToolCall {
            name: "read_file".into(),
            input: input.clone(),
            id: "t1".into(),
        });
        let msg = rx.try_recv().unwrap();
        assert!(msg.contains("read_file"));
        assert!(msg.contains("⚙"));
    }

    #[test]
    fn tool_result_sends_formatted_line_through_channel() {
        let (mut renderer, rx) = renderer_with_channel();
        let result = ToolResult::text("success");
        renderer.render_agent_event(&AgentEvent::ToolResult {
            result: result.clone(),
            id: "t1".into(),
        });
        let msg = rx.try_recv().unwrap();
        assert!(msg.contains("↳"));
        assert!(msg.contains("success"));
    }

    #[test]
    fn finish_streaming_line_sends_newline_for_buffered_text() {
        let (mut renderer, rx) = renderer_with_channel();
        renderer.render_agent_event(&AgentEvent::AssistantText("buffered".into()));
        renderer.finish_streaming_line();
        assert_eq!(rx.try_recv(), Ok("buffered\n".to_string()));
        // 再次 finish 不应发送任何内容
        renderer.finish_streaming_line();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn finish_streaming_line_noop_when_buffer_empty() {
        let (mut renderer, rx) = renderer_with_channel();
        // 完整行已发送，buffer 为空
        renderer.render_agent_event(&AgentEvent::AssistantText("complete\n".into()));
        let _ = rx.try_recv(); // drain "complete\n"
        renderer.finish_streaming_line();
        // buffer 为空，不应发送额外消息
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn render_system_sends_through_channel() {
        let (mut renderer, rx) = renderer_with_channel();
        renderer.render_system("test message");
        let msg = rx.try_recv().unwrap();
        assert!(msg.contains("test message"));
        assert!(msg.contains("·"));
    }

    #[test]
    fn render_user_input_sends_through_channel() {
        let (mut renderer, rx) = renderer_with_channel();
        renderer.render_user_input("hello agent");
        let msg = rx.try_recv().unwrap();
        assert!(msg.contains("hello agent"));
        assert!(msg.contains("你:"));
    }
}
