use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{CardId, CardState};

/// The daemon task states a card can be derived from. Unknown states return
/// `None` so a daemon upgrade never makes the runner act on a guess.
pub fn state_for_task_state(task_state: &str) -> Option<CardState> {
    match task_state {
        "queued" => Some(CardState::Queued),
        "running" | "paused" => Some(CardState::Running),
        "completed" | "completed_no_changes" => Some(CardState::AwaitingMerge),
        "blocked" | "budget_exhausted" | "recovery_required" => Some(CardState::NeedsYou),
        "failed" | "stalled" | "timed_out" => Some(CardState::Failed),
        "cancelled" => Some(CardState::Cancelled),
        _ => None,
    }
}

/// Which cards to start now, in FIFO order, without mutating the board.
///
/// `budget` is the **machine-wide** free capacity (global peak minus the
/// sessions already running anywhere in the host process), already computed by
/// the caller. It is deliberately not reduced by this board's own
/// `running_count`: those sessions are part of the machine-wide count, so
/// subtracting them here would charge them twice.
///
/// Pure so the scheduling rule is testable without a daemon.
pub fn plan_launches(board: &Board, budget: usize) -> Vec<CardId> {
    board.queued_in_order().into_iter().take(budget).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Kept from the brief: the limit is currently supplied by the caller, so the
    // calendar import is not yet exercised by these tests.
    use chrono::{Local, TimeZone};
    #[allow(unused_imports)]
    use superpowers_kanban_core::calendar::ConcurrencyCalendar;

    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(index as u32),
            );
        }
        board
    }

    #[test]
    fn launches_are_bounded_by_the_calendars_limit_for_now() {
        let board = board_with(&["a", "b", "c", "d"]);
        // 工作日 12:00 的上限是 3。
        assert_eq!(
            plan_launches(&board, 3),
            vec![CardId::new("a"), CardId::new("b"), CardId::new("c")]
        );
    }

    #[test]
    fn a_full_queue_launches_nothing_when_the_limit_is_zero() {
        let board = board_with(&["a"]);
        assert!(plan_launches(&board, 0).is_empty());
    }

    #[test]
    fn a_running_card_does_not_shrink_the_budget_again() {
        // `budget` 已经是全机余量：看板内那张 Running 卡也被宿主算进了全机运行
        // 数里，所以 caller 给出 2 就是「还能起 2 个」，不能再减。
        let mut board = board_with(&["a", "b", "c"]);
        board
            .transition(&CardId::new("a"), CardState::Running)
            .unwrap();
        assert_eq!(board.running_count(), 1, "only 'a' is running");
        assert_eq!(
            plan_launches(&board, 2),
            vec![CardId::new("b"), CardId::new("c")],
            "余量由调用方算好，这里不重复扣减"
        );
    }

    #[test]
    fn a_zero_budget_plans_nothing_even_with_a_full_queue() {
        let board = board_with(&["a", "b"]);
        assert!(
            plan_launches(&board, 0).is_empty(),
            "全机没有余量时一个都不许起"
        );
    }

    #[test]
    fn daemon_states_map_to_card_states() {
        assert_eq!(state_for_task_state("running"), Some(CardState::Running));
        assert_eq!(state_for_task_state("queued"), Some(CardState::Queued));
        assert_eq!(
            state_for_task_state("completed"),
            Some(CardState::AwaitingMerge)
        );
        assert_eq!(
            state_for_task_state("completed_no_changes"),
            Some(CardState::AwaitingMerge)
        );
        assert_eq!(
            state_for_task_state("budget_exhausted"),
            Some(CardState::NeedsYou)
        );
        assert_eq!(state_for_task_state("blocked"), Some(CardState::NeedsYou));
        assert_eq!(state_for_task_state("failed"), Some(CardState::Failed));
        assert_eq!(
            state_for_task_state("cancelled"),
            Some(CardState::Cancelled)
        );
    }

    #[test]
    fn an_unknown_daemon_state_yields_none_so_the_card_is_left_alone() {
        assert_eq!(state_for_task_state("something_new"), None);
    }
}
