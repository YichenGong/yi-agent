use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::card::{Card, CardId, CardState};

/// 状态迁移被拒绝的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    UnknownCard(CardId),
    Illegal { from: CardState, to: CardState },
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::UnknownCard(id) => write!(f, "unknown card: {id}"),
            TransitionError::Illegal { from, to } => {
                write!(f, "illegal transition: {from:?} -> {to:?}")
            }
        }
    }
}

impl std::error::Error for TransitionError {}

/// 卡片队列。槽位只由 `CardState::Running` 占用。
///
/// `Serialize`/`Deserialize` 让插件进程能把队列原子落盘到 `<state-dir>/board.json`，
/// 控制面（TUI / desktop）再读同一个文件渲染看板——文件即契约，无需新增 IPC。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Board {
    cards: Vec<Card>,
    next_order: i64,
}

impl Board {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(
        &mut self,
        id: CardId,
        spec_path: PathBuf,
        plan_path: PathBuf,
        now: DateTime<Local>,
    ) -> &Card {
        let order = self.next_order;
        self.next_order += 1;
        let index = self.cards.len();
        self.cards.push(Card {
            id,
            spec_path,
            plan_path,
            state: CardState::Queued,
            enqueued_at: now,
            order,
            workdir: None,
            task_id: None,
            thread_id: None,
            base_commit: None,
        });
        &self.cards[index]
    }

    pub fn get(&self, id: &CardId) -> Option<&Card> {
        self.cards.iter().find(|card| &card.id == id)
    }

    /// 只读遍历全部卡片。CLI 的 `list` 需要报告非排队中的卡片（running /
    /// done / failed / …），而逐个 `get` 需要先知道 id，做不到。
    pub fn cards(&self) -> &[Card] {
        &self.cards
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    pub fn running_count(&self) -> usize {
        self.cards
            .iter()
            .filter(|card| card.state.occupies_slot())
            .count()
    }

    /// 当前还有多少空槽位。`limit` 已由并发日历按时段算好。
    pub fn free_slots(&self, limit: u16) -> usize {
        (limit as usize).saturating_sub(self.running_count())
    }

    /// 记下某张卡片会话要跑在哪个 worktree。插件在启动前调用。
    pub fn set_workdir(&mut self, id: &CardId, workdir: PathBuf) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        card.workdir = Some(workdir);
        Ok(())
    }

