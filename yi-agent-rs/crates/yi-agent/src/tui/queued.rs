use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::wrap::wrap_by_display_width;

/// 提交结果：调用方必须处理三态，避免"拒收"被静默忽略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Agent 空闲：本条开启一个新轮次，已交给 driver 当 prompt 发送。
    ///
    /// 注意"开启轮次"的消息**不进入记账**：它走的是 `run(prompt)`，不是注入
    /// 通道，因此永远不会有 `InterjectionAccepted` 回执来冲销它。
    Sent,
    /// 有轮次在途：消息已投递给 driver，待 core 回执（已受理或已退回）。
    ///
    /// 投递本身由调用方完成（`input_tx.try_send`）；这里只记下"欠着一条回执"。
    Queued,
    /// 有轮次在途且投递未成功（通道满 / 未取到 inbox 句柄）：消息未被接受。
    Rejected,
}

/// 已投递给 driver、尚未收到 core 回执的中途追加消息。
///
/// 它取代了原先"攒到轮次结束再发"的排队队列：消息现在**立即**投出，这里留下的
/// 只是**记账**——哪些投递 core 既没有受理、也没有退回。预览区渲染的就是这份
/// 列表，所以用户看到的条数含义是"已送达，待生效"。
///
/// 不变量：`in_flight` 为真时**只有**本列表是待发缓冲区；`items` 只装中途追加，
/// 不含开启轮次的那条 prompt（见 `SubmitOutcome::Sent`）。
pub struct DeliveredInterjections {
    items: Vec<String>,
    in_flight: bool,
}

