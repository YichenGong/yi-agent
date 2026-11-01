//! 把落盘的 `Item` 还原成 TUI 的 `HistoryCell`。
//!
//! 没有这一层，`resume` 之后上下文是对的、屏幕却是空的——用户会以为历史丢了。

use yi_agent_app_server::protocol::{Item, ToolStatus};

use crate::tui::cell::{CallState, HistoryCell};

/// 按顺序把 items 摊平成可渲染的 cells。
///
/// 一个 `ToolCall` 会产出两个 cell（调用 + 结果），与实时渲染的形态一致。
/// 不插 `Separator`：分隔线只在 turn 边界由实时路径插入，这里再插会让重开
/// 一次会话就多出一堆横线。
pub fn items_to_cells(items: &[Item]) -> Vec<HistoryCell> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::UserMessage { text, .. } | Item::UserInterjection { text, .. } => {
                out.push(HistoryCell::UserMessage { text: text.clone() });
            }
            Item::AgentMessage { text, .. } => {
                out.push(HistoryCell::AssistantMessage {
                    markdown: text.clone(),
                });
            }
            Item::ToolCall {
                id,
                name,
                input,
                status,
                result,
                ..
            } => {
                // `ToolStatus` 与 `CallState` 不同名：Completed -> Success。
                // 写成 Completed -> Failed 会让所有历史工具调用变红。
                let state = match status {
                    ToolStatus::Running => CallState::Running,
                    ToolStatus::Completed => CallState::Success,
                    ToolStatus::Failed => CallState::Failed,
                };
                out.push(HistoryCell::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    state,
                    expanded: false,
                });
                let (result_text, is_error) = match status {
                    ToolStatus::Completed => (result.clone().unwrap_or_default(), false),
                    ToolStatus::Failed => (
                        result.clone().unwrap_or_else(|| "(无输出)".to_string()),
                        true,
                    ),
                    // 崩溃残留：永远停在 Running，不给说明会像卡死。
                    ToolStatus::Running => ("中断".to_string(), false),
                };
                out.push(HistoryCell::ToolResult {
                    id: id.clone(),
                    result_text,
                    is_error,
                    expanded: false,
                });
            }
            Item::Thinking { text, redacted, .. } => {
                // redacted 时 text 是空串；硬映射会渲染出一条空白折叠块。
                if *redacted || text.is_empty() {
                    continue;
                }
                out.push(HistoryCell::Reasoning {
                    text: text.clone(),
                    expanded: false,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::cell::{CallState, HistoryCell};
    use yi_agent_app_server::protocol::ToolStatus;

    fn user(text: &str) -> Item {
        Item::UserMessage {
            id: "u".into(),
            text: text.into(),
            attachments: Vec::new(),
            images: Vec::new(),
        }
    }

    fn tool(status: ToolStatus, result: Option<&str>) -> Item {
        Item::ToolCall {
            id: "t".into(),
            call_id: "c".into(),
            name: "bash".into(),
            input: serde_json::json!({ "cmd": "ls" }),
            status,
            result: result.map(str::to_string),
            images: Vec::new(),
        }
    }

    #[test]
    fn user_message_maps_to_user_cell() {
        let cells = items_to_cells(&[user("hi")]);
        assert_eq!(cells.len(), 1);
        assert!(matches!(&cells[0], HistoryCell::UserMessage { text } if text == "hi"));
    }

    #[test]
    fn completed_tool_call_maps_to_success_not_failed() {
        let cells = items_to_cells(&[tool(ToolStatus::Completed, Some("ok"))]);
        // ToolCall + ToolResult
        assert_eq!(cells.len(), 2);
        assert!(
            matches!(
                &cells[0],
                HistoryCell::ToolCall {
                    state: CallState::Success,
                    ..
                }
            ),
            "Completed 必须映射成 Success；写成 Failed 会让所有历史工具调用变红"
        );
        assert!(matches!(
            &cells[1],
            HistoryCell::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn running_tool_call_reads_as_interrupted() {
        let cells = items_to_cells(&[tool(ToolStatus::Running, None)]);
        assert!(matches!(
            &cells[0],
            HistoryCell::ToolCall {
                state: CallState::Running,
                ..
            }
        ));
        match &cells[1] {
            HistoryCell::ToolResult {
                result_text,
                is_error,
                ..
            } => {
                assert_eq!(result_text, "中断");
                assert!(!is_error, "中断不是错误");
            }
            other => panic!("期望 ToolResult，实际 {other:?}"),
        }
    }

    #[test]
    fn failed_tool_call_without_output_is_marked() {
        let cells = items_to_cells(&[tool(ToolStatus::Failed, None)]);
        assert!(matches!(
            &cells[0],
            HistoryCell::ToolCall {
                state: CallState::Failed,
                ..
            }
        ));
        match &cells[1] {
            HistoryCell::ToolResult {
                result_text,
                is_error,
                ..
            } => {
                assert_eq!(result_text, "(无输出)");
                assert!(is_error);
            }
            other => panic!("期望 ToolResult，实际 {other:?}"),
        }
    }

    #[test]
    fn redacted_thinking_produces_no_cell() {
        let item = Item::Thinking {
            id: "th".into(),
            text: String::new(),
            redacted: true,
        };
        assert!(
            items_to_cells(&[item]).is_empty(),
            "redacted 的 Thinking text 为空，硬映射会渲染出一条空白折叠块"
        );
    }

    #[test]
    fn agent_message_and_interjection_are_distinguished_from_plain_user() {
        let cells = items_to_cells(&[
            Item::AgentMessage {
                id: "a".into(),
                text: "answer".into(),
            },
            Item::UserInterjection {
                id: "i".into(),
                text: "mid-turn".into(),
                attachments: Vec::new(),
                images: Vec::new(),
            },
        ]);
        assert_eq!(cells.len(), 2);
        assert!(
            matches!(&cells[0], HistoryCell::AssistantMessage { markdown } if markdown == "answer")
        );
        // 中途插话是用户说的，渲染成用户气泡；它不是助手回答。
        assert!(matches!(&cells[1], HistoryCell::UserMessage { text } if text == "mid-turn"));
    }

    #[test]
    fn completed_tool_call_without_result_yields_empty_text_not_error() {
        let cells = items_to_cells(&[tool(ToolStatus::Completed, None)]);
        match &cells[1] {
            HistoryCell::ToolResult {
                result_text,
                is_error,
                ..
            } => {
                assert_eq!(result_text, "");
                assert!(!is_error, "Completed 没有 result 不等于失败");
            }
            other => panic!("期望 ToolResult，实际 {other:?}"),
        }
    }

    #[test]
    fn empty_items_yield_no_cells() {
        assert!(items_to_cells(&[]).is_empty());
    }

    #[test]
    fn thinking_is_folded_by_default() {
        let item = Item::Thinking {
            id: "th".into(),
            text: "reasoning".into(),
            redacted: false,
        };
        let cells = items_to_cells(&[item]);
        assert!(matches!(
            &cells[0],
            HistoryCell::Reasoning {
                expanded: false,
                ..
            }
        ));
    }
}
