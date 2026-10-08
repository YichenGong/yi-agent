//! `awaiting_merge` 收敛的纯判据与候选集筛选。
//!
//! 分支是否已并入 base 是 git 事实，本模块不碰 I/O：调用方（runner）把
//! 「分支是否存在」「是否已并入」两个布尔事实喂进来，这里只做分类。
//! 如此三态分类与候选集口径可脱离 git 单测。

use crate::board::Board;
use crate::card::{CardId, CardKind, CardState};

/// 一张 `awaiting_merge` 实现卡（按 git 事实）的三种去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Converge {
    /// 分支已并入 base → 收敛为 `done`。
    Done,
    /// 分支存在但未并入 → 保持 `awaiting_merge`。
    Wait,
    /// 分支不存在 → 无法判定，交人工确认。
    MissingBranch,
}

/// 纯判据：只吃两个布尔事实。
///
/// `existing == false` 时 `merged` 无意义（不可能并入一个不存在的分支），
/// 恒判 `MissingBranch`——绝不用「分支不存在」推断「已合并」。
pub fn classify(existing: bool, merged: bool) -> Converge {
    if !existing {
        Converge::MissingBranch
    } else if merged {
        Converge::Done
    } else {
        Converge::Wait
    }
}

/// 待收敛的实现卡 id：`awaiting_merge`、`implementation`、未归档。
///
/// 排除 `kind == Merge`（合并卡另有合并轮通路）与已归档卡；
/// `needs_you` 不在其列——它的语义是「等人决定」，不自动终态化。
pub fn candidates(board: &Board) -> Vec<CardId> {
    board
        .cards()
        .iter()
        .filter(|card| {
            !card.archived
                && card.state == CardState::AwaitingMerge
                && card.kind == CardKind::Implementation
        })
        .map(|card| card.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Board;
    use crate::card::{CardId, CardKind, CardState};
    use chrono::{Local, TimeZone};
    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    /// 一张停在 `awaiting_merge` 的实现卡（走真实迁移路径）。
    fn board_with_awaiting(id: &str) -> Board {
        let mut board = Board::new();
        let card = CardId::new(id);
        board.enqueue(
            card.clone(),
            format!("{id}.spec.md").into(),
            format!("{id}.plan.md").into(),
            at(0),
        );
        board.transition(&card, CardState::Running).unwrap();
        board.transition(&card, CardState::AwaitingMerge).unwrap();
        board
    }

    #[test]
    fn a_missing_branch_wins_over_any_merge_flag() {
        // 分支不存在时「是否已并入」无意义：绝不用「分支没了」推断「已合并」。
        assert_eq!(classify(false, true), Converge::MissingBranch);
        assert_eq!(classify(false, false), Converge::MissingBranch);
    }

    #[test]
    fn an_existing_merged_branch_converges() {
        assert_eq!(classify(true, true), Converge::Done);
    }

    #[test]
    fn an_existing_unmerged_branch_waits() {
        assert_eq!(classify(true, false), Converge::Wait);
    }

    #[test]
    fn candidates_are_only_awaiting_merge_implementation_cards() {
        let board = board_with_awaiting("a");
        assert_eq!(candidates(&board), vec![CardId::new("a")]);
    }

    #[test]
    fn candidates_skip_needs_you_cards() {
        // needs_you 的语义是「等人决定」，不该被自动终态化。
        let mut board = board_with_awaiting("a");
        // 状态机没有 `awaiting_merge → needs_you` 的直连边（见 card.rs 迁移表）；
        // 现网卡是经「等待验收 → 会话继续跑 → 需人决定」到的 needs_you，故这里走
        // 同一条合法路径。断言只关心 needs_you 不是收敛候选，与到达路径无关。
        board
            .transition(&CardId::new("a"), CardState::Running)
            .unwrap();
        board
            .transition(&CardId::new("a"), CardState::NeedsYou)
            .unwrap();
        assert!(candidates(&board).is_empty());
    }

    #[test]
    fn candidates_skip_archived_cards() {
        // awaiting_merge 不是活跃态，可被归档（隐藏）；归档卡不再参与收敛。
        let mut board = board_with_awaiting("a");
        board.archive(&CardId::new("a")).unwrap();
        assert!(candidates(&board).is_empty());
    }

    #[test]
    fn candidates_skip_queued_merge_cards() {
        // 合并卡（kind=Merge）走自己的合并轮通路，绝不进收敛候选。
        let mut board = Board::new();
        board.enqueue_merge(
            CardId::new("m1"),
            "kanban/x".into(),
            "main".into(),
            None,
            at(0),
        );
        assert!(candidates(&board).is_empty());
        assert_eq!(
            board.get(&CardId::new("m1")).unwrap().kind,
            CardKind::Merge,
            "前置：这确实是一张合并卡"
        );
    }
}