impl DeliveredInterjections {
    /// 与 `main.rs` 的输入通道容量一致。
    pub const CAPACITY: usize = 16;

    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            in_flight: false,
        }
    }

    /// 空闲则开启新轮次，忙则记账一条待回执投递，满则拒收。
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
    ///
    /// 正常路径下这里返回 `None`：结束前 core 会把未消费的投递经
    /// `InterjectionsReturned` 退回（已由 `take_returned` 摘除），已消费的则
    /// 经 `InterjectionAccepted` 摘除（`on_receipt`），故 `items` 已空。它保留
    /// 兜底语义：万一回执缺失而回合已结束，与其让消息永远挂在预览区，不如把它
    /// 转正为下一轮的 prompt。
    pub fn on_turn_end(&mut self) -> Option<String> {
        match self.items.is_empty() {
            false => Some(self.items.remove(0)),
            true => {
                self.in_flight = false;
                None
            }
        }
    }

    /// 收到一条 `InterjectionAccepted`：冲销最旧的一条在途投递。
    ///
    /// 回执按投递顺序到达（inbox 是 FIFO，且本进程是它唯一的生产者），故按
    /// 顺序摘除即可，不需要额外的 id 映射。空列表时是空操作——重复回执不该
    /// 让计数变成负数。
    pub fn on_receipt(&mut self) {
        if !self.items.is_empty() {
            self.items.remove(0);
        }
    }

    /// 取回 `count` 条 core 未能消费的投递（最旧优先），交还调用方恢复输入。
    ///
    /// `count` 由 `InterjectionsReturned` 的条数给出；超出实际在途条数时按可
    /// 用量截断。
    pub fn take_returned(&mut self, count: usize) -> Vec<String> {
        let take = count.min(self.items.len());
        self.items.drain(0..take).collect()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
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

impl Default for DeliveredInterjections {
    fn default() -> Self {
        Self::new()
    }
}

/// 渲染排队预览区。返回若干行,空队列返回空 Vec。
///
/// - 标题行:`⌛ 已送达，待生效 (N)`,dim,N = 总数
/// - 每条消息:`  ↳ ` 前缀,dim + italic
/// - 最多显示 3 行,超出显示 `… 还有 X 条` 计数行
pub fn render_queued_preview(items: &[String], width: u16) -> Vec<Line<'static>> {
    if items.is_empty() {
        return Vec::new();
    }

    let w = width.max(1) as usize;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let dim_italic = Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC);

    let mut lines = Vec::new();
    // Each preview line is folded to the terminal width: a long queued message
    // is otherwise one over-wide `Line`, whose right side ratatui drops.
    lines.push(Line::from(vec![Span::styled(
        fold_to_width(&format!("⌛ 已送达，待生效 ({})", items.len()), w),
        dim,
    )]));

    let visible_count = 3;
    let total = items.len();
    let show = total.min(visible_count);
    for text in items.iter().take(show) {
        for chunk in wrap_by_display_width(text, w, "  ↳ ", "    ") {
            lines.push(Line::styled(chunk, dim_italic));
        }
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

/// Truncate `text` to `width` display columns, appending an ellipsis when it
/// does not fit. Used for the fixed-shape count header.
fn fold_to_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_w > budget {
            break;
        }
        out.push(ch);
        used += ch_w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    use super::{DeliveredInterjections, SubmitOutcome};

    #[test]
    fn queued_preview_folds_long_messages_to_width() {
        // The preview used to take `_width` and ignore it, so a long queued
        // message rendered wider than the terminal and was clipped.
        let long = "这是一条很长的排队消息需要按显示宽度折行否则右侧会被截断掉看不见真的看不见";
        let items = vec![long.to_string(), "short".to_string()];
        for width in [20u16, 40, 80] {
            for (i, line) in render_queued_preview(&items, width).iter().enumerate() {
                let w: usize = line
                    .spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                assert!(
                    w <= width as usize,
                    "width {width}: queued line {i} is {w} cols: {line:?}"
                );
            }
        }
    }

    #[test]
    fn queued_preview_keeps_full_message_text_when_folding() {
        let long = "这是一条很长的排队消息需要按显示宽度折行否则右侧会被截断掉看不见";
        let items = vec![long.to_string()];
        let rendered: String = render_queued_preview(&items, 20)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        for ch in long.chars() {
            assert!(
                rendered.contains(ch),
                "queued preview lost {ch:?}: {rendered:?}"
            );
        }
    }

    #[test]
    fn idle_submit_sends_immediately_and_leaves_queue_empty() {
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn busy_submit_is_reported_as_queued_not_sent() {
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("first".into()), SubmitOutcome::Sent);
        // The caller is responsible for sending on `Queued`; the queue only
        // records that the message is outstanding.
        assert_eq!(q.submit("second".into()), SubmitOutcome::Queued);
        assert_eq!(q.len(), 1);
        assert_eq!(q.items(), ["second".to_string()]);
    }

    #[test]
    fn sent_message_is_not_tracked_because_it_gets_no_receipt() {
        // The opening message of a turn goes out via `Agent::run`, which never
        // emits `InterjectionAccepted`. Tracking it would leave a phantom
        // entry in the preview forever.
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("opening".into()), SubmitOutcome::Sent);
        assert!(q.is_empty());
        assert!(q.items().is_empty());
    }

    #[test]
    fn accepted_receipt_retires_the_oldest_delivery() {
        let mut q = DeliveredInterjections::new();
        q.submit("first".into());
        q.submit("second".into());
        q.submit("third".into());
        // The opening `first` was Sent (untracked), so only two are outstanding.
        assert_eq!(q.items(), ["second".to_string(), "third".to_string()]);
        // FIFO: the inbox hands receipts back in delivery order.
        q.on_receipt();
        assert_eq!(q.items(), ["third".to_string()]);
        q.on_receipt();
        assert!(q.is_empty());
        // Receipts do not end the turn; only `on_turn_end` clears `in_flight`.
        assert_eq!(q.submit("fourth".into()), SubmitOutcome::Queued);
    }

    #[test]
    fn receipt_on_empty_list_is_a_noop() {
        // A duplicate or unexpected receipt must not underflow the count.
        let mut q = DeliveredInterjections::new();
        q.on_receipt();
        assert!(q.is_empty());
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        q.on_receipt();
        assert!(q.is_empty());
    }

    #[test]
    fn returned_items_come_back_in_delivery_order() {
        let mut q = DeliveredInterjections::new();
        q.submit("first".into());
        q.submit("second".into());
        q.submit("third".into());
        let restored = q.take_returned(2);
        assert_eq!(restored, vec!["second".to_string(), "third".to_string()]);
        assert!(q.is_empty());
    }

    #[test]
    fn take_returned_clamps_to_what_is_outstanding() {
        let mut q = DeliveredInterjections::new();
        q.submit("first".into());
        q.submit("second".into());
        let restored = q.take_returned(5);
        assert_eq!(restored, vec!["second".to_string()]);
        assert!(q.is_empty());
    }

    #[test]
    fn header_says_delivered_pending_not_queued() {
        let lines = render_queued_preview(&["one".to_string()], 40);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(header, "⌛ 已送达，待生效 (1)");
    }

    #[test]
    fn full_queue_rejects_and_keeps_existing_items() {
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("first".into()), SubmitOutcome::Sent);
        // 16 slots: the opening `Sent` message is not tracked, so 16 more fill
        // the outstanding list.
        for i in 0..DeliveredInterjections::CAPACITY {
            assert_eq!(q.submit(format!("m{i}")), SubmitOutcome::Queued);
        }
        assert_eq!(q.len(), DeliveredInterjections::CAPACITY);
        assert_eq!(q.submit("overflow".into()), SubmitOutcome::Rejected);
        assert_eq!(q.len(), DeliveredInterjections::CAPACITY);
        assert!(
            !q.items().iter().any(|t| t == "overflow"),
            "a rejected message must not enter the queue"
        );
        // Nothing was displaced by the rejected submit.
        let expected: Vec<String> = (0..DeliveredInterjections::CAPACITY)
            .map(|i| format!("m{i}"))
            .collect();
        assert_eq!(q.items(), expected.as_slice());
    }

    #[test]
    fn turn_end_pops_one_and_keeps_in_flight() {
        let mut q = DeliveredInterjections::new();
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
    fn turn_end_after_receipts_and_returns_finds_nothing_to_promote() {
        // The normal path: everything delivered mid-turn was either accepted
        // (receipt) or handed back (return), so the fallback has nothing to do.
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("opening".into()), SubmitOutcome::Sent);
        assert_eq!(q.submit("kept".into()), SubmitOutcome::Queued);
        assert_eq!(q.submit("dropped".into()), SubmitOutcome::Queued);
        q.on_receipt();
        q.take_returned(1);
        assert!(q.is_empty());
        assert_eq!(q.on_turn_end(), None);
        // in_flight reset by the empty turn end: the next submit opens a turn.
        assert_eq!(q.submit("next".into()), SubmitOutcome::Sent);
    }

    #[test]
    fn turn_end_on_empty_queue_resets_in_flight() {
        let mut q = DeliveredInterjections::new();
        assert_eq!(q.submit("a".into()), SubmitOutcome::Sent);
        assert_eq!(q.on_turn_end(), None);
        // in_flight reset: the next submit sends immediately again.
        assert_eq!(q.submit("b".into()), SubmitOutcome::Sent);
    }

    #[test]
    fn turn_end_emits_one_item_per_call() {
        let mut q = DeliveredInterjections::new();
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
        let mut q = DeliveredInterjections::new();
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
        assert_eq!(title, "⌛ 已送达，待生效 (1)");
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
        assert_eq!(title, "⌛ 已送达，待生效 (10)");
    }
}
