//! InlineRenderer：流式打印到 stdout，用 ANSI 颜色区分角色。

use std::io::{self, Write};

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
/// 维护"当前行是否正在流式输出助手文本"的状态，
/// 以便在切换到其他事件时补换行。
pub struct InlineRenderer {
    /// true 表示上一行是助手流式文本，可能还没换行
    streaming_text_in_progress: bool,
}

impl InlineRenderer {
    pub fn new() -> Self {
        Self {
            streaming_text_in_progress: false,
        }
    }

    /// 如果上一行是未完成的流式文本，补一个换行
    fn finish_streaming_line(&mut self) {
        if self.streaming_text_in_progress {
            println!();
            self.streaming_text_in_progress = false;
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
        println!("{COLOR_USER_BG} 你: {text} {COLOR_RESET}");
    }

    fn render_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Start => {
                // 不打印
            }
            AgentEvent::AssistantText(text) => {
                // 流式追加：不加前缀，不换行
                print!("{text}");
                io::stdout().flush().ok();
                self.streaming_text_in_progress = true;
            }
            AgentEvent::ToolCall { name, input, .. } => {
                self.finish_streaming_line();
                let summary = Self::summarize_input(input);
                println!("  {COLOR_YELLOW}⚙{COLOR_RESET} {name}({summary})");
            }
            AgentEvent::ToolResult { result, .. } => {
                self.finish_streaming_line();
                let summary = Self::summarize_result(result);
                if result.is_error {
                    println!("  {COLOR_RED}↳{COLOR_RESET} {COLOR_DIM}{summary}{COLOR_RESET}");
                } else {
                    println!("  {COLOR_GREEN}↳{COLOR_RESET} {COLOR_DIM}{summary}{COLOR_RESET}");
                }
            }
            AgentEvent::Done { reason } => {
                self.finish_streaming_line();
                match reason {
                    DoneReason::EndTurn => {
                        // 正常完成，不额外打印
                    }
                    DoneReason::MaxTurns => {
                        println!("{COLOR_DIM}· 达到最大轮数限制{COLOR_RESET}");
                    }
                }
            }
            AgentEvent::Usage(_) => {
                // token 计数事件不打印
            }
            AgentEvent::Cancelled => {
                self.finish_streaming_line();
                println!("{COLOR_DIM}· 已中断{COLOR_RESET}");
            }
            AgentEvent::Error(err) => {
                self.finish_streaming_line();
                self.render_error(err);
            }
        }
    }

    fn render_error(&mut self, err: &AgentError) {
        self.finish_streaming_line();
        println!("{COLOR_RED_BOLD}✗ {err}{COLOR_RESET}");
    }

    fn render_system(&mut self, msg: &str) {
        self.finish_streaming_line();
        println!("{COLOR_DIM}· {msg}{COLOR_RESET}");
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
    fn render_cancelled_prints_interrupted_message() {
        let mut renderer = InlineRenderer::new();
        // 捕获 stdout 验证渲染输出包含 "已中断"
        // 注意:InlineRenderer 直接 println! 到 stdout,无法在测试中捕获输出。
        // 我们验证 render_agent_event 对 Cancelled 不 panic 且不影响 streaming 状态。
        // 如果正在流式输出,Cancelled 应先补换行。
        renderer.streaming_text_in_progress = true;
        renderer.render_agent_event(&AgentEvent::Cancelled);
        // 流式状态应被重置(render 完成后不再处于流式状态)
        assert!(!renderer.streaming_text_in_progress);
    }

    #[test]
    fn render_cancelled_after_streaming_resets_state() {
        let mut renderer = InlineRenderer::new();
        // 模拟流式文本输出中
        renderer.render_agent_event(&AgentEvent::AssistantText("partial".into()));
        assert!(renderer.streaming_text_in_progress);
        // Cancelled 事件应补换行并重置状态
        renderer.render_agent_event(&AgentEvent::Cancelled);
        assert!(!renderer.streaming_text_in_progress);
    }

    #[test]
    fn render_usage_event_does_not_print() {
        use yi_agent_core::provider::TokenUsage;
        let mut renderer = InlineRenderer::new();
        // Usage 事件不应影响流式状态
        renderer.streaming_text_in_progress = true;
        renderer.render_agent_event(&AgentEvent::Usage(TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        }));
        // 流式状态不应被重置(Usage 不打印任何内容)
        assert!(renderer.streaming_text_in_progress);
    }
}