    /// 记下某张卡片启动后 daemon 给它的根任务 id，供之后查询是否跑完。
    pub fn set_task_id(&mut self, id: &CardId, task_id: String) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        card.task_id = Some(task_id);
        Ok(())
    }

    /// 记下某张卡片会话在 app-server 里的 thread id。
    pub fn set_thread_id(&mut self, id: &CardId, thread_id: String) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|c| &c.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        card.thread_id = Some(thread_id);
        Ok(())
    }

    /// 记下某张卡片启动时 worktree 的 HEAD，供对账判断「有无新提交」。
    pub fn set_base_commit(
        &mut self,
        id: &CardId,
        base_commit: String,
    ) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|c| &c.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        card.base_commit = Some(base_commit);
        Ok(())
    }

    /// 队首排队卡迁移到 `Launching` 并返回其 id；无名额或队空返回 `None`。
    /// 名额判断与迁移在同一把 `&mut self` 里完成，调用方据此占槽，天然原子。
    pub fn claim_next_launch(&mut self, limit: u16) -> Option<CardId> {
        if self.free_slots(limit) == 0 {
            return None;
        }
        let next = self.next_startable()?;
        if let Some(card) = self.cards.iter_mut().find(|c| c.id == next) {
            card.state = CardState::Launching;
        }
        Some(next)
    }

    /// 队首（`order` 最小）的排队卡片。
    pub fn next_startable(&self) -> Option<CardId> {
        self.cards
            .iter()
            .filter(|card| card.state == CardState::Queued)
            .min_by_key(|card| card.order)
            .map(|card| card.id.clone())
    }

    /// All queued cards in start order, without mutating the board.
    pub fn queued_in_order(&self) -> Vec<CardId> {
        let mut queued: Vec<&Card> = self
            .cards
            .iter()
            .filter(|card| card.state == CardState::Queued)
            .collect();
        queued.sort_by_key(|card| card.order);
        queued.into_iter().map(|card| card.id.clone()).collect()
    }

    /// 在 `limit` 之内启动尽可能多的排队卡片，返回本次启动的卡片（按启动顺序）。
    pub fn start_due(&mut self, limit: u16) -> Vec<CardId> {
        let mut started = Vec::new();
        while self.free_slots(limit) > 0 {
            let Some(next) = self.next_startable() else {
                break;
            };
            if let Some(card) = self.cards.iter_mut().find(|card| card.id == next) {
                card.state = CardState::Running;
                started.push(next);
            } else {
                break;
            }
        }
        started
    }

    /// 迁移一张卡片的状态，非法迁移一律拒绝且不改动队列。
    pub fn transition(&mut self, id: &CardId, next: CardState) -> Result<(), TransitionError> {
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if !card.state.can_transition_to(next) {
            return Err(TransitionError::Illegal {
                from: card.state,
                to: next,
            });
        }
        card.state = next;
        Ok(())
    }

    /// 手动插队：把该卡片的排序键压到当前最小值之下。
    pub fn prioritize(&mut self, id: &CardId) -> Result<(), TransitionError> {
        let min_order = self.cards.iter().map(|card| card.order).min().unwrap_or(0);
        let card = self
            .cards
            .iter_mut()
            .find(|card| &card.id == id)
            .ok_or_else(|| TransitionError::UnknownCard(id.clone()))?;
        if card.state.is_terminal() {
            return Err(TransitionError::Illegal {
                from: card.state,
                to: card.state,
            });
        }
        card.order = min_order.saturating_sub(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32, hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
            .single()
            .expect("valid local time")
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(1, index as u32),
            );
        }
        board
    }

    #[test]
    fn starts_the_oldest_queued_card_first() {
        let mut board = board_with(&["a", "b", "c"]);
        let started = board.start_due(2);
        assert_eq!(
            started,
            vec![CardId::new("a"), CardId::new("b")],
            "FIFO by enqueue time"
        );
    }

    #[test]
    fn never_exceeds_the_slot_limit() {
        let mut board = board_with(&["a", "b", "c"]);
        board.start_due(1);
        assert_eq!(board.running_count(), 1);
        board.start_due(1);
        assert_eq!(
            board.running_count(),
            1,
            "no free slot, nothing else starts"
        );
    }

    #[test]
    fn when_a_running_card_finishes_its_slot_is_reused_by_the_next() {
        let mut board = board_with(&["a", "b", "c"]);
        board.start_due(1);
        board
            .transition(&CardId::new("a"), CardState::AwaitingMerge)
            .unwrap();
        assert_eq!(board.free_slots(1), 1);
        assert_eq!(board.start_due(1), vec![CardId::new("b")]);
    }

    #[test]
    fn needs_you_does_not_hold_the_queue_back() {
        let mut board = board_with(&["a", "b"]);
        board.start_due(1);
        board
            .transition(&CardId::new("a"), CardState::NeedsYou)
            .unwrap();
        assert_eq!(
            board.start_due(1),
            vec![CardId::new("b")],
            "a blocked card frees its slot immediately"
        );
    }

    #[test]
    fn prioritize_moves_a_card_to_the_front() {
        let mut board = board_with(&["a", "b", "c"]);
        board.prioritize(&CardId::new("c")).unwrap();
        assert_eq!(board.next_startable(), Some(CardId::new("c")));
    }

    #[test]
    fn an_illegal_transition_is_rejected() {
        let mut board = board_with(&["a"]);
        let error = board
            .transition(&CardId::new("a"), CardState::AwaitingMerge)
            .unwrap_err();
        assert_eq!(
            error,
            TransitionError::Illegal {
                from: CardState::Queued,
                to: CardState::AwaitingMerge,
            }
        );
    }

    #[test]
    fn a_terminal_card_is_never_started_again() {
        let mut board = board_with(&["a", "b"]);
        board.start_due(1);
        board
            .transition(&CardId::new("a"), CardState::Failed)
            .unwrap();
        board.start_due(1);
        assert_eq!(
            board.get(&CardId::new("a")).unwrap().state,
            CardState::Failed
        );
        assert_eq!(
            board.get(&CardId::new("b")).unwrap().state,
            CardState::Running
        );
    }

    #[test]
    fn a_paused_card_returns_to_the_queue() {
        let mut board = board_with(&["a"]);
        board.start_due(1);
        board
            .transition(&CardId::new("a"), CardState::Paused)
            .unwrap();
        assert_eq!(board.running_count(), 0);
        board
            .transition(&CardId::new("a"), CardState::Queued)
            .unwrap();
        assert_eq!(board.start_due(1), vec![CardId::new("a")]);
    }

    #[test]
    fn a_board_survives_a_serialization_round_trip() {
        // 控制面靠这个文件渲染看板：队列必须能完整地写出去再读回来。
        let mut board = board_with(&["a", "b"]);
        board.start_due(1);
        board
            .set_workdir(&CardId::new("a"), PathBuf::from("/w/a"))
            .unwrap();
        let json = serde_json::to_string(&board).unwrap();
        let restored: Board = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(
            restored.get(&CardId::new("a")).unwrap().state,
            CardState::Running
        );
        assert_eq!(
            restored.get(&CardId::new("a")).unwrap().workdir,
            Some(PathBuf::from("/w/a"))
        );
        assert_eq!(restored.running_count(), 1);
    }

    #[test]
    fn a_card_without_a_workdir_field_still_deserializes() {
        // 前向兼容：早期落盘的 board.json 没有 workdir 字段，读回时必须是 None，
        // 而不是整份状态解析失败。
        let json = r#"{"cards":[{"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T00:00:00+08:00","order":0}],"next_order":1}"#;
        let board: Board = serde_json::from_str(json).unwrap();
        assert_eq!(board.get(&CardId::new("a")).unwrap().workdir, None);
    }
}
