use board_core::board::Board;
use board_core::card::{CardId, CardState};

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

/// How many cards to start now, in FIFO order, without mutating the board.
/// Pure so the scheduling rule is testable without a daemon.
pub fn plan_launches(board: &Board, limit: u16) -> Vec<CardId> {
    let free = board.free_slots(limit);
    board.queued_in_order().into_iter().take(free).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Kept from the brief: the limit is currently supplied by the caller, so the
    // calendar import is not yet exercised by these tests.
    #[allow(unused_imports)]
    use board_core::calendar::ConcurrencyCalendar;
    use chrono::{Local, TimeZone};

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
    fn a_running_card_occupies_one_slot_so_two_more_launch_under_a_limit_of_three() {
        let mut board = board_with(&["a", "b", "c"]);
        board.start_due(1);
        assert_eq!(board.running_count(), 1, "only 'a' is running");
        assert_eq!(
            plan_launches(&board, 3),
            vec![CardId::new("b"), CardId::new("c")],
            "one slot is taken, so two of the three remain"
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
