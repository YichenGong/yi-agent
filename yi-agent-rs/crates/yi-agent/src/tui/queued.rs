use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// 提交结果：调用方必须处理三态，避免"拒收"被静默忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Agent 空闲：消息已立即发送。
    Sent,
    /// 有轮次在途：消息已入队等待。
    Queued,
    /// 有轮次在途且队列已满：消息未被接受。
    Rejected,
}

/// 待发请求队列：队列的唯一所有者。
///
/// 不变量：`in_flight` 为真时队列是**唯一**的待发缓冲区，通道中至多 1 条消息。
/// 因此底层通道永不满，`try_send` 不会阻塞调用线程。
#[allow(dead_code)]
pub struct PendingQueue {
    items: Vec<String>,
    in_flight: bool,
}

#[allow(dead_code)]
impl PendingQueue {
    /// 与 `main.rs` 的输入通道容量一致。
    pub const CAPACITY: usize = 16;

    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            in_flight: false,
        }
    }

    /// 空闲则立即发送，忙则入队，满则拒收。
    pub fn submit(&mut self, text: String) -> SubmitOutcome {
        if !self.in_flight {
            self.in_flight = true;
            return SubmitOutcome::Sent;
        }
        if self.items.len() >= Self::CAPACITY {
            return SubmitOutcome::Rejected;
        }
        self.items.push(text);
        SubmitOutcome::Queued
    }

    /// 回合结束：弹出下一条待发消息。
    ///
    /// 弹出时 `in_flight` 保持为真——被弹出的消息成为新的在途轮次。
    /// 队列为空时才复位，表示真正回到空闲。
    pub fn on_turn_end(&mut self) -> Option<String> {
        match self.items.is_empty() {
            false => Some(self.items.remove(0)),
            true => {
                self.in_flight = false;
                None
            }
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 供预览渲染使用的只读视图。
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// 丢弃全部待发消息并复位 `in_flight`，返回丢弃条数。
    ///
    /// 用于 `/clear`：driver 会重建 agent，TUI 必须同时复位在途标记，
    /// 否则会认为仍有轮次在跑而永不再发送。
    pub fn clear(&mut self) -> usize {
        let dropped = self.items.len();
        self.items.clear();
        self.in_flight = false;
        dropped
    }
}

impl Default for PendingQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// 渲染排队预览区。返回若干行,空队列返回空 Vec。
///
/// - 标题行:`⌛ 排队中 (N)`,dim,N = 总数
/// - 每条消息:`  ↳ ` 前缀,dim + italic
/// - 最多显示 3 行,超出显示 `… 还有 X 条` 计数行
pub fn render_queued_preview(items: &[String], _width: u16) -> Vec<Line<'static>> {
    if items.is_empty() {
        return Vec::new();
    }

    let dim = Style::new().add_modifier(Modifier::DIM);
    let dim_italic = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);

    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        format!("⌛ 排队中 ({})", items.len()),
        dim,
    )]));

    let visible_count = 3;
    let total = items.len();
    let show = total.min(visible_count);
    for text in items.iter().take(show) {
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

    use super::{PendingQueue, SubmitOutcome};

    #[test]
    fn idle_submit_sends_immediately_and_leaves_queue_empty() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn busy_submit_queues_without_sending() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.len(), 1);
        assert_eq!(q.items(), ["b".to_string()]);
    }

    #[test]
    fn full_queue_rejects_and_keeps_existing_items() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("first".into()), SubmitOutcome::Sent);
        // 16 slots: the first turn is in flight, so 16 more fill the queue.
        for i in 0..PendingQueue::CAPACITY {
            assert_eq!(q.submit(format!("m{i}")), SubmitOutcome::Queued);
        }
        assert_eq!(q.len(), PendingQueue::CAPACITY);
        assert_eq!(q.submit("overflow".into()), SubmitOutcome::Rejected);
        assert_eq!(q.len(), PendingQueue::CAPACITY);
        assert!(
            !q.items().iter().any(|t| t == "overflow"),
            "a rejected message must not enter the queue"
        );
    }

    #[test]
    fn turn_end_pops_one_and_keeps_in_flight() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("c".into()), SubmitOutcome::Queued);

        assert_eq!(q.on_turn_end(), Some("b".to_string()));
        assert_eq!(q.len(), 1);
        // The promoted message became the new in-flight turn, so a further
        // submit must still queue rather than send.
        assert_eq!(q.submit("d".into()), SubmitOutcome::Queued);
    }

    #[test]
    fn turn_end_on_empty_queue_resets_in_flight() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.on_turn_end(), None);
        // in_flight reset: the next submit sends immediately again.
        assert_eq!(q.submit("b".into()), SubmitOutcome::Sent);
    }

    #[test]
    fn turn_end_emits_one_item_per_call() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("c".into()), SubmitOutcome::Queued);

        assert_eq!(q.on_turn_end(), Some("b".to_string()));
        assert_eq!(q.len(), 1, "only one message is promoted per turn end");
        assert_eq!(q.on_turn_end(), Some("c".to_string()));
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn clear_drops_waiting_messages_and_resets_in_flight() {
        let mut q = PendingQueue::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("b".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("c".into()), SubmitOutcome::Queued);

        assert_eq!(q.clear(), 2, "clear reports how many messages were dropped");
        assert!(q.is_empty());
        assert_eq!(q.submit("d".into()), SubmitOutcome::Sent);
    }

    #[test]
    fn empty_queue_returns_no_lines() {
        let q: Vec<String> = Vec::new();
        let lines = render_queued_preview(&q, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn single_message_has_header_and_one_row() {
        let q = vec!["hello".to_string()];
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
        let q = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let lines = render_queued_preview(&q, 80);
        // 1 header + 3 messages
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn five_messages_truncates_with_count_line() {
        let q: Vec<String> = (0..5).map(|i| format!("msg{i}")).collect();
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
        let q: Vec<String> = (0..10).map(|_| "x".to_string()).collect();
        let lines = render_queued_preview(&q, 80);
        let title: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(title, "⌛ 排队中 (10)");
    }
}
