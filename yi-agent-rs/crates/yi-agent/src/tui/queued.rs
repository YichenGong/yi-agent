use std::collections::VecDeque;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// 渲染排队预览区。返回若干行,空队列返回空 Vec。
///
/// - 标题行:`⌛ 排队中 (N)`,dim,N = 总数
/// - 每条消息:`  ↳ ` 前缀,dim + italic
/// - 最多显示 3 行,超出显示 `… 还有 X 条` 计数行
#[allow(dead_code)]
pub fn render_queued_preview(queued: &VecDeque<String>, _width: u16) -> Vec<Line<'static>> {
    if queued.is_empty() {
        return Vec::new();
    }

    let dim = Style::new().add_modifier(Modifier::DIM);
    let dim_italic = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("⌛ 排队中 ({})", queued.len()),
        dim,
    )]));

    let visible_count = 3;
    let total = queued.len();
    let show = total.min(visible_count);
    for text in queued.iter().take(show) {
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", dim),
            Span::styled(text.clone(), dim_italic),
        ]));
    }
    if total > visible_count {
        let remaining = total - visible_count;
        lines.push(Line::from(vec![Span::styled(
            format!("  … 还有 {remaining} 条"),
            dim,
        )]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_returns_no_lines() {
        let q = VecDeque::new();
        let lines = render_queued_preview(&q, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn single_message_has_header_and_one_row() {
        let mut q = VecDeque::new();
        q.push_back("hello".to_string());
        let lines = render_queued_preview(&q, 80);
        assert_eq!(lines.len(), 2);
        let title: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(title, "⌛ 排队中 (1)");
    }

    #[test]
    fn three_messages_shows_all_three() {
        let mut q = VecDeque::new();
        q.push_back("a".to_string());
        q.push_back("b".to_string());
        q.push_back("c".to_string());
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn five_messages_truncates_with_count_line() {
        let mut q = VecDeque::new();
        for i in 0..5 {
            q.push_back(format!("msg{i}"));
        }
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages + 1 overflow count
        assert_eq!(lines.len(), 5);
        let last: String = lines[4]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(last, "  … 还有 2 条");
    }

    #[test]
    fn header_shows_total_not_visible() {
        let mut q = VecDeque::new();
        for _ in 0..10 {
            q.push_back("x".to_string());
        }
        let lines = render_queued_preview(&q, 80);
        let title: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(title, "⌛ 排队中 (10)");
    }
}
